/// Environment switch that enables tokio-console diagnostics for the daemon.
pub const TOKIO_CONSOLE_ENV_VAR: &str = "SOLDR_DAEMON_TOKIO_CONSOLE";
/// Optional soldr-owned bridge to console-subscriber's recording path.
///
/// Detached daemons intentionally start from a scrubbed environment and
/// forward only `SOLDR_*`, so callers cannot rely on the upstream
/// `TOKIO_CONSOLE_RECORD_PATH` variable crossing the spawn boundary.
pub const TOKIO_CONSOLE_RECORD_PATH_ENV_VAR: &str = "SOLDR_DAEMON_TOKIO_CONSOLE_RECORD_PATH";
/// Optional soldr-owned bridge for console-subscriber's publish interval.
///
/// The detached daemon only inherits `SOLDR_*` variables, so the upstream
/// `TOKIO_CONSOLE_PUBLISH_INTERVAL` variable cannot configure a normally
/// spawned daemon. Values are milliseconds; `0` and malformed values leave
/// console-subscriber's default unchanged.
pub const TOKIO_CONSOLE_PUBLISH_INTERVAL_MS_ENV_VAR: &str =
    "SOLDR_DAEMON_TOKIO_CONSOLE_PUBLISH_INTERVAL_MS";

fn tokio_console_requested() -> bool {
    std::env::var(TOKIO_CONSOLE_ENV_VAR)
        .map(|v| crate::core::flag_value(&v))
        .unwrap_or(false)
}

fn parse_tokio_console_publish_interval(raw: Option<&str>) -> Option<Duration> {
    raw.and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map(Duration::from_millis)
}

fn tokio_console_publish_interval() -> Option<Duration> {
    parse_tokio_console_publish_interval(
        std::env::var(TOKIO_CONSOLE_PUBLISH_INTERVAL_MS_ENV_VAR)
            .ok()
            .as_deref(),
    )
}

/// Install the tokio-console subscriber layer when
/// [`TOKIO_CONSOLE_ENV_VAR`] is truthy, so `tokio-console` can attach to
/// the daemon's runtime and expose per-task poll/idle time and worker
/// behaviour — the tooling needed to pin down the daemon-worker
/// `sched_yield` spin from soldr#1334 at the async-task level.
///
/// `console_subscriber::spawn()` panics unless the crate was compiled
/// with `--cfg tokio_unstable` (the tokio task instrumentation is
/// otherwise absent). We `catch_unwind` that so a normal release build
/// simply logs a hint instead of crashing the daemon. Mirrors the
/// established `zccache-daemon` pattern. No-op (and no subscriber
/// installed — daemon stays silent as before) when the env var is unset.
#[cfg(feature = "tokio-console")]
fn maybe_init_tokio_console() {
    if !tokio_console_requested() {
        return;
    }
    let spawn = || {
        let mut builder = console_subscriber::Builder::default().with_default_env();
        if let Some(path) = std::env::var_os(TOKIO_CONSOLE_RECORD_PATH_ENV_VAR) {
            builder = builder.recording_path(path);
        }
        if let Some(interval) = tokio_console_publish_interval() {
            builder = builder.publish_interval(interval);
        }
        builder.spawn()
    };
    match std::panic::catch_unwind(spawn) {
        Ok(console_layer) => {
            use tracing_subscriber::prelude::*;
            let _ = tracing_subscriber::registry()
                .with(console_layer)
                .try_init();
            tracing::info!(
                "tokio-console enabled for soldr-daemon; connect with `tokio-console` \
                 (default 127.0.0.1:6669)"
            );
        }
        Err(_) => {
            eprintln!(
                "soldr-daemon: {TOKIO_CONSOLE_ENV_VAR} set but console-subscriber is \
                 inert — rebuild with RUSTFLAGS=\"--cfg tokio_unstable\" to use tokio-console"
            );
        }
    }
}

#[cfg(not(feature = "tokio-console"))]
fn maybe_init_tokio_console() {
    if tokio_console_requested() {
        eprintln!(
            "soldr-daemon: {TOKIO_CONSOLE_ENV_VAR} set but soldr-cli was built without \
             the `tokio-console` feature; rebuild with `--features tokio-console` and \
             RUSTFLAGS=\"--cfg tokio_unstable\" to use tokio-console"
        );
    }
}

// Process-lifetime runtime: the daemon's own lifetime (soldr#3836).
#[cfg_attr(dylint_lib = "ban_raw_sync_primitive", allow(ban_raw_sync_primitive))]
pub fn run(opts: ServerOptions) -> Result<(), ServerError> {
    // soldr#3163: start the bringup clock at process entry, before anything
    // that can be slow. The recorder has no log yet -- `SoldrPaths` is not
    // resolved until `run_async`, and resolving it is itself a timed phase.
    let mut bringup = crate::daemon::bringup::BringupRecorder::new();
    // Opt-in async-runtime instrumentation. Must run before the runtime
    // is built so console-subscriber's aggregator is in place.
    maybe_init_tokio_console();

    // L6 (soldr#980): cap the daemon Tokio runtime to a small fixed
    // worker count so it doesn't oversubscribe the CPU during cargo
    // builds. The daemon handles IPC + bookkeeping; rustc is the CPU
    // bottleneck. Off-CPU profiling showed `tokio-rt-worker` context
    // switches were ~3x higher cold vs bare and `sched_yield` events
    // tripled — capping at 2-4 workers cuts the unused-worker noise.
    // L6 reverted post-#980 perf measurement: capping the worker pool
    // to 2-4 starved the daemon's Compile dispatch (each compile awaits
    // its rustc subprocess on a worker; the cap forced serial dispatch
    // when cargo wanted N-way parallelism). Use the full host
    // parallelism — the tokio scheduler keeps workers cheap when idle.
    let available = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(2);
    let workers = available.max(2);
    tracing::info!("soldr-daemon Tokio runtime: {workers} workers (host parallelism: {available})");
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(workers).enable_all();
    // soldr#3210: a retiring pool thread SIGTERMs the compilers it spawned.
    crate::daemon::runtime_threads::keep_threads_for_daemon_lifetime(&mut builder);
    let runtime = builder.build()?;
    bringup.phase(crate::daemon::bringup::phase::TOKIO_RUNTIME);
    runtime.block_on(run_async_recording(opts, bringup))
}

