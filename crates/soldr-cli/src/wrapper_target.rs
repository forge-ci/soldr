//! Wrapper hot-path target-registry routing.
//!
//! Issue #474 introduced two routing paths for the per-invocation
//! `target/` registry write the wrapper performs:
//!
//! - **Fast path** — `SOLDR_BUILD_SESSION_ID` is NOT set in the env
//!   (i.e. this rustc invocation didn't come from `soldr cargo …`).
//!   The wrapper writes the row directly to redb and returns. No
//!   daemon IPC, no route-claim probe, no spawn attempt.
//! - **Slow path** — the session id IS set. The wrapper sends the
//!   target-touch IPC and opportunistically auto-spawns the daemon.
//!
//! Issue #440 added a third return path: when the cargo front door
//! has already recorded the same `target/` dir for the current build
//! session (signalled via `SOLDR_TARGET_REGISTRY_RECORDED=<dir>`),
//! the wrapper skips redb and the daemon target-touch IPC entirely.
//! Per-crate timing data rides the compile's own SESSION connection
//! rather than opening standalone telemetry connections (soldr#1537,
//! soldr#3224).
//!
//! Lives in its own module so the lib tree exposes it for the
//! integration tests under `tests/cargo_front_door/cli_wrapper_perf.rs` without
//! dragging the full `wrapper.rs` (which depends on bin-only modules)
//! into the lib's compile.

use crate::core::SoldrPaths;
use std::path::{Path, PathBuf};

/// Env var the cargo front door sets after recording the workspace
/// `target/` dir for the build session. When present and matching
/// the resolved target dir on a wrapper invocation, the registry
/// write is short-circuited (issue #440 — eliminates ~14 ms of redb
/// open + upsert per rustc invocation on Windows).
pub const TARGET_REGISTRY_RECORDED_ENV_VAR: &str = "SOLDR_TARGET_REGISTRY_RECORDED";

/// Outcome of a single `record_target_dir_in_registry` call. Used by
/// `wrapper::run_rustc_wrapper` to emit a phase marker matching the
/// path taken, and by the unit tests in this crate to assert that
/// the fast / slow path was selected for the right reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetTouchPath {
    /// No workspace `target/` dir resolvable from the rustc arg list.
    NoTarget,
    /// `SoldrPaths::new()` failed (no `$HOME` / `$USERPROFILE` / cache
    /// dir resolvable).
    NoPaths,
    /// Daemon routing is always used (soldr#2249).
    /// in the environment — direct redb write only.
    DaemonFirst,
    /// `SOLDR_TARGET_REGISTRY_RECORDED` matches the resolved target
    /// dir — the cargo front door already upserted for this build
    /// session. Issue #440. No redb open, no target-touch IPC; the
    /// compile request still carries lifecycle telemetry when in-session.
    MemoSkipped,
}

/// Best-effort upsert of the workspace `target/` dir into the soldr
/// state registry on every wrapper invocation. Silent on failure.
///
/// See module docs for the three-path routing.
pub fn record_target_dir_in_registry(rustc_args: &[String]) -> TargetTouchPath {
    let Some(target) = crate::cache_lib::target_registry::resolve_workspace_target_dir(rustc_args)
    else {
        return TargetTouchPath::NoTarget;
    };
    let Ok(paths) = SoldrPaths::new() else {
        return TargetTouchPath::NoPaths;
    };

    // Memoization (issue #440): if the cargo front door already
    // upserted this target dir for this build session, the redb open
    // + write is pure repetition. Skip both the direct redb write
    // and the daemon target-touch IPC. Per-crate telemetry is attached
    // later to the compile request itself.
    let memo_hit = target_registry_memo_matches(&target);

    if memo_hit {
        // No daemon spawn probe here — the cargo front door already
        // owns spawn for the session.
        return TargetTouchPath::MemoSkipped;
    }

    // Target touches always route through the daemon (soldr#2249).
    // Slow path: in-session, daemon for target-touch. Compile lifecycle
    // telemetry rides the subsequent SESSION compile connection.
    crate::daemon::client::record_target_touch_or_fallback(&paths, &target);

    // Compile dispatch owns bounded daemon startup and recovery. Starting it
    // here would duplicate alias materialization/relocation before that budget.
    TargetTouchPath::DaemonFirst
}

