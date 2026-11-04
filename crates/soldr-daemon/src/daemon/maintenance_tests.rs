//! Unit tests for [`crate::daemon::maintenance`]: the pressure/full
//! maintenance loop, the cross-process sweep barrier, and the status
//! serialization round-trips. Lives in a sibling file referenced via
//! `#[path]` so `maintenance.rs` stays under the 1000-LOC ceiling.

use super::*;

/// Fixture directory shared between the sweep child process and the
/// probing parent — see [`sweep_never_holds_state_db_across_filesystem_work`].
pub(super) const FS_BARRIER_DIR_ENV: &str = "SOLDR_TEST_SWEEP_FS_BARRIER_DIR";
const FS_BARRIER_ROOT_ENV: &str = "SOLDR_TEST_SWEEP_FIXTURE_ROOT";
const SWEEP_CHILD_TEST: &str = "daemon::maintenance::tests::sweep_fs_phase_child_holds_the_barrier";

#[test]
fn pressure_uses_capacity_headroom_and_preserves_custom_absolute_config() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(tmp.path().to_path_buf());
    let gib = crate::cache_lib::auto_gc::GIB;
    assert!(root_below_pressure_threshold(
        &paths,
        Some(74 * gib),
        Some(2000 * gib)
    ));
    assert!(!root_below_pressure_threshold(&paths, Some(74 * gib), None));
    assert!(!root_below_pressure_threshold(
        &paths,
        None,
        Some(2000 * gib)
    ));
    std::fs::write(
        &paths.config_file,
        "[auto_gc]\ntrigger_free_gb = 50\ntarget_free_gb = 80\n",
    )
    .unwrap();
    assert!(!root_below_pressure_threshold(
        &paths,
        Some(74 * gib),
        Some(2000 * gib)
    ));
    std::fs::write(&paths.config_file, "[auto_gc]\nenabled = false\n").unwrap();
    assert!(!root_below_pressure_threshold(
        &paths,
        Some(gib),
        Some(2000 * gib)
    ));
}

// The child half of the cross-process regression test.
//
// Inert unless the parent set [`FS_BARRIER_ROOT_ENV`], so a normal
// suite run treats it as a no-op case.
#[test]
fn sweep_fs_phase_child_holds_the_barrier() {
    let Some(root) = std::env::var_os(FS_BARRIER_ROOT_ENV).map(PathBuf::from) else {
        return;
    };
    let paths = SoldrPaths::with_root(root);
    // The parent's config, not `SoldrConfig::default()`: the default
    // allowlist is `~/dev`, which no tempdir fixture can satisfy, so
    // `evaluate_safety_guards` would reject the seeded row and phase 2
    // would iterate an empty vector (soldr#2225).
    let config = paths.load_config().expect("fixture config");
    // Parks in `fs_phase_barrier` for as long as the parent needs, then
    // runs the real delete loop.
    let outcome = sweep_workspace_targets(&paths, &config, MaintenanceKind::Full);
    // Handed back out-of-band so the parent can assert the sweep really
    // entered its delete loop rather than no-opping past it.
    if let Some(dir) = std::env::var_os(FS_BARRIER_DIR_ENV).map(PathBuf::from) {
        std::fs::write(
            dir.join("sweep-outcome"),
            serde_json::to_vec(&outcome).expect("serialize sweep outcome"),
        )
        .expect("write sweep outcome");
    }
}

