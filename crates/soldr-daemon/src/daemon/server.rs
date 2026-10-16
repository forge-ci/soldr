//! Tokio-based soldr-daemon server. Accepts connections on a platform
//! socket / named pipe, decodes one frame per connection, dispatches
//! through the redb-backed target registry, and tracks lightweight
//! per-process state (request count, last activity, optional linked
//! zccache pid placeholder for Phase 3).

use crate::cache_lib::cook_index::{self, CookEntry, CookKey};
use crate::cache_lib::target_registry::TargetRegistry;
use crate::cache_lib::{data_db_path, soldr_daemon_dir};
use crate::core::SoldrPaths;
use crate::daemon::backend_handle_adoption::{
    current_daemon_process, soldr_backend_endpoint_mux, CONTROL_FRAME_HEADER_BYTES,
};
use crate::daemon::build_session_ops::{
    attach_build_log_history, finalize_build_session, merge_build_session_start,
};
use crate::daemon::db;
use crate::daemon::db_async;
use crate::daemon::disconnect::DispatchOutcome;
use crate::daemon::event_batcher::EventBatcher;
use crate::daemon::ipc::{read_frame_async_with_prefix, write_frame_async};
use crate::daemon::ipc_peer::PeerIdentity;
use crate::daemon::lifecycle::{append_lifecycle_event, claimed_daemon_occupies_route, is_live};
use crate::daemon::protocol::{
    BuildRecord, CookStats, Request, Response, ShutdownAck, StatusInfo, CHUNK_BYTES,
    COMPILE_BACKEND_EMBEDDED, PROTOCOL_VERSION,
};
use crate::zccache_embedded::SoldrZccacheService;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Upper bound on the drift diagnostic returned in [`Response::CookMiss`].
/// Keeps the body well under [`crate::daemon::protocol::MAX_BODY_BYTES`]
/// and matches PR 3's expectation of "most recent N prior recipe
/// hashes for this origin+target".
const COOK_DRIFT_LIMIT: usize = 8;

// soldr#1782 made this `Duration::MAX` so the daemon would stay resident to
// own five-minute pressure checks and daily age retention. That premise does
// not survive contact with `maintenance::run_loop_inner`, which states in its
// own comment that "the first tick is immediate" and that an absent or overdue
// full marker yields an immediate catch-up. The markers are persisted under
// `<root>/cache/soldr-daemon/`, so a daemon that exits while idle resumes the
// schedule on its next start instead of losing it.
//
// What the unbounded default did cost is a population. `broker_route_identity`
// keys a broker route on the canonicalized soldr root, so every caller with
// its own `SOLDR_CACHE_DIR` -- every test fixture, notably -- gets a distinct
// route and a daemon that then lives forever. A single run of this
// repository's own suite left 63 of them resident.
//
// 1800s is the value that predates soldr#1782, restored rather than reinvented.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(1800);
const IDLE_POLL_INTERVAL: Duration = Duration::from_secs(30);
const HANDSHAKE_READ_TIMEOUT: Duration = Duration::from_secs(5);

fn positive_env_value<F>(name: &str, lookup: F) -> Option<usize>
where
    F: FnOnce(&str) -> Option<String>,
{
    lookup(name)?
        .trim()
        .parse::<usize>()
        .ok()
        .filter(|value| *value > 0)
}

fn windows_listener_pool_size_from(logical_cpus: usize, override_value: Option<usize>) -> usize {
    override_value.unwrap_or_else(|| logical_cpus.saturating_mul(4).clamp(16, 128))
}

fn windows_listener_pool_size() -> usize {
    let logical = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    windows_listener_pool_size_from(
        logical,
        positive_env_value("SOLDR_WINDOWS_PIPE_LISTENER_POOL", |name| {
            std::env::var(name).ok()
        }),
    )
}

