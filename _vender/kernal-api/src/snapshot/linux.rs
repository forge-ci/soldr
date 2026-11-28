//! Linux cooperative capture using a reserved realtime signal.
//!
//! A targeted signal supplies the sibling's `ucontext_t`. The handler copies
//! only the three/four unwind registers into atomics and then waits on a
//! probe-owned release flag. The probe thread copies the bounded stack slice
//! while the sibling is stopped in that handler, then releases it immediately.
//! No allocator, lock, stdio, or non-async-signal-safe libc call executes in
//! the handler.
//!
//! # Signal ownership contract
//!
//! The first capture reserves one otherwise-unused realtime signal for the
//! process lifetime. Application code must not replace that signal's
//! disposition or consume it through `signalfd` afterward. Concurrent
//! `sigaction` changes violate this backend's install-time ownership contract;
//! the preflight checks diagnose stable conflicts but cannot make an
//! application's unsynchronized disposition replacement safe.

#![allow(unsafe_code)] // ucontext, sigaction, tgkill, and raw stack copies are FFI-only.

use std::fs;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::{CaptureKind, Snapshot, SnapshotConfig, SnapshotError, SnapshotStats, ThreadSample};

/// One signal wait cannot stall an on-demand snapshot indefinitely.
const SIGNAL_DEADLINE: Duration = Duration::from_millis(250);

/// Capture calls share the process-wide signal handler/slot but no app lock.
static CAPTURE_LOCK: Mutex<()> = Mutex::new(());
static CAPTURE_SIGNAL: OnceLock<Result<i32, i32>> = OnceLock::new();
static ACTIVE_SLOT: AtomicPtr<SignalSlot> = AtomicPtr::new(std::ptr::null_mut());
static ACTIVE_TID: AtomicI32 = AtomicI32::new(0);

/// State exchanged with the handler. Atomics make the handler writes legal;
/// casting `&T` to `*mut T` here would be undefined behavior.
struct SignalSlot {
    ready: AtomicBool,
    release: AtomicBool,
    done: AtomicBool,
    stack_pointer: AtomicU64,
    instruction_pointer: AtomicU64,
    frame_pointer: AtomicU64,
    link_register: AtomicU64,
}

impl SignalSlot {
    fn new() -> Self {
        Self {
            ready: AtomicBool::new(false),
            release: AtomicBool::new(false),
            done: AtomicBool::new(false),
            stack_pointer: AtomicU64::new(0),
            instruction_pointer: AtomicU64::new(0),
            frame_pointer: AtomicU64::new(0),
            link_register: AtomicU64::new(0),
        }
    }
}

/// Register values extracted from the kernel-provided signal context.
struct CapturedRegs {
    sp: u64,
    ip: u64,
    fp: u64,
    lr: u64,
}

#[cfg(target_arch = "x86_64")]
unsafe fn registers_from_context(context: *mut libc::c_void) -> CapturedRegs {
    let context = &*(context.cast::<libc::ucontext_t>());
    let gregs = &context.uc_mcontext.gregs;
    CapturedRegs {
        sp: gregs[libc::REG_RSP as usize] as u64,
        ip: gregs[libc::REG_RIP as usize] as u64,
        fp: gregs[libc::REG_RBP as usize] as u64,
        lr: 0,
    }
}

#[cfg(target_arch = "aarch64")]
unsafe fn registers_from_context(context: *mut libc::c_void) -> CapturedRegs {
    let context = &*(context.cast::<libc::ucontext_t>());
    CapturedRegs {
        sp: context.uc_mcontext.sp,
        ip: context.uc_mcontext.pc,
        // AArch64 x29 = FP, x30 = LR.
        fp: context.uc_mcontext.regs[29],
        lr: context.uc_mcontext.regs[30],
    }
}

/// The workspace ships Linux only on architectures framehop supports.
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
unsafe fn registers_from_context(_context: *mut libc::c_void) -> CapturedRegs {
    CapturedRegs {
        sp: 0,
        ip: 0,
        fp: 0,
        lr: 0,
    }
}

