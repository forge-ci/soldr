//! Graceful-terminate request on Windows (soldr#3096; formerly a parked
//! hook under soldr#3059 / issue #1286 — see `platform/process/signal.rs`
//! for the history of what the caller does with this).
//!
//! Windows has no POSIX SIGTERM: `TerminateProcess` is a kernel kill that
//! runs no code in the target, so a daemon that only had that primitive
//! could never take its fast-exit path (write the `died-signal-fast`
//! lifecycle marker, say so on stderr, exit 1). This module supplies the
//! Windows equivalent of SIGTERM as a named, auto-reset Win32 event:
//!
//! - the daemon creates `Local\soldr-daemon-terminate-<pid>` when it
//!   registers its terminate hook ([`wait_for_terminate_signal`]) and a
//!   dedicated thread blocks on it in `WaitForSingleObject`;
//! - a terminator opens that event with `EVENT_MODIFY_STATE` and
//!   `SetEvent`s it ([`request_graceful_terminate`]); when no such event
//!   exists (not a soldr daemon, or a daemon predating this mechanism)
//!   the caller falls back to `TerminateProcess`.
//!
//! The waiter is a plain `std::thread`, not `tokio::task::spawn_blocking`:
//! the daemon drops its runtime after `block_on` returns on the graceful
//! path, and a runtime drop joins the blocking pool — a thread parked
//! forever in `WaitForSingleObject` would wedge that shutdown. A detached
//! std thread is simply discarded at process exit.
//!
//! The name lives in the `Local\` (session-private) namespace: the daemon
//! and whoever stops it run in the same logon session, and pids are only
//! meaningful within one.

use std::future::Future;
use std::io;
use std::ptr;

use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, WAIT_OBJECT_0};
use windows_sys::Win32::System::Threading::{
    CreateEventW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE, INFINITE,
};

/// Owned kernel-object handle: closed on drop.
///
/// Kernel handles are process-wide, so it is sound to move one to the
/// waiter thread.
pub(crate) struct OwnedEvent(HANDLE);

// SAFETY: a Win32 event handle is a process-wide kernel object reference;
// it carries no thread affinity and every operation on it is thread-safe.
unsafe impl Send for OwnedEvent {}

impl Drop for OwnedEvent {
    fn drop(&mut self) {
        // SAFETY: the handle was returned by `CreateEventW` and is closed
        // exactly once, here.
        unsafe { CloseHandle(self.0) };
    }
}

/// NUL-terminated UTF-16 name of the terminate event for `pid`.
pub(crate) fn terminate_event_name(pid: u32) -> Vec<u16> {
    format!("Local\\soldr-daemon-terminate-{pid}")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect()
}

/// Create the (auto-reset, initially unsignalled) terminate event for `pid`.
///
/// A named kernel object only outlives its last handle, so a stale event
/// from a recycled pid can exist only while that process is still alive;
/// `ERROR_ALREADY_EXISTS` is therefore tolerated — the returned handle is
/// still valid and still receives `SetEvent`.
pub(crate) fn create_terminate_event(pid: u32) -> io::Result<OwnedEvent> {
    let name = terminate_event_name(pid);
    // SAFETY: `name` is a valid NUL-terminated UTF-16 buffer that outlives
    // the call; a null security descriptor means default security.
    let handle = unsafe { CreateEventW(ptr::null(), 0, 0, name.as_ptr()) };
    if handle.is_null() {
        // SAFETY: plain thread-local error read.
        let code = unsafe { GetLastError() };
        return Err(io::Error::from_raw_os_error(code as i32));
    }
    Ok(OwnedEvent(handle))
}

/// Block until `event` is signalled or `timeout_ms` elapses. `true` iff
/// signalled.
pub(crate) fn wait_terminate_event(event: &OwnedEvent, timeout_ms: u32) -> bool {
    // SAFETY: the handle is a live event owned by `event`.
    unsafe { WaitForSingleObject(event.0, timeout_ms) == WAIT_OBJECT_0 }
}