/// soldr#3169 test seam: hold a starting daemon between claiming its control
/// endpoint and serving it, so a test can show what a client meets in that
/// window. Debug builds only; a release daemon never reads the variable.
#[cfg(debug_assertions)]
async fn test_pause_before_serving_control() {
    let Some(milliseconds) = std::env::var("SOLDR_TEST_DAEMON_SERVE_PAUSE_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
    else {
        return;
    };
    tokio::time::sleep(Duration::from_millis(milliseconds)).await;
}

#[cfg(not(debug_assertions))]
async fn test_pause_before_serving_control() {}

/// `[jobs].max_parallel_compiles` from `config.toml`, or `None` when
/// the config is absent or unreadable.
///
/// A malformed config must not stop the daemon from starting — it
/// falls through to the next precedence tier, same as an unset value.
pub(crate) fn config_compile_jobs() -> Option<usize> {
    let paths = crate::core::SoldrPaths::new().ok()?;
    paths.load_config().ok()?.jobs.max_parallel_compiles
}

#[cfg(test)]
mod shutdown_backstop_tests {
    use super::*;

    #[test]
    fn watchdog_grace_defaults_and_is_never_disabled() {
        assert_eq!(parse_watchdog_grace(None), Some(SHUTDOWN_WATCHDOG_GRACE));
        assert_eq!(
            parse_watchdog_grace(Some("90")),
            Some(Duration::from_secs(90))
        );
        assert_eq!(
            parse_watchdog_grace(Some(" 90 ")),
            Some(Duration::from_secs(90))
        );

        // soldr#3648: zero must not remove the only exit backstop; it uses
        // the default like any other unusable override.
        for zero in ["0", " 0 "] {
            assert_eq!(
                parse_watchdog_grace(Some(zero)),
                Some(SHUTDOWN_WATCHDOG_GRACE),
                "zero override {zero:?} must fall back to the default"
            );
        }

        // A typo must NOT silently disable the only thing guaranteeing the
        // process exits — fall back to the default instead.
        for bogus in ["", "abc", "-1", "30s", "1.5"] {
            assert_eq!(
                parse_watchdog_grace(Some(bogus)),
                Some(SHUTDOWN_WATCHDOG_GRACE),
                "malformed override {bogus:?} must fall back to the default"
            );
        }
    }

    #[test]
    fn watchdog_fires_before_the_client_stops_waiting() {
        // If the backstop outlived the client's patience it would be
        // pointless: `daemon stop` would report failure and leave the
        // process running anyway.
        assert!(
            SHUTDOWN_WATCHDOG_GRACE < crate::daemon::lifecycle::GRACEFUL_SHUTDOWN_WAIT_TIMEOUT,
            "watchdog grace must be under the client's graceful-shutdown timeout"
        );
    }

    // `wait()` must observe a `request()` that races it.
    //
    // `notify_waiters()` stores no permit, so the previous
    // `while !is_requested() { notified().await }` could park forever when
    // the request landed between the flag check and the registration. This
    // races the two sides repeatedly; the timeout turns the old hang into a
    // clean failure instead of wedging the suite.
    //
    // Probabilistic by nature — the window is a few instructions wide — so
    // it is a regression net, not a proof. The ordering guarantee itself is
    // established by `enable()`-before-check in `ShutdownSignal::wait`.
    #[test]
    fn shutdown_wait_observes_a_racing_request() {
        use crate::daemon::maintenance::ShutdownSignal;
        use std::sync::Arc;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("tokio rt");

        rt.block_on(async {
            for iteration in 0..500 {
                let signal = Arc::new(ShutdownSignal::default());
                let waiter = tokio::spawn({
                    let signal = signal.clone();
                    async move { signal.wait().await }
                });
                // Let the waiter get as close to its flag check as possible
                // before the request lands.
                tokio::task::yield_now().await;
                signal.request();

                tokio::time::timeout(Duration::from_secs(10), waiter)
                    .await
                    .unwrap_or_else(|_| {
                        panic!("wait() missed the request on iteration {iteration}")
                    })
                    .expect("waiter task panicked");
            }
        });
    }
}

#[cfg(test)]
mod tokio_console_config_tests {
    use super::*;

    #[test]
    fn publish_interval_accepts_positive_milliseconds() {
        assert_eq!(
            parse_tokio_console_publish_interval(Some("20")),
            Some(Duration::from_millis(20))
        );
        assert_eq!(
            parse_tokio_console_publish_interval(Some(" 250 ")),
            Some(Duration::from_millis(250))
        );
    }

    #[test]
    fn publish_interval_ignores_missing_zero_and_malformed_values() {
        for raw in [None, Some(""), Some("0"), Some("-1"), Some("20ms")] {
            assert_eq!(parse_tokio_console_publish_interval(raw), None);
        }
    }
}

#[cfg(test)]
mod listener_pool_tests {
    use super::*;

    #[test]
    fn listener_pool_defaults_and_override_are_bounded() {
        assert_eq!(windows_listener_pool_size_from(1, None), 16);
        assert_eq!(windows_listener_pool_size_from(12, None), 48);
        assert_eq!(windows_listener_pool_size_from(64, None), 128);
        assert_eq!(windows_listener_pool_size_from(2, Some(3)), 3);
    }
}

#[derive(Debug, Clone)]
pub struct ServerOptions {
    pub idle_timeout: Duration,
    /// Tie this daemon's lifetime to another process.
    ///
    /// A test fixture that starts a daemon under its own `SOLDR_CACHE_DIR`
    /// owns a route nothing else will ever use again, so the daemon has no
    /// reason to outlive the fixture -- and if the fixture is killed rather
    /// than allowed to clean up, nothing else will ever stop it. Naming the
    /// owner makes the daemon's death the owner's death.
    ///
    /// This is deliberately not `PR_SET_PDEATHSIG`: the daemon's parent is
    /// the broker, not the process that wanted the daemon, so parent-death
    /// signalling watches the wrong process. See the polling caveat on
    /// [`owner_has_exited`].
    pub owner_pid: Option<u32>,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            idle_timeout: DEFAULT_IDLE_TIMEOUT,
            owner_pid: None,
        }
    }
}