// soldr#2224 acceptance: the daemon's maintenance sweep must not hold
// `state.sqlite3` across its filesystem phase.
//
// A real child process parks at the filesystem-phase barrier while the
// parent verifies that an independent connection can write. This checks
// cross-process reachability at that phase. Since the SQLite migration,
// an open connection alone does not block another writer, so this test
// does not prove connection lifetime or replace transaction-lock tests.
#[test]
#[expect(clippy::too_many_lines, reason = "baseline, zackees/ci.yml#229")]
fn sweep_never_holds_state_db_across_filesystem_work() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let root = fixture.path().join("soldr-root");
    let barrier = fixture.path().join("barrier");
    std::fs::create_dir_all(&barrier).expect("barrier dir");
    let paths = SoldrPaths::with_root(root.clone());
    let db_path = crate::cache_lib::data_db_path(&paths);

    // A registered target the sweep will actually *delete*, not
    // merely snapshot. Three things have to line up or the row is
    // filtered out before phase 2 and the delete loop runs zero
    // times (soldr#2225):
    //
    //  1. cargo markers, or `delete_candidate_dir` refuses (#1671);
    //  2. both age signals past the 30-day `FULL_STALE_AGE`, because
    //     `effective_age_seconds` takes the *younger* of the registry
    //     row and the directory mtime (soldr#2134) — a freshly
    //     created dir reads as age 0 no matter what the row says;
    //  3. an allowlist root containing the workspace, since the
    //     default `~/dev` cannot contain a tempdir.
    let workspace = fixture.path().join("repo");
    let target = workspace.join("target");
    std::fs::create_dir_all(&target).expect("target dir");
    std::fs::write(target.join("CACHEDIR.TAG"), b"Signature: 8a477f597d28d172")
        .expect("cachedir tag");
    // A populated tree, so phase 2 is a real recursive deletion
    // rather than a single unlink.
    for bucket in 0..6 {
        let dir = target.join("deps").join(format!("b{bucket}"));
        std::fs::create_dir_all(&dir).expect("deps bucket");
        for file in 0..200 {
            std::fs::write(dir.join(format!("o{file}.o")), vec![0u8; 256]).expect("object file");
        }
    }

    std::fs::create_dir_all(&paths.root).expect("soldr root");
    let allowlist = workspace.display().to_string().replace('\\', "\\\\");
    std::fs::write(
        &paths.config_file,
        format!("[auto_gc]\nenabled = true\n[gc]\nallowlist_roots = [\"{allowlist}\"]\n"),
    )
    .expect("fixture config");

    let stale = SystemTime::now() - Duration::from_secs(90 * 24 * 60 * 60);
    filetime::set_file_mtime(&target, filetime::FileTime::from_system_time(stale))
        .expect("age the target dir");
    {
        let registry = TargetRegistry::open(&db_path).expect("seed registry");
        registry
            .upsert_with_time(&target, unix_millis(stale) / 1_000)
            .expect("seed row");
    }

    // Configured with `std::process::Command`, executed through
    // running-process: this crate's process-creation boundary is
    // enforced by the `ban_raw_process_creation` dylint, tests
    // included.
    let mut command = std::process::Command::new(std::env::current_exe().expect("test executable"));
    command
        .args(["--exact", SWEEP_CHILD_TEST, "--nocapture"])
        .env(FS_BARRIER_ROOT_ENV, &root)
        .env(FS_BARRIER_DIR_ENV, &barrier);
    let mut child = running_process::spawn(
        &mut command,
        running_process::SpawnStdio {
            stdin: running_process::StdioSource::Null,
            stdout: running_process::StdioSource::Parent,
            stderr: running_process::StdioSource::Parent,
            drain_timeout: Some(Duration::from_secs(5)),
            show_console: false,
        },
    )
    .expect("spawn sweep child");

    let sweeping = barrier.join("sweeping");
    let deadline = std::time::Instant::now() + Duration::from_secs(45);
    while !sweeping.exists() {
        assert!(
            child.try_wait().expect("poll sweep child").is_none(),
            "sweep child exited before reaching its filesystem phase"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "sweep child never reached its filesystem phase"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    // Exactly what `persist_start_fallback_inner` does on the front
    // door when the daemon is unreachable: one handle, three ops.
    let started = std::time::Instant::now();
    let result = (|| -> Result<(), String> {
        let handle = db::open_handle(&db_path).map_err(|e| e.to_string())?;
        let existing = db::get_build_in(&handle, 4242).map_err(|e| e.to_string())?;
        assert!(existing.is_none(), "fixture starts with no session row");
        db::upsert_build_in(
            &handle,
            &crate::daemon::protocol::BuildRecord {
                session_id: 4242,
                repo_root: "/repo".into(),
                started_at_ms: 1_000,
                ended_at_ms: None,
                exit_code: None,
                total_wall_ms: None,
                crate_count: 0,
                slowest_crate_us: None,
                slowest_crate_name: None,
                cache_summary: None,
                log_paths: None,
                miss_reasons: Vec::new(),
            },
        )
        .map_err(|e| e.to_string())?;
        db::append_event_in(
            &handle,
            &db::Event {
                ts_ms: 1_000,
                session_id: Some(4242),
                kind: db::EventKind::SessionStart,
                crate_name: None,
                duration_us: None,
                target_dir: None,
                exit_code: None,
            },
        )
        .map_err(|e| e.to_string())
    })();
    let elapsed = started.elapsed();

    std::fs::write(barrier.join("release"), b"").expect("release sweep child");
    let exit_code = child.wait().expect("wait for sweep child");

    result.unwrap_or_else(|error| {
        panic!(
            "the build-session fallback lost its record while the daemon was \
                 mid-maintenance-sweep: {error}. The sweep is holding state.sqlite3 \
                 across its filesystem phase again (soldr#2224)."
        )
    });
    assert!(
        elapsed < Duration::from_secs(2),
        "the fallback stalled {elapsed:?} waiting for the sweep's handle; it must \
             not wait at all (soldr#2224)"
    );
    assert_eq!(exit_code, 0, "sweep child failed");
    // The record really landed, not just "the open succeeded".
    let stored = db::get_build(&db_path, 4242).expect("read back");
    assert_eq!(stored.expect("record persisted").session_id, 4242);

    // ...and the sweep really deleted a populated tree while the
    // probe above was running. Without this the fixture could be
    // filtered out before phase 2 and the test would still pass,
    // proving only that no handle is held entering an empty loop.
    let outcome: ComponentOutcome = serde_json::from_slice(
        &std::fs::read(barrier.join("sweep-outcome")).expect("read sweep outcome"),
    )
    .expect("parse sweep outcome");
    assert_eq!(outcome.error, None, "sweep reported an error");
    assert_eq!(
        outcome.items_removed, 1,
        "the sweep must have entered its delete loop; the seeded target was \
             filtered out before phase 2 (soldr#2225)"
    );
    assert!(
        !target.exists(),
        "the stale target/ tree must be gone after the sweep"
    );
}

#[test]
fn schedule_has_five_minute_pressure_and_daily_catchup() {
    let day = Duration::from_secs(24 * 60 * 60);
    let base = UNIX_EPOCH + Duration::from_secs(10 * day.as_secs());
    assert_eq!(due_kind(None, None, base), Some(MaintenanceKind::Full));
    assert_eq!(
        due_kind(Some(base), None, base),
        Some(MaintenanceKind::Pressure)
    );
    assert_eq!(
        due_kind(
            Some(base),
            Some(base),
            base + PRESSURE_INTERVAL - Duration::from_secs(1)
        ),
        None
    );
    assert_eq!(
        due_kind(Some(base), Some(base), base + PRESSURE_INTERVAL),
        Some(MaintenanceKind::Pressure)
    );
    assert_eq!(
        due_kind(Some(base), Some(base), base + day),
        Some(MaintenanceKind::Full)
    );
}

#[test]
fn failed_full_attempt_is_backed_off_without_claiming_success() {
    let temp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(temp.path().join("owned"));
    let attempt = UNIX_EPOCH + Duration::from_secs(10 * 24 * 60 * 60);
    record_last_full_attempt(&paths, attempt).unwrap();
    assert_eq!(read_last_full(&paths), None);
    assert_eq!(read_last_full_attempt(&paths), Some(attempt));
    assert_eq!(
        due_kind(
            read_last_full_attempt(&paths),
            Some(attempt),
            attempt + PRESSURE_INTERVAL,
        ),
        Some(MaintenanceKind::Pressure),
    );
    assert_eq!(
        due_kind(
            read_last_full_attempt(&paths),
            Some(attempt),
            attempt + FULL_INTERVAL,
        ),
        Some(MaintenanceKind::Full),
    );
}

#[test]
fn maintenance_paths_are_distinct_for_prod_dev_custom_and_standalone() {
    let temp = tempfile::tempdir().unwrap();
    let prod = SoldrPaths::with_root(temp.path().join(".soldr"));
    let dev = SoldrPaths::with_root(temp.path().join(".soldr-dev"));
    let custom = SoldrPaths::with_root(temp.path().join("custom"));
    let standalone = temp.path().join(".zccache/sentinel");
    std::fs::create_dir_all(standalone.parent().unwrap()).unwrap();
    std::fs::write(&standalone, b"keep").unwrap();
    let paths = [status_path(&prod), status_path(&dev), status_path(&custom)];
    assert_ne!(paths[0], paths[1]);
    assert_ne!(paths[0], paths[2]);
    assert_ne!(paths[1], paths[2]);
    for path in paths {
        assert!(path.starts_with(temp.path()));
        assert!(!path.starts_with(temp.path().join(".zccache")));
    }
    assert_eq!(std::fs::read(&standalone).unwrap(), b"keep");
}

#[test]
fn daemon_maintenance_never_installs_an_os_scheduler() {
    let source = include_str!("maintenance.rs")
        .split("#[cfg(test)]")
        .next()
        .unwrap();
    for forbidden in ["schtasks", "systemctl", "launchctl", "Task Scheduler"] {
        assert!(
            !source.contains(forbidden),
            "daemon maintenance must not invoke/install {forbidden}"
        );
    }
}

#[test]
fn shutdown_waits_for_an_already_started_pass() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().join("owned"));
        std::fs::create_dir_all(&paths.root).unwrap();
        let shutdown = Arc::new(ShutdownSignal::default());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let completed = paths.root.join("pass-completed");
        let task_paths = paths.clone();
        let task_shutdown = Arc::clone(&shutdown);
        let task_release = Arc::clone(&release);
        let task_completed = completed.clone();
        let handle = tokio::spawn(async move {
            let mut started_tx = Some(started_tx);
            run_loop_inner(
                &task_paths,
                task_shutdown,
                Duration::from_secs(3600),
                move |_, _| {
                    let started = started_tx.take();
                    let release = Arc::clone(&task_release);
                    let completed = task_completed.clone();
                    async move {
                        if let Some(started) = started {
                            let _ = started.send(());
                        }
                        release.acquire().await.unwrap().forget();
                        std::fs::write(completed, b"done").unwrap();
                        true
                    }
                },
            )
            .await;
        });
        started_rx.await.unwrap();
        shutdown.request();
        tokio::task::yield_now().await;
        assert!(!handle.is_finished(), "active maintenance was cancelled");
        release.add_permits(1);
        handle.await.unwrap();
        assert!(completed.is_file());
    });
}

