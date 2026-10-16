//! Embedded zccache service wrapper (issue #977 / #980 L1).
//!
//! This module is the **only** soldr-side import site for the
//! `zccache::embedded::*` API. Everything else in soldr talks to
//! [`SoldrZccacheService`] so the upstream-API blast radius is bounded
//! to one file. As of the #980 L1 "no backwards compatibility" pass
//! the embedded service is mandatory — the daemon always instantiates
//! it at boot and the wrapper always dispatches through the
//! broker SESSION route. The legacy fork-zccache.exe path is
//! gone.
//!
//! ## Design constraint: daemon-only
//!
//! Per issue #977 (and the user's explicit instruction in the goal
//! prompt), the embedded `ZccacheService` lives **inside the long-lived
//! soldr-daemon process and nowhere else**. Transient rustc-wrapper
//! invocations (which live for the lifetime of a single rustc command)
//! must not pay `ZccacheService::start` cost — that would defeat the
//! whole point of "one process, one tokio runtime, one console-subscriber
//! pane". Wrappers always talk to the daemon over the
//! broker SESSION route, which ferries the compile to the
//! embedded backend without a `Command::new("zccache")` fork.
//!
//! ## Tokio runtime sharing (the tokio-console story)
//!
//! `RuntimeHooks` accepts an explicit Tokio handle. Soldr starts the service
//! from inside its daemon runtime, so the ambient handle owns zccache's index
//! writer. Soldr explicitly owns the five-minute/24-hour maintenance schedule
//! so only one build-aware scanner runs. `console-subscriber` sees the union
//! of soldr and zccache tasks.
//!
//! Identity: product `soldr`, with the broker route in `instance_id` so
//! daemon images sharing a root have distinct zccache writer locks.
//! `workspace_id` defaults to the same route identity; compile calls set it
//! through `AuditContext`.
//!
//! ## Cache root
//!
//! `<soldr_cache_root>/zccache/` per the issue text. Created at start.
//!
//! ## Audit context
//!
//! Filled per compile by [`SoldrZccacheService::compile`]. Each call
//! gets a synthetic per-invocation `AuditId`; durable correlation with
//! the soldr `BuildSession{Start,End}` events is a follow-up tracked
//! in #977 open questions.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use running_process::broker::backend_handle::DaemonProcess;
use zccache::audit::AuditMode;
use zccache::core::NormalizedPath;
use zccache::embedded::{
    AuditConfig, AuditContext, CacheOutcome, CompileOptions,
    CompileRequest as ZccacheCompileRequest, DiskCacheLimits, DiskMaintenanceKind,
    DiskMaintenancePressure, FlushStepOutcome, HostIdentity, RuntimeHooks, ServiceLimits,
    ShutdownMode, TestHarnessAdmission, ZccacheConfig, ZccacheService,
};
use zccache::hash::StreamHasher;

use crate::core::SoldrPaths;
use crate::daemon::protocol::{
    CacheFlushInfo, CacheFlushStepInfo, CompileRequest, CompileResponseBody, CompileStatsInfo,
    StagedProfileInfo,
};

#[path = "zccache_shutdown_gate.rs"]
mod shutdown_gate;
use shutdown_gate::CompileShutdownGate;

/// Soldr-side handle around a started [`ZccacheService`]. Cheap to
/// clone (the inner handle is `Arc`-shared).
#[derive(Clone)]
pub struct SoldrZccacheService {
    inner: Arc<ZccacheService>,
    shutdown_gate: CompileShutdownGate,
    compile_admission: Arc<crate::resident_compile_admission::ResidentCompileAdmission>,
    identity: HostIdentity,
    cache_root: PathBuf,
    disk_policy: EmbeddedDiskPolicy,
    applied_jobs: crate::core::jobs::ResolvedJobs,
    start_timings: ServiceStartTimings,
    /// soldr#3152 step 4: each unit's last measured peak memory. Admission
    /// holds a clone for its shadow lookup; `compile` records into this one.
    unit_history: crate::daemon::unit_memory_history::UnitMemoryHistory,
}

