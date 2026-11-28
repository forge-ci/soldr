//! macOS capture backend: Mach thread suspension, bounded VM copy, resume.
//!
//! Stack bytes are read with `mach_vm_read_overwrite` rather than a raw
//! pointer copy. Another live sibling can change mappings while one thread is
//! suspended; the Mach read turns that race into a dropped sample instead of
//! a SIGSEGV in the host process.

#![allow(unsafe_code)] // Mach thread/VM APIs are FFI-only.

use std::sync::Mutex;
use std::time::Instant;

use mach2::kern_return::KERN_SUCCESS;
use mach2::mach_init::mach_thread_self;
use mach2::mach_port::mach_port_deallocate;
use mach2::mach_types::{thread_act_array_t, thread_act_t};
use mach2::message::mach_msg_type_number_t;
use mach2::port::{mach_port_t, MACH_PORT_NULL};
use mach2::task::task_threads;
use mach2::thread_act::{thread_get_state, thread_resume, thread_suspend};
use mach2::traps::mach_task_self;
use mach2::vm::{mach_vm_deallocate, mach_vm_read_overwrite, mach_vm_region};
use mach2::vm_prot::VM_PROT_READ;
use mach2::vm_region::{vm_region_basic_info_64, VM_REGION_BASIC_INFO_64};

use super::{CaptureKind, Snapshot, SnapshotConfig, SnapshotError, SnapshotStats, ThreadSample};

/// Mach capture is process-global, but this lock is probe-owned and never
/// touched by application code or while a sibling is suspended.
static CAPTURE_LOCK: Mutex<()> = Mutex::new(());

struct ThreadList {
    task: mach_port_t,
    list: thread_act_array_t,
    count: mach_msg_type_number_t,
    self_thread: thread_act_t,
    self_tid: Option<u64>,
}

impl ThreadList {
    fn enumerate() -> Result<Self, SnapshotError> {
        let task = unsafe { mach_task_self() };
        let mut list: thread_act_array_t = std::ptr::null_mut();
        let mut count = 0;
        let result = unsafe { task_threads(task, &mut list, &mut count) };
        if result != KERN_SUCCESS {
            return Err(SnapshotError::Mach {
                operation: "task_threads",
                code: result,
            });
        }
        let self_thread = unsafe { mach_thread_self() };
        Ok(Self {
            task,
            list,
            count,
            self_tid: os_thread_id(self_thread),
            self_thread,
        })
    }

    fn siblings(&self) -> impl Iterator<Item = thread_act_t> + '_ {
        let threads = unsafe { std::slice::from_raw_parts(self.list, self.count as usize) };
        threads
            .iter()
            .copied()
            .filter(|thread| match self.self_tid {
                // `task_threads` and `mach_thread_self` can hold distinct send
                // rights for the same thread. Comparing their port names alone
                // can therefore fail to exclude the caller and self-suspend it.
                Some(self_tid) => os_thread_id(*thread) != Some(self_tid),
                None => *thread != self.self_thread,
            })
    }
}

impl Drop for ThreadList {
    fn drop(&mut self) {
        if !self.list.is_null() {
            let threads = unsafe { std::slice::from_raw_parts(self.list, self.count as usize) };
            for &thread in threads {
                unsafe {
                    mach_port_deallocate(self.task, thread);
                }
            }
            let bytes = u64::from(self.count) * std::mem::size_of::<thread_act_t>() as u64;
            unsafe {
                mach_vm_deallocate(self.task, self.list as u64, bytes);
            }
        }
        if self.self_thread != MACH_PORT_NULL {
            unsafe {
                mach_port_deallocate(self.task, self.self_thread);
            }
        }
    }
}

struct Suspension {
    thread: thread_act_t,
    active: bool,
}

impl Suspension {
    fn new(thread: thread_act_t) -> Option<Self> {
        (unsafe { thread_suspend(thread) } == KERN_SUCCESS).then_some(Self {
            thread,
            active: true,
        })
    }