/// Async daemon entry point. Use this when calling from inside an
/// existing tokio runtime (e.g. `soldr daemon start --foreground`
/// dispatched from `main`'s `#[tokio::main]` runtime). The synchronous
/// `run` builds its own multi-thread runtime and is the right entry
/// point for the `soldr-daemon` bin target whose `main` has no
/// ambient runtime. Calling `run` from within an ambient runtime
/// panics with "Cannot start a runtime from within a runtime" — that
/// was the failure on soldr#985's perf-matrix CI run.
pub async fn run_async(opts: ServerOptions) -> Result<(), ServerError> {
    run_async_recording(opts, crate::daemon::bringup::BringupRecorder::new()).await
}

/// [`run_async`] with a bringup clock already running.
///
/// Split out so the synchronous [`run`] entry point can time the Tokio runtime
/// build -- which happens before any async code exists to time it -- on the
/// same clock as everything after it (soldr#3163).
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
async fn run_async_recording(
    opts: ServerOptions,
    mut bringup: crate::daemon::bringup::BringupRecorder,
) -> Result<(), ServerError> {
    let paths = SoldrPaths::new()?;
    std::fs::create_dir_all(soldr_daemon_dir(&paths))?;
    init_embedded_service_file_tracing(&paths);
    // The log lands beside `daemon-spawn.log`, which the broker's launcher
    // already redirects this process's stderr into.
    bringup.attach_log(&paths.root);
    bringup.phase(crate::daemon::bringup::phase::RESOLVE_PATHS);

    // Bounded grace for the stop→relaunch race: the previous daemon's lock
    // is released only when its process exits, which lags the stop
    // acknowledgment. Retry while nobody serves the root; back off
    // immediately when a live daemon does (issue #1814 semantics).
    let _root_ownership = {
        use crate::daemon::lifecycle::RootAcquireOutcome;
        let outcome = crate::daemon::lifecycle::RootOwnershipGuard::acquire_with_grace(
            &paths,
            ROOT_OWNERSHIP_ACQUIRE_BUDGET,
            ROOT_OWNERSHIP_ACQUIRE_POLL,
            || existing_daemon_pid(&paths),
        )?;
        match outcome {
            RootAcquireOutcome::Acquired(guard) => guard,
            RootAcquireOutcome::AlreadyServing(pid) => {
                return Err(ServerError::AlreadyRunning(pid));
            }
            RootAcquireOutcome::TimedOut => {
                return Err(ServerError::Io(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    crate::daemon::lifecycle::describe_root_ownership_conflict(&paths),
                )));
            }
        }
    };
    bringup.phase(crate::daemon::bringup::phase::ROOT_OWNERSHIP);

    if let Some(existing) = existing_daemon_pid(&paths) {
        return Err(ServerError::AlreadyRunning(existing));
    }
    // Claim the control endpoint immediately after the version-blind
    // occupancy check. Delaying bind until a detached accept task used to
    // leave a large initialization window in which an older daemon could bind
    // and then have its live socket unlinked by this process. Unix hosts
    // claim a filesystem socket (with the identity fencing the retirement
    // fence); Windows hosts serve a named-pipe pool and take the None branch.
    let control_listener =
        if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
            None
        } else {
            let sock = crate::daemon::session_endpoint::resolved_control_endpoint_path(&paths)?;
            Some(crate::platform::ipc::listener::claim_control_endpoint_at(
                &sock,
            )?)
        };
    bringup.phase(crate::daemon::bringup::phase::CONTROL_ENDPOINT);

    let db_path = data_db_path(&paths);
    let start_instant = Instant::now();
    let idle_timeout_secs = u32::try_from(opts.idle_timeout.as_secs()).ok();
    let daemon_identity = current_daemon_process(&paths, idle_timeout_secs)
        .map_err(|err| ServerError::Io(std::io::Error::other(err.to_string())))?;
    bringup.phase(crate::daemon::bringup::phase::DAEMON_IDENTITY);

    // soldr#3169: serve the control endpoint from here rather than from the
    // end of bringup. Requests park until `State` is published below, but
    // receipt is acknowledged at once -- see `ControlContext`. soldr#3096: the
    // terminate future is constructed before any accept loop can make the
    // daemon observably ready; see the terminate hook below.
    let terminate_signal = crate::platform::process::signal::wait_for_terminate_signal();
    let shutdown = Arc::new(crate::daemon::maintenance::ShutdownSignal::default());
    let (state_publisher, state_source) = tokio::sync::watch::channel(None);
    let control = ControlContext::new(daemon_identity.clone(), Arc::clone(&shutdown), state_source);
    let accept_handle = AcceptTask(match control_listener {
        Some((listener, _identity)) => tokio::spawn(async move {
            let _ = run_accept_loop_unix(listener, control).await;
        }),
        None => {
            let paths_for_accept = paths.clone();
            tokio::spawn(async move {
                let _ = run_accept_loop_windows(paths_for_accept, control).await;
            })
        }
    });

    // Bind before heavyweight startup; SESSION payloads await compile-service publication.
    let session_listener = crate::daemon::session_endpoint::resolve_session_listener(&paths)?
        .ok_or_else(|| {
            ServerError::Io(std::io::Error::other(
                "broker-facing SESSION endpoint was not configured",
            ))
        })?;
    let handoff_endpoint = crate::daemon::session_endpoint::resolve_handoff_endpoint(&paths)?;
    bringup.phase(crate::daemon::bringup::phase::SESSION_LISTENER);

    // soldr#2436 phase 2: bound the journal, attribute any un-drained
    // predecessor, then record this start with version + exe identity.
    crate::daemon::lifecycle::rotate_lifecycle_journal(&paths);
    crate::daemon::lifecycle::detect_unclean_shutdown(&paths);
    crate::daemon::lifecycle::append_lifecycle_event_with(
        &paths,
        "spawn",
        crate::daemon::lifecycle::LifecycleDetails::recording_daemon_identity(),
    );
    // A panic previously left no exit record at all (soldr#2436 fact 4).
    // Chain the default hook so backtraces still print.
    {
        let panic_paths = paths.clone();
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            crate::daemon::lifecycle::append_lifecycle_event_with(
                &panic_paths,
                "died-panic",
                crate::daemon::lifecycle::LifecycleDetails {
                    reason: Some(crate::daemon::lifecycle::LifecycleReason::Panic),
                    ..crate::daemon::lifecycle::LifecycleDetails::recording_daemon_identity()
                },
            );
            default_hook(info);
        }));
    }
    bringup.phase(crate::daemon::bringup::phase::LIFECYCLE_JOURNAL);
    let mut session_identity = daemon_identity.clone();
    session_identity.ipc_endpoint.path =
        crate::daemon::session_endpoint::resolved_session_endpoint_path(&paths)?;
    if let Ok(namespace_id) = std::env::var("RUNNING_PROCESS_BROKER_V1_BACKEND_NAMESPACE") {
        if !namespace_id.is_empty() {
            session_identity.ipc_endpoint.namespace_id = namespace_id;
        }
    }
    crate::daemon::backend_handle_adoption::publish_broker_route_claim(&paths, &session_identity)?;
    // The far end of soldr#3163's unattributed gap: the front door has been
    // waiting for this claim since the broker launched this process.
    bringup.phase(crate::daemon::bringup::phase::ROUTE_CLAIM);
    let session_mux = Arc::new(crate::daemon::session_endpoint::soldr_session_endpoint_mux(
        session_identity,
    ));
    // L4 (issue soldr#980): start the background event-flusher BEFORE we
    // accept any IPC traffic so the very first compile event lands on
    // a live channel. The drain task lives in the daemon's tokio runtime
    // and exits cleanly via `shutdown()` below. soldr#3224: that includes the
    // SESSION endpoint, which records every compile's lifecycle through it; a
    // write that races the database initialization below is retained and
    // retried by the batcher.
    let event_batcher = EventBatcher::start(db_path.clone());
    let (compile_readiness, compile_publisher) =
        crate::daemon::session_endpoint::CompileServiceReadiness::pending(event_batcher.clone());
    // soldr#3102: the handoff endpoint runs on its own control-plane
    // runtime so the broker's 5 s ACK budget never competes with compile
    // runtime workers parked in the embedded cache's synchronous hit path.
    let (session_handle, handoff_handle) =
        crate::daemon::session_endpoint::spawn_session_endpoint_servers(
            session_listener,
            handoff_endpoint,
            compile_readiness,
            paths.clone(),
            session_mux,
        )?;
    bringup.phase(crate::daemon::bringup::phase::ENDPOINT_SERVERS);

    // Embedded zccache initializes asynchronously. The first operation that
    // actually needs it awaits this task through `CompileServiceReadiness`;
    // broker probes remain independent and responsive throughout.
    let compile_paths = paths.clone();
    let compile_identity = daemon_identity.clone();
    let compile_handle = tokio::spawn(async move {
        let result = start_compile_service(&compile_paths, &compile_identity).await;
        match &result {
            Ok(service) => compile_publisher.publish(Ok(Arc::clone(service))),
            Err(error) => compile_publisher.publish(Err(format!(
                "embedded compile service initialization failed: {error:?}"
            )
            .into())),
        }
        result
    });

    // Database initialization runs concurrently on the blocking pool. It is
    // not a prerequisite for broker readiness or SESSION compile execution.
    let startup_db_path = db_path.clone();
    let db_result = tokio::task::spawn_blocking(move || {
        let touched = TargetRegistry::open(&startup_db_path).map(|_| ());
        let _ = db::ensure_initialized(&startup_db_path);
        let _ = cook_index::ensure_initialized(&startup_db_path);
        touched
    })
    .await;
    let db_result = match db_result {
        Ok(result) => result,
        Err(error) => {
            compile_handle.abort();
            session_handle.abort();
            handoff_handle.abort();
            return Err(ServerError::Io(std::io::Error::other(error.to_string())));
        }
    };
    if let Err(error) = db_result {
        compile_handle.abort();
        session_handle.abort();
        handoff_handle.abort();
        return Err(ServerError::Registry(error));
    }
    bringup.phase(crate::daemon::bringup::phase::STATE_STORE);

    let compile_service = match compile_handle.await {
        Ok(Ok(service)) => service,
        Ok(Err(error)) => {
            session_handle.abort();
            handoff_handle.abort();
            return Err(error);
        }
        Err(error) => {
            session_handle.abort();
            handoff_handle.abort();
            return Err(ServerError::Io(std::io::Error::other(format!(
                "embedded compile service initialization task failed: {error}"
            ))));
        }
    };
    // soldr#3174: split the one phase that dominates a gate cold start, so the
    // next reader can see whether the time is soldr's or zccache's.
    bringup.compile_service_breakdown(&compile_service.start_timings());
    bringup.phase(crate::daemon::bringup::phase::COMPILE_SERVICE);


    let state = Arc::new(State {
        db_path,
        paths: paths.clone(),
        daemon_identity,
        start_instant,
        request_count: AtomicU64::new(0),
        last_activity_ms: AtomicU64::new(0),
        exit_via_idle: AtomicBool::new(false),
        cook_hits_this_session: AtomicU64::new(0),
        shutdown,
        event_batcher,
        compile_service,
    });

    test_pause_before_serving_control().await;
    // Every control request parked since bringup began proceeds from here.
    state_publisher.send_replace(Some(Arc::clone(&state)));

    let idle_handle = (opts.idle_timeout != Duration::MAX).then(|| {
        let idle_state = state.clone();
        let idle_timeout = opts.idle_timeout;
        tokio::spawn(async move {
            run_idle_watchdog(idle_state, idle_timeout).await;
        })
    });

    let owner_handle = opts.owner_pid.map(|owner_pid| {
        let owner_state = state.clone();
        tokio::spawn(async move {
            run_owner_watchdog(owner_state, owner_pid).await;
        })
    });

    let maintenance_context = crate::daemon::maintenance::MaintenanceContext {
        paths: paths.clone(),
        db_path: state.db_path.clone(),
        compile_service: Arc::clone(&state.compile_service),
        shutdown: Arc::clone(&state.shutdown),
    };
    let maintenance_handle = tokio::spawn(async move {
        crate::daemon::maintenance::run_loop(maintenance_context).await;
    });

    // soldr#3038 / soldr#3057: opt-in, fail-fast RSS ceiling watchdog.
    // Spawned only when `SOLDR_DAEMON_RSS_CEILING_BYTES` parses to a
    // positive byte count, so an ordinary daemon start (the overwhelming
    // common case) pays no extra timer, file write, or mimalloc stats call
    // at all -- see `rss_ceiling`'s module docs for the full design
    // rationale, including why a breach now dumps memory and exits rather
    // than recording and continuing. The sampled profiler must be started
    // before the watchdog can produce a useful heap.pprof on breach, and
    // only when a ceiling is actually configured -- same gate, same call.
    let rss_ceiling_bytes = crate::daemon::rss_ceiling::ceiling_bytes_from_env();
    crate::daemon::rss_ceiling::start_sampled_profiler_if_configured(rss_ceiling_bytes);
    let rss_ceiling_handle = rss_ceiling_bytes.map(|ceiling_bytes| {
        let rss_paths = paths.clone();
        let rss_shutdown = Arc::clone(&state.shutdown);
        tokio::spawn(async move {
            crate::daemon::rss_ceiling::run_watchdog(
                rss_paths,
                rss_shutdown,
                ceiling_bytes,
                crate::daemon::rss_ceiling::ProcessRole::Daemon,
            )
            .await;
        })
    });

    let signal_state = state.clone();
    tokio::spawn(async move {
        if (tokio::signal::ctrl_c().await).is_ok() {
            signal_state.shutdown.request();
        }
    });
    // Issue #1286 (F1) originally hooked SIGTERM into this same graceful
    // path so `pkill soldr-daemon` / container stop / service managers
    // (which all send TERM, not INT) would drain the embedded zccache's
    // in-memory artifact index + depgraph instead of discarding it.
    //
    // soldr#3059 reverses that. Measurement showed the drain this path
    // triggers is unbounded in practice (maintenance's `spawn_blocking`
    // deletion worker, an untimed zccache publication barrier), while a
    // container SIGKILLs ~10s after TERM and systemd's default is ~90s —
    // both well under the 240s watchdog grace below. "Drain gracefully"
    // therefore meant "get SIGKILLed mid-drain": the same state loss the
    // graceful path exists to avoid, plus a blocked supervisor on top.
    // Fast and honest beats slow and killed.
    //
    // SIGTERM now takes a fast path directly from this task
    // (`fast_exit_on_signal` below) instead of calling
    // `state.shutdown.request()` — see that function's doc comment for why
    // going through the shared flag (which SIGINT and every other shutdown
    // trigger still use) is not equivalent here. SIGINT is deliberately
    // left on the graceful path: it is normally sent interactively
    // (Ctrl-C at a terminal), where the operator can escalate to SIGKILL
    // themselves if a slow drain is unwelcome, unlike the fixed, short
    // supervisor grace periods SIGTERM has to survive. The signal wait
    // lives in the platform process facade. On Windows (soldr#3096) the
    // "signal" is a named terminate event that `signal_pid(pid, false)`
    // sets; the name printed below stays "SIGTERM" because that is the
    // lifecycle contract's word for it and the event is precisely the
    // Windows equivalent. The future is constructed *before* the task is
    // spawned because the Windows implementation registers its event at
    // call time -- a stop that races the task's first poll must still
    // find it (see `platform/process/signal.rs`); it is therefore
    // constructed above, before the accept loop that makes the daemon
    // observably ready is spawned.
    let term_paths = paths.clone();
    tokio::spawn(async move {
        if terminate_signal.await {
            fast_exit_on_signal(&term_paths, "SIGTERM");
        }
    });

    // Every task that makes this daemon serve is spawned; `total_ms` on this
    // line is the complete cold-start cost the front door waited through.
    bringup.phase(crate::daemon::bringup::phase::READY);
    drop(bringup);

    state.shutdown.wait().await;
    arm_shutdown_watchdog(paths.clone());
    accept_handle.abort();
    session_handle.abort();
    handoff_handle.abort();
    if let Some(handle) = idle_handle {
        handle.abort();
    }
    if let Some(handle) = owner_handle {
        handle.abort();
    }
    let shutdown_phase = |name| {
        crate::daemon::lifecycle::append_lifecycle_event_with(
            &paths,
            name,
            crate::daemon::lifecycle::LifecycleDetails::default().for_target_route(
                std::process::id(),
                state.daemon_identity.started_at_unix_ms,
                state.daemon_identity.ipc_endpoint.path.clone(),
            ),
        );
    };
    // soldr#3377: let running compiles finish before the process goes away,
    // so their wrappers get an `Exit` frame instead of a closed stream.
    drain_inflight_compiles(&paths, "graceful-shutdown").await;
    shutdown_phase("shutdown-phase-maintenance");
    // A destructive pass that already acquired the root maintenance lease is
    // allowed to finish. In particular, await its spawn_blocking deletion
    // worker before releasing root ownership.
    let _ = maintenance_handle.await;
    // Same shutdown-aware loop shape as maintenance_handle (it selects on
    // `shutdown.wait()` internally and returns promptly), so it is awaited
    // rather than aborted -- an abort mid-write could leave a torn
    // `rss-ceiling-v1.json` behind.
    if let Some(handle) = rss_ceiling_handle {
        let _ = handle.await;
    }

    // L4 (issue soldr#980): drain whatever the background event flusher
    // has staged in memory before the daemon process exits. Must run
    // BEFORE the redb file lock is released so the final write txn
    // succeeds; `shutdown()` awaits the drain task's ack.
    shutdown_phase("shutdown-phase-event-batcher");
    let _ = state.event_batcher.shutdown().await;

    // Issue #977 / #980 L1: drain the embedded zccache service before
    // any other shutdown work so pending writes flush through.
    shutdown_phase("shutdown-phase-compile-service");
    shutdown_compile_service(&state).await;

    // Deliberately retain the protobuf route claim and Unix socket nodes.
    // A check-then-unlink fence is not atomic: an older Soldr release that
    // does not honor `root-owner.lock` can publish a successor between the
    // check and unlink, and the retiring daemon would delete the successor's
    // state. Startup already probes liveness, atomically replaces stale claims, and
    // reclaims a stale socket before binding, so retaining these artifacts is
    // safe and closes that cross-version race.
    let event = if state.exit_via_idle.load(Ordering::Relaxed) {
        "died-idle"
    } else {
        "died-shutdown"
    };
    append_lifecycle_event(&paths, event);
    Ok(())
}