/// Where [`SoldrZccacheService::start`] spent its time, in milliseconds.
///
/// soldr#3174: the daemon bringup breadcrumbs added in soldr#3163 attributed
/// **97.8 s of a 97.4 s cold start** to the single `compile_service` phase on
/// the Linux gate -- reproducibly, and squarely on the critical path (the
/// "Hand off bootstrap broker to source revision" step ends in the same second
/// the daemon reports ready). That phase is one opaque `await`, which is
/// exactly the state daemon bringup as a whole was in before soldr#3163.
///
/// This splits it along the one boundary that decides where a fix belongs:
/// soldr's own cache-root preparation versus zccache's service start. Carried
/// on the service rather than returned from `start`, so the eight call sites
/// -- six of them tests -- are untouched.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ServiceStartTimings {
    /// `prepare_embedded_cache_root`: legacy migration, directory creation,
    /// owned-path validation. Soldr's code.
    pub prepare_root_ms: u64,
    /// `scrub_existing_compile_journals`: one `read_dir` over the logs
    /// directory. Soldr's code.
    pub scrub_journals_ms: u64,
    /// `ZccacheService::start_with_options_and_host_admission_classifier`:
    /// the embedded cache's own bring-up. Upstream code -- if the time is
    /// here, the fix is a zccache change plus a pin bump, not a local one.
    pub zccache_start_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EmbeddedDiskPolicy {
    pub source: String,
    pub max_cache_bytes: Option<u64>,
    pub max_cache_percent: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EmbeddedDiskMaintenanceReport {
    pub kind: String,
    pub pressure: String,
    pub budget_bytes: u64,
    pub usage_before_bytes: u64,
    pub usage_after_bytes: u64,
    pub bytes_reclaimed: u64,
    pub artifacts_removed: usize,
    pub expired_artifacts_removed: usize,
    pub pending_write_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LegacyCacheSweepReport {
    pub removed: usize,
    pub failed: usize,
    pub bytes_reclaimed: u64,
    /// Retired stores left in place because a running service still holds
    /// their writer lock (soldr#3251). Not a failure: they are reclaimed once
    /// that service exits.
    pub live_retained: usize,
}

/// zccache's cache-root writer lock, `CACHE_ROOT_WRITER_LOCK_FILE` in zccache's
/// `daemon_core::daemon::server::state`, which zccache does not export.
///
/// A running embedded service holds it exclusively on its version root for its
/// whole life, so a held lock proves a store is live (soldr#3251). The
/// maintenance tests check this name against a real running service, so a
/// rename upstream fails loudly instead of silently disarming the check.
pub const ZCCACHE_WRITER_LOCK_FILE: &str = ".writer.lock";

/// Errors raised while starting or stopping the embedded service. Wrap
/// the upstream `EmbeddedError` into a plain string so soldr crates
/// further up the call stack do not have to know the zccache types.
#[derive(Debug, thiserror::Error)]
pub enum EmbeddedServiceError {
    #[error("zccache embedded start failed: {0}")]
    Start(String),
    #[error("zccache embedded shutdown failed: {0}")]
    Shutdown(String),
    #[error("zccache embedded flush failed: {0}")]
    Flush(String),
    #[error("zccache embedded stats failed: {0}")]
    Stats(String),
    #[error("zccache embedded maintenance failed: {0}")]
    Maintenance(String),
    #[error("resident compile capacity failed: {0}")]
    ResidentCapacity(String),
    /// Issue #977 Phase 5 / #980 L1 — surfaced by [`SoldrZccacheService::compile`].
    /// Maps to a soldr-side `Response::Error`; the mandatory broker route
    /// propagates the compile-service failure without changing execution mode.
    #[error("zccache embedded compile failed: {0}")]
    Compile(String),
    /// Issue #977 Phase 5 — the wrapper sent a `CompileRequest` with an
    /// empty args list; we have no rustc path to invoke.
    #[error("Compile request missing compiler path (args[0])")]
    EmptyArgs,
    #[error("io while preparing zccache cache root: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "multiple legacy embedded zccache backends have the same newest mtime under {root}; refusing to choose silently: {candidates:?}"
    )]
    AmbiguousLegacyCache {
        root: PathBuf,
        candidates: Vec<PathBuf>,
    },
}

/// Per-request zccache options for every compile Soldr forwards.
///
/// `--test` harnesses are admitted (zccache#1550 `TestHarnessAdmission::All`)
/// rather than refused: Soldr caches every compile it can. Long-lived and
/// large end-state artifacts are partitioned by `soldr cook`, and the
/// consolidated test targets (soldr#2934) keep harness size and count small,
/// so the original "volatile and huge" reason for zccache#1525's refusal no
/// longer holds. The explicit option outranks `ZCCACHE_CACHE_TEST_BINS` in
/// either environment, so no caller can reintroduce the refusal by accident.
/// zccache still skips storing a single harness above
/// `MAX_ADMITTED_TEST_HARNESS_BYTES` (512 MiB).
fn soldr_compile_options() -> CompileOptions {
    CompileOptions::new().with_test_harness_admission(TestHarnessAdmission::All)
}

impl SoldrZccacheService {
    /// Start the embedded service. Must be called from inside the
    /// daemon's tokio runtime (the function is `async` for exactly
    /// that reason). Creates the cache root directory if missing.
    pub async fn start(
        paths: &SoldrPaths,
        daemon_identity: &DaemonProcess,
    ) -> Result<Self, EmbeddedServiceError> {
        Self::start_with_disk_limits(paths, daemon_identity, disk_cache_limits_from_env()?).await
    }

