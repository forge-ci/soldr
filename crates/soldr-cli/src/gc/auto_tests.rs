//! Unit coverage split from `auto.rs` for the soldr#2493
//! 1,000-line production-source ceiling.

use super::schedule::AUTO_GC_THROTTLE_SECONDS;
use super::*;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Backdate `path` so the sweep sees it as stale without sleeping.
fn age(path: &std::path::Path, older_than_ttl_by: Duration) {
    let when = SystemTime::now() - Duration::from_millis(SCRATCH_TTL_MS as u64) - older_than_ttl_by;
    filetime::set_file_mtime(path, filetime::FileTime::from_system_time(when))
        .expect("backdate scratch entry");
}

#[test]
fn aggressive_cargo_gc_uses_cargo_accepted_duration_syntax() {
    let ages = crate::cache_lib::auto_gc::CargoGcAgeSeconds {
        max_src: 604_800,
        max_crate: 1_209_600,
        max_index: 0,
        max_git_co: 604_800,
        max_git_db: 0,
        max_download: 0,
    };
    let args = aggressive_cargo_gc_args(&ages);
    assert_eq!(args.max_src_age.as_deref(), Some("604800 seconds"));
    assert_eq!(args.max_crate_age.as_deref(), Some("1209600 seconds"));
    assert_eq!(args.max_git_co_age.as_deref(), Some("604800 seconds"));
}

#[test]
fn sweep_reclaims_stale_entries_and_keeps_fresh_ones() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().to_path_buf());
    let scratch = crate::core::ensure_temp_root_for(&paths);

    let stale_dir = scratch.join("stale-dir");
    std::fs::create_dir_all(&stale_dir).expect("stale dir");
    let stale_file = scratch.join("stale-file");
    std::fs::write(&stale_file, b"x").expect("stale file");
    let fresh = scratch.join("fresh-file");
    std::fs::write(&fresh, b"x").expect("fresh file");

    age(&stale_dir, Duration::from_secs(60));
    age(&stale_file, Duration::from_secs(60));

    let removed = sweep_stale_scratch(&paths, now_ms());

    assert_eq!(removed, 2, "both backdated entries must be reclaimed");
    assert!(!stale_dir.exists(), "stale directory must be removed");
    assert!(!stale_file.exists(), "stale file must be removed");
    assert!(
        fresh.exists(),
        "an entry inside the TTL must survive -- the sweep must never race \
             an in-flight download or a running test"
    );
}

#[test]
fn sweep_is_a_no_op_when_scratch_does_not_exist() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join("never-created"));
    assert_eq!(sweep_stale_scratch(&paths, now_ms()), 0);
}

#[test]
fn scratch_root_tracks_the_cache_volume() {
    // The reason scratch is pinned at all: temp -> cache renames are only
    // atomic while both live on one filesystem. It sits *beside* the cache
    // rather than inside it, which is precisely why this sweep has to
    // exist -- nothing that walks `<cache>/**` will ever reclaim it.
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().to_path_buf());
    let scratch = crate::core::temp_root_for(&paths);
    assert!(scratch.starts_with(&paths.root), "same volume as the cache");
    assert!(!scratch.starts_with(&paths.cache), "but outside the cache");
}

#[test]
fn offline_cook_gc_requires_and_releases_root_ownership() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join("owned"));
    let config = crate::core::CookConfig {
        max_total_gb: 1,
        ..crate::core::CookConfig::default()
    };

    let owner = crate::daemon::lifecycle::RootOwnershipGuard::try_acquire(&paths)
        .expect("acquire owner")
        .expect("root is initially unowned");
    assert!(
        run_offline_cook_gc(&paths, &config)
            .expect("offline cook probe")
            .is_none(),
        "the offline pass must not become a second state.sqlite3 owner"
    );
    drop(owner);
    assert!(
        run_offline_cook_gc(&paths, &config)
            .expect("offline cook pass")
            .is_some(),
        "the pass must resume after daemon ownership is released"
    );
}