/// Longest a graceful shutdown waits for running compiles. Kept well under
/// the shutdown watchdog's grace so a hung compile cannot wedge teardown.
const SHUTDOWN_COMPILE_DRAIN: Duration = Duration::from_secs(60);

/// Wait (bounded) for in-flight compiles, then record any that will be cut.
async fn drain_inflight_compiles(paths: &SoldrPaths, reason: &str) {
    let deadline = std::time::Instant::now() + SHUTDOWN_COMPILE_DRAIN;
    let waiting = crate::daemon::inflight_compiles::count();
    if waiting > 0 {
        eprintln!(
            "soldr-daemon: {reason}: waiting up to {}s for {waiting} in-flight compile(s)",
            SHUTDOWN_COMPILE_DRAIN.as_secs()
        );
    }
    while crate::daemon::inflight_compiles::count() > 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    record_compiles_cut(paths, reason);
}

/// Name the compiles a shutdown is about to cut (soldr#3377): each wrapper
/// sees "SESSION relay closed before Exit", and without this the daemon log
/// gave no hint why.
fn record_compiles_cut(paths: &SoldrPaths, reason: &str) {
    let cut = crate::daemon::inflight_compiles::snapshot();
    if cut.is_empty() {
        return;
    }
    let units: Vec<String> = cut
        .iter()
        .take(8)
        .map(|c| c.crate_name.clone().unwrap_or_else(|| format!("#{}", c.id)))
        .collect();
    eprintln!(
        "soldr-daemon: {reason}: cutting {} in-flight compile(s) (e.g. {}); their wrappers \
         will report 'SESSION relay closed before Exit' (soldr#3377)",
        cut.len(),
        units.join(", ")
    );
    crate::daemon::lifecycle::append_lifecycle_event(paths, "shutdown-cut-compiles");
}