    /// Test seam (soldr#3669): start with an explicit artifact budget instead
    /// of the process env, so a test decides the store's disk pressure.
    /// zccache measures the host filesystem for its low-free-space check, so
    /// a budget below the test's own usage is the one way to make the
    /// pressure classification independent of how full the host disk is.
    #[cfg(test)]
    pub(crate) async fn start_with_max_cache_bytes(
        paths: &SoldrPaths,
        daemon_identity: &DaemonProcess,
        max_cache_bytes: u64,
    ) -> Result<Self, EmbeddedServiceError> {
        let limits =
            disk_limits::disk_cache_limits_from_values(Some(&max_cache_bytes.to_string()), None)?;
        Self::start_with_disk_limits(paths, daemon_identity, limits).await
    }

    async fn start_with_disk_limits(
        paths: &SoldrPaths,
        daemon_identity: &DaemonProcess,
        (disk_limits, disk_policy): (DiskCacheLimits, EmbeddedDiskPolicy),
    ) -> Result<Self, EmbeddedServiceError> {
        let identity = derive_identity();
        // Broker generations may share one Soldr root. The route-scoped host
        // identity isolates their mutable zccache snapshots and writer locks.
        let cache_root = private_zccache_cache_root(paths, &identity);
        let prepare_started = std::time::Instant::now();
        prepare_embedded_cache_root(paths, daemon_identity, &cache_root)?;
        let prepare_root_ms = prepare_started.elapsed().as_millis() as u64;
        let scrub_started = std::time::Instant::now();
        scrub_existing_compile_journals(paths)?;
        let scrub_journals_ms = scrub_started.elapsed().as_millis() as u64;
        // zccache#926 strict-validation: `AuditConfig::default()` ships
        // `mode = AuditMode::Normal` + `output_root = None`, which the
        // new audit-sink validation rejects ("audit sink requires
        // output_root when mode > Off"). soldr does not consume zccache
        // audit events today — the per-compile trace site (soldr#985 /
        // zccache#940) lives outside the AuditSink. Set mode = Off
        // explicitly so the embedded service starts cleanly.
        let audit = AuditConfig {
            mode: AuditMode::Off,
            ..AuditConfig::default()
        };
        let resolved_jobs = crate::compile_limit::resolve_and_announce();
        let cfg = ZccacheConfig {
            host: identity.clone(),
            cache_root: cache_root.clone().into(),
            audit,
            // soldr#1761: soldr owns the compile-concurrency limit now.
            // This used to be `ServiceLimits::default()`, i.e.
            // `max_parallel_compiles: None`, so zccache
            // default always governed and the only way to influence it
            // was to get `ZCCACHE_MAX_PARALLEL_COMPILES` into the
            // long-lived daemon's inherited environment. Resolving here
            // means `SOLDR_JOBS` and `config.toml` reach the semaphore
            // through `ServiceLimits`, not through env propagation, and
            // the outer admission queue sizes itself from the same call.
            limits: ServiceLimits {
                max_parallel_compiles: Some(resolved_jobs.jobs),
                ..ServiceLimits::default()
            },
            runtime: RuntimeHooks {
                service_name: Some("soldr-daemon".into()),
                // zccache#922: explicitly keep background maintenance and
                // index tasks on the soldr-daemon runtime.
                handle: Some(tokio::runtime::Handle::current()),
            },
            // zccache#923 — None preserves the prior behavior where
            // only `shutdown(ShutdownMode::Force)` aborts in-flight
            // work. soldr's daemon already has a Notify-based
            // shutdown path that race-completes the same scenarios.
            cancellation: None,
        };

        // soldr#2932 / zccache#1539: zccache owns the one canonical compiler
        // capacity semaphore and fair shared/exclusive gate. It invokes this
        // Soldr-specific classifier only after cache-hit classification, then
        // acquires capacity -> resource admission immediately before spawning
        // a real compiler child. Keeping no Soldr-side gate here ensures an
        // eligible cache hit never drains ordinary compiler work.
        let unit_history = crate::daemon::unit_memory_history::UnitMemoryHistory::start(
            crate::cache_lib::state_db_path(paths),
        );
        let compile_admission = Arc::new(
            crate::resident_compile_admission::ResidentCompileAdmission::new(resolved_jobs.jobs)
                .with_estimate_log(crate::memory_estimate::estimate_log_path(paths))
                .with_unit_history(unit_history.clone()),
        );
        let host_admission: Arc<dyn zccache::embedded::HostAdmissionClassifier> =
            compile_admission.clone();
        let zccache_started = std::time::Instant::now();
        let svc = ZccacheService::start_with_options_and_host_admission_classifier(
            cfg,
            crate::zccache_staging::options(&cache_root, disk_limits),
            host_admission,
        )
        .await
        .map_err(|e| EmbeddedServiceError::Start(e.to_string()))?;
        let zccache_start_ms = zccache_started.elapsed().as_millis() as u64;
        Ok(Self {
            inner: Arc::new(svc),
            shutdown_gate: CompileShutdownGate::default(),
            compile_admission,
            identity,
            cache_root,
            disk_policy,
            applied_jobs: resolved_jobs,
            start_timings: ServiceStartTimings {
                prepare_root_ms,
                scrub_journals_ms,
                zccache_start_ms,
            },
            unit_history,
        })
    }