#[test]
fn deferred_full_pass_remains_due_on_next_pressure_tick() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().join("owned"));
        std::fs::create_dir_all(&paths.root).unwrap();
        let shutdown = Arc::new(ShutdownSignal::default());
        let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed_in_pass = Arc::clone(&observed);
        let shutdown_in_pass = Arc::clone(&shutdown);
        let marker_paths = paths.clone();

        run_loop_inner(
            &paths,
            Arc::clone(&shutdown),
            Duration::from_millis(10),
            move |kind, _| {
                let mut observed = observed_in_pass.lock().unwrap();
                observed.push(kind);
                let pass_started = observed.len() > 1;
                if pass_started {
                    assert_eq!(read_last_full_attempt(&marker_paths), None);
                    shutdown_in_pass.request();
                }
                async move { pass_started }
            },
        )
        .await;

        assert_eq!(
            *observed.lock().unwrap(),
            vec![MaintenanceKind::Full, MaintenanceKind::Full]
        );
        assert!(read_last_full_attempt(&paths).is_some());
    });
}

#[test]
fn acquired_failed_full_pass_is_backed_off_without_success_marker() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let temp = tempfile::tempdir().unwrap();
        let paths = SoldrPaths::with_root(temp.path().join("owned"));
        std::fs::create_dir_all(&paths.root).unwrap();
        let shutdown = Arc::new(ShutdownSignal::default());
        let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed_in_pass = Arc::clone(&observed);
        let shutdown_in_pass = Arc::clone(&shutdown);

        run_loop_inner(
            &paths,
            Arc::clone(&shutdown),
            Duration::from_millis(10),
            move |kind, _| {
                let mut observed = observed_in_pass.lock().unwrap();
                observed.push(kind);
                if observed.len() > 1 {
                    shutdown_in_pass.request();
                }
                // The pass acquired the maintenance lease, even though its
                // component status is modeled as failed/no success marker.
                async { true }
            },
        )
        .await;

        assert_eq!(
            *observed.lock().unwrap(),
            vec![MaintenanceKind::Full, MaintenanceKind::Pressure]
        );
        assert!(read_last_full_attempt(&paths).is_some());
        assert_eq!(read_last_full(&paths), None);
    });
}

