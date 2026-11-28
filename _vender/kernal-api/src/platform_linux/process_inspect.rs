//! Asking this host about another process (Linux).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;

use crate::platform::process::{ProcessId, ProcessInspectError, ProcessInspectErrorKind};

/// A live reference to another process, good for as long as it is held.
///
/// Where the kernel offers one, this holds a pidfd: a PID can be recycled
/// between two questions, but a pidfd cannot, so a handle opened once keeps
/// naming the process it was opened for even after that process exits. Older
/// kernels have no such thing, and there the handle falls back to asking
/// about the PID -- which is the best this host can do, not an equivalent.
pub struct ProcessLiveness {
    pid: u32,
    pid_fd: Option<OwnedFd>,
    /// `/proc` start ticks captured at open, only on the no-pidfd fallback,
    /// so a later exit question can tell the opened process from a successor
    /// that was handed the same PID.
    start_ticks: Option<u64>,
}

impl std::fmt::Debug for ProcessLiveness {
    /// Names the process, not the handle.
    ///
    /// The underlying descriptor or handle value is an artefact of this
    /// process's own table; printing it invites a reader to compare two
    /// numbers that were never comparable.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessLiveness")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

impl ProcessLiveness {
    /// Take a reference to `pid`, failing if no such process is running.
    pub fn open(pid: u32) -> Result<Self, ProcessInspectError> {
        validate_pid(pid)?;
        if !process_exists(pid) {
            return Err(not_found());
        }
        let pid_fd = try_pidfd_open(pid)?;
        // Best effort: `/proc` may be mounted `hidepid`, and opening has never
        // depended on reading it. Without a capture the fallback exit check
        // can only ask whether the PID is in use.
        let start_ticks = match pid_fd {
            Some(_) => None,
            None => read_proc_stat(pid).ok().flatten().map(|stat| stat.start_ticks),
        };
        Ok(Self {
            pid,
            pid_fd,
            start_ticks,
        })
    }

    /// The process ID this handle was opened for.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Whether that process is still running.
    pub fn is_alive(&self) -> bool {
        match self.pid_fd.as_ref() {
            Some(pid_fd) => pidfd_is_alive(pid_fd),
            None => process_exists(self.pid),
        }
    }

    /// Whether that process has exited, or why the host could not tell.
    ///
    /// Where [`Self::is_alive`] folds every failed observation into "not
    /// alive", this reports it: `Err` means the question itself failed, and
    /// says nothing about the process. With a pidfd the answer comes from a
    /// zero-timeout poll of that descriptor. Without one, `kill(pid, 0)` asks
    /// whether the PID is in use, and the `/proc` start ticks captured at
    /// open decide whether it is still in use by the opened process: a zombie
    /// or a different start time means it exited. Where open could not read
    /// `/proc`, an in-use PID is reported as not exited -- the same limit
    /// [`Self::is_alive`] has on such a host.
    pub fn has_exited(&self) -> io::Result<bool> {
        match self.pid_fd.as_ref() {
            Some(pid_fd) => pidfd_has_exited(pid_fd),
            None => self.pid_has_exited(),
        }
    }

    fn pid_has_exited(&self) -> io::Result<bool> {
        let native_pid = validate_pid(self.pid).map_err(|error| error.source)?;
        // SAFETY: `native_pid` is in range; signal 0 delivers nothing.
        if unsafe { libc::kill(native_pid, 0) } != 0 {
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::ESRCH) => return Ok(true),
                // Present but not ours to signal: still a process, so fall
                // through to the identity check.
                Some(libc::EPERM) => {}
                _ => return Err(error),
            }
        }
        let Some(start_ticks) = self.start_ticks else {
            return Ok(false);
        };
        match read_proc_stat(self.pid)? {
            None => Ok(true),
            Some(stat) => Ok(stat.is_zombie || stat.start_ticks != start_ticks),
        }
    }
}

/// The two `/proc/<pid>/stat` fields an exit question needs.
struct ProcStat {
    is_zombie: bool,
    start_ticks: u64,
}

/// Read `/proc/<pid>/stat`; `Ok(None)` when no such process exists.
fn read_proc_stat(pid: u32) -> io::Result<Option<ProcStat>> {
    let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        // ESRCH surfaces when the process vanishes between open and read.
        Err(error)
            if error.kind() == io::ErrorKind::NotFound
                || error.raw_os_error() == Some(libc::ESRCH) =>
        {
            return Ok(None)
        }
        Err(error) => return Err(error),
    };
    parse_proc_stat(&stat)
        .map(Some)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed /proc process stat"))
}

