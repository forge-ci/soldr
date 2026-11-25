//! Explain-plan coverage for canonical selection, without compiling fixtures.
use crate::common::*;
use serde_json::{json, Value};

fn plan(selection: &str) -> Value {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("dylints/lint")).unwrap();
    std::fs::write(dir.path().join("Cargo.toml"), format!(
        "[workspace]\nmembers = []\n[workspace.metadata.dylint]\nlibraries = [{{path = 'dylints/*'}}]\n[workspace.metadata.soldr.cargo]\n{selection}"
    )).unwrap();
    std::fs::write(
        dir.path().join("rust-toolchain.toml"),
        "[toolchain]\nchannel = '1.98.1'",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("dylints/lint/Cargo.toml"),
        "[package]\nname = 'lint'\nversion = '0.1.0'",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("dylints/lint/rust-toolchain.toml"),
        "[toolchain]\nchannel = 'nightly-2026-08-15'",
    )
    .unwrap();
    let host = soldr_cli::pyo3_detect::host_triple();
    let channel = format!("nightly-2026-08-15-{host}");
    let output = isolated_soldr_command()
        .current_dir(dir.path())
        .args(["ci-test", "--explain-plan", "--format", "json"])
        .env("SOLDR_DYLINT_CONFIGURED_TOOLCHAIN", &channel)
        .env("SOLDR_DYLINT_CONFIGURED_RUSTC_RELEASE", "1.89.0-nightly")
        .env(
            "SOLDR_DYLINT_CONFIGURED_RUSTC_COMMIT_HASH",
            "0123456789abcdef0123456789abcdef01234567",
        )
        .env(
            "SOLDR_DYLINT_PREPARED_IDENTITY",
            format!("{channel}|1.89.0-nightly|0123456789abcdef0123456789abcdef01234567"),
        )
        .env("NEXTEST_TEST_THREADS", "4")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn command<'a>(plan: &'a Value, name: &str) -> &'a Value {
    &plan["stages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|stage| stage["name"] == name)
        .unwrap()["command"]
}

#[test]
fn canonical_profile_plan_json_emits_one_selection_and_the_declared_clippy_tail() {
    let plan = plan("features = ['b', 'a']\npackages = ['a', 'b']\ndefault-features = false\nclippy-args = ['-W', 'clippy::all']");
    let host = plan["host_triple"].as_str().unwrap();
    assert_eq!(
        command(&plan, "clippy"),
        &json!([
            "soldr",
            "cargo",
            "clippy",
            "-p",
            "a",
            "-p",
            "b",
            "--features",
            "a,b",
            "--no-default-features",
            "--all-targets",
            "--target",
            host,
            "--",
            "-W",
            "clippy::all"
        ])
    );
    assert_eq!(
        command(&plan, "nextest-compile"),
        &json!([
            "soldr",
            "cargo",
            "nextest",
            "run",
            "-p",
            "a",
            "-p",
            "b",
            "--features",
            "a,b",
            "--no-default-features",
            "--all-targets",
            "--target",
            host,
            "--no-run",
            "--test-threads",
            "4"
        ])
    );
    assert_eq!(
        plan["scope"],
        json!({"packages": ["a", "b"], "features": ["a", "b"], "all_features": false, "no_default_features": true})
    );
}

#[test]
fn canonical_profile_implicit_host_plan_reports_the_actual_artifact_tree() {
    let plan = plan("target = 'implicit'\ndev-targets = false");
    let args = command(&plan, "nextest").as_array().unwrap();
    assert!(!args.iter().any(|arg| arg == "--target"));
    assert!(args.iter().any(|arg| arg == "--bins"));
    let domains = plan["domains"].as_array().unwrap();
    for domain in domains
        .iter()
        .filter(|domain| domain["id"] == "stable" || domain["id"] == "rustdoc")
    {
        assert!(domain["target_directory"]
            .as_str()
            .unwrap()
            .ends_with("target"));
    }
}