/// True once `owner` names a process that is gone. `None` means the daemon
/// has no owner and keeps its own lifetime.
///
/// Liveness is polled rather than awaited, which leaves a PID-reuse window:
/// between two polls the owner can exit and the kernel can hand its number to
/// an unrelated process, and the daemon would then watch a stranger. Closing
/// that properly needs a handle the kernel keeps valid across the exit --
/// `pidfd_open` on Linux, `EVFILT_PROC`/`NOTE_EXIT` on macOS, a process handle
/// or job object on Windows -- which is a portable primitive that belongs in
/// `kernal-api` rather than three copies here. The exposure is one poll
/// interval against a 32-bit PID space, and the failure mode is a daemon that
/// lives too long, which is the status quo this replaces.
fn owner_has_exited(owner: Option<u32>) -> bool {
    match owner {
        None => false,
        Some(pid) => !crate::platform::process::inspect::is_alive(pid),
    }
}

#[derive(Debug)]
pub enum ServerError {
    AlreadyRunning(u32),
    #[allow(dead_code)] // surfaced via {:?} on the Err arm in `run`
    Io(std::io::Error),
    #[allow(dead_code)]
    Registry(crate::cache_lib::target_registry::RegistryError),
    #[allow(dead_code)]
    Paths(crate::core::SoldrError),
}

impl From<std::io::Error> for ServerError {
    fn from(e: std::io::Error) -> Self {
        ServerError::Io(e)
    }
}

impl From<crate::cache_lib::target_registry::RegistryError> for ServerError {
    fn from(e: crate::cache_lib::target_registry::RegistryError) -> Self {
        ServerError::Registry(e)
    }
}

impl From<crate::core::SoldrError> for ServerError {
    fn from(e: crate::core::SoldrError) -> Self {
        ServerError::Paths(e)
    }
}

struct State {
    /// Path to the shared `state.sqlite3`. The daemon opens this on
    /// demand for each write rather than holding the redb handle for
    /// its lifetime — redb refuses concurrent multi-process opens
    /// ("Database already open. Cannot acquire lock."), and the
    /// pre-existing CLI surface (`soldr gc list`, `soldr cache ...`)
    /// also opens the same file directly. Per-request opens are
    /// microseconds in steady state; the daemon's value is single-
    /// writer ordering and build-session correlation, not avoiding
    /// the per-write fs::open cost.
    db_path: PathBuf,
    /// Resolved `~/.soldr/` layout cached for the daemon's lifetime
    /// so cook-artifact path construction does not need to re-probe
    /// HOME/SOLDR_CACHE_DIR on every IPC request.
    paths: SoldrPaths,
    /// Identity returned to `running-process` BackendHandle endpoint
    /// probes. The client-side liveness check only trusts the route
    /// claim after this endpoint response matches.
    daemon_identity: running_process::broker::backend_handle::DaemonProcess,
    start_instant: Instant,
    request_count: AtomicU64,
    last_activity_ms: AtomicU64,
    /// True when the idle watchdog drove the shutdown. Lets the main
    /// task tag the lifecycle event as `died-idle` instead of
    /// `died-shutdown`.
    exit_via_idle: AtomicBool,
    /// In-process count of [`Request::CookLookup`] hits served since
    /// this daemon's last startup. Surfaced via [`Request::Status`]
    /// for `soldr daemon status` and `soldr doctor`. Resets across
    /// daemon restarts by design — long-lived totals belong in the
    /// redb `cook_index_v1` table, not in process-local state.
    cook_hits_this_session: AtomicU64,
    shutdown: Arc<crate::daemon::maintenance::ShutdownSignal>,
    /// L4 (issue soldr#980) — background drain task that coalesces
    /// per-compile redb event writes. The SESSION compile handler
    /// pushes into a tokio mpsc instead of opening redb directly. See
    /// [`crate::daemon::event_batcher`] for the batching contract.
    event_batcher: EventBatcher,
    /// In-process zccache compile service (issue #977 / #980 L1). The
    /// daemon always owns one — the wrapped/fork-zccache.exe
    /// alternative was removed in the L1 second pass. `start()` is
    /// fallible at daemon boot; if it fails the daemon refuses to
    /// start rather than silently degrade.
    compile_service: Arc<SoldrZccacheService>,
}