/// Ask the soldr daemon running as `pid` to take its fast-exit path.
///
/// `Ok(true)` when the daemon's terminate event existed and was signalled;
/// `Ok(false)` when no such event exists (the target is not a soldr daemon
/// with this hook registered), leaving the caller to fall back to
/// `TerminateProcess`. Any other Win32 failure is returned as an error.
pub fn request_graceful_terminate(pid: u32) -> io::Result<bool> {
    let name = terminate_event_name(pid);
    // SAFETY: `name` is a valid NUL-terminated UTF-16 buffer that outlives
    // the call.
    let handle = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
    if handle.is_null() {
        // ERROR_FILE_NOT_FOUND (2) is the "no such event" case; any other
        // code (access denied, ...) is a genuine failure.
        // SAFETY: plain thread-local error read.
        let code = unsafe { GetLastError() };
        const ERROR_FILE_NOT_FOUND: u32 = 2;
        return if code == ERROR_FILE_NOT_FOUND {
            Ok(false)
        } else {
            Err(io::Error::from_raw_os_error(code as i32))
        };
    }
    let event = OwnedEvent(handle);
    // SAFETY: live event handle opened with EVENT_MODIFY_STATE.
    let ok = unsafe { SetEvent(event.0) } != 0;
    if ok {
        Ok(true)
    } else {
        // SAFETY: plain thread-local error read.
        let code = unsafe { GetLastError() };
        Err(io::Error::from_raw_os_error(code as i32))
    }
}

/// Register this process's terminate event **eagerly** (at call time, not
/// at first poll) and resolve to `true` once a terminator signals it.
///
/// Eager creation matters: the daemon's clients consider it ready as soon
/// as its IPC accept loop answers, and a `signal_pid(pid, false)` that
/// races the hook task's first poll must still find the event, or it would
/// fall back to `TerminateProcess` and the fast-exit path would silently
/// not run. Resolves to `false` when the event cannot be created — the
/// caller then runs without the hook, exactly as a Unix host without
/// signal registration does.
pub fn wait_for_terminate_signal() -> impl Future<Output = bool> {
    let event = create_terminate_event(std::process::id());
    async move {
        let Ok(event) = event else {
            return false;
        };
        let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
        // A failed thread spawn drops `tx`, so `rx` resolves to `false`
        // and the daemon runs without the hook.
        let _ = std::thread::Builder::new()
            .name("soldr-terminate-event".into())
            .spawn(move || {
                let signalled = wait_terminate_event(&event, INFINITE);
                let _ = tx.send(signalled);
            });
        rx.await.unwrap_or(false)
    }
}

/// Windows uses console/job termination rather than catchable POSIX signals.
pub struct ChildTerminationGuard;

impl ChildTerminationGuard {
    /// No signal dispositions to install on this host.
    pub fn install() -> std::io::Result<Self> {
        Ok(Self)
    }
}

/// Explicit timeout cleanup uses terminate_tree; no POSIX signal to forward.
pub fn forward_child_termination(_pid: u32) -> std::io::Result<bool> {
    Ok(false)
}

/// Observed Windows children are contained by their kill-on-close Job Object.
pub fn kill_owned_process_group(_pid: u32) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end round trip of the primitive: create the event for our
    /// own pid, request a graceful terminate against that pid, and observe
    /// the event become signalled (auto-reset: a second wait is quiet).
    #[test]
    fn terminate_event_round_trip_for_own_pid() {
        let pid = std::process::id();
        let event = create_terminate_event(pid).expect("create terminate event");
        assert!(
            !wait_terminate_event(&event, 0),
            "freshly created event must start unsignalled"
        );
        assert!(
            request_graceful_terminate(pid).expect("open + SetEvent"),
            "the event exists, so the request must report it was signalled"
        );
        assert!(
            wait_terminate_event(&event, 5_000),
            "event must be signalled"
        );
        assert!(
            !wait_terminate_event(&event, 0),
            "auto-reset: the wait consumed the signal"
        );
    }

    /// No event for a pid means "not a soldr daemon": the caller must be
    /// told to fall back, not handed an error.
    #[test]
    fn request_without_an_event_reports_fallback() {
        // A pid that cannot have a live daemon event registered by this
        // test process; nothing in this binary creates it.
        let pid = u32::MAX - 7;
        assert!(!request_graceful_terminate(pid).expect("missing event is not an error"));
    }

    #[test]
    fn event_name_is_session_local_and_nul_terminated() {
        let name = terminate_event_name(4252);
        let text = String::from_utf16(&name[..name.len() - 1]).unwrap();
        assert_eq!(text, "Local\\soldr-daemon-terminate-4252");
        assert_eq!(name.last(), Some(&0));
    }
}