/// Signal handler: register copy + atomics only.
extern "C" fn capture_signal_handler(
    _signal: libc::c_int,
    _info: *mut libc::siginfo_t,
    context: *mut libc::c_void,
) {
    let slot = ACTIVE_SLOT.load(Ordering::Acquire);
    if slot.is_null() {
        return;
    }

    // SYS_gettid is async-signal-safe: it is a direct syscall, with no libc
    // state or allocator involved.
    let tid = unsafe { libc::syscall(libc::SYS_gettid) as libc::pid_t };
    if ACTIVE_TID.load(Ordering::Acquire) != tid {
        return;
    }

    let regs = unsafe { registers_from_context(context) };
    let slot = unsafe { &*slot };
    slot.stack_pointer.store(regs.sp, Ordering::Relaxed);
    slot.instruction_pointer.store(regs.ip, Ordering::Relaxed);
    slot.frame_pointer.store(regs.fp, Ordering::Relaxed);
    slot.link_register.store(regs.lr, Ordering::Relaxed);
    slot.ready.store(true, Ordering::Release);

    // Holding here is the Linux equivalent of SuspendThread. Only the probe
    // thread can release us, after it has copied a bounded readable slice.
    while !slot.release.load(Ordering::Acquire) {
        std::hint::spin_loop();
    }
    slot.done.store(true, Ordering::Release);
}

/// Install one otherwise-unused realtime signal for the process lifetime.
///
/// A persistent reservation is intentional. Restoring SIG_DFL after a timed
/// out delivery could let the still-pending realtime signal terminate the
/// process later. Signals already owned by the application are skipped.
fn capture_signal() -> io::Result<i32> {
    match CAPTURE_SIGNAL.get_or_init(|| {
        let min = libc::SIGRTMIN();
        let max = libc::SIGRTMAX();
        for signal in min..=max {
            let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
            if unsafe { libc::sigaction(signal, std::ptr::null(), &mut old) } != 0 {
                continue;
            }
            if old.sa_sigaction != libc::SIG_DFL {
                continue;
            }

            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = capture_signal_handler as *const () as usize;
            action.sa_flags = libc::SA_SIGINFO | libc::SA_RESTART;
            // Defer all maskable handlers while the target stack is frozen so
            // no nested application handler mutates that stack during the VM
            // read. The kernel restores the prior mask on handler return.
            unsafe { libc::sigfillset(&mut action.sa_mask) };
            if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } == 0 {
                return Ok(signal);
            }
        }
        Err(io::Error::last_os_error()
            .raw_os_error()
            .unwrap_or(libc::EBUSY))
    }) {
        Ok(signal) => Ok(*signal),
        Err(errno) => Err(io::Error::from_raw_os_error(*errno)),
    }
}

/// Confirm the application has not claimed or reset the reserved signal.
///
/// Reinstalling our handler would overwrite application ownership. This is a
/// diagnostic preflight for the lifetime contract documented above, not a
/// synchronization primitive for concurrent `sigaction` replacement.
fn signal_handler_is_ours(signal: i32) -> io::Result<bool> {
    let mut current: libc::sigaction = unsafe { std::mem::zeroed() };
    if unsafe { libc::sigaction(signal, std::ptr::null(), &mut current) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(current.sa_sigaction == capture_signal_handler as *const () as usize)
}

fn sibling_thread_ids() -> io::Result<Vec<i32>> {
    let self_tid = unsafe { libc::syscall(libc::SYS_gettid) as i32 };
    let mut tids = Vec::new();
    for entry in fs::read_dir("/proc/self/task")? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(tid) = name.parse::<i32>() else {
            continue;
        };
        if tid != self_tid {
            tids.push(tid);
        }
    }
    tids.sort_unstable();
    Ok(tids)
}

/// Whether `signal` is blocked by `tid`, based on the kernel's status view.
///
/// Avoiding delivery to an already-blocked thread prevents a pending signal
/// from outliving its capture slot. A mask change racing this check is handled
/// by the deadline and persistent handler.
fn signal_is_blocked(tid: i32, signal: i32) -> io::Result<bool> {
    let status = fs::read_to_string(format!("/proc/self/task/{tid}/status"))?;
    let mask = status
        .lines()
        .find_map(|line| line.strip_prefix("SigBlk:"))
        .map(str::trim)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "SigBlk missing"))?;
    let bits = u128::from_str_radix(mask, 16)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let bit = u32::try_from(signal - 1)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid signal"))?;
    Ok(bits & (1u128 << bit) != 0)
}

/// Readable mapping containing `address`, parsed before any sibling pauses.
fn readable_mapping_top(address: u64, maps: &[(u64, u64)]) -> Option<u64> {
    maps.iter()
        .find(|(start, end)| *start <= address && address < *end)
        .map(|(_, end)| *end)
}

fn readable_mappings() -> io::Result<Vec<(u64, u64)>> {
    let mut ranges = Vec::new();
    for line in fs::read_to_string("/proc/self/maps")?.lines() {
        let mut fields = line.split_whitespace();
        let Some(range) = fields.next() else {
            continue;
        };
        let Some(perms) = fields.next() else {
            continue;
        };
        if !perms.starts_with('r') {
            continue;
        }
        let Some((start, end)) = range.split_once('-') else {
            continue;
        };
        let Ok(start) = u64::from_str_radix(start, 16) else {
            continue;
        };
        let Ok(end) = u64::from_str_radix(end, 16) else {
            continue;
        };
        ranges.push((start, end));
    }
    ranges.sort_unstable();
    Ok(ranges)
}