impl State {
    fn touch_activity(&self) {
        let elapsed_ms = self.start_instant.elapsed().as_millis() as u64;
        self.last_activity_ms.store(elapsed_ms, Ordering::Relaxed);
    }

    fn idle_for(&self) -> Duration {
        let last_ms = self.last_activity_ms.load(Ordering::Relaxed);
        let now_ms = self.start_instant.elapsed().as_millis() as u64;
        Duration::from_millis(now_ms.saturating_sub(last_ms))
    }

    fn status(&self) -> StatusInfo {
        // SQLite WAL allows this read alongside other connections;
        // no process-wide open mutex is involved.
        let (entries, total_bytes) = cook_index::stats(&self.db_path).unwrap_or((0, 0));
        let (compile_jobs, compile_jobs_source) =
            crate::compile_limit::wire_pair(self.compile_service.applied_jobs());
        StatusInfo {
            version: PROTOCOL_VERSION,
            pid: std::process::id(),
            generation: self.daemon_identity.started_at_unix_ms,
            uptime_secs: self.start_instant.elapsed().as_secs(),
            request_count: self.request_count.load(Ordering::Relaxed),
            cook_stats: Some(CookStats {
                entries,
                total_bytes,
                hits_this_session: self.cook_hits_this_session.load(Ordering::Relaxed),
            }),
            // Always "embedded" since #980 L1 second pass; field
            // retained for telemetry stability.
            compile_backend: COMPILE_BACKEND_EMBEDDED.to_string(),
            compile_jobs,
            compile_jobs_source,
            in_flight_compiles: Some(
                u32::try_from(crate::daemon::inflight_compiles::count()).unwrap_or(u32::MAX),
            ),
        }
    }
}

fn current_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
#[allow(unused_must_use)]
mod build_session_start_tests {
    use super::merge_build_session_start;
    use crate::daemon::protocol::{BuildCacheSummary, BuildLogPaths, BuildMissReason, BuildRecord};

    #[test]
    fn late_start_preserves_finalized_build_history() {
        let cache_summary = BuildCacheSummary {
            hits: 1,
            misses: 2,
            non_cacheable: 3,
            errors: 4,
            compilations: 5,
            time_saved_ms: 6,
        };
        let log_paths = BuildLogPaths {
            zccache_session_id: Some("zccache-session".to_string()),
            cache_dir: Some("cache".to_string()),
            session_log_path: Some("last-session.log".to_string()),
            journal_path: Some("last-session.jsonl".to_string()),
            session_stats_path: Some("last-session-stats.json".to_string()),
            compile_journal_path: Some("compile_journal.jsonl".to_string()),
            archived_session_log_path: Some("history/last-session.log".to_string()),
            archived_journal_path: Some("history/last-session.jsonl".to_string()),
            archived_session_stats_path: Some("history/last-session-stats.json".to_string()),
            archived_compile_journal_path: Some("history/compile_journal.jsonl".to_string()),
            private_daemon_name: Some("private-daemon".to_string()),
        };
        let miss_reason = BuildMissReason {
            reason: "source changed".to_string(),
            count: 7,
        };
        let existing = BuildRecord {
            session_id: 42,
            repo_root: "stale".to_string(),
            started_at_ms: 2000,
            ended_at_ms: Some(2600),
            exit_code: Some(0),
            total_wall_ms: Some(600),
            crate_count: 8,
            slowest_crate_us: Some(900),
            slowest_crate_name: Some("slow_crate".to_string()),
            cache_summary: Some(cache_summary.clone()),
            log_paths: Some(log_paths.clone()),
            miss_reasons: vec![miss_reason.clone()],
        };

        let merged = merge_build_session_start(Some(existing), 42, "repo".to_string(), 1000);

        assert_eq!(merged.session_id, 42);
        assert_eq!(merged.repo_root, "repo");
        assert_eq!(merged.started_at_ms, 1000);
        assert_eq!(merged.ended_at_ms, Some(2600));
        assert_eq!(merged.exit_code, Some(0));
        assert_eq!(merged.total_wall_ms, Some(1600));
        assert_eq!(merged.crate_count, 8);
        assert_eq!(merged.slowest_crate_us, Some(900));
        assert_eq!(merged.slowest_crate_name.as_deref(), Some("slow_crate"));
        assert_eq!(merged.cache_summary, Some(cache_summary));
        assert_eq!(merged.log_paths, Some(log_paths));
        assert_eq!(merged.miss_reasons, vec![miss_reason]);
    }
}