    /// Where [`SoldrZccacheService::start`] spent its time (soldr#3174).
    pub fn start_timings(&self) -> ServiceStartTimings {
        self.start_timings
    }

    /// The compile limit this service started with — the number now baked
    /// into the semaphore, not a fresh resolution.
    ///
    /// soldr#2023: the daemon outlives the environment that spawned it, so
    /// this is the only value that answers "what is the running daemon
    /// actually doing?" Re-resolving would answer "what would a new daemon
    /// do?", which is the other half of the comparison a client makes.
    pub fn applied_jobs(&self) -> crate::core::jobs::ResolvedJobs {
        self.applied_jobs
    }

    /// Reserve compile capacity for a resident compiler process.
    ///
    /// The returned guard releases the reservation when dropped. A valid
    /// reservation always leaves at least one slot for ordinary cache-miss
    /// compiler work.
    pub async fn acquire_resident_capacity(
        &self,
        permits: u32,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, EmbeddedServiceError> {
        self.compile_admission
            .acquire_resident(permits)
            .await
            .map_err(|error| EmbeddedServiceError::ResidentCapacity(error.to_string()))
    }

    /// Resolved on-disk cache root. Useful for diagnostics surfaces.
    pub fn cache_root(&self) -> &std::path::Path {
        &self.cache_root
    }

    /// Stable host identity used to derive the embedded endpoint and
    /// namespace cache entries.
    pub fn identity(&self) -> &HostIdentity {
        &self.identity
    }

    pub fn disk_policy(&self) -> &EmbeddedDiskPolicy {
        &self.disk_policy
    }

    /// Dispatch a soldr [`CompileRequest`] through the embedded zccache
    /// service (issue #977 Phase 5 / #980 L1).
    ///
    /// The conversion lives here so `daemon::server::handle_connection`
    /// stays oblivious to `zccache::embedded::*` types. The default
    /// [`AuditContext`] uses synthetic per-call ids — durable audit
    /// correlation between soldr's build session and zccache compiles is
    /// a Phase 5+ follow-up tracked in #977 open questions.
    ///
    /// `args[0]` from the soldr request is the rustc binary path; we
    /// pass it as `CompileRequest::compiler` and forward `args[1..]` as
    /// the actual rustc argument list, matching the wrapper's existing
    /// `args[2..]` convention (where soldr's argv[0] is the soldr
    /// binary and argv[1] is the tool path).
    pub async fn compile(
        &self,
        req: CompileRequest,
    ) -> Result<CompileResponseBody, EmbeddedServiceError> {
        // soldr#2706: keep this lease through zccache's synchronous staged
        // publication. Shutdown takes the fair write side before it closes
        // zccache to publishers, so a successful cold miss cannot be served
        // during shutdown and then silently discarded.
        let _shutdown_lease =
            self.shutdown_gate.enter().await.ok_or_else(|| {
                EmbeddedServiceError::Compile("service is shutting down".to_string())
            })?;
        let ci_test_report = crate::ci_test_report::prepare(&req);
        let (compiler, rustc_args) = split_compiler_and_args(&req.args)?;
        let cwd: NormalizedPath = std::path::PathBuf::from(req.cwd).into();
        // Kept for the failure path: `cwd` is moved into the request below,
        // and soldr#2781's detector resolves relative source paths against it.
        let compile_cwd = cwd.as_path().to_path_buf();
        // soldr#2781: say so on the way IN, not only in the post-mortem. If
        // this process is killed for memory, the user needs to know which
        // file the compiler was holding -- and by then the compile is gone.
        if let Some(notice) = crate::amalgamation::compile_notice(&req.args, &compile_cwd) {
            eprintln!("{notice}");
        }
        let audit = default_audit_context();
        let zreq = ZccacheCompileRequest {
            audit,
            compiler,
            args: rustc_args,
            cwd,
            env: req.env,
            stdin: req.stdin,
        };
        // soldr#3053: name what was in flight when the RSS-ceiling watchdog
        // dumps. CLAUDE.md's "Diagnosing before capping" is explicit that
        // cgroup OOM counters alone are not sufficient evidence, so the dump
        // needs the per-process picture beside them -- and the only place
        // that knows a compile is running is the scope that awaits it. The
        // guard deregisters on drop, including on the `?` error path below.
        let _inflight = crate::daemon::inflight_compiles::register(
            crate::memory_estimate::unit_key(&req.args),
            crate::amalgamation::rust_crate_name(&req.args).map(str::to_string),
        );
        // Keep zccache's compile state behind one heap indirection. Its
        // streaming implementation nests a large compile pipeline future;
        // carrying that state inline makes this adapter's callers inherit the
        // full stack footprint even when they only use the buffered API.
        crate::daemon::compile_profile::mark_compile_start();
        let zresult = Box::pin(
            self.inner
                .compile_with_options(zreq, soldr_compile_options()),
        )
        .await;
        crate::daemon::compile_profile::mark_compile_end();
        let zresp = zresult
            .map_err(|e| EmbeddedServiceError::Compile(e.to_string()))?
            .response;
        // soldr#3152 step 4: remember this unit's measured memory (zccache
        // 1.13.24 reports it per compile). A cache hit spawned no compiler and
        // reports nothing, which the history ignores.
        if let Some(unit) = crate::memory_estimate::unit_key(&req.args) {
            self.unit_history.record(
                &unit,
                zresp.child_memory.peak_rss_bytes.unwrap_or(0),
                zresp.child_memory.tree_peak_rss_bytes.unwrap_or(0),
            );
        }
        let stderr = if zresp.cached {
            strip_internal_soldr_fallback_notices(zresp.stderr)
        } else {
            zresp.stderr
        };
        let stderr = crate::compiler_exit::annotate_signal_termination(
            zresp.exit_code,
            stderr,
            &req.args,
            &compile_cwd,
        );
        if let Some(report) = ci_test_report {
            crate::ci_test_report::record(report, encode_cache_outcome(zresp.cache_outcome));
        }
        Ok(CompileResponseBody {
            exit_code: zresp.exit_code,
            stdout: zresp.stdout,
            stderr,
            cached: zresp.cached,
            cache_outcome: encode_cache_outcome(zresp.cache_outcome),
        })
    }

    /// Drain pending writes — called from the `Request::FlushCaches`
    /// arm (`soldr save` / `soldr cache flush`) so the on-disk cache
    /// tree is complete before archiving.
    pub async fn flush(&self) -> Result<CacheFlushInfo, EmbeddedServiceError> {
        let report = self
            .inner
            .flush_detailed()
            .await
            .map_err(|e| EmbeddedServiceError::Flush(e.to_string()))?;
        let complete = report.is_complete();
        Ok(CacheFlushInfo {
            complete,
            pending_writes_drained: report.pending_writes_drained,
            index_writer_drained: report.index_writer_drained,
            steps: report
                .steps
                .into_iter()
                .map(|step| {
                    let (status, error) = match step.outcome {
                        FlushStepOutcome::Completed => ("completed", None),
                        FlushStepOutcome::Failed(error) => ("failed", Some(error)),
                        FlushStepOutcome::TimedOut => ("timed_out", None),
                    };
                    CacheFlushStepInfo {
                        step: step.step,
                        status: status.to_owned(),
                        error,
                    }
                })
                .collect(),
            artifact_entries: report.artifact_entries,
            metadata_entries: report.metadata_entries,
        })
    }

    /// Return the embedded service's cumulative compile counters as the
    /// soldr-daemon protocol type (soldr#1368). Keeps `zccache::embedded::*`
    /// types out of `daemon::server` — the conversion lives here.
    pub async fn stats(&self) -> Result<CompileStatsInfo, EmbeddedServiceError> {
        let s = self
            .inner
            .stats()
            .await
            .map_err(|e| EmbeddedServiceError::Stats(e.to_string()))?;
        Ok(CompileStatsInfo {
            total_compilations: s.total_compilations,
            cache_hits: s.cache_hits,
            cache_misses: s.cache_misses,
            non_cacheable: s.non_cacheable,
            compile_errors: s.compile_errors,
            time_saved_ms: s.time_saved_ms,
            staged_profile: Some(StagedProfileInfo {
                counters: s.phase_profile.staged.counters,
                timings_ns: s.phase_profile.staged.timings_ns,
                bytes: s.phase_profile.staged.bytes,
                failures: s.phase_profile.staged.failures,
            }),
        })
    }

    /// Coordinate a host-requested pass with zccache's own startup/periodic
    /// worker.  Upstream serializes the pass against publication and confines
    /// it to this service's exact configured artifact root (#1148).
    pub async fn maintain_disk(
        &self,
        full: bool,
    ) -> Result<EmbeddedDiskMaintenanceReport, EmbeddedServiceError> {
        let report = self
            .inner
            .maintain_disk(if full {
                DiskMaintenanceKind::Full
            } else {
                DiskMaintenanceKind::Pressure
            })
            .await
            .map_err(|error| EmbeddedServiceError::Maintenance(error.to_string()))?;
        Ok(EmbeddedDiskMaintenanceReport {
            kind: match report.kind {
                DiskMaintenanceKind::Pressure => "pressure",
                DiskMaintenanceKind::Full => "full",
            }
            .to_string(),
            pressure: match report.pressure {
                DiskMaintenancePressure::None => "none",
                DiskMaintenancePressure::Soft => "soft",
                DiskMaintenancePressure::Hard => "hard",
            }
            .to_string(),
            budget_bytes: report.budget_bytes,
            usage_before_bytes: report.usage_before_bytes,
            usage_after_bytes: report.usage_after_bytes,
            bytes_reclaimed: report.bytes_reclaimed,
            artifacts_removed: report.artifacts_removed,
            expired_artifacts_removed: report.expired_artifacts_removed,
            pending_write_bytes: report.pending_write_bytes,
        })
    }

    /// Graceful shutdown — called from the daemon's normal exit path
    /// after the accept loop has been aborted.
    pub async fn shutdown(self, mode: ShutdownMode) -> Result<(), EmbeddedServiceError> {
        // Stop new compiles and wait for every already-admitted compile to
        // finish its synchronous publication before zccache observes
        // shutdown. Keep the writer for the whole shutdown so no late caller
        // can enter the closed service.
        let _shutdown_guard = match mode {
            ShutdownMode::Graceful => Some(self.shutdown_gate.close_and_drain().await),
            // Force shutdown deliberately delegates cancellation of in-flight
            // work to zccache. Waiting for read leases here would turn Force
            // into Graceful and violate the upstream mode contract.
            ShutdownMode::Force => None,
        };
        // `ZccacheService` is intentionally Clone: each clone shares the
        // shutdown flag and daemon state. Consume a clone of the service value
        // here instead of trying to unwrap soldr's Arc. Signal-handler and
        // connection tasks may still hold soldr State clones during teardown;
        // requiring Arc uniqueness therefore reduced "shutdown" to a flush and
        // left zccache's index writer alive until Tokio runtime drop.
        self.inner
            .as_ref()
            .clone()
            .shutdown_detailed(mode)
            .await
            .map_err(|e| EmbeddedServiceError::Shutdown(e.to_string()))
            .and_then(|report| ensure_complete_shutdown(&report))
    }
}

fn prepare_embedded_cache_root(
    paths: &SoldrPaths,
    daemon_identity: &DaemonProcess,
    cache_root: &std::path::Path,
) -> Result<(), EmbeddedServiceError> {
    migrate_legacy_cache_root(paths, daemon_identity, cache_root)?;
    std::fs::create_dir_all(cache_root)?;
    crate::cache_lib::path_safety::validate_owned_directory(&paths.root, cache_root)?;
    let version_root = cache_root.join(zccache::core::config::versioned_subdir());
    std::fs::create_dir_all(&version_root)?;
    crate::cache_lib::path_safety::validate_owned_directory(&paths.root, &version_root)?;
    Ok(())
}

#[path = "zccache_disk_limits.rs"]
mod disk_limits;
use disk_limits::disk_cache_limits_from_env;
pub(crate) use disk_limits::{CACHE_SIZE_BYTES_ENV, CACHE_SIZE_PERCENT_ENV};

#[cfg(test)]
#[path = "zccache_embedded_journal_tests.rs"]
mod journal_migration_tests;

fn ensure_complete_shutdown(
    report: &zccache::embedded::DetailedShutdownReport,
) -> Result<(), EmbeddedServiceError> {
    if report.flushed.is_complete() {
        return Ok(());
    }

    Err(EmbeddedServiceError::Shutdown(format!(
        "cache checkpoint incomplete: pending_writes_drained={}, index_writer_drained={}, steps={:?}",
        report.flushed.pending_writes_drained,
        report.flushed.index_writer_drained,
        report.flushed.steps
    )))
}

/// Build the default per-call `AuditContext` for soldr-issued compiles
/// (issue #977 Phase 5). Each call gets a fresh synthetic run_id +
/// trace_id; the daemon already carries build-session correlation via
/// `BuildSessionStart`/`End`, so threading it through here is a Phase
/// 5+ refinement.
fn default_audit_context() -> AuditContext {
    use zccache::audit::AuditId;
    let id = uuid_like_random_id();
    // `AuditId::new` only fails on the empty string, which uuid_like
    // cannot produce. unwrap is intentional and unreachable in practice.
    let run_id = AuditId::new(id.clone()).expect("non-empty audit id");
    let trace_id = AuditId::new(id).expect("non-empty audit id");
    AuditContext::new(run_id, trace_id)
}

/// Generate a 32-char hex id without pulling in `uuid` purely for this
/// module. blake3 of `(pid, current monotonic nanos)` is plenty random
/// for per-call audit ids and avoids a new dependency.
fn uuid_like_random_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut hasher = StreamHasher::new();
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(&nanos.to_le_bytes());
    hex::encode(&hasher.finalize().as_bytes()[..16])
}
/// Encode [`CacheOutcome`] as the integer protocol expects on the wire:
/// 1=Hit, 2=Miss, 3=Error. Kept inside the embedded module so the
/// soldr daemon layer never imports `CacheOutcome` directly.
fn encode_cache_outcome(outcome: CacheOutcome) -> i32 {
    match outcome {
        CacheOutcome::Hit => 1,
        CacheOutcome::Miss => 2,
        CacheOutcome::Error => 3,
    }
}

