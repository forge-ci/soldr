//! Unit coverage for broker spawning and retirement helpers.

use super::*;

// soldr#2024-adjacent hazard: env::set_var/remove_var races across
// threads within one test binary. These tests share one lock so they
// never interleave with each other -- matches the pattern other
// env-var-gated tests in this crate use.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn replacement_ready_marker_is_emitted_only_after_wait_success() {
    const MARKER_ENV: &str = "SOLDR_TEST_KNOWN_BAD_REPLACEMENT_READY_FILE";
    let _lock = ENV_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let root = tempfile::tempdir().expect("tempdir");
    let marker = root.path().join("replacement-ready");
    let _marker_env = crate::EnvVarGuard::remove(MARKER_ENV);

    test_mark_replacement_ready_after_wait(Ok(())).expect("unconfigured marker is a no-op");
    assert!(
        !marker.exists(),
        "a successful replacement must not create a marker without its test-only env"
    );

    let _marker_env = crate::EnvVarGuard::set(MARKER_ENV, &marker);
    let error = test_mark_replacement_ready_after_wait(Err(
        "stable broker did not answer readiness".to_string(),
    ))
    .expect_err("failed wait must not mark replacement readiness");
    assert!(error.contains("did not answer readiness"), "{error}");
    assert!(
        !marker.exists(),
        "a lease/stage/spawn attempt must not masquerade as a ready replacement"
    );

    test_mark_replacement_ready_after_wait(Ok(()))
        .expect("successful readiness may publish marker");
    assert_eq!(
        std::fs::read_to_string(&marker).expect("read replacement marker"),
        "ready\n"
    );
}

#[test]
fn broker_spawn_env_preserves_soldr_and_endpoint_resolver_inputs() {
    use std::ffi::OsString;

    let forwarded = filter_broker_spawn_env(vec![
        (
            OsString::from("SOLDR_CACHE_DIR"),
            OsString::from("/tmp/cache"),
        ),
        (OsString::from("HOME"), OsString::from("/mounted/home")),
        (
            OsString::from("XDG_CONFIG_HOME"),
            OsString::from("/mounted/config"),
        ),
        (
            OsString::from("XDG_RUNTIME_DIR"),
            OsString::from("/run/user/123"),
        ),
        (OsString::from("PATH"), OsString::from("/usr/bin")),
    ]);
    assert_eq!(
        forwarded,
        vec![
            (
                OsString::from("SOLDR_CACHE_DIR"),
                OsString::from("/tmp/cache")
            ),
            (OsString::from("HOME"), OsString::from("/mounted/home")),
            (
                OsString::from("XDG_CONFIG_HOME"),
                OsString::from("/mounted/config")
            ),
            (
                OsString::from("XDG_RUNTIME_DIR"),
                OsString::from("/run/user/123")
            ),
        ],
    );
}

/// soldr#3516: the broker overlays its own home identity onto every daemon
/// it launches, because `UserBaseline` would otherwise rebuild `HOME` from
/// the real account. `SOLDR_*` is deliberately left to the route's labels.
#[test]
fn daemon_home_identity_env_keeps_home_family_only() {
    use std::ffi::OsString;

    let forwarded = filter_daemon_home_identity_env(vec![
        (
            OsString::from("SOLDR_CACHE_DIR"),
            OsString::from("/broker/own/root"),
        ),
        (OsString::from("HOME"), OsString::from("/fixture/home")),
        (
            OsString::from("USERPROFILE"),
            OsString::from("/fixture/home"),
        ),
        (OsString::from("TMPDIR"), OsString::from("/fixture/tmp")),
        (OsString::from("XDG_CONFIG_HOME"), OsString::new()),
        (OsString::from("PATH"), OsString::from("/usr/bin")),
    ]);
    assert_eq!(
        forwarded,
        vec![
            (OsString::from("HOME"), OsString::from("/fixture/home")),
            (
                OsString::from("USERPROFILE"),
                OsString::from("/fixture/home")
            ),
            (OsString::from("TMPDIR"), OsString::from("/fixture/tmp")),
        ],
        "only non-empty home-identity variables cross; SOLDR_* comes from the route labels"
    );
}

#[test]
fn wrapper_invocation_is_never_eligible() {
    let _guard = ENV_LOCK.lock().unwrap();
    let raw_args = vec!["soldr".to_string(), "/usr/bin/rustc".to_string()];
    assert!(crate::wrapper::is_wrapper_invocation(&raw_args[1]));
    assert!(!front_door_broker_spawn_eligible(&raw_args));
}

