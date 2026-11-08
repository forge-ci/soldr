use super::*;

fn fixture(body: &str) -> toml::Value {
    toml::from_str(&format!("[workspace.metadata.soldr]\ntargets = [\"aarch64-apple-darwin\"]\n[workspace.metadata.soldr.attest]\n{body}")).unwrap()
}

#[test]
fn parses_and_expands_host_and_target_checks() {
    let value = fixture(
        r#"schema = 1
checks = [
 { id = "rust/{host}/test", step = "test" },
 { id = "rust/{target}/clippy", step = "clippy", targets = "cross", scope = "all-targets" },
 { id = "rust/all/fmt", step = "fmt" },
]"#,
    );
    let (work, warnings) = parse(&value, "x86_64-unknown-linux-gnu").unwrap();
    assert!(warnings.is_empty());
    assert_eq!(work.len(), 3);
    assert_eq!(work[0].check.id, "rust/aarch64-apple-darwin/clippy");
    assert_eq!(work[0].check.hosts, ["any"]);
    assert!(work[0]
        .args
        .windows(2)
        .any(|v| v == ["--target", "aarch64-apple-darwin"]));
    assert_eq!(work[1].check.id, "rust/all/fmt");
    assert_eq!(work[2].check.hosts, ["x86_64-unknown-linux-gnu"]);
    assert_eq!(work[2].args, ["nextest", "run", "--workspace"]);
}

#[test]
fn unknown_keys_are_warnings() {
    let (work, warnings) = parse(
        &fixture(
            r#"schema = 1
future = true
checks = [{ id = "rust/all/fmt", step = "fmt", future = "value" }]"#,
        ),
        "host",
    )
    .unwrap();
    assert_eq!(work.len(), 1);
    assert_eq!(warnings.len(), 2);
    assert!(warnings.iter().all(|w| w.contains("unknown")));
}

#[test]
fn malformed_declarations_are_rejected() {
    for body in [
        "schema = 2\nchecks = []",
        "checks = []",
        "schema = 1\nchecks = [{ id = 1, step = 'fmt' }]",
        "schema = 1\nchecks = [{ id = 'x', step = 'unknown' }]",
        "schema = 1\nchecks = [{ id = 'x', step = 'fmt', scope = 'crate' }]",
        "schema = 1\nchecks = [{ id = 'x/{future}', step = 'test' }]",
        "schema = 1\nchecks = [{ id = 'x/{target}', step = 'clippy' }]",
        "schema = 1\nchecks = [{ id = 'x', step = 'fmt' }, { id = 'x', step = 'fmt' }]",
    ] {
        assert!(parse(&fixture(body), "host").is_err(), "{body}");
    }
}

#[test]
fn plan_is_canonical_and_deterministic() {
    let a = fixture(
        r#"schema = 1
checks = [{ id = "z/{host}", step = "test" }, { id = "a/{target}", step = "clippy", targets = "cross" }]"#,
    );
    let b = fixture(
        r#"schema = 1
checks = [{ step = "clippy", targets = "cross", id = "a/{target}" }, { step = "test", id = "z/{host}" }]"#,
    );
    let a = client_work(&parse(&a, "host").unwrap().0);
    let b = client_work(&parse(&b, "host").unwrap().0);
    let json = serde_json::to_value(&a).unwrap();
    assert_eq!(json["v"], 1);
    assert_eq!(json["kind"], "client-work");
    assert_eq!(json["checks"][0]["hosts"], serde_json::json!(["any"]));
    assert_eq!(a.digest.len(), 64);
    assert_eq!(
        serde_json::to_string(&a).unwrap(),
        serde_json::to_string(&b).unwrap()
    );
    assert_eq!(
        a.digest,
        hex::encode(Sha256::digest(serde_json::to_vec(&a.checks).unwrap()))
    );
}

fn git_ok(root: &Path, args: &[&str]) {
    assert!(Command::new("git")
        .current_dir(root)
        .args(args)
        .status()
        .unwrap()
        .success());
}