/// True when `SOLDR_TARGET_REGISTRY_RECORDED` is set AND its value
/// path-equals the workspace `target/` the wrapper resolved. Uses
/// canonical-on-best-effort comparison so different casings or
/// absolute-vs-canonical forms of the same path still match.
pub(crate) fn target_registry_memo_matches(resolved: &Path) -> bool {
    let raw = match std::env::var_os(TARGET_REGISTRY_RECORDED_ENV_VAR) {
        Some(v) if !v.is_empty() => v,
        _ => return false,
    };
    let recorded = PathBuf::from(raw);
    if recorded == resolved {
        return true;
    }
    // Path equality fall-back: canonicalize both, ignore errors.
    let recorded_canon = std::fs::canonicalize(&recorded).ok();
    let resolved_canon = std::fs::canonicalize(resolved).ok();
    matches!(
        (recorded_canon.as_deref(), resolved_canon.as_deref()),
        (Some(a), Some(b)) if a == b
    )
}

/// Issue #1814: the fast path must never inherit the 5 s cross-process open
/// budget. This write is GC bookkeeping — a `target/` last-used timestamp that
/// the next rustc invocation re-touches — so under contention from another
/// wrapper, the daemon, or a GC pass we skip it rather than park a compile
/// behind someone else's redb handle. `open_best_effort` already emits the
/// loud + durable contention record, so the skip is never silent.
///
/// Deliberately *not* routed through the daemon IPC: this path is chosen
/// precisely when `SOLDR_BUILD_SESSION_ID` is unset (a bare `cargo` run with
/// `RUSTC_WRAPPER=soldr`), where there may be no daemon at all and #474 exists
/// to avoid paying a connect attempt per invocation.
pub fn read_build_session_id_env() -> Option<u64> {
    std::env::var(crate::cache_lib::SOLDR_BUILD_SESSION_ID_ENV_VAR)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
}

#[cfg(test)]
mod memo_tests {
    use super::*;
    use crate::platform::sync::Mutex;
    use std::ffi::OsString;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        key: &'static str,
        previous: Option<OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }
        fn remove(key: &'static str) -> Self {
            let previous = std::env::var_os(key);
            std::env::remove_var(key);
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(v) = &self.previous {
                std::env::set_var(self.key, v);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    #[test]
    fn memo_returns_false_when_env_unset() {
        let _lock = ENV_LOCK.lock();
        let _g = EnvGuard::remove(TARGET_REGISTRY_RECORDED_ENV_VAR);
        let temp = tempfile::tempdir().unwrap();
        assert!(!target_registry_memo_matches(temp.path()));
    }

    #[test]
    fn memo_returns_false_when_env_empty() {
        let _lock = ENV_LOCK.lock();
        let _g = EnvGuard::set(TARGET_REGISTRY_RECORDED_ENV_VAR, "");
        let temp = tempfile::tempdir().unwrap();
        assert!(!target_registry_memo_matches(temp.path()));
    }

    #[test]
    fn memo_matches_exact_path() {
        let _lock = ENV_LOCK.lock();
        let temp = tempfile::tempdir().unwrap();
        let _g = EnvGuard::set(TARGET_REGISTRY_RECORDED_ENV_VAR, temp.path().as_os_str());
        assert!(target_registry_memo_matches(temp.path()));
    }

    #[test]
    fn memo_rejects_unrelated_path() {
        let _lock = ENV_LOCK.lock();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let _g = EnvGuard::set(TARGET_REGISTRY_RECORDED_ENV_VAR, a.path().as_os_str());
        assert!(!target_registry_memo_matches(b.path()));
    }

    #[test]
    fn memo_matches_via_canonicalization() {
        // Build a path with redundant components (e.g. trailing dot-slash)
        // that canonicalizes to the same dir as the env-var value.
        let _lock = ENV_LOCK.lock();
        let temp = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(temp.path()).unwrap();
        let _g = EnvGuard::set(TARGET_REGISTRY_RECORDED_ENV_VAR, canonical.as_os_str());
        // The wrapper's `resolved` may come in as the non-canonical
        // tempfile path; both routes should compare equal.
        assert!(target_registry_memo_matches(temp.path()));
    }
}
