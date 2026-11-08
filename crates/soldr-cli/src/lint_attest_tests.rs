use super::*;

fn fixture(body: &str) -> toml::Value {
    toml::from_str(&format!("[workspace.metadata.soldr]\ntargets = [\"aarch64-apple-darwin\"]\n[workspace.metadata.soldr.attest]\n{body}")).unwrap()
}

#[test]
fn parse_expands_aliases_without_tools_on_path() {
    let empty = tempfile::tempdir().unwrap();
    let _path = crate::EnvVarGuard::set("PATH", empty.path());
    let mut value = fixture(
        r#"schema = 1
checks = [{ id = "rust/{target}/clippy", step = "clippy", targets = "cross" }]"#,
    );
    value["workspace"]["metadata"]["soldr"]["targets"] =
        toml::Value::try_from(vec!["mac-arm64", "win-x64", "native", "host"]).unwrap();
    // Both host spellings deliberately resolve to the supplied host; duplicates
    // must still be rejected after expansion.
    assert!(parse(&value, "x86_64-unknown-linux-gnu")
        .unwrap_err()
        .to_string()
        .contains("duplicate"));
    value["workspace"]["metadata"]["soldr"]["targets"] =
        toml::Value::try_from(vec!["mac-arm64", "win-x64", "native"]).unwrap();
    let (work, warnings) = parse(&value, "x86_64-unknown-linux-gnu").unwrap();
    assert!(warnings.is_empty());
    assert_eq!(
        work.iter().map(|w| w.check.id.as_str()).collect::<Vec<_>>(),
        [
            "rust/aarch64-apple-darwin/clippy",
            "rust/x86_64-pc-windows-msvc/clippy",
            "rust/x86_64-unknown-linux-gnu/clippy",
        ]
    );
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
    let a = client_work(&parse(&a, "host").unwrap().0).unwrap();
    let b = client_work(&parse(&b, "host").unwrap().0).unwrap();
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
fn target_tests_are_native_and_declared_host_is_preserved() {
    let value = fixture(
        r#"schema = 1
checks = [{ id = "rust/{target}/test", step = "test", targets = "cross" }]"#,
    );
    let (work, _) = parse(&value, "x86_64-unknown-linux-gnu").unwrap();
    assert_eq!(work[0].check.hosts, ["aarch64-apple-darwin"]);
    let (native, _) = parse(&value, "aarch64-apple-darwin").unwrap();
    assert_eq!(native.len(), 1);
    assert_eq!(native[0].check.hosts, ["aarch64-apple-darwin"]);
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

#[test]
fn execution_transports_verdict_keys_and_persists_each_outcome() {
    if crate::platform::host::facts::os() == crate::platform::host::facts::HostOs::Windows {
        return;
    }
    let repo = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    git_ok(repo.path(), &["init", "--quiet"]);
    std::fs::write(repo.path().join("Cargo.toml"), "[workspace]\n").unwrap();
    git_ok(repo.path(), &["add", "."]);
    git_ok(
        repo.path(),
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
    let tree = clean_tree(repo.path()).unwrap().unwrap();
    let runner = state.path().join("soldr");
    for code in [0, 1, 75] {
        let script = format!("#!/bin/sh\nif [ \"$1\" != cargo ]; then exit 99; fi\nif [ -z \"$SOLDR_ATTEST_KEY_FILE\" ]; then exit 0; fi\nprintf 'existing-verdict-key' > \"$SOLDR_ATTEST_KEY_FILE\"\nexit {code}\n");
        std::fs::write(&runner, script).unwrap();
        crate::platform::fs::permissions::make_executable(&runner).unwrap();
        let work = vec![
            Work {
                check: ClientCheck {
                    id: "applicable".into(),
                    hosts: vec!["any".into()],
                },
                args: vec!["fmt".into()],
            },
            Work {
                check: ClientCheck {
                    id: "foreign".into(),
                    hosts: vec!["foreign-host".into()],
                },
                args: vec!["nextest".into()],
            },
        ];
        assert_eq!(
            execute(
                repo.path(),
                vec![crate::lint_steps::PlannedStep::Cargo {
                    label: "fmt".into(),
                    args: vec!["fmt".into(), "--check".into()],
                }],
                work,
                tree.clone(),
                state.path(),
                &runner,
                (true, false)
            )
            .unwrap(),
            code
        );
        let json: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(state.path().join(format!("{tree}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(json["tree"], tree);
        assert_eq!(json["v"], 1);
        assert_eq!(json["results"].as_array().unwrap().len(), 2);
        assert_eq!(json["results"][0]["ok"], code == 0);
        assert_eq!(json["results"][0]["key"], "existing-verdict-key");
        assert_eq!(json["results"][1]["exit_code"], 75);
        assert_eq!(json["autofixed"], serde_json::json!([]));
    }
}

#[test]
fn offline_attestation_refuses_missing_plugins_without_fetching() {
    let _lock = crate::TEST_PROCESS_ENV_LOCK.lock();
    let temp = tempfile::tempdir().unwrap();
    let _path = crate::EnvVarGuard::set("PATH", temp.path());
    let _offline = crate::EnvVarGuard::set("SOLDR_ATTEST_OFFLINE", "1");
    let paths = SoldrPaths::with_root(temp.path().join("state"));
    let runtime = tokio::runtime::Runtime::new().unwrap();
    for sub in ["nextest", "dylint"] {
        let outcome = runtime.block_on(crate::cargo_front_door::ensure_known_subcommand_tool(
            &[sub.into(), "run".into()],
            &paths,
        ));
        let message = match outcome {
            Err(err) => err.to_string(),
            Ok(_) => panic!("missing plugin accepted"),
        };
        assert!(message.contains("offline attestation"), "{message}");
        assert!(!paths.bin.exists());
    }
}
