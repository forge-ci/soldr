//! Daemon-owned, exact-root cache maintenance (#1762–#1764).

use crate::cache_lib::gc::{self, GcOptions};
use crate::cache_lib::target_registry::TargetRegistry;
use crate::core::SoldrPaths;
use crate::daemon::{db, history_gc};

/// The shared cooperative shutdown primitive, re-exported at its original
/// path (soldr#3158 moved it to `daemon::shutdown_signal` when the broker
/// began sharing it).
pub use crate::daemon::shutdown_signal::ShutdownSignal;
use crate::zccache_embedded::{
    EmbeddedDiskMaintenanceReport, EmbeddedDiskPolicy, SoldrZccacheService,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod retired_status;

pub const PRESSURE_INTERVAL: Duration = Duration::from_secs(5 * 60);
pub const FULL_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const EVENT_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
// soldr#3079: the shared soldr-owned staleness gate, not a copy of it.
const PRESSURE_STALE_AGE: Duration = crate::cache_lib::gc_policy::STALENESS_GATE;
const FULL_STALE_AGE: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const STATUS_SCHEMA_VERSION: u32 = 2;
const FULL_MARKER: &str = "last-full-v1";
const FULL_ATTEMPT_MARKER: &str = "last-full-attempt-v1";
/// How often a pass deferred by an active build still maintains the embedded
/// store (soldr#3251). Each store pass scans the whole store, so it is throttled
/// unless the store is already over its budget.
const STORE_PASS_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// Consecutive build-deferred full passes after which a deferred full pass
/// still runs its lease-free, build-safe work (soldr#3329). A deferred full
/// pass is retried every pressure tick, so ~12 pressure ticks = 1h.
const FULL_STARVATION_DEFERRALS: u32 = 12;

#[derive(Clone)]
pub struct MaintenanceContext {
    pub paths: SoldrPaths,
    pub db_path: PathBuf,
    pub compile_service: Arc<SoldrZccacheService>,
    pub shutdown: Arc<ShutdownSignal>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaintenanceKind {
    Pressure,
    Full,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComponentOutcome {
    pub items_removed: u64,
    pub bytes_reclaimed: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaintenanceStatus {
    pub schema_version: u32,
    pub owning_root: String,
    pub daemon_identity: String,
    pub embedded_cache_root: String,
    pub disk_policy: EmbeddedDiskPolicy,
    pub attempted_at_ms: i64,
    pub successful_at_ms: Option<i64>,
    pub last_full_at_ms: Option<i64>,
    pub kind: MaintenanceKind,
    pub deferred_reason: Option<String>,
    pub filesystem_capacity_bytes: Option<u64>,
    pub filesystem_free_bytes: Option<u64>,
    pub filesystem_free_percent: Option<f64>,
    pub zccache: Option<EmbeddedDiskMaintenanceReport>,
    pub zccache_error: Option<String>,
    /// When [`Self::zccache`] was measured. A deferred pass never reaches the
    /// store, so it carries the last measured report forward with its original
    /// time rather than blanking it (soldr#3077). Absent in files written
    /// before this field existed.
    #[serde(default)]
    pub zccache_measured_at_ms: Option<i64>,
    pub cook: ComponentOutcome,
    pub history: ComponentOutcome,
    pub pep517_targets: ComponentOutcome,
    pub pep517_wheels: ComponentOutcome,
    pub trash: ComponentOutcome,
    pub workspace_targets: ComponentOutcome,
    pub daemon_events: ComponentOutcome,
    pub legacy_zccache: ComponentOutcome,
    /// Bytes held by retired embedded zccache version stores, measured before
    /// this pass swept them (soldr#3329). Retired stores sit beside the live
    /// store, so `zccache.usage_before_bytes` alone understates the footprint.
    #[serde(default)]
    pub retired_store_bytes: Option<u64>,
    #[serde(default)]
    pub retired_store_count: Option<u64>,
    /// Full passes deferred by an active build in a row (soldr#3329).
    #[serde(default)]
    pub consecutive_full_deferrals: u32,
}

impl MaintenanceStatus {
    fn new(context: &MaintenanceContext, kind: MaintenanceKind, now: SystemTime) -> Self {
        let (capacity, free) = filesystem_space(&context.paths.root);
        let free_percent = capacity
            .zip(free)
            .filter(|(capacity, _)| *capacity > 0)
            .map(|(capacity, free)| free as f64 * 100.0 / capacity as f64);
        Self {
            schema_version: STATUS_SCHEMA_VERSION,
            owning_root: context.paths.root.display().to_string(),
            daemon_identity: format!(
                "{}:{}",
                context.compile_service.identity().product,
                context.compile_service.identity().instance_id
            ),
            embedded_cache_root: context.compile_service.cache_root().display().to_string(),
            disk_policy: context.compile_service.disk_policy().clone(),
            attempted_at_ms: unix_millis(now),
            successful_at_ms: None,
            last_full_at_ms: read_last_full(&context.paths).map(unix_millis),
            kind,
            deferred_reason: None,
            filesystem_capacity_bytes: capacity,
            filesystem_free_bytes: free,
            filesystem_free_percent: free_percent,
            zccache: None,
            zccache_error: None,
            zccache_measured_at_ms: None,
            cook: ComponentOutcome::default(),
            history: ComponentOutcome::default(),
            pep517_targets: ComponentOutcome::default(),
            pep517_wheels: ComponentOutcome::default(),
            trash: ComponentOutcome::default(),
            workspace_targets: ComponentOutcome::default(),
            daemon_events: ComponentOutcome::default(),
            legacy_zccache: ComponentOutcome::default(),
            retired_store_bytes: None,
            retired_store_count: None,
            consecutive_full_deferrals: 0,
        }
    }
}

pub fn status_path(paths: &SoldrPaths) -> PathBuf {
    paths
        .cache
        .join("soldr-daemon")
        .join("maintenance-status-v1.json")
}

pub fn read_status(paths: &SoldrPaths) -> Option<MaintenanceStatus> {
    let body = std::fs::read_to_string(status_path(paths)).ok()?;
    serde_json::from_str(&body).ok()
}

pub async fn run_loop(context: MaintenanceContext) {
    let paths = context.paths.clone();
    let shutdown = Arc::clone(&context.shutdown);
    run_loop_inner(&paths, shutdown, PRESSURE_INTERVAL, |kind, now| {
        let context = &context;
        async move {
            let outcome = run_once_with_lease_state(context, kind, now).await;
            let status = outcome.status;
            let succeeded = status.successful_at_ms.is_some();
            if succeeded && kind == MaintenanceKind::Full {
                let _ = record_last_full(&context.paths, now);
            }
            let _ = persist_status(&context.paths, status);
            outcome.lease_acquired
        }
    })
    .await;
}

async fn run_loop_inner<F, Fut>(
    paths: &SoldrPaths,
    shutdown: Arc<ShutdownSignal>,
    interval_duration: Duration,
    mut run_pass: F,
) where
    F: FnMut(MaintenanceKind, SystemTime) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let mut last_pressure = None;
    // A persistent component failure remains visible in status, but must not
    // promote every five-minute pressure tick into another expensive full
    // scan. Successful completion has its own marker; this attempt marker is
    // only scheduling backoff.
    let mut last_full_attempt = read_last_full_attempt(paths).or_else(|| read_last_full(paths));
    // The first tick is immediate.  An absent/overdue full marker yields an
    // immediate catch-up; otherwise startup still receives a pressure pass.
    let mut interval = tokio::time::interval(interval_duration);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // soldr#1987: a daemon whose own executable was deleted keeps the
    // root-ownership lock forever, and nothing external can clear it --
    // `soldr daemon stop` probes the pipe while the orphan holds the
    // filesystem lock. Such a daemon can never legitimately own the root
    // again, so it stands down itself.
    let mut missing_image = crate::daemon::lifecycle::MissingImageDetector::default();
    loop {
        tokio::select! {
            _ = shutdown.wait() => break,
            _ = interval.tick() => {}
        }
        if shutdown.is_requested() {
            break;
        }
        if missing_image.observe(crate::daemon::lifecycle::daemon_image_present()) {
            // Loud on purpose: this is the only notice that a daemon
            // disappeared on its own, and a silent exit would look like the
            // crash it is not.
            tracing::warn!(
                event = "daemon_image_deleted",
                strikes = missing_image.strikes(),
                "this daemon's executable no longer exists; releasing the soldr                  root and shutting down so a new daemon can start (soldr#1987)"
            );
            eprintln!(
                "soldr-daemon: own executable no longer exists after {} checks;                  shutting down to release the soldr root (soldr#1987)",
                missing_image.strikes()
            );
            shutdown.request();
            break;
        }
        // soldr#3164: keep this daemon's own image ledger fresh so the
        // broker's route sweep never reclaims the image a live daemon runs.
        if let Ok(executable) = std::env::current_exe() {
            crate::self_relocate::refresh_running_image_ledger(&executable);
        }
        let now = SystemTime::now();
        let kind =
            due_kind(last_full_attempt, last_pressure, now).unwrap_or(MaintenanceKind::Pressure);
        let pass_started = run_pass(kind, now).await;
        if kind == MaintenanceKind::Full && pass_started {
            last_full_attempt = Some(now);
            let _ = record_last_full_attempt(paths, now);
        }
        last_pressure = Some(now);
    }
}

pub async fn run_once(
    context: &MaintenanceContext,
    kind: MaintenanceKind,
    now: SystemTime,
) -> MaintenanceStatus {
    run_once_with_lease_state(context, kind, now).await.status
}

struct RunOnceOutcome {
    status: MaintenanceStatus,
    lease_acquired: bool,
}

async fn run_once_with_lease_state(
    context: &MaintenanceContext,
    kind: MaintenanceKind,
    now: SystemTime,
) -> RunOnceOutcome {
    let mut status = MaintenanceStatus::new(context, kind, now);
    let previous = read_status(&context.paths);
    let previous_deferrals = previous
        .as_ref()
        .map_or(0, |previous| previous.consecutive_full_deferrals);
    status.consecutive_full_deferrals = previous_deferrals;
    // Measured before any sweep, so the report shows what the pass found.
    retired_status::measure_retired(context, &mut status);
    let _maintenance_lease =
        match crate::cache_lib::build_active::MaintenanceLease::try_acquire(&context.paths) {
            Ok(None) => {
                // The build defers soldr's own destructive collectors, which
                // need the exclusive root lease. The store pass does not
                // (soldr#3251): on a host that is always building, deferring it
                // too meant it never ran.
                status.deferred_reason = Some("build_active".to_string());
                let mut escape = false;
                if kind == MaintenanceKind::Full {
                    status.consecutive_full_deferrals = previous_deferrals.saturating_add(1);
                    // soldr#3329: a continuously building host would defer the
                    // full pass forever. At the threshold, run the build-safe
                    // part unconditionally: the store pass and the retired-store
                    // sweep, which proves death by the writer lock and removes
                    // only nlink==1 files (#3365). The counter restarts so the
                    // escape recurs once per threshold, not on every tick.
                    if status.consecutive_full_deferrals >= FULL_STARVATION_DEFERRALS {
                        escape = true;
                        status.consecutive_full_deferrals = 0;
                        status.deferred_reason = Some("build_active_starvation_escape".to_string());
                    }
                }
                if escape || store_pass_due(previous.as_ref(), now) {
                    run_store_pass(context, kind, now, &mut status).await;
                }
                return RunOnceOutcome {
                    status,
                    lease_acquired: false,
                };
            }
            Err(error) => {
                status.deferred_reason = Some(format!("build_lease_probe_failed: {error}"));
                return RunOnceOutcome {
                    status,
                    lease_acquired: false,
                };
            }
            Ok(Some(lease)) => lease,
        };
    if kind == MaintenanceKind::Full {
        status.consecutive_full_deferrals = 0;
    }

    measure_store(context, kind == MaintenanceKind::Full, now, &mut status).await;
    let zccache_pressure = status
        .zccache
        .as_ref()
        .is_some_and(|report| report.pressure != "none");
    let local_pressure = root_below_pressure_threshold(
        &context.paths,
        status.filesystem_free_bytes,
        status.filesystem_capacity_bytes,
    );
    let do_pressure_cleanup = kind == MaintenanceKind::Full || zccache_pressure || local_pressure;

    let paths = context.paths.clone();
    let db_path = context.db_path.clone();
    let local = tokio::task::spawn_blocking(move || {
        run_local_components(&paths, &db_path, kind, now, do_pressure_cleanup)
    })
    .await;
    match local {
        Ok(local) => {
            status.cook = local.cook;
            status.history = local.history;
            status.pep517_targets = local.pep517_targets;
            status.pep517_wheels = local.pep517_wheels;
            status.trash = local.trash;
            status.workspace_targets = local.workspace_targets;
            status.daemon_events = local.daemon_events;
            status.legacy_zccache = local.legacy_zccache;
            let components_ok = status.zccache_error.is_none()
                && [
                    &status.cook,
                    &status.history,
                    &status.pep517_targets,
                    &status.pep517_wheels,
                    &status.trash,
                    &status.workspace_targets,
                    &status.daemon_events,
                    &status.legacy_zccache,
                ]
                .iter()
                .all(|component| component.error.is_none());
            if components_ok {
                status.successful_at_ms = Some(unix_millis(now));
                if kind == MaintenanceKind::Full {
                    status.last_full_at_ms = Some(unix_millis(now));
                }
            } else {
                status.deferred_reason = Some("one_or_more_components_failed".to_string());
            }
        }
        Err(error) => {
            status.deferred_reason = Some(format!("maintenance_worker_failed: {error}"));
        }
    }
    RunOnceOutcome {
        status,
        lease_acquired: true,
    }
}

/// Run a full pass for one explicitly supplied orphaned root.  This is the
/// only manual cross-invocation surface: it does not discover home-directory
/// siblings and refuses a root whose daemon is live.
pub async fn run_manual_root(root: PathBuf) -> Result<MaintenanceStatus, String> {
    if !root.is_absolute() {
        return Err("manual maintenance --root must be an absolute path".to_string());
    }
    let metadata = std::fs::symlink_metadata(&root)
        .map_err(|error| format!("manual maintenance root is unavailable: {error}"))?;
    if !metadata.is_dir() || crate::cache_lib::path_safety::is_link_or_reparse(&metadata) {
        return Err("manual maintenance --root must name a real directory, not a link".to_string());
    }
    let root = std::fs::canonicalize(root)
        .map_err(|error| format!("canonicalize manual maintenance root: {error}"))?;
    let paths = SoldrPaths::with_root(root);
    crate::cache_lib::path_safety::validate_owned_directory(&paths.root, &paths.root)
        .map_err(|error| format!("unsafe manual maintenance root: {error}"))?;
    let _legacy_root_owner =
        crate::daemon::lifecycle::RootOwnershipGuard::try_acquire_legacy_root(&paths)
            .map_err(|error| format!("acquire legacy root ownership: {error}"))?
            .ok_or_else(|| {
                "refusing orphan-root maintenance: legacy root ownership is busy".to_string()
            })?;
    let _root_owner = if crate::daemon::generation_key::generation_state_dir(&paths)
        != crate::cache_lib::soldr_daemon_dir(&paths)
    {
        Some(
            crate::daemon::lifecycle::RootOwnershipGuard::try_acquire(&paths)
                .map_err(|error| format!("acquire root ownership: {error}"))?
                .ok_or_else(|| {
                    "refusing orphan-root maintenance: root ownership is busy".to_string()
                })?,
        )
    } else {
        None
    };
    if let Some(owner) = crate::daemon::other_generations::recorded_generation_owners(&paths)
        .into_iter()
        .find(|owner| crate::daemon::lifecycle::pid_is_alive(owner.pid))
    {
        return Err(format!(
            "refusing orphan-root maintenance: daemon pid {} still owns generation {}",
            owner.pid, owner.generation
        ));
    }
    if let Some(pid) = crate::daemon::lifecycle::claimed_daemon_occupies_route(&paths) {
        return Err(format!(
            "refusing orphan-root maintenance: daemon pid {pid} owns {}",
            paths.root.display()
        ));
    }
    std::fs::create_dir_all(&paths.cache)
        .map_err(|error| format!("create maintenance cache root: {error}"))?;
    let db_path = crate::cache_lib::data_db_path(&paths);
    TargetRegistry::open(&db_path)
        .map_err(|error| format!("open maintenance state database: {error}"))?;
    let identity = crate::daemon::backend_handle_adoption::current_daemon_process(&paths, None)
        .map_err(|error| format!("resolve manual maintenance identity: {error}"))?;
    let service = Arc::new(
        SoldrZccacheService::start(&paths, &identity)
            .await
            .map_err(|error| error.to_string())?,
    );
    let context = MaintenanceContext {
        paths: paths.clone(),
        db_path,
        compile_service: Arc::clone(&service),
        shutdown: Arc::new(ShutdownSignal::default()),
    };
    let now = SystemTime::now();
    let status = run_once(&context, MaintenanceKind::Full, now).await;
    drop(context);
    if status.successful_at_ms.is_some() {
        record_last_full(&paths, now).map_err(|error| error.to_string())?;
    }
    let status = persist_status(&paths, status).map_err(|error| error.to_string())?;
    if let Ok(service) = Arc::try_unwrap(service) {
        service
            .shutdown(zccache::embedded::ShutdownMode::Graceful)
            .await
            .map_err(|error| error.to_string())?;
    }
    Ok(status)
}

/// Run the embedded store's disk pass and record its report or error.
async fn measure_store(
    context: &MaintenanceContext,
    full: bool,
    now: SystemTime,
    status: &mut MaintenanceStatus,
) {
    match context.compile_service.maintain_disk(full).await {
        Ok(report) => {
            status.zccache = Some(report);
            status.zccache_measured_at_ms = Some(unix_millis(now));
        }
        Err(error) => status.zccache_error = Some(error.to_string()),
    }
}

/// Whether a pass deferred by an active build should still run the store pass
/// (soldr#3251).
///
/// Throttled to [`STORE_PASS_INTERVAL`] from the last measurement. The
/// exception is a store the last measurement put at or over its budget: that
/// runs every tick, because it is exactly the store a busy host lets grow.
fn store_pass_due(previous: Option<&MaintenanceStatus>, now: SystemTime) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    let (Some(report), Some(measured_at)) =
        (previous.zccache.as_ref(), previous.zccache_measured_at_ms)
    else {
        return true;
    };
    if report.usage_after_bytes >= report.budget_bytes {
        return true;
    }
    unix_millis(now).saturating_sub(measured_at) >= STORE_PASS_INTERVAL.as_millis() as i64
}

/// The part of a pass that is safe while builds hold the root lease
/// (soldr#3251).
///
/// The embedded store serializes its own eviction against publication and cache
/// hits (zccache#1148), which is why zccache runs it during compiles whenever it
/// owns scheduling. Retired version stores are never the current version and
/// are refused while their writer lock is held. Neither needs the exclusive root
/// lease that soldr's own destructive collectors take.
async fn run_store_pass(
    context: &MaintenanceContext,
    kind: MaintenanceKind,
    now: SystemTime,
    status: &mut MaintenanceStatus,
) {
    // A pressure pass, never a full one: the full pass and its marker stay with
    // the lease-held pass, so a deferred full pass remains due.
    measure_store(context, false, now, status).await;
    let paths = context.paths.clone();
    let legacy = tokio::task::spawn_blocking(move || {
        let config = paths.load_config().unwrap_or_default();
        legacy_zccache_outcome(&paths, kind, now, &daemon_policy_actions(config, kind))
    })
    .await;
    match legacy {
        Ok(Some(outcome)) => status.legacy_zccache = outcome,
        Ok(None) => {}
        Err(error) => {
            status.legacy_zccache.error = Some(format!("legacy_sweep_worker_failed: {error}"));
        }
    }
}

/// The daemon's reclamation plan for one tick kind.
fn daemon_policy_actions(
    config: crate::core::SoldrConfig,
    kind: MaintenanceKind,
) -> Vec<crate::cache_lib::gc_policy::EvictionAction> {
    let policy_context = crate::cache_lib::gc_policy::GcContext {
        driver: crate::cache_lib::gc_policy::Driver::Daemon,
        tick: if kind == MaintenanceKind::Full {
            crate::cache_lib::gc_policy::TickKind::Full
        } else {
            crate::cache_lib::gc_policy::TickKind::Pressure
        },
        free_by_volume: Vec::new(),
        config,
        daemon_events_available: true,
        daemon_live: true,
    };
    crate::cache_lib::gc_policy::plan(&crate::cache_lib::gc_policy::registry(), &policy_context)
}

/// Reclaim retired embedded stores and legacy identity roots, when the plan
/// calls for it.
fn legacy_zccache_outcome(
    paths: &SoldrPaths,
    kind: MaintenanceKind,
    now: SystemTime,
    policy_actions: &[crate::cache_lib::gc_policy::EvictionAction],
) -> Option<ComponentOutcome> {
    let action = policy_actions
        .iter()
        .find(|action| action.category_id == "legacy_zccache")?;
    let default_age = if kind == MaintenanceKind::Full {
        FULL_STALE_AGE
    } else {
        PRESSURE_STALE_AGE
    };
    let legacy = crate::zccache_embedded::sweep_legacy_cache_roots(
        paths,
        now,
        action.older_than.unwrap_or(default_age),
    );
    Some(ComponentOutcome {
        items_removed: legacy.removed as u64,
        bytes_reclaimed: legacy.bytes_reclaimed,
        error: (legacy.failed > 0).then(|| format!("{} legacy roots retained", legacy.failed)),
    })
}

#[derive(Default)]
struct LocalOutcomes {
    cook: ComponentOutcome,
    history: ComponentOutcome,
    pep517_targets: ComponentOutcome,
    pep517_wheels: ComponentOutcome,
    trash: ComponentOutcome,
    workspace_targets: ComponentOutcome,
    daemon_events: ComponentOutcome,
    legacy_zccache: ComponentOutcome,
}

fn run_local_components(
    paths: &SoldrPaths,
    db_path: &Path,
    kind: MaintenanceKind,
    now: SystemTime,
    _pressure: bool,
) -> LocalOutcomes {
    let mut out = LocalOutcomes::default();
    let config = paths.load_config();
    let policy_actions = daemon_policy_actions(config.as_ref().cloned().unwrap_or_default(), kind);
    let has_action = |id: &str| policy_actions.iter().any(|action| action.category_id == id);
    // soldr#3329: retired stores have zero hit value, so they are reclaimed
    // before any collector that evicts live cache content.
    if let Some(legacy) = legacy_zccache_outcome(paths, kind, now, &policy_actions) {
        out.legacy_zccache = legacy;
    }
    match &config {
        Ok(config) if has_action("cook") => {
            let cook = crate::cache_lib::cook_gc::cook_evict_pass_with_absolute_age(
                paths,
                &config.cook,
                policy_actions
                    .iter()
                    .find(|action| action.category_id == "cook")
                    .and_then(|action| action.older_than),
            );
            out.cook = ComponentOutcome {
                items_removed: (cook.time_evicted + cook.size_evicted + cook.quarantine_evicted)
                    as u64,
                bytes_reclaimed: cook.bytes_freed,
                error: (cook.errors > 0).then(|| format!("{} cook eviction errors", cook.errors)),
            };
        }
        Ok(_) => {}
        Err(error) => {
            let message = Some(format!("invalid_config: {error}"));
            out.cook.error = message.clone();
            out.workspace_targets.error = message;
        }
    }

    let history = history_gc::sweep(
        paths,
        db_path,
        &history_gc::HistoryGcOptions {
            now,
            max_age: history_gc::DEFAULT_MAX_AGE,
            max_bytes: history_gc::DEFAULT_MAX_BYTES,
            migrate_pre_redaction: true,
        },
    );
    out.history = ComponentOutcome {
        items_removed: (history.age_removed
            + history.size_removed
            + history.migration_removed
            + history.legacy_files_removed) as u64,
        bytes_reclaimed: history.bytes_reclaimed,
        error: (history.failed > 0).then(|| format!("{} history deletion errors", history.failed)),
    };

    if has_action("pep517_targets") {
        let max_age = policy_actions
            .iter()
            .find(|action| action.category_id == "pep517_targets")
            .and_then(|action| action.older_than)
            .unwrap_or(crate::cache_lib::pep517_gc::PRESSURE_MAX_AGE);
        let pep = crate::cache_lib::pep517_gc::sweep(paths, now, max_age);
        out.pep517_targets = ComponentOutcome {
            items_removed: pep.removed as u64,
            bytes_reclaimed: pep.bytes_reclaimed,
            error: (pep.failed > 0).then(|| format!("{} PEP517 target errors", pep.failed)),
        };
        let wheels = crate::cache_lib::pep517_gc::sweep_wheels(paths, now, max_age);
        out.pep517_wheels = ComponentOutcome {
            items_removed: wheels.removed as u64,
            bytes_reclaimed: wheels.bytes_reclaimed,
            error: (wheels.failed > 0).then(|| format!("{} PEP517 wheel errors", wheels.failed)),
        };
    }

    if has_action("trash") {
        match crate::cache_lib::trash_gc::sweep_trash(paths) {
            Ok(trash) => {
                out.trash.items_removed = trash.removed;
                out.trash.error = (trash.retained > 0)
                    .then(|| format!("{} trash entries retained", trash.retained));
            }
            Err(error) => out.trash.error = Some(error.to_string()),
        }
    }

    if let Ok(config) = &config {
        if has_action("workspace_targets") {
            out.workspace_targets = sweep_workspace_targets(paths, config, kind);
        }
    }
    if has_action("daemon_events") {
        let cutoff = unix_millis(now).saturating_sub(EVENT_RETENTION.as_millis() as i64);
        match db::prune_events_older_than(db_path, cutoff) {
            Ok(removed) => out.daemon_events.items_removed = removed,
            Err(error) => out.daemon_events.error = Some(error.to_string()),
        }
    }
    out
}

/// The daemon's periodic `target/` eviction pass.
///
/// Snapshot-and-release, scan/delete, then bounded bookkeeping (#2224).
/// SQLite connections no longer own redb's exclusive whole-file lock.
/// Keeping the filesystem phase outside database work still prevents an
/// unbounded scan or deletion from extending a write transaction; an open
/// connection by itself does not exclude other readers or writers.
pub(crate) fn sweep_workspace_targets(
    paths: &SoldrPaths,
    config: &crate::core::SoldrConfig,
    kind: MaintenanceKind,
) -> ComponentOutcome {
    let db_path = crate::cache_lib::data_db_path(paths);
    let options = GcOptions {
        older_than_seconds: if kind == MaintenanceKind::Full {
            FULL_STALE_AGE.as_secs()
        } else {
            PRESSURE_STALE_AGE.as_secs()
        }
        .max(config.auto_gc.min_age_secs),
        larger_than_bytes: 0,
        dev_roots: configured_gc_roots(config),
        dry_run: false,
    };
    // Phase 1 — open, snapshot the rows, drop the handle before returning.
    let report = match gc::scan_released(&db_path, &options) {
        Ok(report) => report,
        Err(error) => {
            return ComponentOutcome {
                error: Some(error.to_string()),
                ..ComponentOutcome::default()
            };
        }
    };
    // Phase 2 — sizing and recursive deletion, with no database handle
    // held. The long, unbounded part of the sweep lives entirely here.
    fs_phase_barrier();
    let outcomes: Vec<_> = report
        .candidates
        .into_iter()
        .map(gc::delete_candidate_dir)
        .collect();
    // Phase 3 — bounded reopen: every directory is already gone, so this
    // is one write txn per deleted row and nothing else.
    let registry = match TargetRegistry::open(&db_path) {
        Ok(registry) => registry,
        Err(error) => {
            return ComponentOutcome {
                error: Some(error.to_string()),
                ..ComponentOutcome::default()
            };
        }
    };
    let applied = gc::apply_purge_outcomes(&registry, outcomes);
    drop(registry);
    match applied {
        Ok(report) => ComponentOutcome {
            items_removed: report.succeeded_count as u64,
            bytes_reclaimed: report.reclaimed_bytes,
            error: (report.failed_count > 0)
                .then(|| format!("{} workspace target deletion errors", report.failed_count)),
        },
        Err(error) => ComponentOutcome {
            error: Some(error.to_string()),
            ..ComponentOutcome::default()
        },
    }
}

/// Test seam marking the start of the handle-free filesystem phase.
///
/// A real sweep's phase 2 is slow because it is deleting gigabytes; a test
/// cannot wait for that and cannot make it deterministic. Instead the test
/// binary parks here until the sibling process has finished proving the
/// state DB is reachable, which asserts the property directly ("no handle
/// is held during phase 2") instead of racing it.
///
/// Compiled only into the test binary — the shipping daemon calls the
/// no-op below.
#[cfg(test)]
fn fs_phase_barrier() {
    let Some(dir) = std::env::var_os(tests::FS_BARRIER_DIR_ENV).map(PathBuf::from) else {
        return;
    };
    let _ = std::fs::write(dir.join("sweeping"), b"");
    let release = dir.join("release");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !release.exists() {
        if std::time::Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

#[cfg(not(test))]
fn fs_phase_barrier() {}

fn configured_gc_roots(config: &crate::core::SoldrConfig) -> Vec<PathBuf> {
    let configured = config
        .gc
        .allowlist_roots
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter(|root| !root.trim().is_empty())
        .map(|root| crate::core::expand_user_home(&root))
        .collect::<Vec<_>>();
    if !configured.is_empty() {
        return configured;
    }
    crate::core::user_home_dir()
        .map(|home| vec![home.join("dev")])
        .unwrap_or_default()
}

fn root_below_pressure_threshold(
    paths: &SoldrPaths,
    free_bytes: Option<u64>,
    capacity_bytes: Option<u64>,
) -> bool {
    let Ok(config) = paths.load_config() else {
        return false;
    };
    config.auto_gc.enabled
        && free_bytes.is_some_and(|free| {
            free < crate::cache_lib::auto_gc::free_space_thresholds(&config.auto_gc, capacity_bytes)
                .0
        })
}

pub fn due_kind(
    last_full: Option<SystemTime>,
    last_pressure: Option<SystemTime>,
    now: SystemTime,
) -> Option<MaintenanceKind> {
    let full_due = last_full
        .is_none_or(|last| now.duration_since(last).unwrap_or(FULL_INTERVAL) >= FULL_INTERVAL);
    if full_due {
        return Some(MaintenanceKind::Full);
    }
    let pressure_due = last_pressure.is_none_or(|last| {
        now.duration_since(last).unwrap_or(PRESSURE_INTERVAL) >= PRESSURE_INTERVAL
    });
    pressure_due.then_some(MaintenanceKind::Pressure)
}

fn maintenance_dir(paths: &SoldrPaths) -> PathBuf {
    paths.cache.join("soldr-daemon").join("maintenance")
}

/// Full-pass schedule directory for the store this daemon maintains.
///
/// soldr#3251: each daemon maintains only its own zccache version's store, so
/// the schedule is keyed by that version. A single root-wide marker let a daemon
/// of an old version, maintaining a 1 GiB store, reset the 24-hour clock for a
/// newer version's 216 GiB store that no full pass had touched in days.
fn store_marker_dir(paths: &SoldrPaths) -> PathBuf {
    maintenance_dir(paths).join(zccache::core::config::versioned_subdir())
}

fn full_marker_path(paths: &SoldrPaths) -> PathBuf {
    store_marker_dir(paths).join(FULL_MARKER)
}

fn full_attempt_marker_path(paths: &SoldrPaths) -> PathBuf {
    store_marker_dir(paths).join(FULL_ATTEMPT_MARKER)
}

fn read_last_full(paths: &SoldrPaths) -> Option<SystemTime> {
    let value = std::fs::read_to_string(full_marker_path(paths)).ok()?;
    let millis = value.trim().parse::<u64>().ok()?;
    Some(UNIX_EPOCH + Duration::from_millis(millis))
}

fn read_last_full_attempt(paths: &SoldrPaths) -> Option<SystemTime> {
    let value = std::fs::read_to_string(full_attempt_marker_path(paths)).ok()?;
    let millis = value.trim().parse::<u64>().ok()?;
    Some(UNIX_EPOCH + Duration::from_millis(millis))
}

fn record_last_full(paths: &SoldrPaths, now: SystemTime) -> std::io::Result<()> {
    let dir = store_marker_dir(paths);
    std::fs::create_dir_all(&dir)?;
    atomic_write(
        &full_marker_path(paths),
        format!("{}\n", unix_millis(now)).as_bytes(),
    )
}

fn record_last_full_attempt(paths: &SoldrPaths, now: SystemTime) -> std::io::Result<()> {
    let dir = store_marker_dir(paths);
    std::fs::create_dir_all(&dir)?;
    atomic_write(
        &full_attempt_marker_path(paths),
        format!("{}\n", unix_millis(now)).as_bytes(),
    )
}

/// Write `status`, first carrying the last measured zccache usage forward
/// when this pass did not reach the store (soldr#3077).
///
/// A pass deferred by an active build writes a status with no zccache report.
/// Replacing the file with that erased the only record of how large the
/// artifact store is, so on a host that is usually building `soldr status`
/// showed no store size at all while `.staged-v2` held tens of GiB.
fn persist_status(
    paths: &SoldrPaths,
    mut status: MaintenanceStatus,
) -> std::io::Result<MaintenanceStatus> {
    carry_forward_zccache_measurement(read_status(paths).as_ref(), &mut status);
    write_status(paths, &status)?;
    Ok(status)
}

/// Keep the previous pass's zccache report when this one has neither a fresh
/// report nor an error. A fresh report always wins, and a failed measurement
/// is reported as failed rather than hidden behind a stale number.
fn carry_forward_zccache_measurement(
    previous: Option<&MaintenanceStatus>,
    status: &mut MaintenanceStatus,
) {
    if status.zccache.is_some() || status.zccache_error.is_some() {
        return;
    }
    let Some(previous) = previous else {
        return;
    };
    let Some(report) = previous.zccache.clone() else {
        return;
    };
    status.zccache = Some(report);
    // A status file written before the measured-at field existed still names
    // the pass that produced its report.
    status.zccache_measured_at_ms = previous
        .zccache_measured_at_ms
        .or(previous.successful_at_ms)
        .or(Some(previous.attempted_at_ms));
}

fn write_status(paths: &SoldrPaths, status: &MaintenanceStatus) -> std::io::Result<()> {
    let body = serde_json::to_vec_pretty(status).map_err(std::io::Error::other)?;
    let path = status_path(paths);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write(&path, &body)
}

fn atomic_write(path: &Path, body: &[u8]) -> std::io::Result<()> {
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&temp, body)?;
    if let Err(error) = std::fs::rename(&temp, path) {
        if path.exists() {
            std::fs::remove_file(path)?;
            std::fs::rename(&temp, path)?;
        } else {
            return Err(error);
        }
    }
    Ok(())
}

fn filesystem_space(path: &Path) -> (Option<u64>, Option<u64>) {
    let probe = path
        .ancestors()
        .find(|candidate| candidate.exists())
        .unwrap_or(path);
    (
        fs2::total_space(probe).ok(),
        fs2::available_space(probe).ok(),
    )
}

fn unix_millis(now: SystemTime) -> i64 {
    now.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
#[path = "maintenance_tests.rs"]
mod tests;
