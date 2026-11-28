//! Windows capture backend: `SuspendThread` → `GetThreadContext` → bounded
//! stack copy → `ResumeThread`.
//!
//! The ordering here is load-bearing. See the module docs in
//! [`super`] for why nothing but a register read and a `memcpy` may happen
//! while a thread is suspended.

#![allow(unsafe_code)] // Thread enumeration, suspension, and context reads are FFI-only.

use std::io;
use std::time::Instant;

use winapi::shared::minwindef::FALSE;
use winapi::um::handleapi::{CloseHandle, INVALID_HANDLE_VALUE};
use winapi::um::memoryapi::VirtualQuery;
use winapi::um::processthreadsapi::{
    GetCurrentProcessId, GetCurrentThreadId, GetThreadContext, OpenThread, ResumeThread,
    SuspendThread,
};
use winapi::um::tlhelp32::{
    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use winapi::um::winnt::{
    CONTEXT, MEMORY_BASIC_INFORMATION, MEM_COMMIT, THREAD_GET_CONTEXT,
    THREAD_QUERY_LIMITED_INFORMATION, THREAD_SUSPEND_RESUME,
};

/// Register set to request. x86_64 exposes a `CONTEXT_FULL` alias; aarch64
/// does not, so compose control+integer, which is what the stack walk needs.
#[cfg(target_arch = "x86_64")]
const WANTED_CONTEXT: u32 = winapi::um::winnt::CONTEXT_FULL;
#[cfg(target_arch = "aarch64")]
const WANTED_CONTEXT: u32 = winapi::um::winnt::CONTEXT_CONTROL | winapi::um::winnt::CONTEXT_INTEGER;

/// `CONTEXT` with the alignment `GetThreadContext` requires.
///
/// winapi's `CONTEXT` carries no alignment attribute (its aarch64 definition
/// even says `// FIXME align 16`), but the API requires 16-byte alignment on
/// both architectures and fails with `ERROR_NOACCESS` otherwise. A bare local
/// is aligned only by accident of stack layout — which is exactly the kind of
/// bug that appears when unrelated code shifts the frame, so pin it here.
#[repr(align(16))]
struct AlignedContext(CONTEXT);

/// Registers the unwinder needs, named per architecture.
struct CapturedRegs {
    sp: u64,
    ip: u64,
    fp: u64,
    /// Link register. `Some` only on aarch64.
    lr: Option<u64>,
}

/// Pull the unwind-relevant registers out of a `CONTEXT`.
///
/// The whole architecture difference in this file lives here.
#[cfg(target_arch = "x86_64")]
fn captured_regs(context: &CONTEXT) -> CapturedRegs {
    CapturedRegs {
        sp: context.Rsp,
        ip: context.Rip,
        fp: context.Rbp,
        // x86_64 has no link register; the return address is on the stack.
        lr: None,
    }
}

/// # Safety
///
/// Reads the aarch64 `CONTEXT` union, which is initialized by
/// `GetThreadContext` before this is called.
#[cfg(target_arch = "aarch64")]
fn captured_regs(context: &CONTEXT) -> CapturedRegs {
    // Fp/Lr live in the X-register union rather than as named top-level
    // fields the way Rbp does on x86_64.
    let s = unsafe { context.u.s() };
    CapturedRegs {
        sp: context.Sp,
        ip: context.Pc,
        fp: s.Fp,
        // Load-bearing: a leaf frame's return address is in LR, not on the
        // stack, so dropping it loses the innermost frame.
        lr: Some(s.Lr),
    }
}

use super::{CaptureKind, Snapshot, SnapshotConfig, SnapshotError, SnapshotStats, ThreadSample};

/// `SuspendThread` returns this on failure.
const SUSPEND_FAILED: u32 = u32::MAX;

/// Enumerate sibling thread ids via a toolhelp snapshot.
///
/// Excludes the calling thread — suspending yourself never returns.
fn sibling_thread_ids() -> io::Result<Vec<u32>> {
    let pid = unsafe { GetCurrentProcessId() };
    let self_tid = unsafe { GetCurrentThreadId() };

    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snap == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }

    let mut ids = Vec::new();
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;

    let mut ok = unsafe { Thread32First(snap, &mut entry) };
    while ok != FALSE {
        if entry.th32OwnerProcessID == pid && entry.th32ThreadID != self_tid {
            ids.push(entry.th32ThreadID);
        }
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        ok = unsafe { Thread32Next(snap, &mut entry) };
    }

    unsafe { CloseHandle(snap) };
    Ok(ids)
}

