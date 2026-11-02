//! macOS test-process supervision (see `platform/process/test_child.rs`).

use crate::platform::process::test_child::TestSignal;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicI32, Ordering};

/// Before exec, in the child: start a new session. macOS has no
/// `PR_SET_PDEATHSIG`/`PR_SET_PTRACER` and no cgroups.
pub fn configure_test_child(command: &mut Command, _parent_pid: u32, _cgroup_procs: Option<&Path>) {
    // SAFETY: the hook only calls an async-signal-safe libc function.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
}

fn raw(signal: TestSignal) -> libc::c_int {
    match signal {
        TestSignal::Terminate => libc::SIGTERM,
        TestSignal::Interrupt => libc::SIGINT,
        TestSignal::Kill => libc::SIGKILL,
    }
}

fn pid_t(pid: u32) -> Option<libc::pid_t> {
    libc::pid_t::try_from(pid).ok().filter(|pid| *pid > 0)
}

/// Signal the process group led by `pid`; a vanished group is not an error.
pub fn signal_group(pid: u32, signal: TestSignal) {
    if let Some(pid) = pid_t(pid) {
        // SAFETY: plain syscall.
        unsafe { libc::killpg(pid, raw(signal)) };
    }
}

/// Signal one process; a vanished process is not an error.
pub fn signal_process(pid: u32, signal: TestSignal) {
    if let Some(pid) = pid_t(pid) {
        // SAFETY: plain syscall.
        unsafe { libc::kill(pid, raw(signal)) };
    }
}

/// `kill(pid, 0)` liveness: a process we may not signal still exists.
pub fn pid_alive(pid: u32) -> bool {
    let Some(pid) = pid_t(pid) else {
        return false;
    };
    // SAFETY: plain syscall.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

static RECEIVED: AtomicI32 = AtomicI32::new(0);

extern "C" fn record_termination(signal: libc::c_int) {
    let _ = RECEIVED.compare_exchange(0, signal, Ordering::SeqCst, Ordering::SeqCst);
}

/// Record the first SIGTERM/SIGINT for [`received_termination`] instead of
/// dying. Exec resets the handlers, so the test itself is unaffected.
pub fn install_termination_flag() {
    for signal in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: the handler only performs an atomic store.
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = record_termination as extern "C" fn(libc::c_int) as usize;
            libc::sigemptyset(&mut action.sa_mask);
            action.sa_flags = libc::SA_RESTART;
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
}

/// The first termination signal received since [`install_termination_flag`].
pub fn received_termination() -> Option<TestSignal> {
    match RECEIVED.load(Ordering::SeqCst) {
        libc::SIGTERM => Some(TestSignal::Terminate),
        libc::SIGINT => Some(TestSignal::Interrupt),
        _ => None,
    }
}

/// `mkdir(path, 0700)`.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new().mode(0o700).create(path)
}

/// The host page size, for converting `/proc/<pid>/statm` pages to bytes.
pub fn page_size() -> u64 {
    // SAFETY: plain libc query.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(size)
        .ok()
        .filter(|size| *size > 0)
        .unwrap_or(4096)
}

/// The exit code, or `-signal` for a signal death (Python's `returncode`).
pub fn returncode(status: &std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .or_else(|| status.signal().map(|signal| -signal))
        .unwrap_or(1)
}

/// Inert: only Linux gives tests a private `TMPDIR` to match on (soldr#3739).
pub fn pids_with_env(_name: &str, _value: &std::ffi::OsStr) -> Vec<u32> {
    Vec::new()
}

/// Make `link` a symlink to the directory `target` (a test's private HOME
/// reaching the caller's toolchain homes, soldr#3516).
pub fn link_dir(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}
