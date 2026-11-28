//! Daemon control signals: "start", "stop" and "terminate" sent to a process by PID.
//!
//! A daemon calls [`ControlReceiver::listen`] once and awaits
//! [`ControlReceiver::recv`] in its event loop; a client calls [`send`] with
//! the daemon's PID. On Unix these are the real `SIGUSR1`, `SIGUSR2` and
//! `SIGTERM`, so `kill -USR1 <pid>` from a shell or compositor binding reaches
//! the same receiver. On Windows each signal is a per-PID named event in the
//! session namespace, and [`ControlSignal::Terminate`] also arrives for the
//! console close and system shutdown events.
//!
//! Deliveries of the same signal that arrive before the receiver observes the
//! first one coalesce into one, as POSIX signals do.

use std::io;

/// A control request a daemon can receive.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ControlSignal {
    /// `SIGUSR1` on Unix.
    User1,
    /// `SIGUSR2` on Unix.
    User2,
    /// `SIGTERM` on Unix; also console close and system shutdown on Windows.
    Terminate,
}

/// This process's subscription to the three control signals.
///
/// Must be created inside a Tokio runtime. Once created, the signals no longer
/// take their default action (terminating the process on Unix) for the rest of
/// the process lifetime, even after the receiver is dropped.
pub struct ControlReceiver {
    inner: crate::native_control_signal::Receiver,
}

impl std::fmt::Debug for ControlReceiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlReceiver").finish_non_exhaustive()
    }
}

impl ControlReceiver {
    /// Start receiving control signals addressed to this process.
    ///
    /// On Windows this fails with [`io::ErrorKind::AlreadyExists`] if another
    /// receiver in this process (or a squatter) already owns this PID's events.
    pub fn listen() -> io::Result<Self> {
        crate::native_control_signal::Receiver::listen().map(|inner| Self { inner })
    }

    /// The next control signal. Cancel-safe, so it can sit in a `select!`.
    ///
    /// `None` means the host stopped delivering signals to this receiver.
    pub async fn recv(&mut self) -> Option<ControlSignal> {
        self.inner.recv().await
    }
}

/// Send `signal` to the process `pid`.
///
/// Fails with [`io::ErrorKind::NotFound`] when no such process exists (or, on
/// Windows, it has no [`ControlReceiver`]) and
/// [`io::ErrorKind::PermissionDenied`] when the host refuses.
pub fn send(pid: u32, signal: ControlSignal) -> io::Result<()> {
    crate::native_control_signal::send(pid, signal)
}