#[test]
fn dirty_tree_cannot_bind_evidence() {
    let temp = tempfile::tempdir().unwrap();
    git_ok(temp.path(), &["init", "--quiet"]);
    std::fs::write(temp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
    git_ok(temp.path(), &["add", "."]);
    git_ok(
        temp.path(),
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "fixture",
        ],
    );
    assert!(clean_tree(temp.path()).unwrap().is_some());
    std::fs::write(temp.path().join("untracked"), "dirty").unwrap();
    assert!(clean_tree(temp.path()).unwrap().is_none());
}

fn result(ok: bool, exit_code: Option<i32>) -> ResultRow {
    ResultRow {
        id: "check".into(),
        ok,
        secs: 0.0,
        key: String::new(),
        exit_code,
    }
}

#[test]
fn failures_and_inapplicable_checks_have_distinct_exit_codes() {
    assert_eq!(aggregate(&[result(false, None)], 0), 1);
    assert_eq!(aggregate(&[result(false, Some(75))], 0), 75);
    assert_eq!(
        aggregate(&[result(false, Some(75)), result(true, None)], 0),
        0
    );
    assert_eq!(
        aggregate(&[result(false, Some(75)), result(false, None)], 0),
        1
    );
    assert_eq!(aggregate(&[result(true, None)], 1), 1);
    assert_eq!(
        serde_json::to_value(result(false, Some(75))).unwrap()["exit_code"],
        75
    );
}

#[test]
fn target_tests_are_native_and_host_target_is_not_duplicated() {
    let value = fixture(
        r#"schema = 1
checks = [{ id = "rust/{target}/test", step = "test", targets = "cross" }]"#,
    );
    let (work, _) = parse(&value, "x86_64-unknown-linux-gnu").unwrap();
    assert_eq!(work[0].check.hosts, ["aarch64-apple-darwin"]);
    assert!(parse(&value, "aarch64-apple-darwin").unwrap().0.is_empty());
}

#[test]
fn contract_drift_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("ci-attestations.yml"),
        "gates:\n  rust/all/fmt: {}\n",
    )
    .unwrap();
    let value = fixture("schema = 1\nchecks = [{ id = 'rust/all/fmt', step = 'fmt' }]");
    let (mut work, _) = parse(&value, "host").unwrap();
    validate_contract(temp.path(), &work).unwrap();
    work[0].check.id = "missing".into();
    assert!(validate_contract(temp.path(), &work).is_err());
}

#[test]
fn workspace_is_canonical_from_member_directory() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
    let member = temp.path().join("crates/member");
    std::fs::create_dir_all(&member).unwrap();
    std::fs::write(member.join("Cargo.toml"), "[package]\nname = 'member'\n").unwrap();
    assert_eq!(workspace(&member).unwrap().0, temp.path());
}

#[tokio::test]
async fn dirty_attestation_exits_three_before_running_commands() {
    let temp = tempfile::tempdir().unwrap();
    git_ok(temp.path(), &["init", "--quiet"]);
    std::fs::write(temp.path().join("Cargo.toml"), "[workspace]\n").unwrap();
    assert_eq!(run_at(temp.path(), true, false).await.unwrap(), 3);
}

#[tokio::test]
async fn bad_declaration_and_usage_exit_two() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("Cargo.toml"),
        "[workspace.metadata.soldr.attest]\nschema=2\nchecks=[]\n",
    )
    .unwrap();
    assert_eq!(run_at(temp.path(), true, false).await.unwrap(), 2);
    assert_eq!(
        run(&["deps".into(), "--attest".into()], true, false)
            .await
            .unwrap(),
        2
    );
    assert_eq!(print_plan(&[]).unwrap(), 2);
}

#[test]
fn nonexistent_destination_is_resolved_without_writing() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("missing/attest");
    assert_eq!(resolve_destination(&path).unwrap(), path);
    assert!(!path.exists());
}