fn parse_proc_stat(stat: &str) -> Option<ProcStat> {
    // `comm` is parenthesised and may contain spaces or ')'; only the final
    // ')' starts the fixed-position fields.
    let suffix = stat.get(stat.rfind(')')? + 1..)?;
    let mut fields = suffix.split_ascii_whitespace();
    let state = fields.next()?; // field 3
    let start_ticks = fields.nth(18)?.parse().ok()?; // field 22
    Some(ProcStat {
        is_zombie: matches!(state, "Z" | "X" | "x"),
        start_ticks,
    })
}

/// Resolve the on-disk image a running process was started from.
pub fn process_executable_path(pid: u32) -> Result<PathBuf, io::Error> {
    std::fs::read_link(format!("/proc/{pid}/exe"))
}

/// Ask a process to stop.
#[allow(dead_code)] // PID-only mutation remains private and is not a facade operation.
pub fn process_signal_terminate(pid: u32) -> Result<(), ProcessInspectError> {
    signal(pid, libc::SIGTERM)
}

/// Stop a process without asking.
#[allow(dead_code)] // PID-only mutation remains private and is not a facade operation.
pub fn process_force_kill(pid: u32) -> Result<(), ProcessInspectError> {
    signal(pid, libc::SIGKILL)
}

#[allow(dead_code)]
fn signal(pid: u32, signal: libc::c_int) -> Result<(), ProcessInspectError> {
    let native_pid = validate_pid(pid)?;
    // SAFETY: `native_pid` is in range and the signal number is a constant.
    let rc = unsafe { libc::kill(native_pid, signal) };
    if rc == 0 {
        Ok(())
    } else {
        Err(ProcessInspectError::last_os_error(
            ProcessInspectErrorKind::Host,
        ))
    }
}

/// Signal zero: the permission and existence checks run, nothing is delivered.
///
/// `EPERM` counts as alive. A process we are not allowed to signal is still a
/// process, and reporting it dead would invite a caller to reuse its PID.
fn process_exists(pid: u32) -> bool {
    let Ok(native_pid) = validate_pid(pid) else {
        return false;
    };
    // SAFETY: `native_pid` is in range; signal 0 delivers nothing.
    let rc = unsafe { libc::kill(native_pid, 0) };
    if rc == 0 {
        return true;
    }
    matches!(io::Error::last_os_error().raw_os_error(), Some(libc::EPERM))
}

/// The one range rule, borrowed rather than restated.
///
/// [`ProcessId`] owns the reason a `u32` above `i32::MAX` may not reach
/// `kill(2)`; duplicating the bound here would be a second place for it to
/// drift out of agreement with the first.
fn validate_pid(pid: u32) -> Result<libc::pid_t, ProcessInspectError> {
    ProcessId::new(pid).map(ProcessId::native_signed)
}

/// Open a pidfd, or report that this kernel will not give us one.
///
/// A kernel without the syscall, a seccomp filter that hides it, and a denial
/// are all the same answer to the caller: no pidfd, fall back to the PID.
/// Only `ESRCH` is different -- that is the process being gone, which is
/// worth failing on rather than falling back to asking about a dead PID.
fn try_pidfd_open(pid: u32) -> Result<Option<OwnedFd>, ProcessInspectError> {
    // SAFETY: the syscall takes a pid and a flags word, both passed by value.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0_u32) };
    if raw >= 0 {
        // SAFETY: the syscall succeeded, so `raw` is a fresh descriptor this
        // handle now solely owns.
        return Ok(Some(unsafe { OwnedFd::from_raw_fd(raw as i32) }));
    }

    match io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Err(not_found()),
        _ => Ok(None),
    }
}