/// Start the embedded zccache compile service at daemon boot. Issue
/// #977 / #980 L1 — embedded is mandatory; on failure the daemon
/// itself refuses to start so callers see a hard error instead of a
/// silent degrade. No env-var gating, no feature gating, no fallback.
async fn start_compile_service(
    paths: &SoldrPaths,
    daemon_identity: &running_process::broker::backend_handle::DaemonProcess,
) -> Result<Arc<SoldrZccacheService>, ServerError> {
    match SoldrZccacheService::start(paths, daemon_identity).await {
        Ok(svc) => {
            tracing::info!("soldr-daemon: embedded zccache backend active");
            Ok(Arc::new(svc))
        }
        Err(err) => Err(ServerError::Io(std::io::Error::other(format!(
            "embedded zccache service failed to start: {err}"
        )))),
    }
}

/// Drain and stop the embedded service before the daemon exits. Best-effort:
/// errors are logged but never block daemon exit.
async fn shutdown_compile_service(state: &Arc<State>) {
    match state
        .compile_service
        .as_ref()
        .clone()
        .shutdown(zccache::embedded::ShutdownMode::Graceful)
        .await
    {
        Ok(()) => tracing::debug!("soldr-daemon: embedded zccache shutdown complete"),
        Err(err) => tracing::warn!("soldr-daemon: embedded zccache shutdown failed: {err}"),
    }
}

// Synchronous entry point used by both the `soldr-daemon` bin target
// and `soldr daemon start --foreground`. Builds a tokio runtime and
// blocks until the daemon exits.
// Env var that turns on the tokio-console layer for the daemon:
// `SOLDR_DAEMON_TOKIO_CONSOLE=1`. Only functional when soldr-cli is
// built with the `tokio-console` feature and `RUSTFLAGS="--cfg
// tokio_unstable"`; otherwise it degrades to a warning (see
// [`maybe_init_tokio_console`]).
include!("server_runtime.rs");
include!("server_accept.rs");
include!("server_dispatch.rs");
include!("server_compile.rs");

#[cfg(test)]
mod lifetime_tests {
    use super::*;

    /// soldr#1782 set the default lifetime to `Duration::MAX` on the premise
    /// that the daemon must stay resident to own five-minute pressure checks
    /// and daily age retention. `maintenance::run_loop_inner` disproves that
    /// premise in the same tree: "The first tick is immediate. An
    /// absent/overdue full marker yields an immediate catch-up", and the
    /// markers live under `<root>/cache/soldr-daemon/`, so a restart resumes
    /// the schedule rather than losing it.
    ///
    /// The cost of the unbounded default is a population, not one process:
    /// `broker_route_identity` keys a route on the canonicalized soldr root,
    /// so every test fixture with its own `SOLDR_CACHE_DIR` earns a distinct
    /// route and a daemon that outlives the fixture forever. One measured run
    /// of this repository's own suite left 63 live daemons behind.
    #[test]
    fn the_default_daemon_lifetime_is_bounded() {
        assert_ne!(
            ServerOptions::default().idle_timeout,
            Duration::MAX,
            "an unbounded default lifetime leaks one immortal daemon per soldr root"
        );
    }

    /// The watchdog is only spawned when the timeout is not `Duration::MAX`
    /// (`server_runtime.rs`), so a default that never trips is the same thing
    /// as no watchdog at all. Pin the value that predates soldr#1782.
    #[test]
    fn the_default_daemon_lifetime_is_the_pre_1782_value() {
        assert_eq!(
            ServerOptions::default().idle_timeout,
            Duration::from_secs(1800)
        );
    }

    /// A daemon started for a test fixture must not outlive the process that
    /// asked for it. Without an owner the daemon keeps its own lifetime.
    #[test]
    fn an_owner_pid_is_absent_by_default() {
        assert_eq!(ServerOptions::default().owner_pid, None);
    }

    /// The owner watch is a liveness poll, so it must treat "the owner is
    /// gone" as terminal and "the owner is alive" as no reason to act.
    #[test]
    fn the_owner_watch_stops_exactly_when_the_owner_is_gone() {
        assert!(
            owner_has_exited(Some(u32::MAX)),
            "an unallocated pid is gone"
        );
        assert!(
            !owner_has_exited(Some(std::process::id())),
            "this process is its own proof of liveness"
        );
        assert!(!owner_has_exited(None), "no owner is not a dead owner");
    }
}
