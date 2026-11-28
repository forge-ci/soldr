//! Control signals as POSIX signals.

use std::io;

use tokio::signal::unix::{signal, Signal, SignalKind};

use crate::control_signal::ControlSignal;

pub(crate) struct Receiver {
    user1: Signal,
    user2: Signal,
    terminate: Signal,
}

impl Receiver {
    pub(crate) fn listen() -> io::Result<Self> {
        Ok(Self {
            user1: signal(SignalKind::user_defined1())?,
            user2: signal(SignalKind::user_defined2())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    pub(crate) async fn recv(&mut self) -> Option<ControlSignal> {
        tokio::select! {
            got = self.user1.recv() => got.map(|()| ControlSignal::User1),
            got = self.user2.recv() => got.map(|()| ControlSignal::User2),
            got = self.terminate.recv() => got.map(|()| ControlSignal::Terminate),
        }
    }
}

pub(crate) fn send(pid: u32, signal: ControlSignal) -> io::Result<()> {
    // PID 0 and anything that becomes negative would address a process group.
    let target = libc::pid_t::try_from(pid)
        .ok()
        .filter(|pid| *pid > 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "PID out of range"))?;
    let number = match signal {
        ControlSignal::User1 => libc::SIGUSR1,
        ControlSignal::User2 => libc::SIGUSR2,
        ControlSignal::Terminate => libc::SIGTERM,
    };
    // SAFETY: `target` is a single positive PID; `kill` takes plain integers.
    if unsafe { libc::kill(target, number) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    Err(match error.raw_os_error() {
        Some(libc::ESRCH) => io::Error::new(io::ErrorKind::NotFound, "no such process"),
        _ => error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_addressing_pids_are_refused() {
        for pid in [0, u32::MAX, 1 << 31] {
            let error = send(pid, ControlSignal::User1).expect_err("refused");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "pid {pid}");
        }
    }
}
