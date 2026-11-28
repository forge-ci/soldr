//! Awaiting another process's exit (Linux).

use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

use crate::platform::process::{
    ProcessExitObservation, ProcessId, ProcessInspectError, ProcessInspectErrorKind,
    ProcessSessionExit,
};

/// A subscription to one process's exit, held open until it arrives.
///
/// The pidfd *is* the reuse safety. Opening it either succeeds against the
/// process alive at that moment or fails, and from then on the descriptor
/// names that process rather than the number it was found under -- so the
/// number being handed to something else later cannot retarget the watch.
/// A repeated `kill(pid, 0)` has no equivalent property: between two asks the
/// answer can start describing a stranger.
///
/// There is deliberately no fallback to asking about the PID on a kernel
/// without `pidfd_open`. `ProcessLiveness` has one, and is right to: it
/// promises a best answer. This promises a property, and a fallback would
/// quietly withdraw it.
pub struct ProcessExitWatch {
    pid: ProcessId,
    pid_fd: OwnedFd,
}

impl std::fmt::Debug for ProcessExitWatch {
    /// Names the process, not the descriptor.
    ///
    /// The descriptor number is an artefact of this process's own table;
    /// printing it invites a reader to compare two values that were never
    /// comparable.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessExitWatch")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

impl ProcessExitWatch {
    /// Subscribe to `pid`'s exit, failing if it is not running now.
    ///
    /// Synchronous and runtime-free: acquisition is the part that must happen
    /// at a known moment, and making it `async` would put a scheduling delay
    /// between the caller's decision and the kernel pinning the target.
    pub fn open(pid: ProcessId) -> Result<Self, ProcessInspectError> {
        Ok(Self {
            pid,
            pid_fd: pidfd_open(pid)?,
        })
    }

    /// The process this watch was opened for.
    #[must_use]
    pub fn pid(&self) -> ProcessId {
        self.pid
    }

    /// Wait until that process exits.
    ///
    /// Returns as soon as the kernel says so; nothing here wakes up to ask.
    /// The status is available only for a process this one parented, because
    /// on this host a status belongs to whoever reaps it -- see
    /// [`ProcessExitObservation`].
    ///
    /// Peeking is non-destructive: `WNOWAIT` leaves the zombie for its real
    /// parent, so a `PlatformChild` or `std::process::Child` for the same
    /// process still waits successfully afterwards.
    pub async fn exited(&self) -> Result<ProcessExitObservation, ProcessInspectError> {
        // Registered here rather than in `open` so the watch itself costs no
        // reactor slot, and so acquisition stays usable outside a runtime.
        let readable = tokio::io::unix::AsyncFd::with_interest(
            self.pid_fd.as_fd(),
            tokio::io::Interest::READABLE,
        )
        .map_err(|source| ProcessInspectError {
            kind: ProcessInspectErrorKind::Host,
            source,
        })?;
        // A pidfd becomes readable exactly once, when its process exits, and
        // stays that way; there is no spurious wakeup to loop around.
        // The guard is dropped without clearing readiness on purpose: the
        // readiness is the exit, and it is permanent.
        let _exited = readable
            .readable()
            .await
            .map_err(|source| ProcessInspectError {
                kind: ProcessInspectErrorKind::Host,
                source,
            })?;
        peek_exit_status(&self.pid_fd)
    }
}

/// Open a pidfd, distinguishing "gone" from "this kernel will not".
///
/// A kernel without the syscall and a seccomp filter that hides it are the
/// same answer to the caller: this host cannot give the guarantee. `ESRCH` is
/// different -- that is the process being gone, which is the acquisition
/// failing against a live target rather than the mechanism being absent.
fn pidfd_open(pid: ProcessId) -> Result<OwnedFd, ProcessInspectError> {
    // SAFETY: the syscall takes a pid and a flags word, both passed by value.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid.native_signed(), 0_u32) };
    if raw >= 0 {
        // SAFETY: the syscall succeeded, so `raw` is a fresh descriptor this
        // value now solely owns.
        return Ok(unsafe { OwnedFd::from_raw_fd(raw as i32) });
    }

    let source = io::Error::last_os_error();
    let kind = match source.raw_os_error() {
        Some(libc::ESRCH) => ProcessInspectErrorKind::NotFound,
        Some(libc::ENOSYS) | Some(libc::EPERM) => ProcessInspectErrorKind::Unsupported,
        _ => ProcessInspectErrorKind::Host,
    };
    Err(ProcessInspectError { kind, source })
}

/// Read the exit status without reaping, where this process is entitled to.
///
/// `ECHILD` means the exited process was not ours to reap, and `EINVAL` means
/// a kernel new enough for `pidfd_open` but not for `waitid(P_PIDFD, ..)`.
/// Both are "no status", not failures: the exit itself was already observed.
fn peek_exit_status(pid_fd: &OwnedFd) -> Result<ProcessExitObservation, ProcessInspectError> {
    // SAFETY: an all-zero siginfo_t is a valid one, and waitid fills it in.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    // SAFETY: the descriptor is live, and `info` is valid for the write.
    let rc = unsafe {
        libc::waitid(
            libc::P_PIDFD,
            pid_fd.as_raw_fd() as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOWAIT,
        )
    };
    if rc == -1 {
        let source = io::Error::last_os_error();
        return match source.raw_os_error() {
            Some(libc::ECHILD) | Some(libc::EINVAL) => Ok(ProcessExitObservation::Unreported),
            _ => Err(ProcessInspectError {
                kind: ProcessInspectErrorKind::Host,
                source,
            }),
        };
    }

    // SAFETY: waitid returning success means the SIGCHLD fields are filled in.
    let status = unsafe { info.si_status() };
    Ok(match info.si_code {
        libc::CLD_EXITED => ProcessExitObservation::Reported(ProcessSessionExit::from_native(
            Some(status),
            None,
            ((status & 0xff) << 8) as u32,
        )),
        libc::CLD_KILLED => ProcessExitObservation::Reported(ProcessSessionExit::from_native(
            None,
            Some(status),
            (status & 0x7f) as u32,
        )),
        libc::CLD_DUMPED => ProcessExitObservation::Reported(ProcessSessionExit::from_native(
            None,
            Some(status),
            ((status & 0x7f) | 0x80) as u32,
        )),
        // A stop or continue, which `WEXITED` alone should not have collected.
        _ => ProcessExitObservation::Unreported,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A process that is not there cannot be subscribed to, which is the
    /// acquisition half of reuse safety: no handle, so nothing to retarget.
    #[test]
    fn a_reaped_process_cannot_be_watched() {
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("spawn");
        let pid = ProcessId::new(child.id()).expect("child pid is in range");
        child.wait().expect("reap");
        let error = ProcessExitWatch::open(pid).expect_err("a reaped pid has no process");
        assert_eq!(error.kind, ProcessInspectErrorKind::NotFound);
    }

    /// The wait-status word is reconstructed from the decoded `siginfo_t`, so
    /// pin the encoding next to the code that builds it.
    #[test]
    fn a_normal_exit_reconstructs_its_wait_status_word() {
        let exit = ProcessSessionExit::from_native(Some(7), None, ((7 & 0xff) << 8) as u32);
        assert_eq!(exit.exit_code(), Some(7));
        assert_eq!(exit.native_status(), 0x0700);
    }
}