#[test]
fn disabled_auto_gc_never_sweeps_registered_workspace_targets() {
    let temp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(temp.path().join("owned"));
    let workspace = temp.path().join("workspace");
    let target = workspace.join("target");
    std::fs::create_dir_all(&target).unwrap();
    std::fs::write(target.join("artifact"), b"keep").unwrap();
    std::fs::create_dir_all(&paths.root).unwrap();
    let allowlist = workspace.display().to_string().replace('\\', "\\\\");
    std::fs::write(
        &paths.config_file,
        format!("[auto_gc]\nenabled = false\n[gc]\nallowlist_roots = [\"{allowlist}\"]\n"),
    )
    .unwrap();
    let db_path = crate::cache_lib::data_db_path(&paths);
    {
        let registry = TargetRegistry::open(&db_path).unwrap();
        registry.upsert_with_time(&target, 0).unwrap();
    }

    let outcomes = run_local_components(
        &paths,
        &db_path,
        MaintenanceKind::Full,
        SystemTime::now(),
        true,
    );
    assert_eq!(outcomes.workspace_targets, ComponentOutcome::default());
    assert!(target.join("artifact").is_file());
    let registry = TargetRegistry::open(&db_path).unwrap();
    assert_eq!(registry.list().unwrap().len(), 1);
}

#[test]
fn invalid_config_does_not_disable_independent_collectors() {
    let temp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(temp.path().join("owned"));
    std::fs::create_dir_all(paths.root.join("trash-C/item")).unwrap();
    std::fs::write(paths.root.join("trash-C/item/stale"), b"delete").unwrap();
    std::fs::write(&paths.config_file, "[auto_gc\nenabled = true").unwrap();
    let db_path = crate::cache_lib::data_db_path(&paths);
    TargetRegistry::open(&db_path).unwrap();
    db::ensure_initialized(&db_path).unwrap();

    let outcomes = run_local_components(
        &paths,
        &db_path,
        MaintenanceKind::Full,
        SystemTime::now(),
        true,
    );
    assert!(outcomes
        .cook
        .error
        .as_deref()
        .unwrap()
        .contains("invalid_config"));
    assert!(outcomes
        .workspace_targets
        .error
        .as_deref()
        .unwrap()
        .contains("invalid_config"));
    assert_eq!(outcomes.trash.items_removed, 1);
    assert!(!paths.root.join("trash-C/item").exists());
    assert!(outcomes.history.error.is_none());
    assert!(outcomes.pep517_targets.error.is_none());
    assert!(outcomes.legacy_zccache.error.is_none());
}

fn disk_report(usage_after_bytes: u64) -> EmbeddedDiskMaintenanceReport {
    EmbeddedDiskMaintenanceReport {
        kind: "full".into(),
        pressure: "none".into(),
        budget_bytes: 100 << 30,
        usage_before_bytes: usage_after_bytes,
        usage_after_bytes,
        bytes_reclaimed: 0,
        artifacts_removed: 0,
        expired_artifacts_removed: 0,
        pending_write_bytes: 0,
    }
}