#[test]
fn offline_event_prune_requires_and_releases_root_ownership() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = SoldrPaths::with_root(temp.path().join("owned"));
    let owner = crate::daemon::lifecycle::RootOwnershipGuard::try_acquire(&paths)
        .expect("acquire owner")
        .expect("root is initially unowned");
    assert_eq!(
        run_offline_daemon_event_prune(&paths, 0).expect("offline event probe"),
        None
    );
    drop(owner);
    assert_eq!(
        run_offline_daemon_event_prune(&paths, 0).expect("offline event prune"),
        Some(0)
    );
}

#[test]
fn sweeper_spawn_declares_the_soldr_identity_and_the_self_spawn_edge() {
    // A cargo-named multicall hardlink must not re-enter as `cargo gc`.
    let cmd = sweeper_command(std::path::PathBuf::from("/shims/v1/abc/cargo"));
    let args: Vec<_> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(args, ["gc", "auto-sweep"]);
    let env = |key: &str| {
        cmd.get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new(key))
            .and_then(|(_, v)| v.map(|v| v.to_string_lossy().into_owned()))
    };
    assert_eq!(
        env(crate::multicall::SHIM_ARGV0_ENV).as_deref(),
        Some("soldr")
    );
    assert_eq!(
        env(soldr_core::self_relocate::SELF_SPAWN_EDGE_ENV_VAR).as_deref(),
        Some("1")
    );
}

#[test]
fn stale_target_eviction_is_on_by_default_at_14_days() {
    let config = crate::core::AutoGcConfig::default();
    assert_eq!(config.stale_target_days, 14);
    assert!(config.dedupe_targets);
    assert_eq!(stale_target_age_secs(&config), Some(14 * 86_400));
    let off = crate::core::AutoGcConfig {
        stale_target_days: 0,
        ..crate::core::AutoGcConfig::default()
    };
    assert_eq!(stale_target_age_secs(&off), None);
}

#[test]
fn stale_target_defaults_survive_partial_config_toml() {
    let config: crate::core::SoldrConfig =
        toml::from_str("[auto_gc]\nmin_age_secs = 7200\n").unwrap();
    assert_eq!(config.auto_gc.stale_target_days, 14);
    assert!(config.auto_gc.dedupe_targets);
}

#[test]
fn default_gc_roots_cover_dev_and_agent_worktrees() {
    let home = std::path::Path::new("/home/u");
    assert_eq!(
        super::super::purge::default_gc_dev_roots(home),
        vec![home.join("dev"), home.join(".clud").join("worktrees")]
    );
}

#[test]
fn stale_target_pass_evicts_idle_target_under_default_clud_worktrees_root() {
    let home = tempfile::tempdir().expect("tempdir");
    let workspace = home.path().join(".clud").join("worktrees").join("agent-wt");
    let target = workspace.join("target");
    std::fs::create_dir_all(target.join("debug")).unwrap();
    std::fs::write(workspace.join("Cargo.toml"), b"[package]\nname = \"x\"\n").unwrap();
    let blob = std::fs::File::create(target.join("debug").join("dep.rlib")).unwrap();
    blob.set_len(300 * 1024 * 1024).unwrap();
    let old = SystemTime::now() - Duration::from_secs(30 * 86_400);
    for p in [
        target.join("debug").join("dep.rlib"),
        target.join("debug"),
        target.clone(),
    ] {
        filetime::set_file_mtime(&p, filetime::FileTime::from_system_time(old)).unwrap();
    }
    let root = home.path().join(".soldr");
    std::fs::create_dir_all(&root).unwrap();
    let roots = super::super::purge::default_gc_dev_roots(home.path());
    std::fs::write(
        root.join("config.toml"),
        format!(
            "[gc]\nallowlist_roots = [{}]\n",
            roots
                .iter()
                .map(|r| format!("{:?}", r.to_string_lossy().replace('\\', "/")))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    )
    .unwrap();
    let paths = SoldrPaths::with_root(root.clone());
    let log = root.join("auto_gc.log");

    run_stale_target_pass(&paths, &crate::core::AutoGcConfig::default(), &log);

    assert!(
        !target.exists(),
        "idle agent-worktree target should be evicted"
    );
    assert!(workspace.join("Cargo.toml").exists(), "workspace untouched");
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("stale-targets days=14"), "{text}");
    assert!(text.contains("dedupe-targets"), "{text}");
}