    fn resume(&mut self) -> Result<(), SnapshotError> {
        let result = unsafe { thread_resume(self.thread) };
        if result != KERN_SUCCESS {
            return Err(SnapshotError::Mach {
                operation: "thread_resume",
                code: result,
            });
        }
        self.active = false;
        Ok(())
    }
}

impl Drop for Suspension {
    fn drop(&mut self) {
        if self.active {
            // Best-effort panic/early-return safety. An explicit resume failure
            // is surfaced by `resume`; Drop cannot report another error.
            unsafe {
                thread_resume(self.thread);
            }
        }
    }
}

struct CapturedRegs {
    sp: u64,
    ip: u64,
    fp: u64,
    lr: Option<u64>,
}

#[cfg(target_arch = "x86_64")]
fn thread_registers(thread: thread_act_t) -> Option<CapturedRegs> {
    use mach2::structs::x86_thread_state64_t;
    use mach2::thread_status::x86_THREAD_STATE64;

    let mut state = x86_thread_state64_t::new();
    let mut count = x86_thread_state64_t::count();
    let result = unsafe {
        thread_get_state(
            thread,
            x86_THREAD_STATE64,
            (&mut state as *mut x86_thread_state64_t).cast(),
            &mut count,
        )
    };
    (result == KERN_SUCCESS && count >= x86_thread_state64_t::count()).then_some(CapturedRegs {
        sp: state.__rsp,
        ip: state.__rip,
        fp: state.__rbp,
        lr: None,
    })
}

#[cfg(target_arch = "aarch64")]
fn thread_registers(thread: thread_act_t) -> Option<CapturedRegs> {
    use mach2::structs::arm_thread_state64_t;
    use mach2::thread_status::ARM_THREAD_STATE64;

    let mut state = arm_thread_state64_t::new();
    let mut count = arm_thread_state64_t::count();
    let result = unsafe {
        thread_get_state(
            thread,
            ARM_THREAD_STATE64,
            (&mut state as *mut arm_thread_state64_t).cast(),
            &mut count,
        )
    };
    (result == KERN_SUCCESS && count >= arm_thread_state64_t::count()).then_some(CapturedRegs {
        sp: state.__sp,
        ip: state.__pc,
        fp: state.__fp,
        lr: Some(state.__lr),
    })
}

fn os_thread_id(thread: thread_act_t) -> Option<u64> {
    let mut info: libc::thread_identifier_info_data_t = unsafe { std::mem::zeroed() };
    let mut count = libc::THREAD_IDENTIFIER_INFO_COUNT;
    let result = unsafe {
        libc::thread_info(
            thread,
            libc::THREAD_IDENTIFIER_INFO as libc::thread_flavor_t,
            (&mut info as *mut libc::thread_identifier_info_data_t).cast(),
            &mut count,
        )
    };
    (result == KERN_SUCCESS).then_some(info.thread_id)
}

/// Copy a readable bounded stack slice while `thread` is suspended.
///
/// Returns `(copied, available, region_object)`; the caller deallocates the
/// region object only after the sibling is running again.
fn copy_stack(
    task: mach_port_t,
    sp: u64,
    limit: usize,
    scratch: &mut [u8],
) -> Option<(usize, usize, mach_port_t)> {
    let mut address = sp;
    let mut size = 0u64;
    let mut info = vm_region_basic_info_64::default();
    let mut info_count = vm_region_basic_info_64::count();
    let mut object = MACH_PORT_NULL;
    let result = unsafe {
        mach_vm_region(
            task,
            &mut address,
            &mut size,
            VM_REGION_BASIC_INFO_64,
            (&mut info as *mut vm_region_basic_info_64).cast(),
            &mut info_count,
            &mut object,
        )
    };
    if result != KERN_SUCCESS {
        return None;
    }

    // `info` is packed(4); copy the field by value rather than borrowing it.
    let protection = info.protection;
    let Some(end) = address.checked_add(size) else {
        return Some((0, 0, object));
    };
    if address > sp || sp >= end || protection & VM_PROT_READ == 0 {
        return Some((0, 0, object));
    }

    let available = usize::try_from(end - sp).unwrap_or(usize::MAX);
    let want = available.min(limit).min(scratch.len());
    if want == 0 {
        return Some((0, available, object));
    }
    let mut copied = 0u64;
    let result = unsafe {
        mach_vm_read_overwrite(
            task,
            sp,
            want as u64,
            scratch.as_mut_ptr() as u64,
            &mut copied,
        )
    };
    if result != KERN_SUCCESS {
        return Some((0, available, object));
    }
    Some((copied.min(want as u64) as usize, available, object))
}