fn status_attempted_at(attempted_at_ms: i64) -> MaintenanceStatus {
    MaintenanceStatus {
        schema_version: STATUS_SCHEMA_VERSION,
        owning_root: "/root".into(),
        daemon_identity: "soldr:embedded-v1".into(),
        embedded_cache_root: "/root/cache/zccache/daemon-state/embedded-v1".into(),
        disk_policy: EmbeddedDiskPolicy {
            source: "dynamic_5_percent_clamped_40_200_gib".into(),
            max_cache_bytes: None,
            max_cache_percent: None,
        },
        attempted_at_ms,
        successful_at_ms: None,
        last_full_at_ms: None,
        kind: MaintenanceKind::Full,
        deferred_reason: None,
        filesystem_capacity_bytes: None,
        filesystem_free_bytes: None,
        filesystem_free_percent: None,
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

/// soldr#3329: a status file written before the retired-store and starvation
/// fields existed still deserializes.
#[test]
fn old_status_json_without_retired_and_starvation_fields_deserializes() {
    let mut legacy = serde_json::to_value(completed_pass(100, 1 << 30)).expect("encode");
    let object = legacy.as_object_mut().expect("status object");
    for field in [
        "retired_store_bytes",
        "retired_store_count",
        "consecutive_full_deferrals",
    ] {
        assert!(object.remove(field).is_some(), "{field} must be serialized");
    }
    let decoded: MaintenanceStatus = serde_json::from_value(legacy).expect("old status decodes");
    assert_eq!(decoded.retired_store_bytes, None);
    assert_eq!(decoded.retired_store_count, None);
    assert_eq!(decoded.consecutive_full_deferrals, 0);
}

fn completed_pass(attempted_at_ms: i64, usage_after_bytes: u64) -> MaintenanceStatus {
    let mut status = status_attempted_at(attempted_at_ms);
    status.successful_at_ms = Some(attempted_at_ms);
    status.zccache = Some(disk_report(usage_after_bytes));
    status.zccache_measured_at_ms = Some(attempted_at_ms);
    status
}

fn deferred_pass(attempted_at_ms: i64) -> MaintenanceStatus {
    let mut status = status_attempted_at(attempted_at_ms);
    status.deferred_reason = Some("build_active".into());
    status
}

/// soldr#3077: a pass deferred by an active build must not erase the store
/// size the last completed pass measured.
#[test]
fn a_deferred_pass_keeps_the_last_measured_zccache_usage() {
    let previous = completed_pass(100, 64 << 30);
    let mut status = deferred_pass(200);

    carry_forward_zccache_measurement(Some(&previous), &mut status);

    assert_eq!(status.zccache, Some(disk_report(64 << 30)));
    assert_eq!(status.zccache_measured_at_ms, Some(100));
    // Still reported as the deferred attempt it was.
    assert_eq!(status.successful_at_ms, None);
    assert_eq!(status.deferred_reason.as_deref(), Some("build_active"));
    assert_eq!(status.attempted_at_ms, 200);
}

#[test]
fn a_fresh_zccache_measurement_is_never_replaced_by_an_older_one() {
    let previous = completed_pass(100, 64 << 30);
    let mut status = completed_pass(200, 1 << 30);

    carry_forward_zccache_measurement(Some(&previous), &mut status);

    assert_eq!(status.zccache, Some(disk_report(1 << 30)));
    assert_eq!(status.zccache_measured_at_ms, Some(200));
}

#[test]
fn a_failed_zccache_measurement_is_not_hidden_behind_a_stale_report() {
    let previous = completed_pass(100, 64 << 30);
    let mut status = status_attempted_at(200);
    status.zccache_error = Some("store unavailable".into());

    carry_forward_zccache_measurement(Some(&previous), &mut status);

    assert_eq!(status.zccache, None);
    assert_eq!(status.zccache_measured_at_ms, None);
}

/// The whole path `soldr status` reads: a status file written before the
/// measured-at field existed, then a deferred pass persisted over it.
#[test]
fn a_deferred_pass_persisted_over_an_older_status_file_keeps_its_usage() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().to_path_buf());
    let mut legacy = serde_json::to_value(completed_pass(100, 64 << 30)).expect("encode");
    legacy
        .as_object_mut()
        .expect("status object")
        .remove("zccache_measured_at_ms");
    std::fs::create_dir_all(status_path(&paths).parent().expect("parent")).expect("status dir");
    std::fs::write(
        status_path(&paths),
        serde_json::to_vec(&legacy).expect("json"),
    )
    .expect("legacy status");

    let written = persist_status(&paths, deferred_pass(200)).expect("persist");
    let read_back = read_status(&paths).expect("status file");

    assert_eq!(written, read_back);
    assert_eq!(read_back.zccache, Some(disk_report(64 << 30)));
    // An older file has no measured-at time, so the pass that measured it
    // stands in for it.
    assert_eq!(read_back.zccache_measured_at_ms, Some(100));
    assert_eq!(read_back.deferred_reason.as_deref(), Some("build_active"));
}

/// soldr#3079: the daemon's own reclamation ages are the soldr-owned
/// staleness gate, not separate four-day copies of it. Workspace targets and
/// build history are swept with these constants directly, so a gate changed
/// only in the policy table would never reach them.
#[test]
fn daemon_reclaim_ages_are_the_shared_72h_staleness_gate() {
    let gate = crate::cache_lib::gc_policy::STALENESS_GATE;
    assert_eq!(gate, Duration::from_secs(72 * 60 * 60));
    assert_eq!(PRESSURE_STALE_AGE, gate, "workspace targets / legacy roots");
    assert_eq!(history_gc::DEFAULT_MAX_AGE, gate, "build history");
    assert_eq!(
        crate::cache_lib::pep517_gc::PRESSURE_MAX_AGE,
        gate,
        "pep517 targets and wheels"
    );
}