/// Soldr wrapper failures are operational diagnostics, not compiler output.
/// Older cache entries can contain the once-per-wrapper fallback notice and
/// Cargo persists any replayed stderr under `.fingerprint/output-*`, turning
/// one transient outage into permanent warm-build spam. Drop only that exact
/// internal line at the embedded-cache response boundary.
pub fn strip_internal_soldr_fallback_notices(stderr: Vec<u8>) -> Vec<u8> {
    const PREFIX: &[u8] = b"soldr: compile daemon unavailable after ";
    const MIDDLE: &[u8] =
        b"ms \xe2\x80\x94 falling back to direct uncached rustc (soldr#1657); reason=";
    fn is_internal_notice(line: &[u8]) -> bool {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let Some(rest) = line.strip_prefix(PREFIX) else {
            return false;
        };
        let Some(middle_at) = rest
            .windows(MIDDLE.len())
            .position(|window| window == MIDDLE)
        else {
            return false;
        };
        let (budget, suffix) = rest.split_at(middle_at);
        !budget.is_empty()
            && budget.iter().all(u8::is_ascii_digit)
            && suffix
                .strip_prefix(MIDDLE)
                .is_some_and(|reason| !reason.is_empty())
    }

    if !stderr.split(|byte| *byte == b'\n').any(is_internal_notice) {
        return stderr;
    }

    let mut filtered = Vec::with_capacity(stderr.len());
    for line in stderr.split_inclusive(|byte| *byte == b'\n') {
        if !is_internal_notice(line) {
            filtered.extend_from_slice(line);
        }
    }
    filtered
}

