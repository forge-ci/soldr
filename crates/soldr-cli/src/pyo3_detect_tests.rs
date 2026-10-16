//! Unit coverage split from `pyo3_detect.rs` for the soldr#2493 1,000-line
//! production-source ceiling.

use super::*;

// soldr#1663: the crate-wide barrier, not a private one. These tests
// mutate PYO3_* variables that `env_cmd`'s tests read through
// `caller_pyo3_env()`, and a module-local mutex cannot serialise against
// another module.
use crate::TEST_PROCESS_ENV_LOCK as ENV_LOCK;

struct EnvVarGuard {
    key: &'static str,
    previous: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &Path) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            std::env::set_var(self.key, previous);
        } else {
            std::env::remove_var(self.key);
        }
    }
}

#[test]
fn target_aware_policy_matrix() {
    let cases = [
        (
            "native",
            BuildShape::Extension,
            true,
            "x86_64-unknown-linux-gnu",
            "x86_64-unknown-linux-gnu",
            PlanMode::Native,
            false,
        ),
        (
            "no-pyo3",
            BuildShape::Absent,
            false,
            "x86_64-unknown-linux-gnu",
            "x86_64-pc-windows-msvc",
            PlanMode::NoPyo3,
            false,
        ),
        (
            "abi3-cross",
            BuildShape::Extension,
            true,
            "x86_64-unknown-linux-gnu",
            "x86_64-apple-darwin",
            PlanMode::Abi3NoPython,
            true,
        ),
        (
            "modern-windows",
            BuildShape::Extension,
            false,
            "x86_64-unknown-linux-gnu",
            "x86_64-pc-windows-msvc",
            PlanMode::ModernWindowsRawDylib,
            false,
        ),
        (
            "modern-unix-extension",
            BuildShape::Extension,
            false,
            "x86_64-unknown-linux-gnu",
            "aarch64-apple-darwin",
            PlanMode::ExtensionDefault,
            false,
        ),
        (
            "embedding",
            BuildShape::Embedding,
            false,
            "x86_64-unknown-linux-gnu",
            "x86_64-pc-windows-msvc",
            PlanMode::RequiresExplicitCompatibility,
            false,
        ),
    ];
    for (name, shape, abi3, host, target, expected_mode, no_python) in cases {
        let plan = resolve_policy(PolicyInput::test(shape, abi3, host, target));
        assert_eq!(plan.mode, expected_mode, "{name}");
        assert_eq!(plan.env.contains_key("PYO3_NO_PYTHON"), no_python, "{name}");
    }
}

#[test]
fn non_abi3_and_legacy_are_never_silently_abi3() {
    for (shape, version, target) in [
        (BuildShape::Embedding, "0.29.0", "x86_64-pc-windows-msvc"),
        (BuildShape::Extension, "0.22.6", "x86_64-pc-windows-msvc"),
        (BuildShape::Extension, "0.22.6", "aarch64-apple-darwin"),
    ] {
        let mut input = PolicyInput::test(shape, false, "x86_64-unknown-linux-gnu", target);
        input.detected.as_mut().unwrap().versions = BTreeSet::from([version.to_string()]);
        let plan = resolve_policy(input);
        assert_eq!(plan.mode, PlanMode::RequiresExplicitCompatibility);
        assert!(!plan.env.contains_key("PYO3_NO_PYTHON"));
    }

    let mut raw_dylib_disabled = PolicyInput::test(
        BuildShape::Extension,
        false,
        "x86_64-unknown-linux-gnu",
        "x86_64-pc-windows-msvc",
    );
    raw_dylib_disabled.raw_dylib_disabled = true;
    assert_eq!(
        resolve_policy(raw_dylib_disabled).mode,
        PlanMode::RequiresExplicitCompatibility
    );
}

#[test]
fn explicit_compatibility_and_caller_overrides_win() {
    let mut compatibility = PolicyInput::test(
        BuildShape::Embedding,
        false,
        "x86_64-unknown-linux-gnu",
        "x86_64-pc-windows-msvc",
    );
    compatibility.compatibility_sysroot = true;
    let plan = resolve_policy(compatibility);
    assert_eq!(plan.mode, PlanMode::CompatibilitySysroot);
    assert!(plan.needs_python_sysroot);

    let mut caller = PolicyInput::test(
        BuildShape::Extension,
        true,
        "x86_64-unknown-linux-gnu",
        "x86_64-apple-darwin",
    );
    caller
        .caller_pyo3
        .insert("PYO3_CROSS_LIB_DIR".into(), "/caller".into());
    let plan = resolve_policy(caller);
    assert_eq!(plan.mode, PlanMode::CallerConfigured);
    assert!(plan.env.is_empty());
}

