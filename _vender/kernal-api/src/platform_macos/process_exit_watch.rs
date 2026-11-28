//! Awaiting another process's exit (macOS).

use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::platform::process::{
    ProcessExitObservation, ProcessId, ProcessInspectError, ProcessInspectErrorKind,
    ProcessSessionExit,
};

/// A subscription to one process's exit, held open until it arrives.
///
/// This host has no pidfd, so the subscription is a kqueue registration
/// instead, and registration is what pins the identity: the kernel accepts it
/// only against a process that exists now, and the resulting watch follows
/// that process rather than the number it was found under. A repeated
/// `kill(pid, 0)` has no equivalent property -- between two asks the answer
/// can start describing a stranger.
///
/// `NOTE_EXIT` is delivered once and `EV_CLEAR` makes a second collection
/// look indistinguishable from "still running", so the exit is latched here
/// the first time it is seen.
pub struct ProcessExitWatch {
    pid: ProcessId,
    kqueue_fd: OwnedFd,
    /// Whether the registration also asked for -- and was granted -- the exit
    /// status. Only a parent may have it, so this is false for exactly the
    /// watches whose target this process did not spawn.
    status_requested: bool,
    exited: AtomicBool,
    observation: std::sync::Mutex<Option<ProcessExitObservation>>,
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
        let (kqueue_fd, status_requested) = open_exit_kqueue(pid)?;
        Ok(Self {
            pid,
            kqueue_fd,
            status_requested,
            exited: AtomicBool::new(false),
            observation: std::sync::Mutex::new(None),
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
    /// on this host `NOTE_EXITSTATUS` is granted to the parent alone -- see
    /// [`ProcessExitObservation`].
    ///
    /// Observing costs the real parent nothing: the kqueue notification is
    /// not a reap, so a `PlatformChild` or `std::process::Child` for the same
    /// process still waits successfully afterwards.
    pub async fn exited(&self) -> Result<ProcessExitObservation, ProcessInspectError> {
        if let Some(observation) = self.latched() {
            return Ok(observation);
        }

        // Registered here rather than in `open` so the watch itself costs no
        // reactor slot, and so acquisition stays usable outside a runtime.
        let readable = tokio::io::unix::AsyncFd::with_interest(
            self.kqueue_fd.as_fd(),
            tokio::io::Interest::READABLE,
        )
        .map_err(|source| ProcessInspectError {
            kind: ProcessInspectErrorKind::Host,
            source,
        })?;

        loop {
            let mut guard = readable
                .readable()
                .await
                .map_err(|source| ProcessInspectError {
                    kind: ProcessInspectErrorKind::Host,
                    source,
                })?;
            match collect_exit(&self.kqueue_fd, self.status_requested)? {
                Some(observation) => {
                    self.latch(observation);
                    return Ok(observation);
                }
                // Nothing to collect. Either another caller on this same watch
                // already took the one `NOTE_EXIT` -- the queue is drained and
                // no further edge is coming, so the latch is the only place
                // the answer still exists -- or the pending event was not
                // ours, in which case waiting for the next edge is right and
                // spinning on a drained queue is not.
                None => match self.latched() {
                    Some(observation) => return Ok(observation),
                    None => guard.clear_ready(),
                },
            }
        }
    }

    fn latched(&self) -> Option<ProcessExitObservation> {
        if !self.exited.load(Ordering::Acquire) {
            return None;
        }
        *self.observation.lock().expect("exit observation lock")
    }

    fn latch(&self, observation: ProcessExitObservation) {
        *self.observation.lock().expect("exit observation lock") = Some(observation);
        self.exited.store(true, Ordering::Release);
    }
}

/// Register for `pid`'s exit, asking for its status only where allowed.
///
/// `NOTE_EXITSTATUS` is the parent's privilege on this host, and asking for
/// it against a process this one did not spawn fails the whole registration
/// rather than downgrading it. Since the daemon-watching case is exactly the
/// non-parent one, the ask is retried without it: a watch that reports the
/// exit and not its status is the point of
/// [`ProcessExitObservation::Unreported`], while no watch at all is useless.
fn open_exit_kqueue(pid: ProcessId) -> Result<(OwnedFd, bool), ProcessInspectError> {
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

    match register(&kqueue_fd, pid, libc::NOTE_EXIT | libc::NOTE_EXITSTATUS) {
        Ok(()) => return Ok((kqueue_fd, true)),
        Err(error) if error.kind == ProcessInspectErrorKind::NotFound => return Err(error),
        Err(_) => {}
    }
    register(&kqueue_fd, pid, libc::NOTE_EXIT)?;
    Ok((kqueue_fd, false))
}

fn register(kqueue_fd: &OwnedFd, pid: ProcessId, fflags: u32) -> Result<(), ProcessInspectError> {
    let change = libc::kevent {
        ident: pid.native_signed() as libc::uintptr_t,
        filter: libc::EVFILT_PROC,
        flags: libc::EV_ADD | libc::EV_CLEAR,
        fflags,
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
        return Ok(());
    }

    let source = io::Error::last_os_error();
    let kind = match source.raw_os_error() {
        Some(libc::ESRCH) => ProcessInspectErrorKind::NotFound,
        _ => ProcessInspectErrorKind::Host,
    };
    Err(ProcessInspectError { kind, source })
}

/// Collect one pending event, if the pending one is the exit we asked for.
fn collect_exit(
    kqueue_fd: &OwnedFd,
    status_requested: bool,
) -> Result<Option<ProcessExitObservation>, ProcessInspectError> {
    let mut event = std::mem::MaybeUninit::<libc::kevent>::uninit();
    let timeout = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: no changes are submitted, room for one event is provided, and
    // the zero timeout makes this a collection rather than a wait.
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
        return Err(ProcessInspectError::last_os_error(
            ProcessInspectErrorKind::Host,
        ));
    }
    if rc == 0 {
        return Ok(None);
    }

    // SAFETY: kevent reported one event, so it initialised the slot.
    let event = unsafe { event.assume_init() };
    if event.filter != libc::EVFILT_PROC || event.fflags & libc::NOTE_EXIT == 0 {
        return Ok(None);
    }
    if !status_requested || event.fflags & libc::NOTE_EXITSTATUS == 0 {
        return Ok(Some(ProcessExitObservation::Unreported));
    }
    Ok(Some(ProcessExitObservation::Reported(wait_status(
        event.data as i32,
    ))))
}

/// `NOTE_EXITSTATUS` carries the ordinary `wait(2)` status word.
fn wait_status(raw: i32) -> ProcessSessionExit {
    if libc::WIFEXITED(raw) {
        ProcessSessionExit::from_native(Some(libc::WEXITSTATUS(raw)), None, raw as u32)
    } else if libc::WIFSIGNALED(raw) {
        ProcessSessionExit::from_native(None, Some(libc::WTERMSIG(raw)), raw as u32)
    } else {
        ProcessSessionExit::from_native(None, None, raw as u32)
    }
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

    /// The status word decodes the same way the kernel encoded it.
    #[test]
    fn a_normal_exit_and_a_signal_decode_separately() {
        let exited = wait_status(7 << 8);
        assert_eq!(exited.exit_code(), Some(7));
        assert_eq!(exited.signal(), None);

        let signalled = wait_status(libc::SIGKILL);
        assert_eq!(signalled.exit_code(), None);
        assert_eq!(signalled.signal(), Some(libc::SIGKILL));
    }
}
