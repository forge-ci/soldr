//! Timeout-only blocking waits (soldr#3836).
//!
//! A raw blocking wait -- a channel `recv`, a thread `join`, a child `wait`,
//! a file `lock_exclusive`, a `Barrier::wait`, a `block_on` -- has no bound,
//! and a wait that never ends is invisible: on 2026-10-16 one wedged the soldr
//! daemon for half an hour. The `ban_raw_blocking_wait` Dylint rejects those
//! calls outside this module, and every function here takes the bound as a
//! required argument, so the only way to wait is with a timeout.
//!
//! A wait that is legitimately long (a whole `cargo build`, a toolchain
//! install) passes one of the named budgets below rather than a magic number,
//! so every long wait is justified at a single definition and greppable.
//!
//! Lock acquisition is the other half of this policy and lives in
//! [`super::sync`].

use std::fs::File;
use std::io;
use std::process::{Child, Command, ExitStatus};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use wait_timeout::ChildExt;

/// Budget for a child that does a whole build or install with inherited
/// output (`cargo build`, `rustup toolchain install`, a source build): long
/// enough for any real run, short enough that a wedged one still ends.
pub const LONG_CHILD: Duration = Duration::from_secs(24 * 60 * 60);

/// Budget for a short host probe (`--version`, `git rev-parse`, `xcrun`).
pub const PROBE: Duration = Duration::from_secs(30);

/// Budget for a probe that crosses the network (`git ls-remote`).
pub const NETWORK_PROBE: Duration = Duration::from_secs(5 * 60);

/// Budget for joining a helper thread whose work is itself bounded (a pipe
/// reader whose child was already reaped, a worker told to stop).
pub const HELPER_JOIN: Duration = Duration::from_secs(60);

/// Budget for driving one whole CLI command (a top-level `block_on`): the
/// same ceiling as [`LONG_CHILD`], since a command can wrap a whole build.
pub const WHOLE_COMMAND: Duration = LONG_CHILD;

/// Budget for a cross-process lock held by a peer for one short operation
/// (a journal append, a metadata rewrite).
pub const SHORT_LOCK: Duration = Duration::from_secs(30);

/// Budget for a cross-process lock a peer may hold across a whole install,
/// download or cook.
pub const LONG_LOCK: Duration = LONG_CHILD;

/// A bounded wait ran out of time, or its counterpart went away.
#[derive(Debug, thiserror::Error)]
pub enum WaitError {
    /// The wait exceeded its bound.
    #[error("wait exceeded its {0:?} bound")]
    TimedOut(Duration),
    /// The other side disconnected (channel closed, thread panicked).
    #[error("the other side disconnected")]
    Disconnected,
}

impl From<WaitError> for io::Error {
    fn from(error: WaitError) -> Self {
        match error {
            WaitError::TimedOut(_) => io::Error::new(io::ErrorKind::TimedOut, error),
            WaitError::Disconnected => io::Error::new(io::ErrorKind::BrokenPipe, error),
        }
    }
}

/// Back off from 50 us to 5 ms between polls, never past the deadline.
fn backoff_sleep(backoff: &mut Duration, deadline: Instant) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    std::thread::sleep((*backoff).min(remaining));
    *backoff = (*backoff * 2).min(Duration::from_millis(5));
}

// ---------------------------------------------------------------------------
// Channels
// ---------------------------------------------------------------------------

/// Receive one value, waiting at most `bound`.
pub fn recv_for<T>(rx: &Receiver<T>, bound: Duration) -> Result<T, WaitError> {
    rx.recv_timeout(bound).map_err(|error| match error {
        RecvTimeoutError::Timeout => WaitError::TimedOut(bound),
        RecvTimeoutError::Disconnected => WaitError::Disconnected,
    })
}