/// soldr#3251: under a build, the store pass is throttled, but a store the last
/// measurement put over its budget is maintained on every tick.
#[test]
fn store_pass_under_a_build_is_throttled_unless_the_store_is_over_budget() {
    let now = UNIX_EPOCH + Duration::from_secs(10 * 24 * 60 * 60);
    let now_ms = unix_millis(now);
    let minute_ago = now_ms - 60_000;
    // `disk_report` budgets 100 GiB.
    assert!(store_pass_due(None, now), "no status yet");
    assert!(
        store_pass_due(Some(&deferred_pass(minute_ago)), now),
        "never measured"
    );
    assert!(
        !store_pass_due(Some(&completed_pass(minute_ago, 1 << 30)), now),
        "measured a minute ago and under budget"
    );
    assert!(
        store_pass_due(
            Some(&completed_pass(
                now_ms - STORE_PASS_INTERVAL.as_millis() as i64,
                1 << 30
            )),
            now
        ),
        "measured a full interval ago"
    );
    assert!(
        store_pass_due(Some(&completed_pass(minute_ago, 216 << 30)), now),
        "over budget, however recent the measurement"
    );
}

/// soldr#3251: a daemon of one zccache version must not mark another version's
/// store as maintained.
#[test]
fn full_pass_markers_are_keyed_by_the_embedded_zccache_version() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join("owned"));
    let now = UNIX_EPOCH + Duration::from_secs(10 * 24 * 60 * 60);
    let stamp = format!("{}\n", unix_millis(now));
    // Markers left by the root-wide schedule, and by a daemon of another version.
    let shared = maintenance_dir(&paths);
    let other_version = shared.join("v0.0.1");
    std::fs::create_dir_all(&other_version).expect("marker dirs");
    for dir in [&shared, &other_version] {
        std::fs::write(dir.join(FULL_MARKER), &stamp).expect("full marker");
        std::fs::write(dir.join(FULL_ATTEMPT_MARKER), &stamp).expect("attempt marker");
    }

    assert_eq!(read_last_full(&paths), None);
    assert_eq!(read_last_full_attempt(&paths), None);

    record_last_full(&paths, now).expect("record full");
    record_last_full_attempt(&paths, now).expect("record attempt");
    assert_eq!(read_last_full(&paths), Some(now));
    assert_eq!(read_last_full_attempt(&paths), Some(now));
    let own = shared.join(zccache::core::config::versioned_subdir());
    assert!(full_marker_path(&paths).starts_with(&own));
    assert!(full_attempt_marker_path(&paths).starts_with(&own));
}

fn store_test_daemon_identity() -> running_process::broker::backend_handle::DaemonProcess {
    let endpoint = running_process::broker::protocol::Endpoint {
        namespace_id: "shared".into(),
        path: "soldr-maintenance-store-pass".into(),
    };
    running_process::broker::backend_handle::DaemonProcess::current_process(endpoint, Some(30))
        .expect("current-process identity")
}

/// Age every entry under `root`, directories included, so the retired-store
/// sweep's newest-mtime gate sees the whole tree as old.
fn age_tree(root: &Path, age: Duration) {
    let old = filetime::FileTime::from_system_time(SystemTime::now() - age);
    let mut entries = vec![root.to_path_buf()];
    let mut index = 0;
    while index < entries.len() {
        if entries[index].is_dir() {
            for entry in std::fs::read_dir(&entries[index]).expect("read tree") {
                entries.push(entry.expect("entry").path());
            }
        }
        index += 1;
    }
    // Deepest first, so aging a child does not refresh its parent afterwards.
    for path in entries.iter().rev() {
        filetime::set_file_mtime(path, old).expect("age entry");
    }
}

