//! Unit tests for [`super`].
//!
//! In a sibling file rather than an inline `mod`: `dylint_cook.rs` reached the
//! 1000-line production ceiling that `.github/scripts/loc_ceiling.py` enforces,
//! and CLAUDE.md's rule for that is that the addition belongs in a new module.
//! Moving the tests keeps the production surface at its real size -- 855 lines
//! -- rather than counting test scaffolding against it.

use super::*;

fn argv(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

#[test]
fn parses_check_shaped_dylint_cook_scope() {
    let parsed = parse_args(&argv(&[
        "--json",
        "--plan-only",
        "--release",
        "--target",
        "aarch64-unknown-linux-gnu",
        "--workspace",
        "-p",
        "demo",
        "--features",
        "serde,trace",
        "--all-targets",
        "--config",
        "build.jobs=2",
        "--toolchain",
        "nightly-2026-04-16",
    ]))
    .expect("parse");
    assert!(parsed.json && parsed.plan_only && parsed.release);
    assert_eq!(parsed.target.as_deref(), Some("aarch64-unknown-linux-gnu"));
    assert!(parsed.workspace && parsed.all_targets);
    assert_eq!(parsed.packages, ["demo"]);
    assert_eq!(parsed.features, ["serde", "trace"]);
    assert_eq!(parsed.cargo_config, ["build.jobs=2"]);
}

#[test]
fn semantic_key_ignores_source_but_tracks_manifests() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(
        root.join(concat!("Car", "go.toml")),
        "[package]\nname='demo'\nversion='0.1.0'\n",
    )
    .unwrap();
    std::fs::write(root.join(concat!("Car", "go.lock")), "version = 3\n").unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn one() {}\n").unwrap();
    let args = DylintCookArgs::default();
    let first = semantic_input_hash(root, &args).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn two() {}\n").unwrap();
    assert_eq!(first, semantic_input_hash(root, &args).unwrap());
    std::fs::write(root.join(concat!("Car", "go.lock")), "version = 4\n").unwrap();
    assert_ne!(first, semantic_input_hash(root, &args).unwrap());
}

#[test]
fn conflicting_custom_lint_toolchains_are_actionable() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    for name in ["lint-a", "lint-b"] {
        std::fs::create_dir_all(root.join(name)).unwrap();
    }
    std::fs::write(
        root.join(concat!("Car", "go.toml")),
        "[workspace]\nmembers=[]\n[workspace.metadata.dylint]\n\
         libraries=[{path='lint-a'},{path='lint-b'}]\n",
    )
    .unwrap();
    for (name, date) in [("lint-a", "16"), ("lint-b", "17")] {
        std::fs::write(
            root.join(name).join("rust-toolchain.toml"),
            format!("[toolchain]\nchannel='nightly-2026-04-{date}'\n"),
        )
        .unwrap();
    }
    let error = configured_library_toolchain(root).unwrap_err().to_string();
    assert!(error.contains("conflicting"));
    assert!(error.contains("lint-a") && error.contains("lint-b"));
}

#[test]
fn inherited_custom_lint_toolchain_requires_a_dated_nightly_root() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir_all(root.join("lint-a")).unwrap();
    std::fs::write(
        root.join(concat!("Car", "go.toml")),
        "[workspace]\nmembers=[]\n[workspace.metadata.dylint]\nlibraries=[{path='lint-a'}]\n",
    )
    .unwrap();
    std::fs::write(
        root.join("rust-toolchain.toml"),
        "[toolchain]\nchannel='1.95.0'\n",
    )
    .unwrap();

    let error = configured_library_toolchain(root).unwrap_err().to_string();
    assert!(error.contains("lint-a"), "{error}");
    assert!(error.contains("1.95.0"), "{error}");
    assert!(
        error.contains("published only for dated nightly"),
        "{error}"
    );

    std::fs::write(
        root.join("rust-toolchain.toml"),
        "[toolchain]\nchannel='nightly-2026-05-28'\n",
    )
    .unwrap();
    assert_eq!(
        configured_library_toolchain(root).unwrap().as_deref(),
        Some("nightly-2026-05-28")
    );
}