/// SIGTERM's fast-exit path (soldr#3059). Never returns.
///
/// Called directly from the SIGTERM-wait task above, NOT via
/// `state.shutdown.request()`. That distinction is load-bearing, not
/// stylistic: the ordinary shutdown flag is read exactly once, by the
/// `state.shutdown.wait()` call near the top of `run_async`'s teardown. If
/// a graceful drain triggered by something else — idle timeout, owner
/// death, an explicit `soldr daemon stop` — is already in flight when
/// SIGTERM arrives (the realistic escalation case: a supervisor asks
/// nicely, then insists), that `wait()` call has already returned and
/// nothing re-reads the flag again. Setting it a second time would be
/// silently absorbed, and the daemon would keep draining until the 240s
/// watchdog — exactly the slow-then-killed outcome this path exists to
/// avoid. Calling `std::process::exit` straight from this task instead
/// fires unconditionally, at any point in the daemon's life, regardless of
/// what else is running — the same shape `rss_ceiling::die_on_breach` uses
/// to call `std::process::exit` directly rather than routing through
/// `ShutdownSignal`, for the same reason: a process in a state that
/// justifies skipping the graceful path cannot wait for that path's own
/// machinery to notice.
///
/// Ordered so the one genuinely corruption-preventing step happens first
/// and cannot be skipped by an early return:
///
/// 1. Append `died-signal-fast` to the lifecycle JSONL soldr owns outright
///    (`crate::daemon::lifecycle::append_lifecycle_event` — the same
///    writer `died-idle` / `died-shutdown` already use, not a
///    reimplementation). This one line is both the shutdown-reason record
///    *and* the end-of-stream marker the owner asked for: because its
///    reason string carries the existing `died-*` prefix,
///    `detect_unclean_shutdown` (`lifecycle/journal_hygiene.rs`) already
///    recognizes it as a deliberate exit rather than a crash needing an
///    `unclean-shutdown-detected` record on the next start — see
///    `journal_hygiene_tests::dead_pid_with_died_signal_fast_record_is_not_reported`.
///
///    `compile_journal.jsonl` was considered instead, or as well — it is
///    the append-only stream a reader most needs to trust — but it is
///    written by a background thread inside the *vendored* zccache crate
///    via un-atomic, multi-`write()` `writeln!` calls, with no public API
///    to request a bounded flush or to have it write a marker of its own.
///    Appending to it from a second, soldr-owned file handle while that
///    thread might be mid-write would be a new corruption source this
///    path must not introduce, so it is left untouched. A reader who wants
///    to know whether a given daemon generation's compile journal is
///    complete gets the answer from the lifecycle event instead:
///    `died-signal-fast` means "this generation exited without draining
///    its compile journal — trust it only up to whatever it had already
///    written before this signal, and do not assume a clean-looking last
///    line means nothing more was coming."
/// 2. `eprintln!` naming the signal, the pid, and what was (the marker)
///    and was not (the state DB, the embedded compile cache) flushed.
/// 3. `std::process::exit(1)`.
///
/// Deliberately skipped, with justification rather than silent omission:
///
/// - `maintenance_handle` — explicitly out of scope: it can hold the root
///   maintenance lease through an in-progress destructive `spawn_blocking`
///   deletion pass with no cancellation point.
/// - `state.event_batcher.shutdown()` — an unbounded await on a SQLite
///   write via a `oneshot` ack. SQLite's own crash safety (WAL) means an
///   abrupt exit here loses at most the not-yet-flushed tail and never
///   corrupts the file, so skipping it degrades to "a bit more cold," not
///   "broken."
/// - `shutdown_compile_service` — the embedded zccache drain: an untimed
///   publication write barrier plus an index-writer join. Same
///   crash-safety argument, and the same reason a real memory-ceiling
///   breach (`die_on_breach` above) also skips straight past it.
///
/// Neither skipped step writes anything append-only and otherwise
/// unprotected the way `compile_journal.jsonl` is — which is exactly why
/// that file (via the lifecycle event referencing it) needed a marker and
/// these do not.
fn fast_exit_on_signal(paths: &SoldrPaths, signal_name: &str) -> ! {
    record_compiles_cut(paths, signal_name);
    crate::daemon::lifecycle::append_lifecycle_event(paths, "died-signal-fast");
    eprintln!(
        "soldr-daemon: {signal_name} received (pid {pid}); taking the fast-exit path \
         (soldr#3059) instead of the graceful drain -- lifecycle end-of-stream marker \
         'died-signal-fast' written; state DB and embedded compile-cache drains \
         skipped (unbounded, not corruption-risking); exiting 1",
        pid = std::process::id(),
    );
    std::process::exit(1);
}