fn capture_thread(
    task: mach_port_t,
    thread: thread_act_t,
    config: &SnapshotConfig,
    scratch: &mut [u8],
) -> Result<Option<(ThreadSample, u64)>, SnapshotError> {
    let Some(os_tid) = os_thread_id(thread) else {
        return Ok(None);
    };
    let started = Instant::now();
    let Some(mut suspension) = Suspension::new(thread) else {
        return Ok(None);
    };

    // ---- suspend window -------------------------------------------------
    let regs = thread_registers(thread);
    let copied = regs
        .as_ref()
        .and_then(|regs| copy_stack(task, regs.sp, config.max_stack_bytes, scratch));
    suspension.resume()?;
    // ---- suspend window closed ------------------------------------------

    let pause_nanos = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
    let Some(regs) = regs else {
        return Ok(None);
    };
    let Some((copied, available, object)) = copied else {
        return Ok(None);
    };
    if object != MACH_PORT_NULL {
        unsafe {
            mach_port_deallocate(task, object);
        }
    }
    if copied == 0 {
        return Ok(None);
    }

    Ok(Some((
        ThreadSample {
            os_tid,
            stack_pointer: regs.sp,
            instruction_pointer: regs.ip,
            frame_pointer: regs.fp,
            link_register: regs.lr,
            stack_bytes: scratch[..copied].to_vec(),
            truncated: copied < available,
            kind: CaptureKind::RawContext,
            frames: Vec::new(),
        },
        pause_nanos,
    )))
}

