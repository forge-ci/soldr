//! soldr#2302 — read the build-scoped cache summary file.
//!
//! Split out of `cache_states` to stay under the per-file production-line
//! ceiling (soldr#2302 / house style). The file name is fixed and shared by
//! every front-door invocation, so the read is keyed to the session that
//! wrote it: a file from an earlier build is reported as stale instead of
//! being shown as this build's numbers (soldr#3540).

use std::path::Path;

use crate::daemon::protocol::BuildCacheSummary;

/// Read the per-build cache summary from the session stats file (the
/// baseline-diff `last-session-stats.json`).
///
/// soldr#3540: the file name is fixed and shared by every front-door
/// invocation, and `finalize_build_session_stats` is a no-op when the daemon
/// is unreachable at end of build — so the counters on disk are frequently
/// *some earlier build's*. The session key stamped into the body is
/// therefore checked, and a file from another session is reported as
/// [`SessionStatsRead::Stale`] rather than returned as this build's numbers.
pub(crate) fn read_build_cache_summary(stats_path: &Path, session_id: &str) -> SessionStatsRead {
    let Ok(raw) = std::fs::read_to_string(stats_path) else {
        return SessionStatsRead::Missing;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(raw.trim()) else {
        return SessionStatsRead::Missing;
    };
    if json.get("status").and_then(serde_json::Value::as_str) != Some("ok") {
        return SessionStatsRead::Missing;
    }
    // soldr#3540: no key, or another session's key, means these numbers are
    // not this build's — including a body written before the key existed,
    // which is exactly the unkeyed file the bug report describes.
    if json.get("session_id").and_then(serde_json::Value::as_str) != Some(session_id) {
        return SessionStatsRead::Stale;
    }
    let hits = json_u64(&json, "hits").unwrap_or(0);
    let misses = json_u64(&json, "misses").unwrap_or(0);
    let non_cacheable = json_u64(&json, "non_cacheable").unwrap_or(0);
    let errors = json_u64(&json, "errors").unwrap_or(0);
    SessionStatsRead::Fresh(BuildCacheSummary {
        hits,
        misses,
        non_cacheable,
        errors,
        compilations: json_u64(&json, "compilations").unwrap_or(hits + misses),
        time_saved_ms: json_u64(&json, "time_saved_ms").unwrap_or(0),
    })
}

/// What [`read_build_cache_summary`] found in the stats file (soldr#3540).
pub(crate) enum SessionStatsRead {
    /// The file was keyed to the session reading it, so the numbers are
    /// this build's own.
    Fresh(BuildCacheSummary),
    /// No usable stats file: absent, unreadable, or `status != "ok"`.
    Missing,
    /// The file exists but belongs to an earlier session (or carries no
    /// session key at all). Reporting it as this build's is the stale-
    /// summary bug.
    Stale,
}

impl SessionStatsRead {
    /// The summary, only when it provably belongs to the reading session.
    pub(crate) fn fresh(self) -> Option<BuildCacheSummary> {
        match self {
            Self::Fresh(summary) => Some(summary),
            Self::Missing | Self::Stale => None,
        }
    }
}

fn json_u64(value: &serde_json::Value, key: &str) -> Option<u64> {
    value.get(key).and_then(serde_json::Value::as_u64)
}
