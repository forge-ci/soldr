//! No-progress watchdog for `soldr cook` (soldr#3043 CI silence follow-up).
//!
//! `soldr cook` drives cargo-chef's `prepare`/`cook`/exact-package compile
//! phases through the cargo front door. Those phases can legitimately run
//! for many minutes with no terminal output, and CI's own wrapper
//! (`.github/scripts/run_stable_cook.py`, retired by soldr#3396) used to
//! capture output silently until the process exited -- so a genuine hang produced only a timeout
//! from the *outer* CI job, with nothing logged about where soldr was
//! stuck.
//!
//! This module adds an *inner*, progress-aware watchdog: it samples the
//! cook target directory for new build artifacts every 30 seconds, emits a
//! heartbeat line every 5 minutes, and -- only when no new artifacts have
//! appeared for `SOLDR_COOK_NO_PROGRESS_SECS` (default 900s; `0` disables
//! it) -- writes a best-effort forensic dump and fails fast instead of
//! waiting for an outer CI timeout.
//!
//! The pure decision logic (`heartbeat_due`, `timed_out`, `progressed`) is
//! dependency-injectable and covered by non-sleeping unit tests; the real
//! driver (`run_with_watchdog`) is a thin `tokio::select!` loop around it.

use crate::core::{SoldrError, SoldrPaths};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
// The loop sleeps on tokio's clock, so it must measure elapsed time on the
// same clock. With `std::time::Instant`, a paused-time test advances the
// sleeps but never the elapsed time, and the no-progress timeout never fires.
use tokio::time::Instant;

/// Env var controlling the no-progress deadline. `0` disables the watchdog.
pub const NO_PROGRESS_ENV_VAR: &str = "SOLDR_COOK_NO_PROGRESS_SECS";
/// Default no-progress deadline: 15 minutes.
pub const DEFAULT_NO_PROGRESS_SECS: u64 = 900;
/// Heartbeat cadence while the watchdog is armed.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// How often the target directory is sampled for new artifacts.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(30);
/// Bound on how many directory entries a single sample pass may visit, so a
/// huge `target/` tree never turns "cheap heartbeat sampling" into a full
/// tree walk.
const SAMPLE_ENTRY_BUDGET: usize = 20_000;

/// Resolved watchdog configuration for one `soldr cook` invocation.
#[derive(Debug, Clone, Copy)]
pub struct WatchdogConfig {
    /// `None` means the watchdog is disabled (`SOLDR_COOK_NO_PROGRESS_SECS=0`).
    pub no_progress_timeout: Option<Duration>,
}

