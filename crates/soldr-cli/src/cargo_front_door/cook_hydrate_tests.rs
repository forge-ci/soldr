//! Unit tests for [`super`] (`cook_hydrate.rs`), in a sibling file so that file
//! stays under the 1000-line production ceiling.

use super::*;
use crate::platform::sync::Mutex;
use tempfile::TempDir;

// Env-mutating tests are serialized — `auto_hydrate_enabled`
// reads `SOLDR_COOK_AUTO_HYDRATE` from the process environment.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// soldr#3694: `soldr cargo run -- --target foo` passes `--target foo`
/// to the program; cook hydrate must not scope itself to target `foo`.
#[test]
fn target_after_double_dash_is_not_a_cargo_target() {
    let args: Vec<String> = ["run", "--", "--target", "foo"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let temp = TempDir::new().unwrap();
    assert_ne!(
        resolve_target_triple(temp.path(), &args).as_deref(),
        Some("foo")
    );
    let _guard = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_ne!(explicit_target_scope(&args).as_deref(), Some("foo"));
}

/// soldr#3381: a failing `rustc -V` must not silently turn a cook hit
/// into a miss — the warning carries the fake rustc's stderr.
#[test]
fn hydrate_rustc_version_failure_warns_with_stderr() {
    let temp = TempDir::new().unwrap();
    let rustc = tool_output::write_fake_tool(temp.path(), "rustc", "", "MARKER_HYDRATE_3381", 1);
    let log = temp.path().join("small-tools.jsonl");
    let mut warn = Vec::new();
    let version = rustc_version_for_hydrate_with(&rustc, &mut warn, Some(log.clone()));
    assert!(version.is_none());
    let warn = String::from_utf8(warn).unwrap();
    assert!(warn.contains("cook hydrate skipped"), "{warn}");
    assert!(warn.contains("MARKER_HYDRATE_3381"), "{warn}");
    let logged = std::fs::read_to_string(&log).unwrap();
    assert!(logged.contains("MARKER_HYDRATE_3381"), "{logged}");
}

/// soldr#3389 contract: a successful probe still forwards and logs stderr.
#[test]
fn hydrate_rustc_version_success_forwards_and_logs_stderr() {
    let temp = TempDir::new().unwrap();
    let rustc =
        tool_output::write_fake_tool(temp.path(), "rustc", "rustc 9.9.9", "MARKER_HYDRATE_OK", 0);
    let log = temp.path().join("small-tools.jsonl");
    let mut warn = Vec::new();
    let version = rustc_version_for_hydrate_with(&rustc, &mut warn, Some(log.clone()));
    assert_eq!(version.as_deref(), Some("rustc 9.9.9"));
    let warn = String::from_utf8(warn).unwrap();
    assert!(warn.contains("-V: MARKER_HYDRATE_OK"), "{warn}");
    let logged = std::fs::read_to_string(&log).unwrap();
    assert!(logged.contains("MARKER_HYDRATE_OK"), "{logged}");
}

fn clear_env() {
    std::env::remove_var(SOLDR_COOK_AUTO_HYDRATE_ENV);
}

/// The env state every `resolve_target_dir` assertion below depends on:
/// no `CARGO_TARGET_DIR` and no cook target scope, so the expected root is
/// the tempdir's own `target/`. Not hypothetical — the documented Linux
/// dev loop (`ci/perf_local.py`) exports `CARGO_TARGET_DIR=/target` into
/// the runner container, and an inherited value moves the resolved root
/// out of the tempdir and fails these tests for reasons unrelated to the
/// behaviour they pin. Callers must already hold [`ENV_LOCK`].
fn clear_target_dir_env() {
    std::env::remove_var("CARGO_TARGET_DIR");
    std::env::remove_var(SOLDR_COOK_HYDRATE_TARGET_ENV);
}

#[test]
fn env_var_disables_overriding_everything() {
    let _g = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.94.1\"\n[soldr.cook]\nauto_hydrate = true\n",
    )
    .unwrap();
    std::env::set_var(SOLDR_COOK_AUTO_HYDRATE_ENV, "0");
    let cfg = CookConfig {
        auto_hydrate: true,
        ..CookConfig::default()
    };
    assert!(!auto_hydrate_enabled(dir.path(), &cfg));
    clear_env();
}

