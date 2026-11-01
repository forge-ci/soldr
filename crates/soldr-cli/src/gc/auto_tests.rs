//! Unit coverage split from `auto.rs` for the soldr#2493
//! 1,000-line production-source ceiling.

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
fn stale_target_age_is_disabled_by_default() {
    assert_eq!(
        stale_target_age_secs(&crate::core::AutoGcConfig::default()),
        None
    );
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

    run_stale_target_eviction(&paths, &config, &log);

    assert!(!target.exists(), "idle target should be evicted");
    assert!(workspace.join("Cargo.toml").exists(), "workspace untouched");
    assert!(stale_target_marker_path(&paths).exists());
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("stale-targets days=14"), "{text}");
}