/// soldr#3251: the lease-gated pass deferred the whole pass, store included,
/// whenever any build held the root lease, so on a busy host the store was
/// never maintained. With a real embedded service and a build holding the
/// lease, the store pass and the retired-store sweep must now still run, while
/// soldr's own collectors stay deferred.
#[test]
fn a_build_holding_the_root_lease_no_longer_starves_the_store_pass() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("owned"));
        std::fs::create_dir_all(&paths.root).expect("root");
        let service = Arc::new(
            SoldrZccacheService::start_with_max_cache_bytes(
                &paths,
                &store_test_daemon_identity(),
                FORCED_PRESSURE_BUDGET_BYTES,
            )
            .await
            .expect("start embedded zccache service"),
        );
        let retired = crate::zccache_embedded::embedded_cache_root(&paths).join("v0.0.1");
        std::fs::create_dir_all(&retired).expect("retired store");
        std::fs::write(retired.join("artifact"), b"payload").expect("retired artifact");
        age_tree(
            &retired,
            30 * 24 * 60 * 60 * Duration::from_secs(1) + Duration::from_secs(60),
        );
        let context = MaintenanceContext {
            paths: paths.clone(),
            db_path: crate::cache_lib::data_db_path(&paths),
            compile_service: Arc::clone(&service),
            shutdown: Arc::new(ShutdownSignal::default()),
        };
        let build = crate::cache_lib::build_active::BuildActivityLease::acquire(&paths, 7)
            .expect("build lease");

        let status = run_once(&context, MaintenanceKind::Pressure, SystemTime::now()).await;

        assert_eq!(status.deferred_reason.as_deref(), Some("build_active"));
        assert_eq!(
            status.successful_at_ms, None,
            "local collectors stay deferred"
        );
        assert!(
            status.zccache.is_some() && status.zccache_measured_at_ms.is_some(),
            "the store pass must run while a build holds the lease: {status:?}"
        );
        assert!(
            retired_store_reclaimed(&status),
            "the retired store must be reclaimed while a build holds the lease: {status:?}"
        );
        assert!(!retired.exists());

        // The live service holds the writer lock under the name the retired-store
        // sweep checks, so an upstream rename cannot silently disarm that check.
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(
                crate::zccache_embedded::embedded_version_root(&paths)
                    .join(crate::zccache_embedded::ZCCACHE_WRITER_LOCK_FILE),
            )
            .expect("live service writer lock");
        assert!(
            fs2::FileExt::try_lock_exclusive(&lock).is_err(),
            "a running service must hold its writer lock"
        );

        drop(lock);
        drop(build);
        drop(context);
        if let Ok(service) = Arc::try_unwrap(service) {
            service
                .shutdown(zccache::embedded::ShutdownMode::Graceful)
                .await
                .expect("shutdown");
        }
    });
}

/// soldr#3365 RED: this is the real 5-minute daemon pressure tick's planning
/// path (`daemon_policy_actions` + `legacy_zccache_outcome`), not just the
/// underlying sweep. It must expire the host-shaped retired store per file,
/// link-count aware, the same way the direct
/// `zccache_embedded::retired_store_tests` suite requires: `nlink == 1`
/// files are purged eagerly, `nlink > 1` files are age-gated and only the
/// cache's own link is removed, and `bytes_reclaimed` counts only the former.
#[test]
fn pressure_tick_expires_the_host_shaped_retired_store() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join("owned"));
    let now = SystemTime::now();
    let fixture = crate::zccache_embedded::retired_store_tests::host_shaped_stores(&paths, now);

    let actions = daemon_policy_actions(
        crate::core::SoldrConfig::default(),
        MaintenanceKind::Pressure,
    );
    let outcome = legacy_zccache_outcome(&paths, MaintenanceKind::Pressure, now, &actions)
        .expect("pressure tick plans the legacy sweep");

    assert!(
        outcome.bytes_reclaimed >= fixture.unlinked_artifact_bytes,
        "expected at least {} bytes reclaimed (the unlinked/nlink==1 artifacts), got {}: \
         outcome={outcome:?}",
        fixture.unlinked_artifact_bytes,
        outcome.bytes_reclaimed,
    );

    let unlinked_survivors: Vec<&PathBuf> = fixture
        .cold_unlinked
        .iter()
        .chain(fixture.fresh_unlinked.iter())
        .filter(|path| path.exists())
        .collect();
    assert!(
        unlinked_survivors.is_empty(),
        "{} unlinked artifacts survived the pressure tick, e.g. {:?}; outcome={outcome:?}",
        unlinked_survivors.len(),
        unlinked_survivors.iter().take(5).collect::<Vec<_>>(),
    );

    let cold_linked_cache_survivors: Vec<&PathBuf> = fixture
        .cold_linked
        .iter()
        .filter(|path| path.exists())
        .collect();
    assert!(
        cold_linked_cache_survivors.is_empty(),
        "{} cold linked artifacts survived in the cache after the pressure tick (expected the \
         cache's own link removed once older than the pressure gate): {:?}; outcome={outcome:?}",
        cold_linked_cache_survivors.len(),
        cold_linked_cache_survivors
            .iter()
            .take(5)
            .collect::<Vec<_>>(),
    );

    for file in fixture
        .cold_linked_targets
        .iter()
        .chain(fixture.warm_linked.iter())
        .chain(fixture.warm_linked_targets.iter())
        .chain(fixture.current_files.iter())
    {
        assert!(
            file.exists(),
            "must survive the pressure tick: {}; outcome={outcome:?}",
            file.display()
        );
    }
}