/// Env override for [`SHUTDOWN_WATCHDOG_GRACE`], in seconds. Zero, empty, or malformed values use the default;
/// the backstop cannot be disabled (soldr#3648).
pub const SHUTDOWN_WATCHDOG_ENV_VAR: &str = "SOLDR_SHUTDOWN_WATCHDOG_SECS";

/// How long teardown may run before the process exits regardless.
///
/// Deliberately under `GRACEFUL_SHUTDOWN_WAIT_TIMEOUT` (300s) so the process
/// is gone before the client gives up and reports failure.
///
/// soldr#3059: SIGTERM no longer goes through this budget at all —
/// `fast_exit_on_signal` above exits within milliseconds, before
/// `state.shutdown.wait()` even needs to return. This grace period now
/// guards only the triggers that still take the graceful path: idle
/// timeout, owner-process death, an explicit `soldr daemon stop`, and
/// SIGINT. None of those race an external supervisor's kill deadline the
/// way SIGTERM did (container ~10s, systemd default ~90s) — they are
/// either self-paced (idle, owner death) or bounded from the client side by
/// the 300s `GRACEFUL_SHUTDOWN_WAIT_TIMEOUT` this constant was already
/// sized against. So there is no forcing function to lower 240s in this
/// pass, and it is left unchanged; if `soldr daemon stop` wants a tighter
/// bound in the future that is a question about the CLI-side timeout, not
/// this watchdog.
pub const SHUTDOWN_WATCHDOG_GRACE: Duration = Duration::from_secs(240);