/// A pidfd becomes readable exactly when its process exits.
///
/// `EINTR` is retried; any other poll failure, or a descriptor the kernel
/// reports as invalid or in error, is returned rather than guessed at.
fn pidfd_has_exited(pid_fd: &OwnedFd) -> io::Result<bool> {
    loop {
        let mut poll_fd = libc::pollfd {
            fd: pid_fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one initialised pollfd is described, and the zero timeout
        // makes this a poll rather than a wait.
        let rc = unsafe { libc::poll(&mut poll_fd, 1, 0) };
        if rc < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if rc == 0 {
            return Ok(false);
        }
        if poll_fd.revents & (libc::POLLNVAL | libc::POLLERR) != 0 {
            return Err(io::Error::other(format!(
                "pidfd poll reported an error condition (revents {:#x})",
                poll_fd.revents
            )));
        }
        return Ok(poll_fd.revents & (libc::POLLIN | libc::POLLHUP) != 0);
    }
}

/// A pidfd becomes readable exactly when its process exits.
fn pidfd_is_alive(pid_fd: &OwnedFd) -> bool {
    let mut poll_fd = libc::pollfd {
        fd: pid_fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one initialised pollfd is described, and the zero timeout makes
    // this a poll rather than a wait.
    let rc = unsafe { libc::poll(&mut poll_fd, 1, 0) };
    rc == 0
}

fn not_found() -> ProcessInspectError {
    ProcessInspectError::stated(ProcessInspectErrorKind::NotFound, "no such process")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PID zero names no process on any host, and is rejected before the
    /// kernel is asked -- signal(0, ...) would mean "the whole process group".
    #[test]
    fn pid_zero_is_never_valid() {
        let error = ProcessLiveness::open(0).expect_err("pid 0");
        assert_eq!(error.kind, ProcessInspectErrorKind::InvalidPid);
        assert!(!process_exists(0));
    }

    /// This process is alive, and knows where it was started from.
    #[test]
    fn this_process_is_alive_and_locatable() {
        let me = std::process::id();
        let handle = ProcessLiveness::open(me).expect("open self");
        assert_eq!(handle.pid(), me);
        assert!(handle.is_alive());
        assert_eq!(
            process_executable_path(me).expect("exe"),
            std::env::current_exe().expect("current_exe")
        );
    }

    /// A handle keeps naming the process it was opened for. With a pidfd the
    /// kernel guarantees this; without one, the PID could in principle be
    /// recycled, which is exactly why the pidfd is preferred.
    #[test]
    fn a_dead_process_reports_dead() {
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn");
        let pid = child.id();
        let handle = ProcessLiveness::open(pid).expect("open child");
        let mut child = child;
        child.wait().expect("reap");
        assert!(!handle.is_alive(), "a reaped child must report dead");
        assert!(handle.has_exited().expect("observe exit"));
    }

    /// The no-pidfd fallback answers from `kill(pid, 0)` plus the start
    /// ticks captured at open, so a PID now owned by another process -- here
    /// simulated by a mismatched capture -- reads as exited, not alive.
    #[test]
    fn the_pid_fallback_detects_exit_and_a_successor() {
        let me = std::process::id();
        let start_ticks = read_proc_stat(me).expect("stat").expect("self").start_ticks;
        let live = ProcessLiveness {
            pid: me,
            pid_fd: None,
            start_ticks: Some(start_ticks),
        };
        assert!(!live.has_exited().expect("observe self"));

        let successor = ProcessLiveness {
            pid: me,
            pid_fd: None,
            start_ticks: Some(start_ticks.wrapping_add(1)),
        };
        assert!(successor.has_exited().expect("observe successor"));

        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn");
        let pid = child.id();
        let start_ticks = read_proc_stat(pid).expect("stat").map(|stat| stat.start_ticks);
        let handle = ProcessLiveness {
            pid,
            pid_fd: None,
            start_ticks,
        };
        child.wait().expect("reap");
        assert!(handle.has_exited().expect("observe reaped child"));
    }

    /// `comm` may itself contain `) ` and spaces; the fixed fields follow the
    /// last `)`.
    #[test]
    fn proc_stat_parsing_survives_a_hostile_comm() {
        let fields: Vec<String> = (4..=22).map(|field| field.to_string()).collect();
        let stat = format!("42 (a) b) c) Z {}", fields.join(" "));
        let parsed = parse_proc_stat(&stat).expect("parse");
        assert!(parsed.is_zombie);
        assert_eq!(parsed.start_ticks, 22);
    }
}

/// Whether two spellings name the same executable image on this host.
///
/// This host's paths are case-sensitive and distinguish nothing else, so once
/// both sides are resolved the comparison is exact. A path that cannot be
/// canonicalised is compared as written rather than treated as a mismatch,
/// because "the file moved" and "the caller lacks permission to resolve it"
/// arrive here identically.
pub fn process_same_executable_path(actual: &std::path::Path, expected: &std::path::Path) -> bool {
    let resolve =
        |path: &std::path::Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    resolve(actual) == resolve(expected)
}

#[cfg(test)]
mod path_tests {
    use super::*;
    use std::path::Path;

    /// Case is meaningful here; two spellings that differ by it are two files.
    #[test]
    fn case_distinguishes_two_images() {
        assert!(!process_same_executable_path(
            Path::new("/tmp/Daemon"),
            Path::new("/tmp/daemon"),
        ));
    }

    /// A path resolves to itself, canonicalisable or not.
    #[test]
    fn a_path_matches_itself() {
        assert!(process_same_executable_path(
            Path::new("/tmp/rp-does-not-exist/daemon"),
            Path::new("/tmp/rp-does-not-exist/daemon"),
        ));
        let me = std::env::current_exe().expect("current_exe");
        assert!(process_same_executable_path(&me, &me));
    }
}
