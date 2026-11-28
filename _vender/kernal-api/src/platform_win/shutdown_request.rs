//! Hearing the host ask this process to stop (console control events).

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::platform::process::ShutdownRequest;

/// Set by the console control handler, read by whoever asked for it.
///
/// `static` for the same reason the Unix side needs one: the handler is a
/// bare function pointer the OS calls, with no way to carry context.
static REQUESTED: AtomicBool = AtomicBool::new(false);

/// The whole handler for an event we recognise: one relaxed atomic store.
///
/// Windows runs this on a thread it injects into the process, so the same
/// restraint applies as for a signal handler: no allocation, no logging, no
/// joining threads.
///
/// Returning `TRUE` claims the event. For Ctrl-C and Ctrl-Break that
/// suppresses the default terminate, which is the point -- the caller needs
/// to observe the flag and wind down rather than being killed where it
/// stands.
///
/// Anything not in this list is **declined**, not swallowed. Claiming an
/// event this process has no opinion about would suppress whatever handler
/// comes next for it.
///
/// Close, logoff and shutdown are claimed too, but Windows bounds how long it
/// waits before terminating anyway. For those, claiming converts a guaranteed
/// hard kill into a chance to drain -- not a promise of one.
unsafe extern "system" fn console_ctrl_handler(
    ctrl_type: winapi::shared::minwindef::DWORD,
) -> winapi::shared::minwindef::BOOL {
    use winapi::um::wincon::{
        CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
    };

    match ctrl_type {
        CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT
        | CTRL_SHUTDOWN_EVENT => {
            REQUESTED.store(true, Ordering::Relaxed);
            winapi::shared::minwindef::TRUE
        }
        _ => winapi::shared::minwindef::FALSE,
    }
}

/// Ask this host to report shutdown requests.
pub fn install_shutdown_request_handler() -> io::Result<ShutdownRequest> {
    REQUESTED.store(false, Ordering::Relaxed);
    // SAFETY: `console_ctrl_handler` has the required `system` ABI, lives for
    // the process lifetime, and performs only an atomic store.
    let installed = unsafe {
        winapi::um::consoleapi::SetConsoleCtrlHandler(
            Some(console_ctrl_handler),
            winapi::shared::minwindef::TRUE,
        )
    };
    if installed == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ShutdownRequest::watching(&REQUESTED))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every test resets and observes the same process-global latch.
    static SIGNAL_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Installing succeeds and reports "not asked yet".
    ///
    /// Delivery is exercised by calling the handler directly rather than
    /// through `GenerateConsoleCtrlEvent`, which broadcasts to a whole process
    /// group -- including the test runner and its siblings.
    #[test]
    fn installing_reports_nothing_asked_yet() {
        let _guard = SIGNAL_TEST.lock().unwrap();
        let request = install_shutdown_request_handler().expect("install");
        assert!(!request.requested());
    }

    /// Every shutdown event is claimed and requests shutdown.
    ///
    /// Claiming matters as much as the flag: an unclaimed Ctrl-C gets the
    /// default terminate and the caller never gets to drain.
    #[test]
    fn every_shutdown_event_is_claimed_and_requests_shutdown() {
        let _guard = SIGNAL_TEST.lock().unwrap();
        use winapi::um::wincon::{
            CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT, CTRL_LOGOFF_EVENT,
            CTRL_SHUTDOWN_EVENT,
        };

        let request = install_shutdown_request_handler().expect("install");
        for event in [
            CTRL_C_EVENT,
            CTRL_BREAK_EVENT,
            CTRL_CLOSE_EVENT,
            CTRL_LOGOFF_EVENT,
            CTRL_SHUTDOWN_EVENT,
        ] {
            REQUESTED.store(false, Ordering::Relaxed);
            // SAFETY: calling the handler directly; it only stores an atomic.
            let handled = unsafe { console_ctrl_handler(event) };
            assert_eq!(
                handled,
                winapi::shared::minwindef::TRUE,
                "event {event} must be claimed, or Windows applies the default                  terminate and the drain never runs"
            );
            assert!(
                request.requested(),
                "event {event} did not request shutdown"
            );
        }
        REQUESTED.store(false, Ordering::Relaxed);
    }

    /// An event we do not recognise must be passed on, not swallowed.
    ///
    /// Claiming it would suppress whatever handler comes next for an event
    /// this process has no opinion about.
    #[test]
    fn an_unrecognized_console_event_is_declined() {
        let _guard = SIGNAL_TEST.lock().unwrap();
        let request = install_shutdown_request_handler().expect("install");
        REQUESTED.store(false, Ordering::Relaxed);
        // SAFETY: as above.
        let handled = unsafe { console_ctrl_handler(0xDEAD_BEEF) };
        assert_eq!(
            handled,
            winapi::shared::minwindef::FALSE,
            "an unknown event must be declined so the next handler sees it"
        );
        assert!(
            !request.requested(),
            "an unknown event must not request shutdown"
        );
    }
}
