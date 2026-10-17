//! Progressive "still waiting" heartbeats for long daemon IPC waits (#1838).
//!
//! # The problem this addresses
//!
//! A compile dispatched to the daemon waits up to
//! `SOLDR_COMPILE_REPLY_TIMEOUT_SECS` (default 30 minutes) for a reply, and
//! until now said nothing for the whole of it. #1838 puts the point
//! precisely: three daemon-lifecycle defects all presented to the user the
//! same way — *the build stopped making progress, and nothing said why until
//! a multi-minute backstop expired*. The bound is not the bug; the silence
//! is.
//!
//! # Why a watchdog thread rather than a shorter read timeout
//!
//! The obvious alternative is to shorten the socket read timeout and loop.
//! That changes IPC semantics: `read_frame_sync` would start returning
//! `WouldBlock`/`TimedOut` mid-frame, and every caller would need to
//! distinguish "no reply yet" from "the daemon died". A thread that only
//! prints leaves the transport untouched, so a heartbeat can never turn a
//! healthy slow compile into a failed one.
//!
//! # Message shape
//!
//! Deliberately identical to the cargo front door's existing heartbeat
//! (`cargo_front_door::cargo_wait_heartbeat_message`): operation, elapsed
//! seconds, the active deadline, and the env var that controls it. #1838
//! calls that shape out as prior art to reuse rather than reinvent, and
//! matching it means one format to learn. It is not shared code because
//! `soldr-daemon` cannot depend on `soldr-cli` — the dependency runs the
//! other way.
//!
//! Output goes to **stderr**, which keeps `--json` modes on stdout intact.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How often to report that a wait is still outstanding. Matches the cargo
/// front door's `CARGO_WAIT_HEARTBEAT_SECS`, so the two surfaces tick at the
/// same rate and a user does not have to learn two cadences.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);

/// Poll granularity for the stop flag. Small enough that the thread joins
/// promptly on a fast compile, large enough not to spin.
const STOP_POLL: Duration = Duration::from_millis(100);

/// Schedules heartbeats against a caller-supplied elapsed duration.
///
/// Keeping this state separate from the thread gives the timing contract a
/// deterministic test seam: a beat can only report an elapsed duration that
/// has actually reached the first or next threshold. When a host pauses the
/// watchdog past a threshold, it emits one truthful beat and resumes a full
/// interval from that real elapsed time instead of rapidly replaying stale
/// 60/120/180-second labels.
#[derive(Debug)]
struct HeartbeatSchedule {
    interval: Duration,
    next_threshold: Duration,
}

impl HeartbeatSchedule {
    fn new(interval: Duration) -> Self {
        debug_assert!(!interval.is_zero(), "heartbeat interval must be nonzero");
        Self {
            interval,
            next_threshold: interval,
        }
    }

    /// Returns the real elapsed duration when a new heartbeat is due.
    fn take_due(&mut self, elapsed: Duration) -> Option<Duration> {
        if elapsed < self.next_threshold {
            return None;
        }

        self.next_threshold = elapsed.saturating_add(self.interval);
        Some(elapsed)
    }
}

/// Emits a heartbeat every [`HEARTBEAT_INTERVAL`] until dropped.
///
/// Nothing is printed if the guarded operation finishes inside the first
/// interval, which is the overwhelmingly common case — a warm compile
/// returns in milliseconds, so this is silent unless something is actually
/// slow.
pub(crate) struct WaitHeartbeat {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl WaitHeartbeat {
    /// Start reporting on `operation`, whose deadline is `timeout`,
    /// overridable via `env_var`.
    pub(crate) fn start(
        operation: &'static str,
        timeout: Duration,
        env_var: Option<&'static str>,
    ) -> Self {
        Self::start_with_interval(operation, timeout, env_var, HEARTBEAT_INTERVAL)
    }