/// Send one value on a bounded channel, waiting at most `bound` for room.
/// On failure the value is returned with the reason.
pub fn send_for<T>(tx: &SyncSender<T>, value: T, bound: Duration) -> Result<(), (T, WaitError)> {
    let deadline = Instant::now() + bound;
    let mut backoff = Duration::from_micros(50);
    let mut value = value;
    loop {
        match tx.try_send(value) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Disconnected(back)) => return Err((back, WaitError::Disconnected)),
            Err(TrySendError::Full(back)) => {
                if Instant::now() >= deadline {
                    return Err((back, WaitError::TimedOut(bound)));
                }
                value = back;
                backoff_sleep(&mut backoff, deadline);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Threads
// ---------------------------------------------------------------------------

/// Join a thread, waiting at most `bound` for it to finish. On timeout the
/// handle is returned so the caller can decide (detach, retry, report).
/// A panicked thread is reported as [`WaitError::Disconnected`].
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
    // Finished: this join returns immediately.
    #[allow(clippy::disallowed_methods)]
    handle.join().map_err(|_| (None, WaitError::Disconnected))
}

// ---------------------------------------------------------------------------
// Child processes
// ---------------------------------------------------------------------------

/// Wait for a child to exit, at most `bound`. On timeout the child is
/// killed and reaped (bounded) and `TimedOut` is returned.
pub fn wait_for(child: &mut Child, bound: Duration) -> io::Result<ExitStatus> {
    if let Some(status) = child.wait_timeout(bound)? {
        return Ok(status);
    }
    let _ = child.kill();
    let _ = child.wait_timeout(Duration::from_secs(5));
    Err(WaitError::TimedOut(bound).into())
}

/// Run `command` with stdout and stderr captured and stdin closed, waiting
/// at most `bound` for it to exit. The command is owned by this invocation
/// and its process group is terminated on expiry. For crates
/// below soldr-core; others use soldr-core's `command_output_with_timeout*`,
/// which add inactivity budgets and context-rich errors.
pub fn output_for(
    command: &mut std::process::Command,
    bound: Duration,
) -> io::Result<std::process::Output> {
    use std::io::Read;
    let mut child = spawn_for_output(command)?;
    let started = Instant::now();
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
    let (status, stdout, stderr) =
        finish_owned_output(&mut child, started, bound, stdout_reader, stderr_reader)?;
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

pub(crate) fn spawn_for_output(command: &mut Command) -> io::Result<Child> {
    use std::process::Stdio;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    spawn_owned(command)
}

/// Capture both pipes and report each line as it is read.
pub fn output_for_observed(
    command: &mut std::process::Command,
    bound: Duration,
    capture_limit: Option<usize>,
    on_line: impl Fn(bool, &[u8]) -> io::Result<()> + Send + Sync + 'static,
) -> io::Result<std::process::Output> {
    use std::io::{BufRead, BufReader, Read};
    use std::sync::Arc;

    fn drain(
        pipe: impl Read,
        is_stderr: bool,
        capture_limit: Option<usize>,
        on_line: impl Fn(bool, &[u8]) -> io::Result<()>,
    ) -> io::Result<Vec<u8>> {
        let mut reader = BufReader::new(pipe);
        let mut bytes = Vec::new();
        let mut chunk = Vec::new();
        let mut overflow = false;
        loop {
            let pending = chunk.len();
            if reader
                .by_ref()
                .take(64 * 1024)
                .read_until(b'\n', &mut chunk)?
                == 0
            {
                if !chunk.is_empty() {
                    on_line(is_stderr, &chunk)?;
                }
                return if overflow {
                    Err(io::Error::other(
                        "remote output exceeds the buffered protocol limit; use --stream",
                    ))
                } else {
                    Ok(bytes)
                };
            }
            if let Some(limit) = capture_limit {
                if chunk.len() - pending <= limit.saturating_sub(bytes.len()) && !overflow {
                    bytes.extend_from_slice(&chunk[pending..]);
                } else {
                    overflow = true;
                }
            }
            let end = complete_utf8_prefix(&chunk);
            if end > 0 {
                on_line(is_stderr, &chunk[..end])?;
            }
            drop(chunk.drain(..end));
        }
    }

    let mut child = spawn_for_output(command)?;
    let started = Instant::now();
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let on_line = Arc::new(on_line);
    let on_stdout = Arc::clone(&on_line);
    let stdout = std::thread::spawn(move || {
        drain(stdout, false, capture_limit, |s, line| on_stdout(s, line))
    });
    let stderr =
        std::thread::spawn(move || drain(stderr, true, capture_limit, |s, line| on_line(s, line)));
    let (status, stdout, stderr) = finish_owned_output(&mut child, started, bound, stdout, stderr)?;
    Ok(std::process::Output {
        status,
        stdout: stdout?,
        stderr: stderr?,
    })
}

fn complete_utf8_prefix(bytes: &[u8]) -> usize {
    let tail = bytes.len().saturating_sub(3);
    let candidate = bytes[tail..]
        .iter()
        .rposition(|byte| byte & 0xc0 != 0x80)
        .map(|index| tail + index);
    match candidate {
        Some(start)
            if std::str::from_utf8(&bytes[start..])
                .is_err_and(|error| error.error_len().is_none()) =>
        {
            start
        }
        _ => bytes.len(),
    }
}

// These commands are synchronous work owned by the caller. Persistent
// services must use the separate daemon launch path.
/// Start invocation-owned work with caller-configured stdio. The caller must
/// enforce a deadline and terminate the process group on cancellation.
pub fn spawn_owned(command: &mut Command) -> io::Result<Child> {
    super::process::command::configure_process_group(command);
    super::process::command::configure_parent_death(command);
    let _spawn = super::process::spawn_exclusion::spawn_shared();
    command.spawn()
}

fn finish_owned_output<T>(
    child: &mut Child,
    started: Instant,
    bound: Duration,
    stdout: JoinHandle<T>,
    stderr: JoinHandle<T>,
) -> io::Result<(ExitStatus, T, T)> {
    let remaining = || bound.saturating_sub(started.elapsed());
    // Keep the group leader unreaped while draining. If a descendant holds
    // the pipe after the leader exits, its process group is still ours to kill.
    let collect = |reader| {
        join_for(reader, remaining()).map_err(|(_, error)| match error {
            WaitError::TimedOut(_) => WaitError::TimedOut(bound),
            error => error,
        })
    };
    let result = (|| {
        let stdout = collect(stdout)?;
        let stderr = collect(stderr)?;
        let status = child
            .wait_timeout(remaining())?
            .ok_or(WaitError::TimedOut(bound))?;
        Ok((status, stdout, stderr))
    })();
    if result.is_err() {
        terminate_owned(child);
    }
    result
}

fn terminate_owned(child: &mut Child) {
    let _ = super::process::terminate::terminate_tree(child);
    let _ = child.wait_timeout(Duration::from_secs(5));
}

/// Run `command` with its stdio as configured (typically inherited) and wait
/// at most `bound` for it to exit; the timeout-only `Command::status()`.
pub fn status_for(command: &mut std::process::Command, bound: Duration) -> io::Result<ExitStatus> {
    let mut child = command.spawn()?;
    wait_for(&mut child, bound)
}

// ---------------------------------------------------------------------------
// Cross-process file locks
// ---------------------------------------------------------------------------

fn lock_for(
    file: &File,
    bound: Duration,
    try_lock: impl Fn(&File) -> io::Result<()>,
) -> io::Result<()> {
    let deadline = Instant::now() + bound;
    let mut backoff = Duration::from_micros(50);
    loop {
        match try_lock(file) {
            Ok(()) => return Ok(()),
            Err(error) if is_lock_contention(&error) => {
                if Instant::now() >= deadline {
                    return Err(WaitError::TimedOut(bound).into());
                }
                backoff_sleep(&mut backoff, deadline);
            }
            Err(error) => return Err(error),
        }
    }
}

fn is_lock_contention(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::WouldBlock
        || error.raw_os_error() == fs2::lock_contended_error().raw_os_error()
}

/// Take an exclusive advisory lock on `file`, waiting at most `bound`.
pub fn lock_exclusive_for(file: &File, bound: Duration) -> io::Result<()> {
    lock_for(file, bound, fs2::FileExt::try_lock_exclusive)
}

/// Take a shared advisory lock on `file`, waiting at most `bound`.
pub fn lock_shared_for(file: &File, bound: Duration) -> io::Result<()> {
    lock_for(file, bound, fs2::FileExt::try_lock_shared)
}

/// Like [`lock_exclusive_for`], announcing on stderr when another process
/// holds the lock so a long wait is never silent.
pub fn lock_exclusive_announced(file: &File, bound: Duration, what: &str) -> io::Result<()> {
    if fs2::FileExt::try_lock_exclusive(file).is_ok() {
        return Ok(());
    }
    eprintln!("soldr: waiting up to {bound:?} for {what} -- another process holds it");
    lock_exclusive_for(file, bound)
}

// ---------------------------------------------------------------------------
// Barrier
// ---------------------------------------------------------------------------

/// A reusable rendezvous whose wait is bounded.
#[derive(Debug)]
pub struct Barrier {
    parties: usize,
    state: std::sync::Mutex<(usize, u64)>,
    changed: std::sync::Condvar,
}

impl Barrier {
    /// A barrier releasing once `parties` callers have arrived.
    pub fn new(parties: usize) -> Self {
        Self {
            parties: parties.max(1),
            state: std::sync::Mutex::new((0, 0)),
            changed: std::sync::Condvar::new(),
        }
    }

    /// Arrive and wait for the rest of this generation, at most `bound`.
    #[allow(clippy::disallowed_methods)]
    pub fn wait_for(&self, bound: Duration) -> Result<(), WaitError> {
        let deadline = Instant::now() + bound;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let generation = state.1;
        state.0 += 1;
        if state.0 >= self.parties {
            state.0 = 0;
            state.1 = state.1.wrapping_add(1);
            self.changed.notify_all();
            return Ok(());
        }
        while state.1 == generation {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                state.0 = state.0.saturating_sub(1);
                return Err(WaitError::TimedOut(bound));
            }
            state = self
                .changed
                .wait_timeout(state, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Async bridge
// ---------------------------------------------------------------------------

/// Drive `future` to completion on `runtime`, at most `bound`.
pub fn block_on_for<F: std::future::Future>(
    runtime: &tokio::runtime::Runtime,
    future: F,
    bound: Duration,
) -> Result<F::Output, WaitError> {
    #[allow(clippy::disallowed_methods)]
    runtime
        .block_on(async { tokio::time::timeout(bound, future).await })
        .map_err(|_| WaitError::TimedOut(bound))
}

/// Bridge from a blocking worker to an async operation with a time limit.
pub fn block_on_handle_for<F: std::future::Future>(
    runtime: &tokio::runtime::Handle,
    future: F,
    bound: Duration,
) -> Result<F::Output, WaitError> {
    #[allow(clippy::disallowed_methods)]
    runtime
        .block_on(async { tokio::time::timeout(bound, future).await })
        .map_err(|_| WaitError::TimedOut(bound))
}

#[cfg(test)]
#[path = "wait_tests.rs"]
mod tests;