#[test]
fn warm_marker_requires_real_dependency_payload() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join(MARKER_NAME), "{}").unwrap();
    assert!(!target_has_dependency_payload(temp.path()));
    std::fs::create_dir_all(temp.path().join("debug/deps")).unwrap();
    std::fs::write(temp.path().join("debug/deps/libserde-deadbeef.rmeta"), "x").unwrap();
    assert!(target_has_dependency_payload(temp.path()));
}

#[test]
fn marker_replacement_handles_windows_existing_destination() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join(MARKER_NAME);
    let temporary = temp.path().join("replacement.tmp");
    std::fs::write(&path, b"old").unwrap();
    std::fs::write(&temporary, b"new").unwrap();
    let mut attempts = 0;

    replace_marker_file(&temporary, &path, |from, to| {
        attempts += 1;
        if attempts == 1 {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "simulated Windows destination-exists failure",
            ))
        } else {
            std::fs::rename(from, to)
        }
    })
    .unwrap();

    assert_eq!(attempts, 2);
    assert_eq!(std::fs::read(&path).unwrap(), b"new");
    assert!(!temporary.exists());
}

#[test]
fn workspace_source_lock_is_shared_across_target_and_nightly_shapes() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::write(root.join(concat!("Car", "go.lock")), "version = 3\n").unwrap();
    let _nightly_a_target = root.join("target/dylint/target/nightly-2026-01-17");
    let _nightly_b_target = root.join("other-target/dylint/target/nightly-2026-01-18");
    let first = lock_workspace_source(root).unwrap();
    let contender = OpenOptions::new()
        .read(true)
        .write(true)
        .open(workspace_source_lock_path(root))
        .unwrap();

    assert!(
        kernal_api::platform::fs::try_lock_exclusive(&contender).is_err(),
        "different nightly/target shapes must contend on one workspace source lock"
    );
    kernal_api::fs_unlock(&first).unwrap();
    let _guard = kernal_api::platform::fs::try_lock_exclusive(&contender)
        .expect("workspace lock must become available after release");
}

#[test]
fn check_shape_preserves_scope_and_private_marker() {
    let parsed = parse_args(&argv(&[
        "--profile=ci",
        "--target=aarch64-unknown-linux-gnu",
        "--workspace",
        "--package=demo",
        "--features=serde,trace",
        "--tests",
        "--locked",
        "--config=build.jobs=2",
    ]))
    .unwrap();
    let plan = DylintToolchainPlan::identity(
        "nightly-2099-01-02".into(),
        "1.99.0-nightly".into(),
        "0123456789abcdef".into(),
    );
    let built = build_check_args(&parsed, &plan, Path::new("isolated-target"));
    assert_eq!(built[0], "+nightly-2099-01-02");
    assert_eq!(built[1], DYLINT_DEPENDENCY_COOK_FLAG);
    assert!(built.windows(2).any(|pair| pair == ["--profile", "ci"]));
    assert!(built
        .windows(2)
        .any(|pair| pair == ["--target-dir", "isolated-target"]));
    assert!(built.contains(&"--workspace".to_string()));
    assert!(built.contains(&"--tests".to_string()));
    assert!(built.contains(&"--locked".to_string()));
}

#[test]
fn rejects_ambiguous_feature_scope() {
    let error = parse_args(&argv(&["--all-features", "--no-default-features"]))
        .unwrap_err()
        .to_string();
    assert!(error.contains("conflicts"));
}

fn write_lockfile_root(root: &std::path::Path) {
    std::fs::write(
        root.join(concat!("Car", "go.toml")),
        "[package]\nname='demo'\nversion='0.1.0'\n",
    )
    .unwrap();
    std::fs::write(root.join(concat!("Car", "go.lock")), "version = 3\n").unwrap();
}