#[test]
fn stale_target_age_converts_days_and_honors_floor() {
    let config = crate::core::AutoGcConfig {
        stale_target_days: 7,
        ..crate::core::AutoGcConfig::default()
    };
    assert_eq!(stale_target_age_secs(&config), Some(7 * 86_400));
    let floored = crate::core::AutoGcConfig {
        stale_target_days: 1,
        min_age_secs: 3 * 86_400,
        ..crate::core::AutoGcConfig::default()
    };
    assert_eq!(stale_target_age_secs(&floored), Some(3 * 86_400));
}

#[test]
fn stale_target_pass_prunes_orphan_store_without_workspace_targets() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("soldr");
    let orphan = root.join("target-store/hard/unused");
    std::fs::create_dir_all(orphan.parent().unwrap()).unwrap();
    std::fs::write(&orphan, b"unused dedupe artifact").unwrap();
    let empty = tmp.path().join("empty-dev");
    std::fs::create_dir_all(&empty).unwrap();
    std::fs::write(
        root.join("config.toml"),
        format!("[gc]\nallowlist_roots = [{:?}]\n", empty.to_string_lossy()),
    )
    .unwrap();
    let paths = SoldrPaths::with_root(root.clone());
    run_stale_target_pass(
        &paths,
        &crate::core::AutoGcConfig::default(),
        &root.join("gc.log"),
    );
    if std::fs::metadata(&root)
        .ok()
        .and_then(|m| crate::platform::fs::identity::hardlink_identity(&m))
        .is_some()
    {
        assert!(!orphan.exists(), "no target references this store blob");
    }
}

#[test]
fn stale_target_pass_evicts_idle_discovered_target_once_per_day() {
    let home = tempfile::tempdir().expect("tempdir");
    let dev = home.path().join("dev");
    let workspace = dev.join("idle-worktree");
    let target = workspace.join("target");
    std::fs::create_dir_all(target.join("debug")).unwrap();
    std::fs::write(workspace.join("Cargo.toml"), b"[package]\nname = \"x\"\n").unwrap();
    let blob = std::fs::File::create(target.join("debug").join("dep.rlib")).unwrap();
    blob.set_len(300 * 1024 * 1024).unwrap();
    let old = SystemTime::now() - Duration::from_secs(30 * 86_400);
    for p in [
        target.join("debug").join("dep.rlib"),
        target.join("debug"),
        target.clone(),
    ] {
        filetime::set_file_mtime(&p, filetime::FileTime::from_system_time(old)).unwrap();
    }
    let root = home.path().join(".soldr");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("config.toml"),
        format!(
            "[gc]\nallowlist_roots = [{:?}]\n",
            dev.to_string_lossy().replace('\\', "/")
        ),
    )
    .unwrap();
    let paths = SoldrPaths::with_root(root.clone());
    let log = root.join("auto_gc.log");
    let config = crate::core::AutoGcConfig {
        stale_target_days: 14,
        ..crate::core::AutoGcConfig::default()
    };

    run_stale_target_pass(&paths, &config, &log);

    assert!(!target.exists(), "idle target should be evicted");
    assert!(workspace.join("Cargo.toml").exists(), "workspace untouched");
    assert!(stale_target_marker_path(&paths).exists());
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("stale-targets days=14"), "{text}");
}