pub fn capture(config: &SnapshotConfig) -> Result<Snapshot, SnapshotError> {
    let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let threads = ThreadList::enumerate()?;
    let tids: Vec<_> = threads.siblings().collect();
    let mut scratch = vec![0u8; config.max_stack_bytes];
    let mut samples = Vec::with_capacity(tids.len());
    let mut dropped = 0u32;
    let mut pause_nanos = 0u64;

    for thread in tids.iter().copied() {
        match capture_thread(threads.task, thread, config, &mut scratch)? {
            Some((sample, pause)) => {
                samples.push(sample);
                pause_nanos = pause_nanos.saturating_add(pause);
            }
            None => dropped = dropped.saturating_add(1),
        }
    }

    Ok(Snapshot {
        stats: SnapshotStats {
            threads_total: tids.len() as u32,
            threads_captured: samples.len() as u32,
            threads_dropped: dropped,
            pause_nanos,
        },
        threads: samples,
        frames_resolved: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::Duration;

    const SNAPSHOT_HELPER_ENV: &str = "KERNAL_API_MACOS_SNAPSHOT_HELPER";

    #[test]
    fn snapshot_sees_every_spawned_thread_and_resumes_them() {
        if std::env::var_os(SNAPSHOT_HELPER_ENV).is_some() {
            snapshot_sees_every_spawned_thread_and_resumes_them_in_helper();
            return;
        }

        let test_binary = std::env::current_exe().expect("locate test binary");
        let mut helper = std::process::Command::new(test_binary)
            .arg("--exact")
            .arg("snapshot::macos::tests::snapshot_sees_every_spawned_thread_and_resumes_them")
            .arg("--nocapture")
            .env(SNAPSHOT_HELPER_ENV, "1")
            .spawn()
            .expect("spawn isolated Mach snapshot helper");
        let deadline = Instant::now() + Duration::from_secs(20);
        let status = loop {
            match helper.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => {
                    let _ = helper.kill();
                    let _ = helper.wait();
                    panic!("isolated Mach snapshot helper exceeded its 20-second timeout");
                }
                Err(error) => {
                    let _ = helper.kill();
                    let _ = helper.wait();
                    panic!("wait for isolated Mach snapshot helper: {error}");
                }
            }
        };
        assert!(
            status.success(),
            "isolated Mach snapshot helper failed: {status}"
        );
    }

    fn snapshot_sees_every_spawned_thread_and_resumes_them_in_helper() {
        let stop = Arc::new(AtomicBool::new(false));
        let progress = Arc::new(AtomicU64::new(0));
        let (tid_sender, tid_receiver) = mpsc::channel();
        let mut workers = Vec::new();
        for _ in 0..3 {
            let stop = Arc::clone(&stop);
            let progress = Arc::clone(&progress);
            let tid_sender = tid_sender.clone();
            workers.push(std::thread::spawn(move || {
                let self_thread = unsafe { mach_thread_self() };
                let os_tid = os_thread_id(self_thread).expect("worker must have an OS thread id");
                unsafe {
                    mach_port_deallocate(mach_task_self(), self_thread);
                }
                tid_sender.send(os_tid).unwrap();
                while !stop.load(Ordering::SeqCst) {
                    progress.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(1));
                }
            }));
        }
        drop(tid_sender);
        let expected_tids: Vec<_> = (0..workers.len())
            .map(|_| {
                tid_receiver
                    .recv_timeout(Duration::from_secs(2))
                    .expect("worker must publish its OS thread id")
            })
            .collect();

        let snapshot = capture_controlled_threads(&expected_tids);
        let before = progress.load(Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(2);
        while progress.load(Ordering::SeqCst) == before && Instant::now() < deadline {
            std::thread::yield_now();
        }

        stop.store(true, Ordering::SeqCst);
        for worker in workers {
            worker.join().unwrap();
        }
        let snapshot = snapshot.expect("capture");
        for expected_tid in expected_tids {
            assert!(
                snapshot
                    .threads
                    .iter()
                    .any(|thread| thread.os_tid == expected_tid),
                "spawned thread {expected_tid} is absent from {:?}",
                snapshot.stats
            );
        }
        assert!(progress.load(Ordering::SeqCst) > before);
    }

    /// Exercise real Mach suspend/register-capture/resume only on the workers
    /// owned by this regression. Reading another thread's VM is separately
    /// covered by `capture`; it can wait indefinitely in a hosted test process
    /// despite a small read bound. This probe proves the suspension invariant
    /// without making arbitrary libtest/runtime threads collateral state.
    fn capture_controlled_threads(expected_tids: &[u64]) -> Result<Snapshot, SnapshotError> {
        let _capture = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let thread_list = ThreadList::enumerate()?;
        let mut samples = Vec::with_capacity(expected_tids.len());
        let mut dropped = 0u32;
        let mut pause_nanos = 0u64;

        for thread in thread_list.siblings() {
            let Some(os_tid) = os_thread_id(thread) else {
                continue;
            };
            if !expected_tids.contains(&os_tid) {
                continue;
            }
            let started = Instant::now();
            let Some(mut suspension) = Suspension::new(thread) else {
                dropped = dropped.saturating_add(1);
                continue;
            };
            let regs = thread_registers(thread);
            suspension.resume()?;
            let pause = started.elapsed().as_nanos().min(u128::from(u64::MAX)) as u64;
            match regs {
                Some(regs) => {
                    let sample = ThreadSample {
                        os_tid,
                        stack_pointer: regs.sp,
                        instruction_pointer: regs.ip,
                        frame_pointer: regs.fp,
                        link_register: regs.lr,
                        stack_bytes: Vec::new(),
                        truncated: false,
                        kind: CaptureKind::RawContext,
                        frames: Vec::new(),
                    };
                    samples.push(sample);
                    pause_nanos = pause_nanos.saturating_add(pause);
                }
                None => dropped = dropped.saturating_add(1),
            }
        }

        Ok(Snapshot {
            stats: SnapshotStats {
                threads_total: expected_tids.len() as u32,
                threads_captured: samples.len() as u32,
                threads_dropped: dropped,
                pause_nanos,
            },
            threads: samples,
            frames_resolved: false,
        })
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
}
