//! Wall-clock deadline for the in-process screenshot path.
//!
//! `ExternalWebviewHost::run` owns the main thread and has no time limit of
//! its own, so a webview that never finishes would keep the runner silent
//! forever. The watchdog streams progress on every poll, and once the
//! deadline passes it flushes progress one last time, names the stage that
//! hung, and hands that diagnostic to `on_elapsed`. The runner exits nonzero
//! there; it never waits longer or recovers on its own.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Named runner stages, reported in the deadline diagnostic.
pub const STAGES: &[&str] = &["event-loop", "result", "drain"];

/// Shared label for the stage the runner is in.
#[derive(Clone, Default)]
pub struct Stage(Arc<AtomicUsize>);

impl Stage {
    /// Record that the runner entered `STAGES[index]`.
    pub fn enter(&self, index: usize) {
        self.0.store(index.min(STAGES.len() - 1), Ordering::SeqCst);
    }

    pub fn name(&self) -> &'static str {
        STAGES[self.0.load(Ordering::SeqCst).min(STAGES.len() - 1)]
    }
}

/// Dropping the guard (or calling `disarm`) stops the watchdog without firing.
pub struct Watchdog {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Watchdog {
    pub fn disarm(mut self) {
        self.stop_and_join();
    }

    fn stop_and_join(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

/// The diagnostic line the runner prints when the deadline elapses.
pub fn diagnostic(stage: &str, deadline: Duration, elapsed: Duration) -> String {
    format!(
        "kernal-screenshot-runner deadline-elapsed stage={stage} deadline_ms={} elapsed_ms={}",
        deadline.as_millis(),
        elapsed.as_millis()
    )
}

/// Start the watchdog. `progress` runs every `poll` and once more just before
/// `on_elapsed`, so trace output streams instead of waiting for completion.
pub fn arm<P, E>(
    deadline: Duration,
    poll: Duration,
    stage: Stage,
    mut progress: P,
    on_elapsed: E,
) -> Watchdog
where
    P: FnMut() + Send + 'static,
    E: FnOnce(String) + Send + 'static,
{
    let (stop, stopped) = mpsc::channel::<()>();
    let start = Instant::now();
    let thread = std::thread::Builder::new()
        .name("screenshot-deadline".into())
        .spawn(move || loop {
            let remaining = deadline.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                progress();
                on_elapsed(diagnostic(stage.name(), deadline, start.elapsed()));
                return;
            }
            match stopped.recv_timeout(poll.min(remaining)) {
                Err(RecvTimeoutError::Timeout) => progress(),
                // Disarmed: the guarded work finished in time.
                Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
            }
        })
        .expect("spawn screenshot deadline watchdog");
    Watchdog {
        stop: Some(stop),
        thread: Some(thread),
    }
}