#[test]
fn broker_subcommand_itself_does_not_recursively_spawn() {
    let _guard = ENV_LOCK.lock().unwrap();
    let raw_args = vec![
        "soldr".to_string(),
        "broker".to_string(),
        "serve".to_string(),
    ];
    assert!(!front_door_broker_spawn_eligible(&raw_args));
}

#[test]
fn teardown_commands_remain_broker_eligible() {
    let raw_args = vec![
        "soldr".to_string(),
        "cache".to_string(),
        "shutdown".to_string(),
        "--json".to_string(),
    ];
    assert!(front_door_broker_spawn_eligible(&raw_args));
    assert!(is_teardown_command(&raw_args));

    let flags_first = vec![
        "soldr".to_string(),
        "cache".to_string(),
        "--json".to_string(),
        "shutdown".to_string(),
    ];
    assert!(front_door_broker_spawn_eligible(&flags_first));
    assert!(is_teardown_command(&flags_first));

    let daemon_stop = vec![
        "soldr".to_string(),
        "daemon".to_string(),
        "stop".to_string(),
    ];
    assert!(front_door_broker_spawn_eligible(&daemon_stop));
    assert!(is_teardown_command(&daemon_stop));

    let status = vec!["soldr".to_string(), "status".to_string()];
    assert!(!is_teardown_command(&status));
}

// soldr#2388: the broker is unconditional — an ordinary invocation is
// always eligible (there is no opt-out).
#[test]
fn ordinary_invocation_is_eligible() {
    let _guard = ENV_LOCK.lock().unwrap();
    let raw_args = vec!["soldr".to_string(), "status".to_string()];
    assert!(front_door_broker_spawn_eligible(&raw_args));
}

#[test]
fn no_positional_arg_is_ineligible() {
    let _guard = ENV_LOCK.lock().unwrap();
    let raw_args = vec!["soldr".to_string()];
    assert!(!front_door_broker_spawn_eligible(&raw_args));
}

/// A flag first argument is not a command. `soldr --version` is the
/// load-bearing case: `global_upgrade::probe_version` runs it as a child
/// of every invocation in a `prefer_newer_global` checkout, and an
/// eligible `--version` made that probe spawn a broker under whatever
/// HOME it inherited -- including the isolated homes of the target-run
/// broker-absent tests, which then found "a broker" running.
#[test]
fn flag_first_argument_is_ineligible() {
    let _guard = ENV_LOCK.lock().unwrap();
    for flag in ["--version", "-V", "--help", "-h", "--as"] {
        let raw_args = vec!["soldr".to_string(), flag.to_string()];
        assert!(
            !front_door_broker_spawn_eligible(&raw_args),
            "flag-shaped first argument {flag} must not boot broker infrastructure"
        );
    }
}

/// Global pre-verb flags must not hide the command from broker
/// bringup: `soldr --debug cargo check` against a cold root used to
/// skip the spawn entirely, so every cacheable compile died with
/// "soldr broker is unreachable".
#[test]
fn global_flags_before_the_verb_stay_eligible() {
    let _guard = ENV_LOCK.lock().unwrap();
    let cases: &[&[&str]] = &[
        &["soldr", "--debug", "cargo", "check"],
        &["soldr", "--no-cache", "--debug", "cargo", "build"],
        &["soldr", "--trust-inherited-soldr-env", "cargo", "test"],
        &["soldr", "--allow-unpinned", "status"],
        &["soldr", "--zccache", "managed", "cargo", "build"],
        &["soldr", "--zccache=managed", "cargo", "build"],
        &["soldr", "--jobs", "4", "cargo", "build"],
    ];
    for case in cases {
        let raw_args: Vec<String> = case.iter().map(|arg| arg.to_string()).collect();
        assert!(
            front_door_broker_spawn_eligible(&raw_args),
            "global flags before the verb must stay broker-eligible: {case:?}"
        );
    }
    // Trailing global flags with no verb at all remain ineligible.
    let raw_args = vec!["soldr".to_string(), "--debug".to_string()];
    assert!(!front_door_broker_spawn_eligible(&raw_args));
    // The wrapper and broker exclusions still see through the flags.
    let raw_args: Vec<String> = ["soldr", "--debug", "broker", "status"]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
    assert!(!front_door_broker_spawn_eligible(&raw_args));
}