/// soldr#3329: the status reports the retired stores beside the live one, so
/// `usage_before_bytes` next to a huge retired store is never read alone.
#[test]
fn status_reports_retired_store_bytes() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("owned"));
        std::fs::create_dir_all(&paths.root).expect("root");
        let service = Arc::new(
            SoldrZccacheService::start(&paths, &store_test_daemon_identity())
                .await
                .expect("start embedded zccache service"),
        );
        let retired = crate::zccache_embedded::embedded_cache_root(&paths).join("v0.0.1");
        std::fs::create_dir_all(&retired).expect("retired store");
        std::fs::write(retired.join("artifact"), vec![0u8; 4321]).expect("retired artifact");
        let retired_route = paths
            .cache
            .join("zccache/daemon-state/embedded-v1-soldr-daemon-retired")
            .join(zccache::core::config::versioned_subdir());
        std::fs::create_dir_all(&retired_route).expect("retired route");
        std::fs::write(retired_route.join("artifact"), vec![0u8; 1234])
            .expect("retired route artifact");
        let context = MaintenanceContext {
            paths: paths.clone(),
            db_path: crate::cache_lib::data_db_path(&paths),
            compile_service: Arc::clone(&service),
            shutdown: Arc::new(ShutdownSignal::default()),
        };

        use fs2::FileExt as _;
        let writer = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(retired_route.join(crate::zccache_embedded::ZCCACHE_WRITER_LOCK_FILE))
            .expect("retired route writer lock");
        writer.try_lock_exclusive().expect("hold live sibling lock");
        let mut while_live =
            MaintenanceStatus::new(&context, MaintenanceKind::Pressure, SystemTime::now());
        super::retired_status::measure_retired(&context, &mut while_live);
        assert_eq!(while_live.retired_store_bytes, Some(4321));
        assert_eq!(while_live.retired_store_count, Some(1));
        drop(writer);

        let status = run_once(&context, MaintenanceKind::Pressure, SystemTime::now()).await;

        assert_eq!(status.retired_store_bytes, Some(5555), "{status:?}");
        assert_eq!(status.retired_store_count, Some(2), "{status:?}");

        drop(context);
        if let Ok(service) = Arc::try_unwrap(service) {
            service
                .shutdown(zccache::embedded::ShutdownMode::Graceful)
                .await
                .expect("shutdown");
        }
    });
}

/// soldr#3329: on a host that is always building, a full pass deferred
/// `FULL_STARVATION_DEFERRALS` times in a row still runs its build-safe work.
#[test]
fn deferred_full_pass_escapes_starvation_after_threshold() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = SoldrPaths::with_root(temp.path().join("owned"));
        std::fs::create_dir_all(&paths.root).expect("root");
        let service = Arc::new(
            SoldrZccacheService::start_with_max_cache_bytes(
                &paths,
                &store_test_daemon_identity(),
                FORCED_PRESSURE_BUDGET_BYTES,
            )
            .await
            .expect("start embedded zccache service"),
        );
        let context = MaintenanceContext {
            paths: paths.clone(),
            db_path: crate::cache_lib::data_db_path(&paths),
            compile_service: Arc::clone(&service),
            shutdown: Arc::new(ShutdownSignal::default()),
        };
        let build = crate::cache_lib::build_active::BuildActivityLease::acquire(&paths, 7)
            .expect("build lease");

        for expected in 1..FULL_STARVATION_DEFERRALS {
            let status = run_once(&context, MaintenanceKind::Full, SystemTime::now()).await;
            assert_eq!(status.deferred_reason.as_deref(), Some("build_active"));
            assert_eq!(status.consecutive_full_deferrals, expected, "{status:?}");
            persist_status(&paths, status).expect("persist");
        }

        // A retired store with no writer lock: its service is dead. Created only
        // now so the earlier throttled store passes cannot have swept it.
        let retired = crate::zccache_embedded::embedded_cache_root(&paths).join("v0.0.1");
        std::fs::create_dir_all(&retired).expect("retired store");
        std::fs::write(retired.join("artifact"), b"payload").expect("retired artifact");
        age_tree(
            &retired,
            30 * 24 * 60 * 60 * Duration::from_secs(1) + Duration::from_secs(60),
        );

        let status = run_once(&context, MaintenanceKind::Full, SystemTime::now()).await;

        assert_eq!(
            status.deferred_reason.as_deref(),
            Some("build_active_starvation_escape"),
            "{status:?}"
        );
        assert_eq!(status.successful_at_ms, None, "collectors stay deferred");
        assert!(status.zccache_measured_at_ms.is_some(), "{status:?}");
        assert!(retired_store_reclaimed(&status), "{status:?}");
        assert!(!retired.exists());

        drop(build);
        drop(context);
        if let Ok(service) = Arc::try_unwrap(service) {
            service
                .shutdown(zccache::embedded::ShutdownMode::Graceful)
                .await
                .expect("shutdown");
        }
    });
}

/// soldr#3669: an artifact budget below the 7-byte retired store, so zccache
/// never classifies the store pass as pressure-free, whatever the host disk.
/// The label is still host-shaped (soft from the budget alone, hard when the
/// host is also below 5% free), but in both cases the store pass reclaims the
/// retired store itself. Without the budget the outcome flipped with how full
/// the machine running the tests was.
const FORCED_PRESSURE_BUDGET_BYTES: u64 = 1;

/// Under the forced budget pressure, the embedded store's own pass runs first
/// and reclaims the 7-byte retired store, leaving soldr's legacy sweep nothing.
fn retired_store_reclaimed(status: &MaintenanceStatus) -> bool {
    let report = status.zccache.as_ref().expect("store pass report");
    assert_ne!(
        report.pressure, "none",
        "forced budget must give store pressure: {status:?}"
    );
    assert_eq!(
        status.legacy_zccache.items_removed, 0,
        "the store pass reclaims the retired store before the legacy sweep: {status:?}"
    );
    report.bytes_reclaimed >= 7
}