#[test]
fn sweep_requests_share_a_cooldown_after_launch_failure() {
    let root = tempfile::tempdir().expect("temporary GC root");
    let marker = root.path().join("marker");
    let launches = std::cell::Cell::new(0);
    request_sweep(&marker, || {
        launches.set(launches.get() + 1);
        Err(std::io::Error::other("mock spawn failure"))
    });
    request_sweep(&marker, || {
        launches.set(launches.get() + 1);
        Ok(())
    });
    assert_eq!(launches.get(), 1);
    let expired = SystemTime::now() - Duration::from_secs(AUTO_GC_THROTTLE_SECONDS + 1);
    filetime::set_file_mtime(&marker, filetime::FileTime::from_system_time(expired))
        .expect("expire marker");
    request_sweep(&marker, || {
        launches.set(launches.get() + 1);
        Ok(())
    });
    assert_eq!(launches.get(), 2);
}

#[test]
fn sweep_requests_coalesce_while_worker_is_running() {
    let root = tempfile::tempdir().expect("temporary GC root");
    let marker = root.path().join("marker");
    let worker = sweep_lock(&marker).expect("reserve worker lock");
    request_sweep(&marker, || panic!("a running sweep owns the lock"));
    assert!(!marker.exists());
    drop(worker);

    let launched = std::cell::Cell::new(false);
    request_sweep(&marker, || {
        launched.set(true);
        Ok(())
    });
    assert!(launched.get());
}

#[test]
fn active_build_deferral_keeps_cooldown() {
    let root = tempfile::tempdir().expect("temporary GC root");
    let marker = root.path().join("marker");
    let log = root.path().join("logs/auto-gc.log");
    request_sweep(&marker, || {
        defer_for_active_build(&log, "mock");
        Ok(())
    });
    request_sweep(&marker, || panic!("deferral must preserve cooldown"));
    assert!(std::fs::read_to_string(log).unwrap().contains("stage=mock"));
}

#[test]
fn worker_admission_does_not_take_the_build_lease() {
    use crate::cache_lib::build_active::BuildRootLease;

    let root = tempfile::tempdir().expect("temporary GC root");
    let paths = SoldrPaths::with_root(root.path().to_path_buf());
    let marker = crate::cache_lib::auto_gc_throttle_marker_path(&paths);
    let worker = sweep_lock(&marker).expect("reserve worker lock");
    let build = BuildRootLease::acquire(&paths).expect("build can start during a sweep");
    let log = root.path().join("logs/auto-gc.log");
    assert!(try_target_maintenance(&paths, &log).is_none());
    assert!(std::fs::read_to_string(&log)
        .unwrap()
        .contains("build_active"));
    drop(build);
    assert!(try_target_maintenance(&paths, &log).is_some());
    drop(worker);
}

#[test]
fn long_sweep_starts_cooldown_when_it_finishes() {
    let root = tempfile::tempdir().expect("temporary GC root");
    let marker = root.path().join("marker");
    touch_auto_gc_marker(&marker).unwrap();
    run_reserved_sweep(&marker, || {
        let expired = SystemTime::now() - Duration::from_secs(AUTO_GC_THROTTLE_SECONDS + 1);
        filetime::set_file_mtime(&marker, filetime::FileTime::from_system_time(expired)).unwrap();
        request_sweep(&marker, || panic!("worker holds the lock"));
    });
    request_sweep(&marker, || panic!("completed sweep refreshed cooldown"));
}

#[test]
fn target_used_after_planning_is_not_evicted() {
    use crate::cache_lib::gc::GcOptions;
    use crate::cache_lib::target_registry::{current_unix_seconds, TargetRow};

    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("target");
    std::fs::create_dir(&target).unwrap();
    age(&target, Duration::from_secs(1));
    let options = GcOptions {
        older_than_seconds: 3600,
        larger_than_bytes: 0,
        dev_roots: vec![root.path().to_path_buf()],
        dry_run: false,
    };
    assert!(target_still_cold(&target, &options, || Ok(vec![])));
    assert!(!target_still_cold(&target, &options, || Ok(vec![
        TargetRow {
            path: target.clone(),
            last_used: current_unix_seconds().unwrap(),
        }
    ])));
    assert!(!target_still_cold(&target, &options, || Err(
        crate::core::SoldrError::Other("mock registry was unavailable".into())
    )));
}