/// Highest address we may read from, given a stack pointer.
///
/// The stack region's committed extent bounds the copy. Without this a
/// fixed-size read past the top of the stack would touch unmapped pages and
/// fault — while holding a thread suspended.
fn committed_stack_top(sp: u64) -> Option<u64> {
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    let queried = unsafe {
        VirtualQuery(
            sp as *const _,
            &mut info,
            std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    if queried == 0 || info.State != MEM_COMMIT {
        return None;
    }
    Some(info.BaseAddress as u64 + info.RegionSize as u64)
}

/// Capture one thread. Returns `None` if it could not be captured, which the
/// caller counts as a drop rather than failing the whole snapshot — a thread
/// exiting mid-capture is normal.
///
/// `scratch` is preallocated by the caller so the suspend window contains no
/// allocation.
fn capture_thread(tid: u32, max_stack_bytes: usize, scratch: &mut [u8]) -> Option<ThreadSample> {
    let handle = unsafe {
        OpenThread(
            THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_LIMITED_INFORMATION,
            FALSE,
            tid,
        )
    };
    if handle.is_null() {
        return None;
    }

    // ---- suspend window opens ------------------------------------------
    // Only a register read and a bounded memcpy may happen from here until
    // ResumeThread. No allocation, no locks, no logging.
    let suspended = unsafe { SuspendThread(handle) };
    if suspended == SUSPEND_FAILED {
        unsafe { CloseHandle(handle) };
        return None;
    }

    let mut aligned: AlignedContext = unsafe { std::mem::zeroed() };
    aligned.0.ContextFlags = WANTED_CONTEXT;
    let got_context = unsafe { GetThreadContext(handle, &mut aligned.0) };

    let mut copied = 0usize;
    let regs = if got_context != FALSE {
        let regs = captured_regs(&aligned.0);
        let sp = regs.sp;
        if let Some(top) = committed_stack_top(sp) {
            let available = top.saturating_sub(sp) as usize;
            let want = available.min(max_stack_bytes).min(scratch.len());
            if want > 0 {
                unsafe {
                    std::ptr::copy_nonoverlapping(sp as *const u8, scratch.as_mut_ptr(), want);
                }
                copied = want;
            }
        }
        regs
    } else {
        CapturedRegs {
            sp: 0,
            ip: 0,
            fp: 0,
            lr: None,
        }
    };

    unsafe { ResumeThread(handle) };
    // ---- suspend window closes -----------------------------------------

    unsafe { CloseHandle(handle) };

    if got_context == FALSE {
        return None;
    }

    // Allocation happens here, after the thread is running again.
    let stack_bytes = scratch[..copied].to_vec();
    let truncated = copied == max_stack_bytes;

    Some(ThreadSample {
        os_tid: u64::from(tid),
        stack_pointer: regs.sp,
        instruction_pointer: regs.ip,
        frame_pointer: regs.fp,
        link_register: regs.lr,
        stack_bytes,
        truncated,
        kind: CaptureKind::RawContext,
        frames: Vec::new(),
    })
}

/// Capture every sibling thread.
pub fn capture(config: &SnapshotConfig) -> Result<Snapshot, SnapshotError> {
    let tids = sibling_thread_ids()?;

    // Preallocated once, reused per thread, so no allocator call occurs inside
    // any suspend window.
    let mut scratch = vec![0u8; config.max_stack_bytes];

    let mut threads = Vec::with_capacity(tids.len());
    let mut dropped = 0u32;
    let started = Instant::now();

    for tid in &tids {
        match capture_thread(*tid, config.max_stack_bytes, &mut scratch) {
            Some(sample) => threads.push(sample),
            None => dropped += 1,
        }
    }

    let pause_nanos = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;

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
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// The probe thread must never appear in its own enumeration.
    #[test]
    fn enumeration_excludes_the_calling_thread() {
        let self_tid = unsafe { GetCurrentThreadId() };
        let ids = sibling_thread_ids().expect("enumerate");
        assert!(
            !ids.contains(&self_tid),
            "suspending the calling thread would deadlock immediately"
        );
    }

    /// All spawned threads must show up in the snapshot.
    #[test]
    fn snapshot_sees_every_spawned_thread() {
        const N: usize = 4;
        let stop = Arc::new(AtomicBool::new(false));
        let running = Arc::new(AtomicU64::new(0));

        let handles: Vec<_> = (0..N)
            .map(|_| {
                let stop = Arc::clone(&stop);
                let running = Arc::clone(&running);
                std::thread::spawn(move || {
                    running.fetch_add(1, Ordering::SeqCst);
                    while !stop.load(Ordering::Relaxed) {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                })
            })
            .collect();

        while running.load(Ordering::SeqCst) < N as u64 {
            std::thread::sleep(Duration::from_millis(5));
        }

        let snap = capture(&SnapshotConfig::default()).expect("capture");
        assert!(
            snap.stats.threads_captured >= N as u32,
            "expected at least the {N} spawned threads, got {:?}",
            snap.stats
        );

        stop.store(true, Ordering::Relaxed);
        for h in handles {
            h.join().unwrap();
        }
    }

    /// Every thread must be running again after the capture.
    ///
    /// A missed `ResumeThread` leaves the process wedged, and the symptom
    /// would otherwise appear far from the cause.
    #[test]
    fn threads_make_progress_after_capture() {
        let stop = Arc::new(AtomicBool::new(false));
        let ticks = Arc::new(AtomicU64::new(0));

        let worker = {
            let stop = Arc::clone(&stop);
            let ticks = Arc::clone(&ticks);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    ticks.fetch_add(1, Ordering::Relaxed);
                    std::thread::sleep(Duration::from_millis(1));
                }
            })
        };

        while ticks.load(Ordering::Relaxed) == 0 {
            std::thread::sleep(Duration::from_millis(1));
        }

        capture(&SnapshotConfig::default()).expect("capture");

        let before = ticks.load(Ordering::Relaxed);
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut progressed = false;
        while Instant::now() < deadline {
            if ticks.load(Ordering::Relaxed) > before {
                progressed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        assert!(
            progressed,
            "a thread stopped ticking after capture — it was not resumed"
        );
    }

    /// The capture must not need any lock the application holds.
    ///
    /// A thread is suspended while holding a mutex that the capturing thread
    /// never touches; the snapshot must still complete. If the capture path
    /// allocated or logged inside the suspend window, this is the shape of
    /// test that catches the resulting deadlock.
    #[test]
    fn capture_completes_while_a_thread_holds_an_app_lock() {
        let lock = Arc::new(Mutex::new(0u64));
        let stop = Arc::new(AtomicBool::new(false));
        let holding = Arc::new(AtomicBool::new(false));

        let hog = {
            let lock = Arc::clone(&lock);
            let stop = Arc::clone(&stop);
            let holding = Arc::clone(&holding);
            std::thread::spawn(move || {
                let _guard = lock.lock().unwrap();
                holding.store(true, Ordering::SeqCst);
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(5));
                }
            })
        };

        while !holding.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(5));
        }

        let snap = capture(&SnapshotConfig::default()).expect("capture must not block");
        assert!(snap.stats.threads_captured > 0);

        stop.store(true, Ordering::Relaxed);
        hog.join().unwrap();
    }

    /// The copy honors its bound.
    #[test]
    fn stack_copy_respects_the_configured_cap() {
        let cfg = SnapshotConfig {
            max_stack_bytes: 4096,
        };
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(5));
                }
            })
        };
        std::thread::sleep(Duration::from_millis(20));

        let snap = capture(&cfg).expect("capture");
        for sample in &snap.threads {
            assert!(
                sample.stack_bytes.len() <= cfg.max_stack_bytes,
                "copied {} bytes, cap is {}",
                sample.stack_bytes.len(),
                cfg.max_stack_bytes
            );
        }

        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();
    }

    /// Captures are raw until unwinding lands.
    #[test]
    fn samples_are_marked_raw_and_unresolved() {
        let snap = capture(&SnapshotConfig::default()).expect("capture");
        assert!(!snap.frames_resolved);
        for sample in &snap.threads {
            assert_eq!(sample.kind, CaptureKind::RawContext);
        }
    }
}