pub(crate) fn shutdown_watchdog_grace() -> Option<Duration> {
    parse_watchdog_grace(std::env::var(SHUTDOWN_WATCHDOG_ENV_VAR).ok().as_deref())
}

/// Always returns `Some`. Zero, empty, or malformed overrides fall back to the
/// 240 s default so no override can remove the one thing guaranteeing the
/// process exits (soldr#3648; see docs/DAEMON_TIMEOUTS.md).
pub(crate) fn parse_watchdog_grace(raw: Option<&str>) -> Option<Duration> {
    let Some(raw) = raw else {
        return Some(SHUTDOWN_WATCHDOG_GRACE);
    };
    match raw.trim().parse::<u64>() {
        Ok(0) => Some(SHUTDOWN_WATCHDOG_GRACE),
        Ok(secs) => Some(Duration::from_secs(secs)),
        Err(_) => Some(SHUTDOWN_WATCHDOG_GRACE),
    }
}

/// Guarantee the process exits once teardown has started.
///
/// Covers only the graceful shutdown triggers (idle timeout, owner death,
/// `soldr daemon stop`, SIGINT) — SIGTERM bypasses this backstop entirely
/// by exiting from `fast_exit_on_signal` before `state.shutdown.wait()`
/// even returns (soldr#3059; see that function's and
/// [`SHUTDOWN_WATCHDOG_GRACE`]'s doc comments).
///
/// Every step the graceful path still takes after this point is a
/// best-effort flush, and several are unbounded: the maintenance join
/// waits out an in-flight pass whose `spawn_blocking` deletion worker
/// cannot be cancelled, the embedded zccache drain takes an untimed
/// publication write barrier and joins its index writer, and dropping the
/// multi-thread runtime waits for every outstanding blocking task. Any one
/// of them wedging left a daemon alive forever holding the route claim and
/// endpoints — and the CLI deliberately never force-kills a daemon that
/// acknowledged shutdown, so nothing else bounded it.
///
/// A detached OS thread, NOT a tokio task: it has to fire even when the
/// runtime is stalled or its blocking pool is fully occupied, which is
/// exactly the situation it exists for.
///
/// soldr#3059: this originally called `std::process::exit(0)` on expiry,
/// telling a supervisor, a test, or a CI lane that watched the exit status
/// that a daemon which blew its entire [`SHUTDOWN_WATCHDOG_GRACE`] exited
/// cleanly. A first pass changed that to a distinctive `124` (the GNU
/// `timeout(1)` convention); the owner then asked for plain
/// [`SHUTDOWN_WATCHDOG_EXIT_CODE`] (`1`) instead — see that constant's doc
/// comment for what that costs and why it was still the right call. A
/// clean drain (the normal `Ok(())` return from the caller) still reaches
/// the process's ordinary `main` exit path and still exits 0; only this
/// expiry arm changes.
fn arm_shutdown_watchdog(paths: SoldrPaths) {
    let Some(grace) = shutdown_watchdog_grace() else {
        return;
    };
    std::thread::Builder::new()
        .name("soldr-shutdown-watchdog".into())
        .spawn(move || {
            std::thread::sleep(grace);
            tracing::error!(
                grace_secs = grace.as_secs(),
                exit_code = SHUTDOWN_WATCHDOG_EXIT_CODE,
                "graceful shutdown did not complete within the watchdog grace; \
                 exiting now. Cache state may not be fully flushed."
            );
            // Record the expiry in the lifecycle JSONL before exiting: the
            // ordinary `died-idle`/`died-shutdown` event at the end of the
            // graceful path may never run if teardown is genuinely wedged,
            // so without this line a watchdog-forced exit and a clean one
            // are indistinguishable in the log.
            crate::daemon::lifecycle::append_lifecycle_event(
                &paths,
                "shutdown-watchdog-expired",
            );
            std::process::exit(SHUTDOWN_WATCHDOG_EXIT_CODE);
        })
        .ok();
}