fn wait_until_ready(slot: &SignalSlot) -> bool {
    let deadline = Instant::now() + SIGNAL_DEADLINE;
    while Instant::now() < deadline {
        if slot.ready.load(Ordering::Acquire) {
            return true;
        }
        std::thread::yield_now();
    }
    false
}

fn capture_thread(
    tid: i32,
    signal: i32,
    config: &SnapshotConfig,
    maps: &[(u64, u64)],
    scratch: &mut [u8],
) -> Option<(ThreadSample, u64)> {
    if !signal_handler_is_ours(signal).ok()? {
        return None;
    }
    if signal_is_blocked(tid, signal).ok()? {
        return None;
    }

    // Heap ownership lets a timed-out slot be intentionally leaked if a
    // signal raced the deadline. That pathological path is preferable to
    // letting a late handler access a dead stack frame.
    let slot = Box::new(SignalSlot::new());
    ACTIVE_TID.store(tid, Ordering::Release);
    ACTIVE_SLOT.store((&*slot as *const SignalSlot).cast_mut(), Ordering::Release);

    let pid = unsafe { libc::getpid() };
    let started = Instant::now();
    let sent = unsafe { libc::syscall(libc::SYS_tgkill, pid, tid, signal) };
    if sent != 0 || !wait_until_ready(&slot) {
        slot.release.store(true, Ordering::Release);
        ACTIVE_SLOT.store(std::ptr::null_mut(), Ordering::Release);
        ACTIVE_TID.store(0, Ordering::Release);
        if !slot.done.load(Ordering::Acquire) {
            // Delivery may be pending or the handler may already hold the
            // pointer. Keep its storage alive for the process lifetime.
            Box::leak(slot);
        }
        return None;
    }

    let sp = slot.stack_pointer.load(Ordering::Relaxed);
    let top = readable_mapping_top(sp, maps).unwrap_or(sp);
    let available = top.saturating_sub(sp) as usize;
    let wanted = available.min(config.max_stack_bytes).min(scratch.len());
    let copied = if wanted == 0 {
        0
    } else {
        let local = libc::iovec {
            iov_base: scratch.as_mut_ptr().cast(),
            iov_len: wanted,
        };
        let remote = libc::iovec {
            iov_base: sp as *mut libc::c_void,
            iov_len: wanted,
        };
        // Unlike a raw memcpy, process_vm_readv reports EFAULT/short reads if
        // another live sibling invalidates the mapping during capture.
        let result = unsafe { libc::process_vm_readv(pid, &local, 1, &remote, 1, 0) };
        usize::try_from(result).unwrap_or(0).min(wanted)
    };

    // Release before allocation or constructing the sample.
    slot.release.store(true, Ordering::Release);
    while !slot.done.load(Ordering::Acquire) {
        std::hint::spin_loop();
    }
    let pause_nanos = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
    ACTIVE_SLOT.store(std::ptr::null_mut(), Ordering::Release);
    ACTIVE_TID.store(0, Ordering::Release);

    let lr = slot.link_register.load(Ordering::Relaxed);
    Some((
        ThreadSample {
            os_tid: tid as u64,
            stack_pointer: sp,
            instruction_pointer: slot.instruction_pointer.load(Ordering::Relaxed),
            frame_pointer: slot.frame_pointer.load(Ordering::Relaxed),
            link_register: (lr != 0).then_some(lr),
            stack_bytes: scratch[..copied].to_vec(),
            truncated: copied < available,
            kind: CaptureKind::RawContext,
            frames: Vec::new(),
        },
        pause_nanos,
    ))
}

