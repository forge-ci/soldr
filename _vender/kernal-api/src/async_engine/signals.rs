//! Ambient process-signal subscriptions on the currently entered runtime.
//!
//! [`super::Runtime::interrupt_signal`] is the runtime-scoped form. These
//! owned subscriptions exist for launched `'static` work (a daemon's shutdown
//! watcher) that cannot borrow the [`super::Runtime`] that drives it.

use std::future::poll_fn;

fn require_runtime() -> std::io::Result<()> {
    super::RuntimeHandle::current()
        .map(drop)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::Unsupported, "no runtime entered"))
}

/// Register for native Ctrl+C when first polled, then wait for one
/// notification.
///
/// Unix observes SIGINT; Windows observes console `CTRL_C_EVENT`. Requires a
/// currently entered runtime built with `enable_all`; without a runtime this
/// returns `Unsupported`, and a runtime without its signal driver panics.
/// Registration errors propagate. Registration changes process-wide handling
/// permanently: dropping the future does not restore the default action.
/// Notifications may coalesce.
pub async fn wait_for_interrupt() -> std::io::Result<()> {
    require_runtime()?;
    let mut receiver = crate::platform_imp::interrupt::InterruptReceiver::new()?;
    receiver.recv().await.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::BrokenPipe, "interrupt receiver closed")
    })
}

/// Owned subscription to the host's graceful-termination request.
///
/// This is the signal a supervisor sends to ask a process to exit cleanly,
/// distinct from an interactive Ctrl+C ([`wait_for_interrupt`]):
///
/// - Unix: `SIGTERM` (systemd, container runtimes, `kill <pid>`).
/// - Windows: console `CTRL_BREAK_EVENT` (what a supervisor sends a process
///   group with `GenerateConsoleCtrlEvent`), `CTRL_CLOSE_EVENT` (console
///   window closed) and `CTRL_SHUTDOWN_EVENT` (system shutdown). For the
///   close and shutdown events Windows terminates the process shortly after
///   the notification regardless; the notification is the chance to drain.
///
/// Registration is process-wide and permanent: dropping the subscription does
/// not restore the default disposition, so after the first registration the
/// host no longer kills the process on these events and the application owns
/// its exit policy. Notifications may coalesce.
pub struct TerminationSignal {
    inner: crate::platform_imp::termination::TerminationReceiver,
}

impl TerminationSignal {
    /// Register immediately on the currently entered runtime.
    ///
    /// # Errors
    ///
    /// Returns `Unsupported` outside a runtime and propagates native
    /// registration failures.
    ///
    /// # Panics
    ///
    /// Panics inside a runtime whose signal driver is disabled (a runtime not
    /// built with `enable_all`).
    pub fn new() -> std::io::Result<Self> {
        require_runtime()?;
        crate::platform_imp::termination::TerminationReceiver::new().map(|inner| Self { inner })
    }

    /// Wait for one notification; `None` means the native receiver closed.
    ///
    /// Cancellation-safe: dropping a pending wait does not consume a later
    /// notification.
    pub async fn recv(&mut self) -> Option<()> {
        poll_fn(|context| self.inner.poll_recv(context)).await
    }
}

impl std::fmt::Debug for TerminationSignal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TerminationSignal")
            .finish_non_exhaustive()
    }
}