#[test]
fn ci_diagnostics_preserve_machine_readable_output() {
    assert!(!ci_endpoint_diagnostics_eligible(&[
        "soldr".into(),
        "env".into(),
        "--json".into(),
    ]));
    assert!(!ci_endpoint_diagnostics_eligible(&[
        "soldr".into(),
        "prepare".into(),
        "--github-env=output.env".into(),
    ]));
    assert!(!ci_endpoint_diagnostics_eligible(&[
        "soldr".into(),
        "env".into(),
        "--shell-export".into(),
    ]));
    assert!(ci_endpoint_diagnostics_eligible(&[
        "soldr".into(),
        "build".into(),
    ]));
}

/// soldr#2549 acceptance criterion: "an identity mismatch emits an
/// actionable warning naming `soldr broker remove`".
#[test]
fn image_mismatch_warning_is_actionable_and_names_the_remove_command() {
    let observed = format!("soldr-0.9.0-{}", "0".repeat(64));
    let expected = format!("soldr-0.9.0-{}", "1".repeat(64));
    let warning = broker_image_mismatch_warning(&observed, &expected);

    assert!(warning.contains(&observed), "{warning}");
    assert!(warning.contains(&expected), "{warning}");
    assert!(warning.contains(BROKER_REMOVE_COMMAND), "{warning}");
    assert_eq!(BROKER_REMOVE_COMMAND, "soldr broker remove");
    // Never promise a lifecycle action the front door no longer performs.
    for forbidden in ["replacing", "restarting", "stopping the broker"] {
        assert!(!warning.contains(forbidden), "{warning}");
    }
}

#[test]
fn ci_diagnostics_show_the_one_stable_path_derived_endpoint() {
    let diagnostics = BrokerEndpointDiagnostics {
        executable: std::path::PathBuf::from("/home/me/.soldr/broker/soldr-broker"),
        logical: "/home/me/.soldr/broker/soldr-broker.sock".into(),
        bind: "/home/me/.soldr/broker/soldr-broker.sock".into(),
        log: std::path::PathBuf::from("/home/me/.soldr/broker/broker-spawn.log"),
    };
    let rendered = render_ci_endpoint_diagnostics("github_actions", &diagnostics);
    assert!(rendered.contains("ci=github_actions"));
    assert!(rendered.contains("logical=/home/me/.soldr/broker/soldr-broker.sock"));
    assert!(rendered.contains("bind=/home/me/.soldr/broker/soldr-broker.sock"));
}

#[test]
fn known_bad_retirement_diagnostic_is_machine_readable_and_complete() {
    let crate::broker_policy::BrokerInstanceDecision::RetireKnownBadOlder(candidate) =
        crate::broker_policy::classify_broker_instance(
            &format!("soldr-0.9.0-{}", "a".repeat(64)),
            "0.9.1",
        )
    else {
        panic!("fixture must classify as the exact known-bad retirement");
    };
    let record = known_bad_broker_retirement_record(
        &candidate,
        crate::broker_cmd::KnownBadBrokerRetirementOutcome::Retired {
            broker_pid: 42,
            shutdown_path: crate::broker_cmd::BrokerShutdownPath::Cooperative,
            staged_image_removed: true,
        },
    );
    assert_eq!(record["schema_version"], 1);
    assert_eq!(record["event"], "known_bad_broker_retirement");
    assert_eq!(record["old_version"], "0.9.0");
    assert_eq!(record["new_version"], "0.9.1");
    assert_eq!(record["matched_range"], "=0.9.0");
    assert_eq!(record["shutdown_path"], "cooperative");
    assert_eq!(record["outcome"], "retired");
    assert_eq!(record["staged_image_removed"], true);
}

/// soldr#3193: the one opt-out from front-door spawning.
#[test]
fn the_autospawn_switch_is_off_only_for_an_exact_zero() {
    use std::ffi::OsStr;
    assert!(broker_enabled_for(None));
    assert!(broker_enabled_for(Some(OsStr::new(""))));
    assert!(broker_enabled_for(Some(OsStr::new("1"))));
    assert!(broker_enabled_for(Some(OsStr::new("false"))));
    assert!(!broker_enabled_for(Some(OsStr::new("0"))));
}

#[test]
fn archived_replay_never_starts_broker() {
    for args in [
        vec![
            "soldr",
            "replay-nextest",
            "--runner",
            "/runner",
            "--",
            "nextest",
            "--version",
        ],
        vec![
            "soldr",
            "--as",
            "0.0.1",
            "replay-nextest",
            "--runner",
            "/runner",
            "--",
            "nextest",
            "--version",
        ],
    ] {
        let raw: Vec<String> = args.into_iter().map(str::to_owned).collect();
        assert!(!front_door_command_shape(&raw));
    }
}
