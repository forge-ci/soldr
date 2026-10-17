//! Timeout-only blocking waits for zccache (soldr#3836).
//!
//! The counterpart of soldr's `soldr_platform::wait`: a raw blocking wait --
//! a channel `recv`, a thread `join`, a child `wait`, a `Command`
//! output/status, a file lock, a `block_on` -- has no bound, and on
//! 2026-10-16 one wedged the daemon that hosts zccache for half an hour.
//! Every function here takes the bound as a required argument, and
//! `.github/scripts/check_zccache_raw_locks.py` rejects the raw calls in
//! zccache's production code outside this module and [`crate::sync`].
//!
//! Legitimately long waits pass one of the named budgets below.

use std::fs::File;
use std::io;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// A whole build, install or tool run with inherited output.
pub const LONG_CHILD: Duration = Duration::from_secs(24 * 60 * 60);
/// A short host probe (`--version`, `git rev-parse`).
pub const PROBE: Duration = Duration::from_secs(30);
/// Joining a helper thread whose work is itself bounded.
pub const HELPER_JOIN: Duration = Duration::from_secs(60);
/// Driving one whole CLI command (a top-level `block_on`).
pub const WHOLE_COMMAND: Duration = LONG_CHILD;
/// A cross-process lock a peer holds for one short operation.
pub const SHORT_LOCK: Duration = Duration::from_secs(30);
/// A cross-process lock a peer may hold across a whole warm, GC or cook.
pub const LONG_LOCK: Duration = LONG_CHILD;

/// A bounded wait ran out of time, or its counterpart went away.
#[derive(Debug)]
pub enum WaitError {
    /// The wait exceeded its bound.
    TimedOut(Duration),
    /// The other side disconnected (channel closed, thread panicked).
    Disconnected,
}

impl std::fmt::Display for WaitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimedOut(bound) => write!(f, "wait exceeded its {bound:?} bound"),
            Self::Disconnected => write!(f, "the other side disconnected"),
        }
    }
}

impl std::error::Error for WaitError {}

impl From<WaitError> for io::Error {
    fn from(error: WaitError) -> Self {
        match error {
            WaitError::TimedOut(_) => io::Error::new(io::ErrorKind::TimedOut, error),
            WaitError::Disconnected => io::Error::new(io::ErrorKind::BrokenPipe, error),
        }
    }
}

fn backoff_sleep(backoff: &mut Duration, deadline: Instant) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    std::thread::sleep((*backoff).min(remaining));
    *backoff = (*backoff * 2).min(Duration::from_millis(5));
}

/// Receive one value, waiting at most `bound`.
pub fn recv_for<T>(rx: &Receiver<T>, bound: Duration) -> Result<T, WaitError> {
    rx.recv_timeout(bound).map_err(|error| match error {
        RecvTimeoutError::Timeout => WaitError::TimedOut(bound),
        RecvTimeoutError::Disconnected => WaitError::Disconnected,
    })
}

/// Join a thread, waiting at most `bound`. On timeout the handle comes back.
pub fn join_for<T>(
    handle: JoinHandle<T>,
    bound: Duration,
) -> Result<T, (Option<JoinHandle<T>>, WaitError)> {
    let deadline = Instant::now() + bound;
    let mut backoff = Duration::from_micros(50);
    while !handle.is_finished() {
        if Instant::now() >= deadline {
            return Err((Some(handle), WaitError::TimedOut(bound)));
        }
        backoff_sleep(&mut backoff, deadline);
    }
    handle.join().map_err(|_| (None, WaitError::Disconnected))
}

/// Wait for a child to exit, at most `bound`; on expiry it is killed.
pub fn wait_for(child: &mut Child, bound: Duration) -> io::Result<ExitStatus> {
    let deadline = Instant::now() + bound;
    let mut backoff = Duration::from_micros(50);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let reap_deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < reap_deadline {
                if child.try_wait()?.is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            return Err(WaitError::TimedOut(bound).into());
        }
        backoff_sleep(&mut backoff, deadline);
        backoff = backoff.max(Duration::from_millis(1));
    }
}

/// The timeout-only `Command::status()`.
pub fn status_for(command: &mut Command, bound: Duration) -> io::Result<ExitStatus> {
    let mut child = command.spawn()?;
    wait_for(&mut child, bound)
}

/// The timeout-only `Command::output()` (stdin closed, both pipes captured).
pub fn output_for(command: &mut Command, bound: Duration) -> io::Result<Output> {
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    wait_with_output_for(child, bound)
}

/// The timeout-only `Child::wait_with_output()`.
pub fn wait_with_output_for(mut child: Child, bound: Duration) -> io::Result<Output> {
    use std::io::Read;
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let stdout_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(pipe) = stdout_pipe.as_mut() {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(pipe) = stderr_pipe.as_mut() {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    });
    let status = wait_for(&mut child, bound)?;
    let stdout = join_for(stdout_reader, HELPER_JOIN).unwrap_or_default();
    let stderr = join_for(stderr_reader, HELPER_JOIN).unwrap_or_default();
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn lock_with<G>(
    bound: Duration,
    mut attempt: impl FnMut() -> io::Result<G>,
) -> io::Result<G> {
    let deadline = Instant::now() + bound;
    let mut backoff = Duration::from_micros(50);
    loop {
        match attempt() {
            Ok(guard) => return Ok(guard),
            Err(error) if kernal_api::platform::fs::is_lock_conflict(&error) => {
                if Instant::now() >= deadline {
                    return Err(WaitError::TimedOut(bound).into());
                }
                backoff_sleep(&mut backoff, deadline);
            }
            Err(error) => return Err(error),
        }
    }
}

/// Exclusive advisory lock on `file`, waiting at most `bound`.
pub fn lock_exclusive_for(
    file: &File,
    bound: Duration,
) -> io::Result<kernal_api::platform::fs::FileLock<'_>> {
    lock_with(bound, || kernal_api::platform::fs::try_lock_exclusive(file))
}

/// Shared advisory lock on `file`, waiting at most `bound`.
pub fn lock_shared_for(
    file: &File,
    bound: Duration,
) -> io::Result<kernal_api::platform::fs::FileLock<'_>> {
    lock_with(bound, || kernal_api::platform::fs::try_lock_shared(file))
}

/// Owned shared lock, waiting at most `bound`. The file is reopened by the
/// caller-supplied `open` on each attempt because a failed owned try
/// consumes the handle.
pub fn lock_shared_owned_for(
    mut open: impl FnMut() -> io::Result<File>,
    bound: Duration,
) -> io::Result<kernal_api::platform::fs::OwnedFileLock> {
    lock_with(bound, || kernal_api::platform::fs::try_lock_shared_owned(open()?))
}

/// Owned exclusive lock, waiting at most `bound` (see [`lock_shared_owned_for`]).
pub fn lock_exclusive_owned_for(
    mut open: impl FnMut() -> io::Result<File>,
    bound: Duration,
) -> io::Result<kernal_api::platform::fs::OwnedFileLock> {
    lock_with(bound, || kernal_api::platform::fs::try_lock_exclusive_owned(open()?))
}

/// Drive `future` to completion on `runtime`, at most `bound`.
pub fn block_on_for<F: std::future::Future>(
    runtime: &tokio::runtime::Runtime,
    future: F,
    bound: Duration,
) -> Result<F::Output, WaitError> {
    runtime
        .block_on(async { tokio::time::timeout(bound, future).await })
        .map_err(|_| WaitError::TimedOut(bound))
}