    fn start_with_interval(
        operation: &'static str,
        timeout: Duration,
        env_var: Option<&'static str>,
        interval: Duration,
    ) -> Self {
        Self::start_with_interval_and_sink(operation, timeout, env_var, interval, |msg| {
            eprintln!("{msg}");
        })
    }

    /// The core loop, with the emit routed through `sink` so a test can assert
    /// the heartbeat actually fires at the interval without capturing process
    /// stderr. Production always passes the `eprintln!` sink above.
    fn start_with_interval_and_sink<S>(
        operation: &'static str,
        timeout: Duration,
        env_var: Option<&'static str>,
        interval: Duration,
        sink: S,
    ) -> Self
    where
        S: Fn(String) + Send + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = std::thread::Builder::new()
            .name("soldr-wait-heartbeat".to_string())
            .spawn(move || {
                let started = Instant::now();
                let mut schedule = HeartbeatSchedule::new(interval);
                while !thread_stop.load(Ordering::Relaxed) {
                    std::thread::sleep(STOP_POLL);
                    if thread_stop.load(Ordering::Relaxed) {
                        return;
                    }
                    if let Some(elapsed) = schedule.take_due(started.elapsed()) {
                        sink(heartbeat_message(operation, elapsed, timeout, env_var));
                    }
                }
            })
            .ok();
        Self { stop, handle }
    }
}

impl Drop for WaitHeartbeat {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            // Joining bounds the thread's lifetime to the wait it describes,
            // so a heartbeat can never outlive its operation and print about
            // a compile that already finished.
            let _ = crate::platform::wait::join_for(handle, crate::platform::wait::HELPER_JOIN);
        }
    }
}

