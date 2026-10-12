//! Linux test-process supervision (see `platform/process/test_child.rs`).

use crate::platform::process::test_child::TestSignal;
use std::ffi::CString;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicI32, Ordering};

const PR_SET_PTRACER: libc::c_int = 0x5961_6D61;
const PTRACE_UNAVAILABLE: &[u8] = b"nextest timeout wrapper: ptrace authorization unavailable\n";

/// Before exec, in the child: join `cgroup_procs` (when given), start a new
/// session, die with the wrapper (`PR_SET_PDEATHSIG`), let the wrapper
/// attach a debugger (`PR_SET_PTRACER`), and refuse to run if the wrapper
/// already died between fork and `prctl`.
pub fn configure_test_child(command: &mut Command, parent_pid: u32, cgroup_procs: Option<&Path>) {
    let procs = cgroup_procs.and_then(|path| CString::new(path.as_os_str().as_bytes()).ok());
    // SAFETY: the hook only calls async-signal-safe libc functions.
    unsafe {
        command.pre_exec(move || {
            if let Some(path) = &procs {
                let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
                if fd >= 0 {
                    libc::write(fd, b"0".as_ptr().cast(), 1);
                    libc::close(fd);
                }
            }
            libc::setsid();
            if libc::prctl(
                libc::PR_SET_PDEATHSIG,
                libc::SIGKILL as libc::c_ulong,
                0,
                0,
                0,
            ) != 0
            {
                libc::_exit(126);
            }
            if libc::prctl(PR_SET_PTRACER, libc::c_ulong::from(parent_pid), 0, 0, 0) != 0 {
                libc::write(
                    2,
                    PTRACE_UNAVAILABLE.as_ptr().cast(),
                    PTRACE_UNAVAILABLE.len(),
                );
            }
            if libc::getppid() as u32 != parent_pid {
                libc::_exit(127);
            }
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

/// Every live process whose environment carries `name=value` exactly.
///
/// soldr#3739: the wrapper gives each test a private `TMPDIR`, so a process
/// still carrying that value after the test exited is something the test
/// left behind -- typically a daemon it auto-started, which `setsid` and
/// double-forking have detached from the test's process group. Processes
/// whose environment we may not read are skipped.
pub fn pids_with_env(name: &str, value: &std::ffi::OsStr) -> Vec<u32> {
    let mut needle = Vec::with_capacity(name.len() + 1 + value.len());
    needle.extend_from_slice(name.as_bytes());
    needle.push(b'=');
    needle.extend_from_slice(value.as_bytes());
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            std::fs::read(format!("/proc/{pid}/environ"))
                .is_ok_and(|environ| environ.split(|byte| *byte == 0).any(|var| var == needle))
        })
        .collect()
}
