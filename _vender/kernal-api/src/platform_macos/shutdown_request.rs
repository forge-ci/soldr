//! Hearing the host ask this process to stop (POSIX signals).

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::platform::process::ShutdownRequest;

/// Set by the signal handler, read by whoever asked for it.
///
/// `static` because a signal handler has no way to receive context: the
/// kernel calls a bare function pointer, so the only thing it can reach is
/// something at a fixed address.
static REQUESTED: AtomicBool = AtomicBool::new(false);

/// The whole handler. One relaxed atomic store, nothing else.
///
/// A signal can arrive on any thread, between any two instructions --
/// including inside the allocator or while a lock is held. Allocating,
/// logging, or locking here can deadlock the process against itself, so this
/// does the one thing that is async-signal-safe and lets the caller act later.
extern "C" fn record_shutdown_request(_signal: libc::c_int) {
    REQUESTED.store(true, Ordering::Relaxed);
}

/// Ask this host to report shutdown requests.
///
/// `SIGTERM` is what a supervisor or `kill` sends; `SIGINT` is Ctrl-C. Both
/// mean the same thing to a daemon, and both default to terminating it
/// outright -- which is what installing a handler replaces with a request the
/// process can act on.
pub fn install_shutdown_request_handler() -> io::Result<ShutdownRequest> {
    REQUESTED.store(false, Ordering::Relaxed);
    for signal in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: `record_shutdown_request` has C ABI, lives for the process
        // lifetime, and performs only an atomic store.
        let previous = unsafe {
            libc::signal(
                signal,
                record_shutdown_request as *const () as libc::sighandler_t,
            )
        };
        if previous == libc::SIG_ERR {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(ShutdownRequest::watching(&REQUESTED))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Installation resets the process-global latch. Keep each test's reset,
    // real signal delivery, and observations in one exclusive interval.
    static SIGNAL_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Installing reports "not asked yet", and a delivered signal is seen.
    ///
    /// The signal is raised in this process, which is the only way to test the
    /// real delivery path rather than a stand-in for it.
    #[test]
    fn a_delivered_signal_is_observed() {
        let _guard = SIGNAL_TEST.lock().unwrap();
        let request = install_shutdown_request_handler().expect("install");
        assert!(!request.requested(), "nothing has asked yet");

        // SAFETY: raising a signal this process has just installed a handler
        // for; the handler only stores an atomic.
        assert_eq!(unsafe { libc::raise(libc::SIGTERM) }, 0);
        assert!(request.requested(), "a delivered SIGTERM must be observed");
    }

    /// The answer latches, so a caller cannot miss a request by checking late.
    #[test]
    fn the_request_latches() {
        let _guard = SIGNAL_TEST.lock().unwrap();
        let request = install_shutdown_request_handler().expect("install");
        // SAFETY: see above.
        assert_eq!(unsafe { libc::raise(libc::SIGINT) }, 0);
        assert!(request.requested());
        assert!(request.requested(), "and stays asked");
    }
}
