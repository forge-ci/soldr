//! Asking this host about another process (macOS).

use std::ffi::CStr;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::platform::process::{ProcessId, ProcessInspectError, ProcessInspectErrorKind};

/// A live reference to another process, good for as long as it is held.
///
/// This host has no pidfd, so the handle registers for the process's exit
/// with kqueue instead. Registration is what pins the identity: the kernel
/// accepts it only for a process that exists *now*, and the resulting
/// subscription follows that process rather than the number naming it.
///
/// `NOTE_EXIT` is delivered once. `EV_CLEAR` means a second read would report
/// nothing and look indistinguishable from "still running", so the exit is
/// latched here the first time it is seen and never asked about again.
pub struct ProcessLiveness {
    pid: u32,
    exit_kqueue: OwnedFd,
    exited: AtomicBool,
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
        Ok(Self {
            pid,
            exit_kqueue: open_exit_kqueue(pid)?,
            exited: AtomicBool::new(false),
        })
    }

    /// The process ID this handle was opened for.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Whether that process is still running.
    pub fn is_alive(&self) -> bool {
        !self.exited.load(Ordering::Relaxed)
            && kqueue_process_is_alive(&self.exit_kqueue, &self.exited)
    }

    /// Whether that process has exited, or why the host could not tell.
    ///
    /// Where [`Self::is_alive`] folds every failed observation into "not
    /// alive", this reports it: `Err` means the question itself failed, and
    /// says nothing about the process. The answer comes from a zero-timeout
    /// read of the `NOTE_EXIT` subscription taken at open, latched once seen
    /// exactly as [`Self::is_alive`] latches it.
    pub fn has_exited(&self) -> io::Result<bool> {
        if self.exited.load(Ordering::Relaxed) {
            return Ok(true);
        }
        kqueue_process_has_exited(&self.exit_kqueue, &self.exited)
    }
}

/// Resolve the on-disk image a running process was started from.
///
/// `proc_pidpath` asks about one process. The obvious alternative -- walking
/// every process on the host and picking the matching one -- answers the same
/// question at a cost that grows with everything else running.
pub fn process_executable_path(pid: u32) -> Result<PathBuf, io::Error> {
    let mut buffer = [0_u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer and its true length are passed together, and the
    // call writes at most that many bytes.
    let written = unsafe {
        libc::proc_pidpath(
            pid as libc::c_int,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
        )
    };
    if written <= 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `proc_pidpath` NUL-terminates, and `written` excludes the NUL.
    let path = unsafe { CStr::from_ptr(buffer.as_ptr().cast()) };
    Ok(PathBuf::from(path.to_str().map_err(|_| {
        io::Error::other("executable path is not valid UTF-8")
    })?))
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

/// The one range rule, borrowed rather than restated.
///
/// [`ProcessId`] owns the reason a `u32` above `i32::MAX` may not reach
/// `kill(2)`; duplicating the bound here would be a second place for it to
/// drift out of agreement with the first.
fn validate_pid(pid: u32) -> Result<libc::pid_t, ProcessInspectError> {
    ProcessId::new(pid).map(ProcessId::native_signed)
}

fn open_exit_kqueue(pid: u32) -> Result<OwnedFd, ProcessInspectError> {
    let native_pid = validate_pid(pid)?;
    // SAFETY: kqueue takes no arguments and returns a descriptor or -1.
    let raw_fd = unsafe { libc::kqueue() };
    if raw_fd < 0 {
        return Err(ProcessInspectError::last_os_error(
            ProcessInspectErrorKind::Host,
        ));
    }

    // SAFETY: `raw_fd` is a fresh descriptor this scope solely owns; wrapping
    // it here means the early returns below still close it.
    let kqueue_fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    let change = libc::kevent {
        ident: native_pid as libc::uintptr_t,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_CLEAR,
        fflags: libc::NOTE_EXIT,
        data: 0,
        udata: ptr::null_mut(),
    };
    // SAFETY: one initialised change is described and no events are collected.
    let rc = unsafe {
        libc::kevent(
            kqueue_fd.as_raw_fd(),
            &change,
            1,
            ptr::null_mut(),
            0,
            ptr::null(),
        )
    };
    if rc == 0 {
        return Ok(kqueue_fd);
    }

    let source = io::Error::last_os_error();
    if matches!(source.raw_os_error(), Some(libc::ESRCH)) {
        Err(ProcessInspectError::stated(
            ProcessInspectErrorKind::NotFound,
            "no such process",
        ))
    } else {
        Err(ProcessInspectError {
            kind: ProcessInspectErrorKind::Host,
            source,
        })
    }
}

/// Collect a pending `NOTE_EXIT` without waiting.
///
/// `EINTR` is retried; any other `kevent` failure, or an `EV_ERROR` event, is
/// returned rather than read as an exit.
fn kqueue_process_has_exited(kqueue_fd: &OwnedFd, exited: &AtomicBool) -> io::Result<bool> {
    loop {
        let mut event = std::mem::MaybeUninit::<libc::kevent>::uninit();
        let timeout = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: no changes are submitted, room for one event is provided,
        // and the zero timeout makes this a poll rather than a wait.
        let rc = unsafe {
            libc::kevent(
                kqueue_fd.as_raw_fd(),
                ptr::null(),
                0,
                event.as_mut_ptr(),
                1,
                &timeout,
            )
        };
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
        // SAFETY: `kevent` reported one collected event, so it wrote it.
        let event = unsafe { event.assume_init() };
        if event.flags & libc::EV_ERROR != 0 {
            return Err(io::Error::from_raw_os_error(event.data as i32));
        }
        // The only subscription on this queue is `NOTE_EXIT`.
        exited.store(true, Ordering::Relaxed);
        return Ok(true);
    }
}

fn kqueue_process_is_alive(kqueue_fd: &OwnedFd, exited: &AtomicBool) -> bool {
    let mut event = std::mem::MaybeUninit::<libc::kevent>::uninit();
    let timeout = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: no changes are submitted, room for one event is provided, and
    // the zero timeout makes this a poll rather than a wait.
    let rc = unsafe {
        libc::kevent(
            kqueue_fd.as_raw_fd(),
            ptr::null(),
            0,
            event.as_mut_ptr(),
            1,
            &timeout,
        )
    };
    if rc == 0 {
        return true;
    }

    exited.store(true, Ordering::Relaxed);
    false
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

    /// The exit latch is what makes a second question safe to ask.
    ///
    /// `EV_CLEAR` hands `NOTE_EXIT` over once; without the latch the next call
    /// would collect nothing and report the dead process alive again.
    #[test]
    fn a_dead_process_stays_dead_when_asked_twice() {
        let child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn");
        let pid = child.id();
        let handle = ProcessLiveness::open(pid).expect("open child");
        let mut child = child;
        child.wait().expect("reap");
        assert!(!handle.is_alive(), "a reaped child must report dead");
        assert!(!handle.is_alive(), "and must still report dead");
        assert!(handle.has_exited().expect("observe the latched exit"));
    }

    /// The fallible question collects the exit itself and latches it too.
    #[test]
    fn has_exited_observes_and_latches_an_exit() {
        let me = ProcessLiveness::open(std::process::id()).expect("open self");
        assert!(!me.has_exited().expect("observe self"));

        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn");
        let handle = ProcessLiveness::open(child.id()).expect("open child");
        child.wait().expect("reap");
        assert!(handle.has_exited().expect("observe exit"));
        assert!(handle.has_exited().expect("observe the latched exit"));
        assert!(!handle.is_alive());
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