#[test]
fn env_var_enables_overriding_project_disable() {
    let _g = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.94.1\"\n[soldr.cook]\nauto_hydrate = false\n",
    )
    .unwrap();
    std::env::set_var(SOLDR_COOK_AUTO_HYDRATE_ENV, "1");
    let cfg = CookConfig {
        auto_hydrate: false,
        ..CookConfig::default()
    };
    assert!(auto_hydrate_enabled(dir.path(), &cfg));
    clear_env();
}

#[test]
fn rust_toolchain_disable_beats_global_enable() {
    let _g = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_env();
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("rust-toolchain.toml"),
        "[toolchain]\nchannel = \"1.94.1\"\n[soldr.cook]\nauto_hydrate = false\n",
    )
    .unwrap();
    let cfg = CookConfig {
        auto_hydrate: true,
        ..CookConfig::default()
    };
    assert!(!auto_hydrate_enabled(dir.path(), &cfg));
}

#[test]
fn no_overrides_means_global_default_wins() {
    let _g = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_env();
    let dir = TempDir::new().unwrap();
    let cfg = CookConfig::default();
    assert!(cfg.auto_hydrate, "global default must be ON");
    assert!(auto_hydrate_enabled(dir.path(), &cfg));
}

#[test]
fn profile_resolution_matches_cargo_semantics() {
    assert_eq!(resolve_profile_name(&[]), "dev");
    assert_eq!(
        resolve_profile_name(&["build".into(), "--release".into()]),
        "release"
    );
    assert_eq!(
        resolve_profile_name(&["build".into(), "--profile".into(), "ci".into()]),
        "ci"
    );
    assert_eq!(
        resolve_profile_name(&["build".into(), "--profile=ci".into()]),
        "ci"
    );
    // soldr#4108: with no flag, the subcommand picks the profile.
    for (subcommand, profile) in [
        ("build", "dev"),
        ("check", "dev"),
        ("test", "test"),
        ("t", "test"),
        ("bench", "bench"),
    ] {
        assert_eq!(resolve_profile_name(&[subcommand.into()]), profile);
    }
    // An explicit choice beats the subcommand.
    assert_eq!(
        resolve_profile_name(&["test".into(), "--release".into()]),
        "release"
    );
    assert_eq!(
        resolve_profile_name(&["bench".into(), "--profile".into(), "ci".into()]),
        "ci"
    );

    // Cargo's profile -> target/ directory table.
    assert_eq!(profile_dir_name("dev"), "debug");
    assert_eq!(profile_dir_name("test"), "debug");
    assert_eq!(profile_dir_name("release"), "release");
    assert_eq!(profile_dir_name("bench"), "release");
    assert_eq!(profile_dir_name("ci"), "ci");

    // `cargo test` still lands in target/debug and still looks up `dev`.
    let test_profile = resolve_profile_name(&["test".into()]);
    assert_eq!(profile_dir_name(&test_profile), "debug");
    assert_eq!(cook_index_profile_name(&test_profile), "dev");
    // `cargo bench` moves to target/release and looks up `release`.
    let bench_profile = resolve_profile_name(&["bench".into()]);
    assert_eq!(profile_dir_name(&bench_profile), "release");
    assert_eq!(cook_index_profile_name(&bench_profile), "release");
    assert_eq!(cook_index_profile_name("ci"), "ci");
}

#[test]
fn cook_cost_gate_restores_when_estimated_restore_is_cheaper() {
    // 500 MiB at the 16 MiB/s floor is ~31.3 s, against a two-minute compile.
    let archive: u64 = 500 * 1024 * 1024;
    let expected = archive.div_ceil(COOK_RESTORE_FLOOR_BYTES_PER_MS);
    assert!((31_000..32_000).contains(&expected), "{expected}");
    assert_eq!(
        decide_cook_restore(archive, 120_000),
        CookRestoreDecision::Restore {
            estimated_transport_ms: expected
        }
    );
}