#[cfg(test)]
mod fallback_output_tests {
    use super::strip_internal_soldr_fallback_notices;

    #[test]
    fn cached_internal_fallback_notice_is_removed_without_touching_diagnostics() {
        let input = b"warning: first\n\
soldr: compile daemon unavailable after 30000ms \xe2\x80\x94 falling back to direct uncached rustc (soldr#1657); reason=daemon unavailable\n\
error[E0001]: real compiler diagnostic\r\n\
note: contains soldr: compile daemon unavailable after but is user text"
                .to_vec();
        let filtered = strip_internal_soldr_fallback_notices(input);
        assert_eq!(
            filtered,
            b"warning: first\n\
error[E0001]: real compiler diagnostic\r\n\
note: contains soldr: compile daemon unavailable after but is user text"
        );
    }

    #[test]
    fn near_prefix_compiler_diagnostic_is_preserved() {
        let input = b"soldr: compile daemon unavailable after lunch; this is user text\n".to_vec();
        assert_eq!(strip_internal_soldr_fallback_notices(input.clone()), input);
    }

    #[test]
    fn compiler_stderr_without_internal_notice_is_byte_identical() {
        let input = b"\0non-utf8:\xff\r\nreal stderr without trailing newline".to_vec();
        assert_eq!(strip_internal_soldr_fallback_notices(input.clone()), input);
    }
}