#[test]
fn compatibility_sysroot_exports_explicit_target_python_config() {
    let env = compatibility_sysroot_env(Path::new("/sdk/python/package"), "3.13.14");
    assert_eq!(env.get("PYO3_CROSS").map(String::as_str), Some("1"));
    let expected_lib_dir = Path::new("/sdk/python/package")
        .join("lib")
        .display()
        .to_string();
    assert_eq!(
        env.get("PYO3_CROSS_LIB_DIR").map(String::as_str),
        Some(expected_lib_dir.as_str())
    );
    assert_eq!(
        env.get("PYO3_CROSS_PYTHON_VERSION").map(String::as_str),
        Some("3.13")
    );
    assert_eq!(
        env.get("PYO3_CROSS_PYTHON_IMPLEMENTATION")
            .map(String::as_str),
        Some("CPython")
    );
}

#[test]
fn target_precedence_is_args_env_project_host() {
    let args = ["--target".into(), "win-x64".into()];
    assert_eq!(
        choose_build_target(
            &args,
            Some("x86_64-apple-darwin"),
            Some("aarch64-apple-darwin"),
            "x86_64-unknown-linux-gnu",
        ),
        "x86_64-pc-windows-msvc"
    );
    assert_eq!(
        choose_build_target(
            &[],
            Some("x86_64-apple-darwin"),
            Some("aarch64-apple-darwin"),
            "x86_64-unknown-linux-gnu",
        ),
        "x86_64-apple-darwin"
    );
    assert_eq!(
        choose_build_target(
            &[],
            None,
            Some("aarch64-apple-darwin"),
            "x86_64-unknown-linux-gnu",
        ),
        "aarch64-apple-darwin"
    );
    assert_eq!(
        choose_build_target(&[], None, None, "x86_64-unknown-linux-gnu"),
        "x86_64-unknown-linux-gnu"
    );
}

#[test]
fn cargo_metadata_resolves_active_version_features_and_shape() {
    let metadata = serde_json::json!({
        "workspace_members": ["app 0.1.0 (path+file:///app)"],
        "packages": [
            {
                "id": "app 0.1.0 (path+file:///app)",
                "name": "app",
                "version": "0.1.0",
                "targets": [{"kind": ["cdylib"], "crate_types": ["cdylib"]}]
            },
            {
                "id": "registry+pyo3#0.29.0",
                "name": "pyo3",
                "version": "0.29.0",
                "targets": [{"kind": ["lib"], "crate_types": ["lib"]}]
            }
        ],
        "resolve": {"nodes": [
            {"id": "app 0.1.0 (path+file:///app)", "deps": [{"pkg": "registry+pyo3#0.29.0"}], "features": []},
            {"id": "registry+pyo3#0.29.0", "deps": [], "features": ["abi3-py310"]}
        ]}
    });
    let detected = detect_from_metadata_json(&serde_json::to_vec(&metadata).unwrap(), &[])
        .unwrap()
        .unwrap();
    assert_eq!(detected.shape, BuildShape::Extension);
    assert_eq!(detected.versions, BTreeSet::from(["0.29.0".to_string()]));
    assert!(detected.abi3());
}

#[test]
fn cargo_metadata_probe_captures_child_stdout() {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        // The probe fake is a POSIX shell script; Windows hosts
        // cannot execute it and there is no .cmd fixture for it.
        return;
    }
    let _lock = ENV_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let cargo = workspace.path().join("fake-cargo");
    let metadata = serde_json::json!({
        "workspace_members": ["app"],
        "workspace_default_members": ["app"],
        "padding": "x".repeat(512 * 1024),
        "packages": [
            {
                "id": "app",
                "name": "app",
                "version": "0.1.0",
                "targets": [{"kind": ["cdylib"], "crate_types": ["cdylib"]}]
            },
            {
                "id": "pyo3",
                "name": "pyo3",
                "version": "0.29.0",
                "targets": [{"kind": ["lib"], "crate_types": ["lib"]}]
            }
        ],
        "resolve": {"nodes": [
            {"id": "app", "deps": [{"pkg": "pyo3"}], "features": []},
            {"id": "pyo3", "deps": [], "features": ["abi3-py310"]}
        ]}
    });
    assert!(
        metadata.to_string().len() > 512 * 1024,
        "fixture must exceed ordinary OS pipe capacity"
    );
    std::fs::write(
        &cargo,
        format!("#!/bin/sh\nprintf '%s\\n' '{}'\n", metadata),
    )
    .expect("write fake cargo");
    let permissions = std::fs::metadata(&cargo)
        .expect("stat fake cargo")
        .permissions();
    crate::platform::fs::permissions::make_executable_from(&cargo, &permissions)
        .expect("chmod fake cargo");
    let _cargo_guard = EnvVarGuard::set(crate::TEST_CARGO_BIN_ENV_VAR, &cargo);

    let detected = detect_workspace_pyo3(workspace.path(), &[], host_triple(), None)
        .expect("metadata probe should capture and parse cargo stdout")
        .expect("PyO3 should be detected");

    assert_eq!(detected.versions, BTreeSet::from(["0.29.0".to_string()]));
    assert!(detected.abi3());
}