/// Non-zero exit code for a watchdog-forced exit (soldr#3059).
///
/// A first pass set this to `124` (the GNU coreutils `timeout(1)`
/// convention) specifically so it would read as distinct from
/// `rss_ceiling::die_on_breach`'s plain `1` for a memory-ceiling breach.
/// The owner overruled that: this is `1` too. Exit code alone therefore no
/// longer distinguishes "a stuck graceful drain" from "an over-budget
/// process" (or, since soldr#3059, from `fast_exit_on_signal`'s own `1` on
/// a fast SIGTERM exit) — that collapse is the owner's deliberate call, not
/// an oversight reintroduced by accident. A caller that needs to tell
/// these apart reads the lifecycle JSONL or the stderr line instead of the
/// exit code: `shutdown-watchdog-expired` (this path) vs.
/// `daemon_rss_ceiling_breach_dump_written` / a `memory-breach-*` dump
/// directory (`die_on_breach`) vs. `died-signal-fast`
/// (`fast_exit_on_signal`) are each unambiguous, and the process never
/// writes more than one of them.
const SHUTDOWN_WATCHDOG_EXIT_CODE: i32 = 1;

fn existing_daemon_pid(paths: &SoldrPaths) -> Option<u32> {
    select_existing_daemon_pid(
        crate::daemon::lifecycle::claimed_daemon_occupies_route(paths),
        is_live(paths),
    )
}

fn select_existing_daemon_pid(
    version_blind: Option<u32>,
    protocol_aware: Option<u32>,
) -> Option<u32> {
    version_blind.or(protocol_aware)
}

#[cfg(test)]
mod endpoint_occupancy_tests {
    use super::select_existing_daemon_pid;