// soldr#3049: the analysis-tree cook must land where `cargo dylint` actually
// analyses the workspace. cargo-dylint names that directory
// `dylint/target/<RUSTUP_TOOLCHAIN>`, the host-qualified toolchain, and main's
// ci-test log shows `dylint/target/nightly-2026-05-28-x86_64-unknown-linux-gnu`.
// The cook used to write the bare `nightly-2026-05-28`, a directory nothing reads.
#[test]
fn dylint_cook_defaults_to_the_analysis_tree_path() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write_lockfile_root(root);
    let args = DylintCookArgs {
        target_root: Some(root.join("target")),
        ..Default::default()
    };
    let plan = DylintToolchainPlan::identity(
        "nightly-2026-05-28".into(),
        "1.99.0-nightly".into(),
        "0123456789abcdef".into(),
    );
    let output = build_output(root, &args, &plan, false).unwrap();
    let directory = PathBuf::from(&output.target_directory);
    let tail: Vec<_> = directory
        .components()
        .rev()
        .take(3)
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let expected_leaf = format!(
        "nightly-2026-05-28-{}",
        crate::core::TargetTriple::host().unwrap().triple()
    );
    assert_eq!(
        tail,
        vec![expected_leaf, "target".to_string(), "dylint".to_string()]
    );
}

// FACT 1 (soldr#3042 step 3): `ci_test/plan.rs`'s UI-test stages resolve
// their target dir with the host-triple-suffixed `canonical_channel`, so
// the tests-tree cook must land in the identically-suffixed directory.
#[test]
fn dylint_cook_tests_tree_matches_the_ui_test_stage_directory() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write_lockfile_root(root);
    let args = DylintCookArgs {
        target_root: Some(root.join("target")),
        tree: CookTree::Tests,
        ..Default::default()
    };
    let plan = DylintToolchainPlan::identity(
        "nightly-2026-05-28".into(),
        "1.99.0-nightly".into(),
        "0123456789abcdef".into(),
    );
    let output = build_output(root, &args, &plan, false).unwrap();
    let directory = PathBuf::from(&output.target_directory);
    let tail: Vec<_> = directory
        .components()
        .rev()
        .take(3)
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let expected_leaf = format!(
        "nightly-2026-05-28-{}",
        crate::core::TargetTriple::host().unwrap().triple()
    );
    assert_eq!(
        tail,
        vec![expected_leaf, "tests".to_string(), "dylint".to_string()]
    );
}

#[test]
fn the_two_trees_never_share_a_cook_key() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    write_lockfile_root(root);
    let plan = DylintToolchainPlan::identity(
        "nightly-2026-05-28".into(),
        "1.99.0-nightly".into(),
        "0123456789abcdef".into(),
    );
    let analysis_args = DylintCookArgs {
        target_root: Some(root.join("target")),
        ..Default::default()
    };
    let tests_args = DylintCookArgs {
        target_root: Some(root.join("target")),
        tree: CookTree::Tests,
        ..Default::default()
    };
    let analysis_output = build_output(root, &analysis_args, &plan, false).unwrap();
    let tests_output = build_output(root, &tests_args, &plan, false).unwrap();
    assert_ne!(analysis_output.cache_key, tests_output.cache_key);
}

#[test]
fn tests_tree_cooks_are_build_shaped() {
    let plan = DylintToolchainPlan::identity(
        "nightly-2099-01-02".into(),
        "1.99.0-nightly".into(),
        "0123456789abcdef".into(),
    );
    let analysis_args = DylintCookArgs::default();
    let analysis_built = build_check_args(&analysis_args, &plan, Path::new("isolated-target"));
    assert_eq!(analysis_built[1], DYLINT_DEPENDENCY_COOK_FLAG);
    assert!(analysis_built.contains(&"check".to_string()));
    assert!(!analysis_built.contains(&"build".to_string()));

    let tests_args = DylintCookArgs {
        tree: CookTree::Tests,
        ..Default::default()
    };
    let tests_built = build_check_args(&tests_args, &plan, Path::new("isolated-target"));
    assert_eq!(tests_built[1], DYLINT_DEPENDENCY_COOK_FLAG);
    assert!(tests_built.contains(&"build".to_string()));
    assert!(!tests_built.contains(&"check".to_string()));
}

#[test]
fn unknown_tree_values_are_rejected() {
    let error = parse_args(&argv(&["--tree", "analysis-tree"]))
        .unwrap_err()
        .to_string();
    assert!(error.contains("unknown --tree"), "{error}");
    assert!(
        error.contains("analysis") && error.contains("tests"),
        "{error}"
    );
}

#[test]
fn target_with_tests_tree_is_rejected() {
    let error = parse_args(&argv(&[
        "--tree",
        "tests",
        "--target",
        "aarch64-unknown-linux-gnu",
    ]))
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("--target conflicts with --tree tests"),
        "{error}"
    );
}