pub fn capture(config: &SnapshotConfig) -> Result<Snapshot, SnapshotError> {
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        let _ = config;
        return Err(SnapshotError::Unsupported);
    }

    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let signal = capture_signal()?;
    if !signal_handler_is_ours(signal)? {
        return Err(SnapshotError::Os(io::Error::new(
            io::ErrorKind::ResourceBusy,
            "reserved snapshot signal disposition was replaced",
        )));
    }
    let tids = sibling_thread_ids()?;
    let maps = readable_mappings()?;
    let mut scratch = vec![0u8; config.max_stack_bytes];
    let mut threads = Vec::with_capacity(tids.len());
    let mut dropped = 0u32;
    let mut pause_nanos = 0u64;

    for &tid in &tids {
        match capture_thread(tid, signal, config, &maps, &mut scratch) {
            Some((sample, pause)) => {
                pause_nanos = pause_nanos.saturating_add(pause);
                threads.push(sample);
            }
            None => dropped = dropped.saturating_add(1),
        }
    }

    Ok(Snapshot {
        stats: SnapshotStats {
            threads_total: tids.len() as u32,
            threads_captured: threads.len() as u32,
            threads_dropped: dropped,
            pause_nanos,
        },
        threads,
        frames_resolved: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Barrier, Mutex};

    #[test]
    fn snapshot_sees_every_spawned_thread_and_resumes_them() {
        let stop = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(0));
        let ready = Arc::new(Barrier::new(4));
        let mut workers = Vec::new();
        for _ in 0..3 {
            let stop = Arc::clone(&stop);
            let progress = Arc::clone(&progress);
            let ready = Arc::clone(&ready);
            workers.push(std::thread::spawn(move || {
                ready.wait();
                while !stop.load(Ordering::Relaxed) {
                    progress.fetch_add(1, Ordering::Relaxed);
                    std::hint::spin_loop();
                }
            }));
        }
        ready.wait();

        let snapshot = capture(&SnapshotConfig::default()).expect("capture");
        let before = progress.load(Ordering::Relaxed);
        let deadline = Instant::now() + Duration::from_secs(2);
        while progress.load(Ordering::Relaxed) == before && Instant::now() < deadline {
            std::thread::yield_now();
        }

        stop.store(true, Ordering::Relaxed);
        for worker in workers {
            worker.join().unwrap();
        }

        assert_eq!(
            snapshot.stats.threads_total,
            snapshot.stats.threads_captured + snapshot.stats.threads_dropped,
            "every enumerated thread is accounted for: {:?}",
            snapshot.stats
        );
        assert!(
            snapshot.stats.threads_captured >= 3,
            "all three workers must be present: {:?}",
            snapshot.stats
        );
        assert!(
            progress.load(Ordering::Relaxed) > before,
            "workers did not resume after capture"
        );
    }

    #[test]
    fn capture_does_not_wait_on_an_application_mutex() {
        let held = Arc::new(Mutex::new(()));
        let acquired = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let worker = {
            let held = Arc::clone(&held);
            let acquired = Arc::clone(&acquired);
            let release = Arc::clone(&release);
            std::thread::spawn(move || {
                let _guard = held.lock().unwrap();
                acquired.store(true, Ordering::Release);
                while !release.load(Ordering::Acquire) {
                    std::hint::spin_loop();
                }
            })
        };
        while !acquired.load(Ordering::Acquire) {
            std::thread::yield_now();
        }

        let started = Instant::now();
        let snapshot = capture(&SnapshotConfig::default()).expect("capture");
        release.store(true, Ordering::Release);
        worker.join().unwrap();

        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(snapshot.stats.threads_captured >= 1);
    }

    #[test]
    fn mapping_lookup_bounds_stack_copy() {
        let ranges = [(0x1000, 0x2000), (0x4000, 0x5000)];
        assert_eq!(readable_mapping_top(0x1800, &ranges), Some(0x2000));
        assert_eq!(readable_mapping_top(0x3000, &ranges), None);
    }

    #[test]
    fn replaced_signal_handler_is_detected_before_delivery() {
        let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let signal = capture_signal().expect("reserve signal");
        let mut ours: libc::sigaction = unsafe { std::mem::zeroed() };
        assert_eq!(
            unsafe { libc::sigaction(signal, std::ptr::null(), &mut ours) },
            0
        );

        let mut ignored: libc::sigaction = unsafe { std::mem::zeroed() };
        ignored.sa_sigaction = libc::SIG_IGN;
        unsafe { libc::sigemptyset(&mut ignored.sa_mask) };
        assert_eq!(
            unsafe { libc::sigaction(signal, &ignored, std::ptr::null_mut()) },
            0
        );
        assert!(!signal_handler_is_ours(signal).expect("query disposition"));

        assert_eq!(
            unsafe { libc::sigaction(signal, &ours, std::ptr::null_mut()) },
            0
        );
        assert!(signal_handler_is_ours(signal).expect("restored disposition"));
    }

    #[test]
    fn reserved_signal_ownership_is_stable_under_concurrent_preflight() {
        let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let signal = capture_signal().expect("reserve signal");
        let workers: Vec<_> = (0..4)
            .map(|_| {
                std::thread::spawn(move || {
                    for _ in 0..256 {
                        assert!(signal_handler_is_ours(signal).expect("query disposition"));
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
    }
}
