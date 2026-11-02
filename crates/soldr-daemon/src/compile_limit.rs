//! The daemon's single resolution of the compile-concurrency limit
//! (soldr#1761, soldr#2023).
//!
//! Called exactly once per daemon, from
//! [`SoldrZccacheService::start`](crate::zccache_embedded::SoldrZccacheService::start).
//! The result is stored on the service, which makes it the one number the
//! zccache semaphore sizes itself from, and the value the daemon publishes
//! over `Status` and `BuildSessionStart`.
//!
//! #1761 already had both layers evaluating the same *expression*; #2023
//! made them share one *evaluation*. The difference is small but real: two
//! reads of `config.toml` milliseconds apart can disagree, and a queue that
//! believes in more slots than the semaphore grants is the exact defect
//! #1761 set out to remove.

use crate::core::jobs::ResolvedJobs;
use crate::daemon::protocol::Response;

/// The limit split into the `(jobs, source)` pair every wire surface
/// reports it as. One place to widen `usize` to the wire's `u32` and to
/// render the precedence tier, so `StatusInfo` and `BuildSessionStarted`
/// cannot drift into describing the same limit differently.
pub(crate) fn wire_pair(applied: ResolvedJobs) -> (u32, String) {
    (applied.jobs as u32, applied.source.describe().to_string())
}

/// The `BuildSessionStart` acknowledgement, carrying the applied limit.
pub(crate) fn build_session_started(applied: ResolvedJobs) -> Response {
    let (compile_jobs, compile_jobs_source) = wire_pair(applied);
    Response::BuildSessionStarted {
        compile_jobs,
        compile_jobs_source,
    }
}

/// Resolve the limit and say so on stderr.
///
/// The announcement is the point, not a side effect: before #1761 the
/// effective concurrency came from a vendored default via an env var the
/// daemon may or may not have inherited, with nothing reporting what
/// actually applied.
///
/// stderr rather than `tracing::info!` because the daemon installs its
/// subscriber at `Level::WARN` (see `server.rs`), so an info record is
/// dropped and reaches nobody — which would reproduce the very
/// undiscoverability this exists to fix. The detached daemon redirects
/// stderr into its log file, and `daemon start --foreground` shows it live.
pub(crate) fn resolve_and_announce() -> ResolvedJobs {
    if let Some(line) = affinity_reset_line("soldr-daemon") {
        eprintln!("{line}");
    }
    let resolved =
        crate::core::jobs::resolve_compile_jobs(crate::daemon::server::config_compile_jobs());
    eprintln!(
        "soldr-daemon: compile concurrency = {} (from {})",
        resolved.jobs,
        resolved.source.describe(),
    );
    resolved
}

/// Drop a CPU pin inherited from whoever auto-started this shared process,
/// before anything sizes itself from `available_parallelism()` (soldr#3995).
/// Returns the diagnostic line only when the mask actually changed.
pub(crate) fn affinity_reset_line(who: &str) -> Option<String> {
    let reset = crate::platform::process::affinity::widen_cpu_affinity()?;
    (reset.after != reset.before).then(|| {
        format!(
            "{who}: widened inherited CPU affinity from {} to {} CPU(s) (soldr#3995)",
            reset.before, reset.after,
        )
    })
}