    #[test]
    fn protocol_mismatched_daemon_still_blocks_direct_startup() {
        assert_eq!(select_existing_daemon_pid(Some(41), None), Some(41));
        assert_eq!(select_existing_daemon_pid(None, Some(42)), Some(42));
        assert_eq!(select_existing_daemon_pid(None, None), None);
    }
}

// The Unix endpoint exclusivity and retirement-fence tests live in
// `tests/daemon_unix_endpoint.rs` (`#![cfg(unix)]`) — they exercise
// the platform listener leaf now that server.rs is host-neutral.

/// The embedded zccache service runs in-process, so its `tracing::warn!`
/// events otherwise have no durable destination in the normal daemon mode.
/// Keep a daily rolling file under soldr's daemon state for post-build
/// investigation; startup must remain best-effort when the cache is on a
/// read-only or otherwise unusual filesystem.
fn init_embedded_service_file_tracing(paths: &SoldrPaths) {
    let log_dir = embedded_service_log_dir(paths);
    if let Err(error) = std::fs::create_dir_all(&log_dir) {
        eprintln!(
            "soldr-daemon: cannot create embedded zccache warning log directory {}: {error}",
            log_dir.display()
        );
        return;
    }
    let appender = tracing_appender::rolling::daily(log_dir, "embedded-zccache.warn.log");
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_ansi(false)
        .with_writer(appender)
        .try_init();
}

fn embedded_service_log_dir(paths: &SoldrPaths) -> PathBuf {
    soldr_daemon_dir(paths).join("logs")
}

/// Read budget for draining a doomed connection (#1853). Short on purpose:
/// we only need to clear what the peer already queued, not wait for more.
const DRAIN_READ_TIMEOUT: Duration = Duration::from_millis(250);

/// How long a starting daemon waits for a busy root-owner lock when no live
/// daemon serves the root — the window where the previous owner has
/// acknowledged `daemon stop` but its process (and thus the lock handle) has
/// not finished exiting. Well under the broker's 60s daemon-readiness budget
/// so a genuine conflict still surfaces inside one spawn attempt.
const ROOT_OWNERSHIP_ACQUIRE_BUDGET: Duration = Duration::from_secs(10);
const ROOT_OWNERSHIP_ACQUIRE_POLL: Duration = Duration::from_millis(200);

/// Tell a peer whose protocol we cannot speak *why* we are hanging up, then
/// close cleanly (#1853).
///
/// The reject record is version-independent by construction (see
/// [`crate::daemon::ipc::REJECT_RECORD_VERSION`]), so even a client several
/// versions old gets a diagnosable `InvalidData` with a message instead of an
/// opaque `ECONNRESET` after burning its whole retry budget.
async fn reject_version_mismatch<S>(stream: &mut S, peer_version: u32)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;
    use tokio::time::timeout;
    tracing::warn!(
        peer_version,
        daemon_version = PROTOCOL_VERSION,
        "soldr-daemon: rejecting IPC peer speaking a different protocol version",
    );
    let record = crate::daemon::ipc::encode_reject_record(&format!(
        "protocol version mismatch: peer={peer_version}, daemon={PROTOCOL_VERSION}"
    ));
    // Bounded by the drain budget, not the handshake budget: we are already
    // hanging up, so a peer that will not read must not hold a worker.
    let _ = timeout(DRAIN_READ_TIMEOUT, stream.write_all(&record)).await;
    let _ = timeout(DRAIN_READ_TIMEOUT, stream.flush()).await;
    drain_then_close(stream).await;
}

/// Half-close, then discard whatever the peer already queued, before dropping
/// the connection (#1853).
///
/// On an AF_UNIX SOCK_STREAM socket, closing while bytes remain in our receive
/// queue makes the kernel raise `ECONNRESET` on the peer (Linux
/// `unix_release_sock`), which a client cannot distinguish from a daemon
/// crash — so it retries for its entire budget and then hard-fails with no
/// fallback. Draining first turns that into a clean EOF, which every existing
/// client already classifies as "daemon unavailable" and degrades on. That is
/// what makes this fix reach clients that were shipped long before it.
/// Unix only, deliberately. `ECONNRESET`-on-unread-data is an AF_UNIX /
/// SOCK_STREAM behavior; Windows named pipes have no RST concept, so there is
/// nothing to prevent there — and paying a shutdown/drain round trip on every
/// rejected connection measurably slows the Windows hot path.
async fn drain_then_close<S>(stream: &mut S)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        // No-op on Windows: named pipes cannot raise `ECONNRESET`, so
        // dropping the handle already yields the clean end-of-stream the
        // Unix path has to work for.
        return;
    }
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::timeout;
    // Half-close first so a peer still streaming a large body sees EOF and
    // stops writing, instead of us draining against a live producer.
    //
    // Every step here is bounded by DRAIN_READ_TIMEOUT, not the handshake
    // budget: this is a courtesy on a connection we have already decided to
    // drop, so it must never become a latency source. An earlier revision
    // used the 5s handshake timeout here and pushed
    // `dependency_failure_cancels_sibling_lint_children` past its 5s budget.
    let _ = timeout(DRAIN_READ_TIMEOUT, stream.shutdown()).await;
    let cap =
        u64::from(crate::daemon::protocol::MAX_BODY_BYTES) + CONTROL_FRAME_HEADER_BYTES as u64;
    let mut sink = [0_u8; 8192];
    let mut drained = 0_u64;
    while drained < cap {
        match timeout(DRAIN_READ_TIMEOUT, stream.read(&mut sink)).await {
            Ok(Ok(0)) | Ok(Err(_)) | Err(_) => break,
            Ok(Ok(n)) => drained += n as u64,
        }
    }
}