#[test]
fn metadata_ignores_unreachable_pyo3_versions_and_features() {
    let metadata = serde_json::json!({
        "workspace_members": ["app"],
        "workspace_default_members": ["app"],
        "packages": [
            {"id": "app", "name": "app", "version": "0.1.0", "targets": [{"kind": ["cdylib"], "crate_types": ["cdylib"]}]},
            {"id": "pyo3-new", "name": "pyo3", "version": "0.29.0", "targets": []},
            {"id": "pyo3-old", "name": "pyo3", "version": "0.22.6", "targets": []}
        ],
        "resolve": {"nodes": [
            {"id": "app", "deps": [{"pkg": "pyo3-new"}], "features": []},
            {"id": "pyo3-new", "deps": [], "features": ["abi3-py310"]},
            {"id": "pyo3-old", "deps": [], "features": ["auto-initialize"]}
        ]}
    });
    let detected = detect_from_metadata_json(&serde_json::to_vec(&metadata).unwrap(), &[])
        .unwrap()
        .unwrap();
    assert_eq!(detected.versions, BTreeSet::from(["0.29.0".to_string()]));
    assert!(!detected.features.contains("auto-initialize"));
    assert_eq!(detected.shape, BuildShape::Extension);
}

#[test]
fn maturin_target_aliases_are_normalized_before_exec() {
    assert_eq!(
        normalize_explicit_target_args(&[
            "pep517".into(),
            "build-wheel".into(),
            "--target".into(),
            "win-x64".into(),
        ]),
        [
            "pep517",
            "build-wheel",
            "--target",
            "x86_64-pc-windows-msvc",
        ]
    );
    assert_eq!(
        normalize_explicit_target_args(&["build".into(), "--target=mac-arm64".into()]),
        ["build", "--target=aarch64-apple-darwin"]
    );
}

#[test]
fn only_build_producing_maturin_commands_receive_policy() {
    for args in [
        vec!["build".into()],
        vec!["develop".into()],
        vec!["pep517".into(), "build-wheel".into()],
        vec!["pep517".into(), "write-dist-info".into()],
    ] {
        assert!(maturin_args_are_build(&args), "{args:?}");
    }
    for args in [
        vec!["--version".into()],
        vec!["build".into(), "--help".into()],
        vec!["pep517".into(), "write-sdist".into()],
        vec!["list-python".into()],
    ] {
        assert!(!maturin_args_are_build(&args), "{args:?}");
    }
}

#[test]
fn host_triple_resolves_to_known_triple() {
    let host = host_triple();
    assert!(host.is_empty() || crate::core::is_canonical(host) || host.contains('-'));
}

#[test]
fn known_native_cargo_resolution_does_not_require_workspace_metadata() {
    let temp = tempfile::tempdir().expect("tempdir");
    let missing_workspace = temp.path().join("does-not-exist");
    let host = host_triple();

    let plan = resolve_for_cargo_invocation(
        &missing_workspace,
        &[],
        Some(host),
        CargoProbeHints::default(),
    );

    assert_eq!(plan.mode, PlanMode::Native);
    assert!(plan.env.is_empty());
    assert!(plan.diagnostic.is_none());
}

#[test]
fn unknown_cargo_target_does_not_assume_native() {
    let temp = tempfile::tempdir().expect("tempdir");
    let missing_workspace = temp.path().join("does-not-exist");

    let plan =
        resolve_for_cargo_invocation(&missing_workspace, &[], None, CargoProbeHints::default());

    assert_eq!(plan.mode, PlanMode::Unresolved);
    assert!(plan
        .diagnostic
        .as_deref()
        .is_some_and(|message| message.contains("metadata")));
}

#[test]
fn cargo_config_target_keeps_conservative_metadata_path() {
    let temp = tempfile::tempdir().expect("tempdir");
    let missing_workspace = temp.path().join("does-not-exist");
    let args = vec![
        "build".to_string(),
        "--config".to_string(),
        "build.target=\"aarch64-unknown-linux-gnu\"".to_string(),
    ];

    let plan = resolve_for_cargo_invocation(
        &missing_workspace,
        &args,
        Some(host_triple()),
        CargoProbeHints::default(),
    );

    assert_eq!(plan.mode, PlanMode::Unresolved);
}