/// Pull the rustc binary path out of the wrapper's argv. The wrapper
/// sends `args[0] = rustc-path`, `args[1..] = rustc-args` (i.e. it has
/// already stripped its own argv[0] = soldr-binary entry). An empty
/// args list is a soldr-side bug — surface it as `EmptyArgs` so the
/// daemon returns a clean structured error instead of panicking.
fn split_compiler_and_args(
    args: &[String],
) -> Result<(NormalizedPath, Vec<String>), EmbeddedServiceError> {
    let mut it = args.iter().cloned();
    let compiler_str = it.next().ok_or(EmbeddedServiceError::EmptyArgs)?;
    let compiler: NormalizedPath = std::path::PathBuf::from(compiler_str).into();
    Ok((compiler, it.collect()))
}

fn derive_identity() -> HostIdentity {
    let route = crate::daemon::backend_handle_adoption::broker_service_name().ok();
    identity_for_route(route.as_deref())
}

fn identity_for_route(route: Option<&str>) -> HostIdentity {
    let id = match route {
        Some(route) => format!("embedded-v1-{route}"),
        // Direct test helpers without a registered broker route still need a
        // deterministic identity; production daemons receive their route in
        // SOLDR_BROKER_SERVICE from the singleton broker.
        None => format!("embedded-v1-soldr-v{}", crate::core::MANAGED_SHIM_VERSION),
    };
    HostIdentity {
        product: "soldr".to_string(),
        instance_id: id.clone(),
        workspace_id: id,
    }
}

/// Effective version directory used by the embedded backend. Keeping this in
/// the sole zccache adapter prevents CLI history from guessing the layout.
pub fn embedded_version_root(paths: &SoldrPaths) -> PathBuf {
    private_zccache_cache_root(paths, &derive_identity())
        .join(zccache::core::config::versioned_subdir())
}

pub fn embedded_compile_journal_path(paths: &SoldrPaths) -> PathBuf {
    embedded_version_root(paths)
        .join("logs")
        .join("compile_journal.jsonl")
}

/// Name of the marker recording that [`scrub_existing_compile_journals`] has
/// already run against this logs directory.
///
/// The trailing version is the re-trigger mechanism: bump it and every store
/// scrubs once more. Lives beside the journals, so a wiped or
/// zccache-version-rotated store correctly loses it and re-scrubs.
const JOURNAL_SCRUB_MARKER: &str = ".compile-journal-scrub-v1";