#[test]
fn cook_cost_gate_is_the_ci_shape_that_used_to_be_declined() {
    // soldr#3117: the CI stable tree -- 187 MiB archive, a compile the
    // compile cache served in 95 s, and a pack that took minutes. The old
    // estimate charged the pack twice over and declined; the restore
    // estimate is about 12 s and must win.
    let archive: u64 = 187 * 1024 * 1024;
    assert!(matches!(
        decide_cook_restore(archive, 95_000),
        CookRestoreDecision::Restore {
            estimated_transport_ms
        } if estimated_transport_ms < 15_000
    ));
}

#[test]
fn cook_cost_gate_skips_when_transport_would_lose() {
    let archive: u64 = 500 * 1024 * 1024;
    let expected = archive.div_ceil(COOK_RESTORE_FLOOR_BYTES_PER_MS);
    assert_eq!(
        decide_cook_restore(archive, 9_000),
        CookRestoreDecision::Skip {
            estimated_transport_ms: Some(expected),
            reason: "estimated transport is not cheaper than the avoided compile",
        }
    );
}

#[test]
fn cook_cost_gate_skips_without_prior_observations() {
    assert!(matches!(
        decide_cook_restore(1, 0),
        CookRestoreDecision::Skip {
            reason: "no prior compile duration is recorded",
            ..
        }
    ));
    assert!(matches!(
        decide_cook_restore(0, 5_000),
        CookRestoreDecision::Skip {
            reason: "archive has no recorded bytes",
            ..
        }
    ));
}

#[test]
fn a_tree_cooked_in_place_needs_no_restore() {
    let dir = TempDir::new().unwrap();
    assert!(!target_already_cooked(dir.path(), "debug"));
    // The marker lives in the cooked profile directory, not the root.
    std::fs::write(dir.path().join(crate::cook::COOK_MARKER_FILE_NAME), b"{}").unwrap();
    assert!(!target_already_cooked(dir.path(), "debug"));
    std::fs::create_dir_all(dir.path().join("debug")).unwrap();
    std::fs::write(
        dir.path()
            .join("debug")
            .join(crate::cook::COOK_MARKER_FILE_NAME),
        b"{}",
    )
    .unwrap();
    assert!(target_already_cooked(dir.path(), "debug"));
    assert!(!target_already_cooked(dir.path(), "release"));
}

#[test]
fn only_a_daemon_that_is_not_running_makes_the_lookup_pointless() {
    assert!(!lookup_worthwhile_from_status(Err(
        client::ClientError::NotRunning
    )));
    // soldr#3117: a status probe that timed out against a daemon still
    // warming up is not evidence of an empty index.
    assert!(lookup_worthwhile_from_status(Err(client::ClientError::Io(
        std::io::Error::from(std::io::ErrorKind::WouldBlock)
    ))));
    assert!(lookup_worthwhile_from_status(Err(
        client::ClientError::Protocol("garbled".into())
    )));
}

#[test]
fn cook_performance_miss_is_an_honest_wall_time_comparison() {
    assert!(is_cook_performance_miss(10_000, 10_000));
    assert!(is_cook_performance_miss(10_001, 10_000));
    assert!(!is_cook_performance_miss(9_999, 10_000));
    assert!(!is_cook_performance_miss(10_000, 0));
}

#[test]
fn resolve_target_dir_without_explicit_target_is_the_bare_target_dir() {
    let _g = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_target_dir_env();
    let dir = TempDir::new().unwrap();
    let args = [
        "chef".to_string(),
        "cook".to_string(),
        "--recipe-path".to_string(),
        "r.json".to_string(),
        "--workspace".to_string(),
    ];
    let expected = dir.path().join("target");
    assert_eq!(resolve_target_dir(dir.path(), &args), expected);
}

#[test]
fn resolve_target_dir_appends_the_explicit_target_triple() {
    // Serialized with the CARGO_TARGET_DIR-mutating test below: that one
    // sets a process-global var this assertion depends on being unset.
    let _g = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_target_dir_env();
    let dir = TempDir::new().unwrap();
    let args = [
        "build".to_string(),
        "--target".to_string(),
        "x86_64-unknown-linux-gnu".to_string(),
    ];
    let expected = dir.path().join("target").join("x86_64-unknown-linux-gnu");
    assert_eq!(resolve_target_dir(dir.path(), &args), expected);
}