#[test]
fn explicit_cross_target_beats_weaker_known_native_target() {
    let temp = tempfile::tempdir().expect("tempdir");
    let missing_workspace = temp.path().join("does-not-exist");
    let cross_target = if host_triple().contains("windows") {
        "aarch64-apple-darwin"
    } else {
        "x86_64-pc-windows-msvc"
    };
    let args = vec![
        "build".to_string(),
        "--target".to_string(),
        cross_target.to_string(),
    ];

    let plan = resolve_for_cargo_invocation(
        &missing_workspace,
        &args,
        Some(host_triple()),
        CargoProbeHints::default(),
    );

    assert_eq!(plan.mode, PlanMode::Unresolved);
    assert_eq!(plan.target, cross_target);
}

#[test]
fn target_after_separator_cannot_override_known_cargo_target() {
    let temp = tempfile::tempdir().expect("tempdir");
    let missing_workspace = temp.path().join("does-not-exist");
    let cross_target = if host_triple().contains("windows") {
        "aarch64-apple-darwin"
    } else {
        "x86_64-pc-windows-msvc"
    };
    let args = vec![
        "run".to_string(),
        "--".to_string(),
        "--target".to_string(),
        host_triple().to_string(),
    ];

    let plan = resolve_for_cargo_invocation(
        &missing_workspace,
        &args,
        Some(cross_target),
        CargoProbeHints::default(),
    );

    assert_eq!(plan.mode, PlanMode::Unresolved);
    assert_eq!(plan.target, cross_target);
}

#[test]
fn public_native_plan_keeps_metadata_reporting_semantics() {
    let temp = tempfile::tempdir().expect("tempdir");
    let missing_workspace = temp.path().join("does-not-exist");

    let plan = resolve_for_invocation(&missing_workspace, &[], Some(host_triple()));

    assert_eq!(plan.mode, PlanMode::Unresolved);
}

fn proven_hints() -> CargoProbeHints<'static> {
    CargoProbeHints {
        cargo: None,
        lock_proves_no_pyo3: true,
    }
}

#[test]
fn lock_proof_skips_probe_and_yields_no_pyo3_plan() {
    let temp = tempfile::tempdir().expect("tempdir");
    // The workspace does not exist, so a probe would be Unresolved.
    let missing_workspace = temp.path().join("does-not-exist");

    let plan = resolve_for_cargo_invocation(&missing_workspace, &[], None, proven_hints());

    assert_eq!(plan.mode, PlanMode::Native);
    assert!(plan.pyo3_versions.is_empty());
    assert!(plan.env.is_empty());
    assert!(plan.diagnostic.is_none());
}

#[test]
fn lock_proof_does_not_skip_probe_for_target_config_or_dash_c() {
    let temp = tempfile::tempdir().expect("tempdir");
    let missing_workspace = temp.path().join("does-not-exist");
    let cross = if host_triple().contains("windows") {
        "aarch64-apple-darwin"
    } else {
        "x86_64-pc-windows-msvc"
    };
    for extra in [
        vec!["--target", cross],
        vec!["--config", "build.rustflags=[]"],
        vec!["-C", "."],
    ] {
        let mut args = vec!["build".to_string()];
        args.extend(extra.iter().map(|value| (*value).to_string()));
        let plan = resolve_for_cargo_invocation(&missing_workspace, &args, None, proven_hints());
        assert_eq!(plan.mode, PlanMode::Unresolved, "{args:?}");
    }
}

#[test]
fn unproven_lock_still_runs_probe() {
    let temp = tempfile::tempdir().expect("tempdir");
    let missing_workspace = temp.path().join("does-not-exist");
    // No proof: the probe runs (and fails on the missing workspace).
    let plan =
        resolve_for_cargo_invocation(&missing_workspace, &[], None, CargoProbeHints::default());
    assert_eq!(plan.mode, PlanMode::Unresolved);
}

#[cfg(unix)]
#[test]
fn probe_runs_the_cargo_the_front_door_passed() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().expect("tempdir");
    let marker = temp.path().join("ran");
    let cargo = temp.path().join("fake-cargo");
    std::fs::write(
        &cargo,
        format!("#!/bin/sh\necho \"$@\" > '{}'\nexit 1\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();

    let hints = CargoProbeHints {
        cargo: Some(&cargo),
        lock_proves_no_pyo3: false,
    };
    let plan = resolve_for_cargo_invocation(temp.path(), &[], None, hints);

    assert_eq!(plan.mode, PlanMode::Unresolved);
    let recorded = std::fs::read_to_string(&marker).expect("fake cargo was run");
    assert!(
        recorded.starts_with("metadata --format-version 1"),
        "{recorded}"
    );
}