impl WatchdogConfig {
    pub fn from_env() -> Self {
        Self {
            no_progress_timeout: no_progress_timeout_from_env(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_timeout(timeout: Duration) -> Self {
        Self {
            no_progress_timeout: Some(timeout),
        }
    }

    #[cfg(test)]
    pub(crate) fn disabled() -> Self {
        Self {
            no_progress_timeout: None,
        }
    }
}

/// Parse `SOLDR_COOK_NO_PROGRESS_SECS`. Unset or unparsable keeps the
/// default; an explicit `0` disables the watchdog (soldr#2740: this is the
/// repo's existing "positive seconds, 0 disables" convention, the same
/// shape as `SOLDR_INSTALLER_HEARTBEAT_SECS` / `installer_watchdog.rs`, not
/// a new hand-rolled parser).
fn no_progress_timeout_from_env() -> Option<Duration> {
    match std::env::var(NO_PROGRESS_ENV_VAR) {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(0) => None,
            Ok(seconds) => Some(Duration::from_secs(seconds)),
            Err(_) => Some(Duration::from_secs(DEFAULT_NO_PROGRESS_SECS)),
        },
        Err(_) => Some(Duration::from_secs(DEFAULT_NO_PROGRESS_SECS)),
    }
}

// ---------------------------------------------------------------------
// Pure decision logic (unit-testable without real sleeping).
// ---------------------------------------------------------------------

/// Whether a heartbeat line is due: one full interval has elapsed since the
/// previous heartbeat emission (unconditional on activity -- a heartbeat
/// during active-but-silent-on-stdout compilation is exactly the reassurance
/// the CI wrapper needs).
pub(crate) fn heartbeat_due(since_last_heartbeat: Duration, interval: Duration) -> bool {
    since_last_heartbeat >= interval
}

/// Whether the no-progress deadline has been crossed.
pub(crate) fn timed_out(since_progress: Duration, timeout: Duration) -> bool {
    since_progress >= timeout
}

/// Whether a fresh sample counts as progress relative to the previous one.
pub(crate) fn progressed(previous: u64, current: u64) -> bool {
    current != previous
}

fn heartbeat_line(phase: &str, new_artifacts: u64) -> String {
    format!("soldr cook: still running — {new_artifacts} new artifacts in last 5m, phase={phase}")
}

// ---------------------------------------------------------------------
// Progress probe.
// ---------------------------------------------------------------------

/// Anything that can be sampled for a monotonically-informative "progress
/// marker". The real probe returns a marker derived from target-directory
/// mtimes; tests inject a fake counter instead of touching the filesystem
/// or real wall-clock time.
pub(crate) trait ProgressProbe {
    fn sample(&mut self) -> u64;
}

/// Samples a bounded set of `target/**/{deps,.fingerprint,build}` entries
/// for the newest modification time seen, combined with a running count of
/// distinct newest-mtimes observed so a heartbeat can report "N new
/// artifacts" even when the newest mtime doesn't advance across a
/// millisecond boundary (rare, but cheap to also track via entry count).
pub(crate) struct TargetDirProbe {
    target_dir: PathBuf,
}

impl TargetDirProbe {
    pub(crate) fn new(target_dir: PathBuf) -> Self {
        Self { target_dir }
    }

    fn newest_mtime_secs(&self) -> u64 {
        let mut newest = 0_u64;
        let mut budget = SAMPLE_ENTRY_BUDGET;
        for sub in ["deps", ".fingerprint", "build"] {
            walk_bounded(&self.target_dir, sub, &mut budget, &mut newest);
            if budget == 0 {
                break;
            }
        }
        newest
    }
}

impl ProgressProbe for TargetDirProbe {
    fn sample(&mut self) -> u64 {
        self.newest_mtime_secs()
    }
}

/// Walk profile subdirectories (`target/<profile>/<sub>`) up to a bounded
/// number of entries, tracking the newest mtime found. Not a full recursive
/// tree walk: at most one level under each `<profile>/<sub>` directory is
/// visited, which is enough to detect "cargo just wrote a new artifact"
/// cheaply every 30 seconds.
fn walk_bounded(target_dir: &Path, sub: &str, budget: &mut usize, newest: &mut u64) {
    let Ok(profiles) = std::fs::read_dir(target_dir) else {
        return;
    };
    for profile in profiles.flatten() {
        if *budget == 0 {
            return;
        }
        let dir = profile.path().join(sub);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if *budget == 0 {
                return;
            }
            *budget -= 1;
            if let Ok(metadata) = entry.metadata() {
                if let Ok(modified) = metadata.modified() {
                    if let Ok(secs) = modified.duration_since(UNIX_EPOCH) {
                        *newest = (*newest).max(secs.as_secs());
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------
// Real async driver.
// ---------------------------------------------------------------------

/// Race `fut` against the no-progress watchdog. When the watchdog is
/// disabled (`config.no_progress_timeout == None`), this is exactly
/// `fut.await`.
pub(crate) async fn run_with_watchdog<F, T>(
    phase: &'static str,
    target_dir: PathBuf,
    paths: &SoldrPaths,
    config: WatchdogConfig,
    fut: F,
) -> Result<T, SoldrError>
where
    F: std::future::Future<Output = Result<T, SoldrError>>,
{
    let Some(timeout) = config.no_progress_timeout else {
        return fut.await;
    };

    tokio::pin!(fut);
    let mut probe = TargetDirProbe::new(target_dir);
    let mut daemon_counters = None;
    let mut last_marker = probe.sample();
    let mut last_progress = Instant::now();
    let mut last_heartbeat = Instant::now();
    let mut artifacts_since_heartbeat: u64 = 0;

    loop {
        tokio::select! {
            biased;
            result = &mut fut => return result,
            _ = tokio::time::sleep(SAMPLE_INTERVAL) => {
                let marker = probe.sample();
                if progressed(last_marker, marker) {
                    artifacts_since_heartbeat = artifacts_since_heartbeat.saturating_add(1);
                    last_marker = marker;
                    last_progress = Instant::now();
                }

                let now = Instant::now();
                if heartbeat_due(now.duration_since(last_heartbeat), HEARTBEAT_INTERVAL) {
                    let status = tokio::select! {
                        result = &mut fut => return result,
                        status = tokio::task::spawn_blocking(crate::compile_stall_watch::daemon_progress) => status,
                    };
                    if let Ok(Ok(status)) = status {
                        if let Some(a) = status.admission {
                            let counters = (status.generation, a.admissions, a.completions);
                            if daemon_counters.is_some_and(|previous| previous != counters) && (a.queued > 0 || a.running > 0) {
                                last_progress = Instant::now();
                            }
                            daemon_counters = Some(counters);
                        }
                    }
                    eprintln!("{}", heartbeat_line(phase, artifacts_since_heartbeat));
                    last_heartbeat = now;
                    artifacts_since_heartbeat = 0;
                }

                if timed_out(now.duration_since(last_progress), timeout) {
                    let dump = crate::cook_watchdog_dump::write_stall_dump(paths, phase)
                        .unwrap_or_else(|err| {
                            eprintln!("soldr cook: failed to write stall dump: {err}");
                            paths.cache.join("logs").join("cook-stall-dump-failed")
                        });
                    eprintln!(
                        "soldr cook: no progress for {}s — stack dump at {}",
                        timeout.as_secs(),
                        dump.display()
                    );
                    crate::cook_watchdog_dump::terminate_cook_descendants();
                    return Err(SoldrError::Other(format!(
                        "soldr cook: no progress for {}s — stack dump at {}",
                        timeout.as_secs(),
                        dump.display()
                    )));
                }
            }
        }
    }
}

fn unix_now_string() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", now.as_millis())
}

pub(crate) fn stall_dump_dirname() -> String {
    format!("cook-stall-{}", unix_now_string())
}

#[cfg(test)]
#[path = "cook_watchdog_tests.rs"]
mod tests;