#[test]
fn resolve_target_dir_accepts_the_equals_spelling() {
    let _g = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_target_dir_env();
    let dir = TempDir::new().unwrap();
    let args = [
        "build".to_string(),
        "--target=aarch64-unknown-linux-gnu".to_string(),
    ];
    let expected = dir.path().join("target").join("aarch64-unknown-linux-gnu");
    assert_eq!(resolve_target_dir(dir.path(), &args), expected);
}

#[test]
fn resolve_target_dir_honours_cargo_target_dir_with_an_explicit_target() {
    let _g = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let manifest_dir = TempDir::new().unwrap();
    let target_dir = TempDir::new().unwrap();
    std::env::set_var("CARGO_TARGET_DIR", target_dir.path());
    let args = [
        "build".to_string(),
        "--target".to_string(),
        "x86_64-unknown-linux-gnu".to_string(),
    ];
    let resolved = resolve_target_dir(manifest_dir.path(), &args);
    std::env::remove_var("CARGO_TARGET_DIR");
    let expected = target_dir.path().join("x86_64-unknown-linux-gnu");
    assert_eq!(resolved, expected);
}

// soldr#3043: `cargo chef prepare` (soldr cook's Phase 1) carries no
// `--target`, so without this fallback the hydrate would extract a
// `--target`-scoped archive to the bare `target/` root — a duplicate
// extraction Cargo never reads, with the warm-cook marker landing outside
// `resolve_cook_target_dir` so soldr#621's short-circuit can never fire.
#[test]
fn resolve_target_dir_falls_back_to_the_cook_target_scope_env() {
    let _g = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_target_dir_env();
    let dir = TempDir::new().unwrap();
    let args = [
        "chef".to_string(),
        "prepare".to_string(),
        "--recipe-path".to_string(),
        "r.json".to_string(),
    ];
    std::env::set_var(SOLDR_COOK_HYDRATE_TARGET_ENV, "aarch64-apple-darwin");
    let resolved = resolve_target_dir(dir.path(), &args);
    std::env::remove_var(SOLDR_COOK_HYDRATE_TARGET_ENV);
    let expected = dir.path().join("target").join("aarch64-apple-darwin");
    assert_eq!(resolved, expected);
}

#[test]
fn an_explicit_target_argv_beats_the_cook_target_scope_env() {
    let _g = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_target_dir_env();
    let dir = TempDir::new().unwrap();
    let args = [
        "chef".to_string(),
        "cook".to_string(),
        "--target".to_string(),
        "x86_64-unknown-linux-gnu".to_string(),
    ];
    std::env::set_var(SOLDR_COOK_HYDRATE_TARGET_ENV, "aarch64-apple-darwin");
    let resolved = resolve_target_dir(dir.path(), &args);
    std::env::remove_var(SOLDR_COOK_HYDRATE_TARGET_ENV);
    let expected = dir.path().join("target").join("x86_64-unknown-linux-gnu");
    assert_eq!(resolved, expected);
}

#[test]
fn a_blank_cook_target_scope_env_is_ignored() {
    let _g = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    clear_target_dir_env();
    let dir = TempDir::new().unwrap();
    let args = ["chef".to_string(), "prepare".to_string()];
    std::env::set_var(SOLDR_COOK_HYDRATE_TARGET_ENV, "   ");
    let resolved = resolve_target_dir(dir.path(), &args);
    std::env::remove_var(SOLDR_COOK_HYDRATE_TARGET_ENV);
    assert_eq!(resolved, dir.path().join("target"));
}

#[test]
fn cook_restore_elapsed_includes_archive_verification() {
    let (outcome, elapsed_ms) = timed_verified_extract(
        || {
            std::thread::sleep(std::time::Duration::from_millis(25));
            Ok::<_, ()>(true)
        },
        || Some(()),
    );

    assert!(matches!(outcome, TimedRestoreOutcome::Restored(())));
    assert!(
        elapsed_ms >= 20,
        "verification delay was excluded from restore elapsed time: {elapsed_ms} ms"
    );
}