/// Remove pre-#1149 raw environment values from live and rotated journals
/// before zccache opens the current writer. Invalid legacy lines are dropped
/// closed because retaining an unparseable line could retain a credential.
///
/// # Runs once per store, not once per daemon start (soldr#3174)
///
/// This is a **migration**, and it was running unconditionally on every daemon
/// start: reading every journal into memory, `serde_json`-parsing and
/// re-serializing every line, and rewriting every file. The cost grows with
/// the journal, which grows with use, so it got quietly worse forever.
///
/// The soldr#3163 bringup breadcrumbs measured it at **73.6 s of a 77.6 s
/// daemon cold start** on the Linux gate — 95% of the phase, and on the
/// critical path (the gate's broker-handoff step ends the same second the
/// daemon reports ready). soldr#3174 had guessed the time was in zccache's own
/// start; the sub-phase split showed zccache at 3.8 s and this at 73.6 s.
///
/// A marker makes it run once. That is sound because the thing being migrated
/// cannot come back: #1149 fixed the *writer*, so every line appended since is
/// already sanitized, and rotation only ever derives `compile_journal.jsonl.N`
/// from a file this function already cleaned. Only a store written by a
/// pre-#1149 soldr has anything to scrub, and such a store has no marker.
fn scrub_existing_compile_journals(paths: &SoldrPaths) -> std::io::Result<()> {
    let logs = embedded_version_root(paths).join("logs");
    match std::fs::symlink_metadata(&logs) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
        Ok(_) => crate::cache_lib::path_safety::validate_owned_directory(&paths.root, &logs)?,
    }
    let marker = logs.join(JOURNAL_SCRUB_MARKER);
    if marker.exists() {
        return Ok(());
    }
    scrub_journal_files(&logs)?;
    // Best effort: a store that cannot record the marker re-scrubs next start,
    // which is the old behaviour and is correct, just slow. Failing startup
    // over an unwritable marker would be strictly worse.
    let _ = std::fs::write(&marker, b"soldr#3174\n");
    Ok(())
}

/// The scrub itself, split out so the marker check above reads as one thought.
fn scrub_journal_files(logs: &std::path::Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(logs)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name != "compile_journal.jsonl" && !name.starts_with("compile_journal.jsonl.") {
            continue;
        }
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if !metadata.is_file() || crate::cache_lib::path_safety::is_link_or_reparse(&metadata) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("unsafe compile journal path: {}", entry.path().display()),
            ));
        }
        scrub_compile_journal_file(&entry.path())?;
    }
    Ok(())
}

fn scrub_compile_journal_file(path: &std::path::Path) -> std::io::Result<()> {
    let body = std::fs::read_to_string(path)?;
    let mut sanitized = String::new();
    for line in body.lines() {
        let Ok(mut value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(object) = value.as_object_mut() else {
            continue;
        };
        if let Some(raw_env) = object.remove("env") {
            if let Ok(env) = serde_json::from_value::<Vec<(String, String)>>(raw_env) {
                if let Some(env) = zccache::daemon::compile_journal::sanitize_journal_env(Some(env))
                {
                    object.insert(
                        "env".to_string(),
                        serde_json::to_value(env).map_err(std::io::Error::other)?,
                    );
                }
            }
        }
        sanitized.push_str(&serde_json::to_string(&value).map_err(std::io::Error::other)?);
        sanitized.push('\n');
    }
    let temp = path.with_extension(format!("scrub-{}.tmp", std::process::id()));
    std::fs::write(&temp, sanitized)?;
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

#[path = "zccache_embedded_legacy.rs"]
mod legacy;
pub(crate) use legacy::migrate_legacy_cache_root;
pub use legacy::sweep_legacy_cache_roots;
#[cfg(test)]
use legacy::{derive_legacy_identity, select_legacy_candidate};
pub use legacy::{measure_retired_stores, RetiredStoresUsage};

// The broken-symlink retention test moved to `tests/daemon_zccache_embedded.rs`
// (`#![cfg(unix)]`) — creating a dangling link is inherently host-specific
// (#2493).

fn private_zccache_cache_root(paths: &SoldrPaths, identity: &HostIdentity) -> std::path::PathBuf {
    paths
        .cache
        .join("zccache")
        .join("daemon-state")
        .join(&identity.instance_id)
}

/// Top-level cache root passed to the embedded zccache service.
///
/// Callers that need to locate zccache-owned state should derive it from this
/// shared boundary rather than duplicating the stable host identity.
pub fn embedded_cache_root(paths: &SoldrPaths) -> std::path::PathBuf {
    private_zccache_cache_root(paths, &derive_identity())
}

#[cfg(test)]
#[path = "zccache_embedded_process_tests.rs"]
mod zccache_embedded_process_tests;

#[cfg(test)]
#[path = "zccache_embedded_private_root_tests.rs"]
mod private_root_tests;

#[cfg(test)]
#[path = "zccache_embedded_test_harness_tests.rs"]
mod test_harness_tests;

#[cfg(test)]
#[path = "zccache_embedded_retired_store_tests.rs"]
pub(crate) mod retired_store_tests;