/// The message body, split out so the wording is testable without spawning a
/// thread or waiting a minute.
///
/// `env_var` is optional because not every long wait has an override to
/// name — the cache flush and graceful shutdown budgets are fixed. Saying
/// "deadline 300s" without inventing a knob is better than implying one
/// exists.
pub(crate) fn heartbeat_message(
    operation: &str,
    elapsed: Duration,
    timeout: Duration,
    env_var: Option<&str>,
) -> String {
    let deadline = match env_var {
        Some(var) => format!("deadline {}s from {var}", timeout.as_secs()),
        None => format!("fixed deadline {}s", timeout.as_secs()),
    };
    format!(
        "soldr: {operation} still waiting after {}s ({deadline}); \
         if this is a wedged cache rather than slow work, \
         `ZCCACHE_DISABLE=1 soldr cargo ...` bypasses the daemon",
        elapsed.as_secs(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeats_do_not_claim_unreached_thresholds() {
        let mut schedule = HeartbeatSchedule::new(Duration::from_secs(60));

        assert_eq!(schedule.take_due(Duration::from_secs(59)), None);
        assert_eq!(
            schedule.take_due(Duration::from_secs(60)),
            Some(Duration::from_secs(60))
        );
        assert_eq!(schedule.take_due(Duration::from_secs(119)), None);
        assert_eq!(
            schedule.take_due(Duration::from_secs(120)),
            Some(Duration::from_secs(120))
        );
        assert_eq!(schedule.take_due(Duration::from_secs(179)), None);
        assert_eq!(
            schedule.take_due(Duration::from_secs(180)),
            Some(Duration::from_secs(180))
        );

        // A delayed watchdog wake-up reports its real elapsed duration once,
        // then waits a full cadence instead of rapidly replaying stale 60s /
        // 120s / 180s labels.
        let mut delayed = HeartbeatSchedule::new(Duration::from_secs(60));
        assert_eq!(
            delayed.take_due(Duration::from_secs(181)),
            Some(Duration::from_secs(181))
        );
        assert_eq!(delayed.take_due(Duration::from_secs(240)), None);
        assert_eq!(
            delayed.take_due(Duration::from_secs(241)),
            Some(Duration::from_secs(241))
        );
    }

    #[test]
    fn message_names_operation_elapsed_deadline_and_override() {
        let msg = heartbeat_message(
            "daemon compile reply",
            Duration::from_secs(120),
            Duration::from_secs(1800),
            Some("SOLDR_COMPILE_REPLY_TIMEOUT_SECS"),
        );
        assert!(msg.contains("daemon compile reply"), "{msg}");
        assert!(msg.contains("after 120s"), "{msg}");
        assert!(msg.contains("deadline 1800s"), "{msg}");
        assert!(msg.contains("SOLDR_COMPILE_REPLY_TIMEOUT_SECS"), "{msg}");
    }

    #[test]
    fn message_carries_the_corrective_action_inline() {
        // #1838 Phase 2: the remedy belongs in the message, not only in
        // CLAUDE.md, because the person reading a stalled build is not
        // reading the repo docs.
        let msg = heartbeat_message(
            "daemon compile reply",
            Duration::from_secs(60),
            Duration::from_secs(1800),
            Some("SOLDR_COMPILE_REPLY_TIMEOUT_SECS"),
        );
        assert!(msg.contains("ZCCACHE_DISABLE=1"), "{msg}");
        // soldr#2424: `--no-cache` is deprecated and `hide = true`, so a
        // reader cannot find it in `soldr --help`. Advice must name only the
        // supported kill-switch.
        assert!(!msg.contains("--no-cache"), "{msg}");
    }

    #[test]
    fn a_fast_operation_prints_nothing() {
        // The common case. A warm compile returns in milliseconds and must
        // not emit a heartbeat, or every build grows noise.
        let guard = WaitHeartbeat::start_with_interval(
            "unit test",
            Duration::from_secs(1800),
            Some("SOLDR_COMPILE_REPLY_TIMEOUT_SECS"),
            Duration::from_secs(3600),
        );
        std::thread::sleep(Duration::from_millis(50));
        drop(guard);
    }

    #[test]
    fn a_slow_operation_fires_repeated_heartbeats() {
        // #1838 Phase 1 box 5: assert the heartbeat actually EMITS once the
        // interval elapses — the message tests above only cover wording. A
        // sink captures the emissions so the assertion never touches process
        // stderr. `STOP_POLL` (100 ms) bounds how often the loop can fire, so
        // ~400 ms comfortably yields at least two.
        let emitted: Arc<crate::platform::sync::Mutex<Vec<String>>> =
            Arc::new(crate::platform::sync::Mutex::new(Vec::new()));
        let sink_target = Arc::clone(&emitted);
        let guard = WaitHeartbeat::start_with_interval_and_sink(
            "unit test",
            Duration::from_secs(1800),
            Some("SOLDR_COMPILE_REPLY_TIMEOUT_SECS"),
            Duration::from_millis(10),
            move |msg| sink_target.lock().push(msg),
        );
        // Poll for the condition instead of sleeping a fixed 400ms: the
        // heartbeat thread fires every ~100ms (STOP_POLL-bounded), and a
        // contended runner can starve it past a fixed window's 2x margin
        // (darwin lane failures on 2026-08-15/16). Healthy runs still exit
        // in ~200ms; only a genuine stall burns the 5s bound.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if emitted.lock().len() >= 2 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "a slow op must emit repeated heartbeats within 5s; got {}",
                emitted.lock().len()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(guard);
        let hits = emitted.lock();
        assert!(
            hits[0].contains("unit test") && hits[0].contains("after"),
            "{}",
            hits[0]
        );
    }

    #[test]
    fn the_guard_joins_its_thread_on_drop() {
        // A heartbeat that outlived its operation would report on a compile
        // that already finished, which is worse than silence.
        let guard = WaitHeartbeat::start_with_interval(
            "unit test",
            Duration::from_secs(1800),
            Some("SOLDR_COMPILE_REPLY_TIMEOUT_SECS"),
            Duration::from_millis(10),
        );
        std::thread::sleep(Duration::from_millis(30));
        let stop = Arc::clone(&guard.stop);
        drop(guard);
        assert!(
            stop.load(Ordering::Relaxed),
            "drop must signal the heartbeat thread to stop",
        );
    }
}
