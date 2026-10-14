//! Unit tests for the dispatch layer in `main.rs`: clap top-level
//! parsing, `--as` extraction, version normalisation, and the
//! self-relocate gate. Lives in a sibling file referenced via
//! `#[path]` so `main.rs` stays comfortably under the 1000-LOC ceiling.

use super::*;
use clap::{CommandFactory, Parser};
use std::ffi::{OsStr, OsString};
use std::sync::Mutex;

/// Serialises tests that mutate process-wide environment variables so
/// they do not race with each other under parallel `cargo test`. The
/// guard objects below restore the previous value on drop, but two
/// tests touching the same key concurrently would still observe each
/// other's mid-test state without this lock.
use crate::TEST_PROCESS_ENV_LOCK as ENV_LOCK;

#[test]
#[ignore = "subprocess helper"]
fn subprocess_maturin_build_lease_then_exit() {
    let root = std::env::var_os("SOLDR_TEST_MATURIN_LEASE_ROOT").expect("root");
    let ready = std::env::var_os("SOLDR_TEST_MATURIN_LEASE_READY").expect("ready");
    let paths = SoldrPaths::with_root(std::path::PathBuf::from(root));
    let args = vec!["pep517".to_string(), "build-wheel".to_string()];
    let _lease = acquire_maturin_build_lease(&paths, &args)
        .unwrap()
        .expect("PEP517 maturin must acquire a build lease");
    std::fs::write(ready, b"acquired").unwrap();
    std::process::exit(0);
}

#[test]
fn maturin_build_lease_defers_gc_and_survives_abrupt_exit() {
    let temp = tempfile::tempdir().unwrap();
    let paths = SoldrPaths::with_root(temp.path().join("owned"));
    let direct_args = vec!["build".to_string()];
    let lease = acquire_maturin_build_lease(&paths, &direct_args)
        .unwrap()
        .expect("direct maturin build lease");
    assert!(
        crate::cache_lib::build_active::MaintenanceLease::try_acquire(&paths)
            .unwrap()
            .is_none()
    );
    drop(lease);
    crate::cache_lib::build_active::MaintenanceLease::try_acquire(&paths)
        .unwrap()
        .expect("maintenance resumes after direct maturin");

    let ready = temp.path().join("ready");
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "soldr_main::tests::subprocess_maturin_build_lease_then_exit",
            "--nocapture",
        ])
        .env("SOLDR_TEST_MATURIN_LEASE_ROOT", &paths.root)
        .env("SOLDR_TEST_MATURIN_LEASE_READY", &ready)
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(std::fs::read(&ready).unwrap(), b"acquired");
    crate::cache_lib::build_active::MaintenanceLease::try_acquire(&paths)
        .unwrap()
        .expect("abrupt PEP517 process exit releases its OS lease");

    assert!(
        acquire_maturin_build_lease(&paths, &["--version".to_string()])
            .unwrap()
            .is_none()
    );
}

// soldr#1663: `EnvVarGuard` moved to the crate root so every module
// shares one panic-safe guard instead of re-implementing it.
use crate::EnvVarGuard;

#[test]
fn maturin_xwin_policy_prefers_blessed_toolchain_unless_overridden() {
    assert_eq!(
        maturin_xwin_policy("x86_64-pc-windows-msvc", None),
        Some("0")
    );
    assert_eq!(
        maturin_xwin_policy("aarch64-pc-windows-msvc", None),
        Some("0")
    );
    assert_eq!(
        maturin_xwin_policy("x86_64-pc-windows-msvc", Some("1")),
        None,
        "an explicit MATURIN_USE_XWIN value must remain caller-owned"
    );
    assert_eq!(maturin_xwin_policy("x86_64-unknown-linux-gnu", None), None);
}

#[test]
fn prepend_path_dirs_preserves_declared_priority() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().expect("tempdir");
    let shim = tmp.path().join("clang-shim");
    let llvm = tmp.path().join("llvm").join("bin");
    let inherited = tmp.path().join("inherited").join("bin");
    let inherited_path = std::env::join_paths([&inherited]).expect("inherited PATH");
    let _path = EnvVarGuard::set("PATH", inherited_path);

    prepend_path_dirs_to_env(&[shim.clone(), llvm.clone()]);

    let actual =
        std::env::split_paths(&std::env::var_os("PATH").expect("PATH")).collect::<Vec<_>>();
    assert_eq!(actual, vec![shim, llvm, inherited]);
}

#[test]
fn gc_cli_parses_summary_and_purge_modes() {
    let summary = Cli::try_parse_from(["soldr", "gc", "--json"]).unwrap();
    match summary.command {
        Commands::Gc {
            command: None,
            json,
            ..
        } => assert!(json, "gc --json should parse as summary JSON"),
        _ => panic!("expected gc summary command"),
    }

    let purge = Cli::try_parse_from([
        "soldr",
        "gc",
        "purge",
        "--all",
        "--older-than",
        "30d",
        "--larger-than",
        "1GB",
    ])
    .unwrap();
    match purge.command {
        Commands::Gc {
            command:
                Some(GcSubcommand::Purge {
                    all,
                    older_than,
                    larger_than,
                    ..
                }),
            ..
        } => {
            assert!(all);
            assert_eq!(older_than, "30d");
            assert_eq!(larger_than, "1GB");
        }
        _ => panic!("expected gc purge command"),
    }
}

#[test]
fn self_relocate_gate_targets_managed_cacheable_cargo_builds() {
    let _lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _wrapper = EnvVarGuard::remove(RUSTC_WRAPPER_OVERRIDE_ENV_VAR);

    assert!(should_self_relocate_for_invocation(&[
        "soldr".into(),
        "cargo".into(),
        "build".into(),
    ]));
    assert!(should_self_relocate_for_invocation(&[
        "soldr".into(),
        "--as".into(),
        env!("CARGO_PKG_VERSION").into(),
        "cargo".into(),
        "test".into(),
    ]));
    // --zccache <value> in either form is a value-taking flag; the
    // self-relocate parser must not stop at the value as if it were the
    // subcommand.
    assert!(should_self_relocate_for_invocation(&[
        "soldr".into(),
        "--zccache".into(),
        "system".into(),
        "cargo".into(),
        "build".into(),
    ]));
    assert!(should_self_relocate_for_invocation(&[
        "soldr".into(),
        "--zccache=system".into(),
        "cargo".into(),
        "build".into(),
    ]));
    assert!(!should_self_relocate_for_invocation(&[
        "soldr".into(),
        "cargo".into(),
        "--version".into(),
    ]));
    assert!(!should_self_relocate_for_invocation(&[
        "soldr".into(),
        "--no-cache".into(),
        "cargo".into(),
        "build".into(),
    ]));
    assert!(!should_self_relocate_for_invocation(&[
        "soldr".into(),
        "version".into(),
    ]));

    let _custom = EnvVarGuard::set(RUSTC_WRAPPER_OVERRIDE_ENV_VAR, "sccache");
    assert!(!should_self_relocate_for_invocation(&[
        "soldr".into(),
        "cargo".into(),
        "build".into(),
    ]));
}

#[test]
fn cli_parses_zccache_flag_values() {
    let default = Cli::try_parse_from(["soldr", "cargo", "build"]).unwrap();
    assert_eq!(default.zccache, ZccacheSourceArg::Managed);

    let system = Cli::try_parse_from(["soldr", "--zccache=system", "cargo", "build"]).unwrap();
    assert_eq!(system.zccache, ZccacheSourceArg::System);

    let managed = Cli::try_parse_from(["soldr", "--zccache", "managed", "cargo", "build"]).unwrap();
    assert_eq!(managed.zccache, ZccacheSourceArg::Managed);

    let invalid = Cli::try_parse_from(["soldr", "--zccache=bogus", "cargo", "build"]);
    assert!(invalid.is_err(), "unknown zccache source must fail clap");
}

#[test]
fn cli_parses_trust_inherited_soldr_env_flag() {
    let default = Cli::try_parse_from(["soldr", "cargo", "build"]).unwrap();
    assert!(!default.trust_inherited_soldr_env);

    let trusted =
        Cli::try_parse_from(["soldr", "--trust-inherited-soldr-env", "cargo", "build"]).unwrap();
    assert!(trusted.trust_inherited_soldr_env);
}

#[test]
fn trust_inherited_flag_is_bound_to_its_env_var_with_owned_flag_rules() {
    // soldr#2748 option C: one binding, visible in --help, parsed like every
    // other owned SOLDR_* switch (soldr#2740) rather than clap's boolish set.
    let command = Cli::command();
    let arg = command
        .get_arguments()
        .find(|a| a.get_id() == "trust_inherited_soldr_env")
        .unwrap();
    assert_eq!(
        arg.get_env().and_then(|v| v.to_str()),
        Some(crate::TRUST_INHERITED_SOLDR_ENV_VAR)
    );
    for (raw, want) in [("1", true), ("on", true), ("off", false), ("maybe", false)] {
        assert_eq!(crate::cli_args::owned_flag(raw), Ok(want), "{raw}");
    }
}

#[test]
fn root_help_groups_commands_and_keeps_lines_short() {
    let help = Cli::command().render_help().to_string();

    let common = help.find("Common commands:").unwrap();
    let less_common = help.find("Less common toolchain commands:").unwrap();
    let cache = help.find("soldr cache & build:").unwrap();
    let ops = help.find("soldr ops & infrastructure:").unwrap();
    assert!(
        common < less_common && less_common < cache && cache < ops,
        "command groups should render in familiarity order:\n{help}"
    );

    assert!(help.contains("  cargo                  Run cargo through soldr"));
    assert!(help.contains("  cook                   Prebuild dependencies"));
    assert!(help.contains("Examples:\n  soldr cargo build --release"));
    assert!(
        !help.contains("\nCommands:\n"),
        "flat command list returned:\n{help}"
    );
    assert!(
        !help.contains("\n  version"),
        "version subcommand should be hidden from the index:\n{help}"
    );

    let long_lines: Vec<_> = help.lines().filter(|line| line.len() > 80).collect();
    assert!(
        long_lines.is_empty(),
        "root help lines should fit 80 columns: {long_lines:?}\n{help}"
    );
}

#[test]
fn long_root_help_expands_intro_and_zccache_details() {
    let help = Cli::command().render_long_help().to_string();

    assert!(help.contains("soldr wraps cargo and the rustup toolchain"));
    assert!(help.contains("Select the zccache integration backing the compilation cache."));
    assert!(help.contains("`managed` (default) uses the zccache service compiled into"));
    assert!(help.contains("`system` is retained as a compatibility spelling"));
}

#[test]
fn verbose_command_details_live_on_subcommand_help() {
    let root = Cli::command().render_help().to_string();
    assert!(!root.contains("https://static.rust-lang.org/rustup/dist"));
    assert!(!root.contains("SOLDR_NO_BOOTSTRAP"));

    let mut cmd = Cli::command();
    let bootstrap = cmd
        .find_subcommand_mut("bootstrap")
        .expect("bootstrap subcommand should exist");
    let help = bootstrap.render_long_help().to_string();
    assert!(help.contains("https://static.rust-lang.org/rustup/dist"));
    assert!(help.contains("SOLDR_NO_BOOTSTRAP"));
}

#[test]
fn extract_as_pin_extracts_space_separated_flag_before_subcommand() {
    let (version, rest) = extract_as_pin(&[
        "--as".into(),
        "0.5.2".into(),
        "cargo".into(),
        "build".into(),
    ])
    .unwrap();
    assert_eq!(version, Some("0.5.2".into()));
    assert_eq!(rest, vec!["cargo".to_string(), "build".into()]);
}

#[test]
fn extract_as_pin_extracts_equals_form() {
    let (version, rest) =
        extract_as_pin(&["--as=0.5.2".into(), "cargo".into(), "build".into()]).unwrap();
    assert_eq!(version, Some("0.5.2".into()));
    assert_eq!(rest, vec!["cargo".to_string(), "build".into()]);
}

#[test]
fn extract_as_pin_preserves_other_leading_flags() {
    let (version, rest) = extract_as_pin(&[
        "--no-cache".into(),
        "--as".into(),
        "0.5.2".into(),
        "cargo".into(),
    ])
    .unwrap();
    assert_eq!(version, Some("0.5.2".into()));
    assert_eq!(rest, vec!["--no-cache".to_string(), "cargo".into()]);
}

#[test]
fn extract_as_pin_ignores_flag_after_subcommand() {
    let args = vec!["cargo".into(), "--as".into(), "0.5.2".into()];
    let (version, rest) = extract_as_pin(&args).unwrap();
    assert_eq!(version, None);
    assert_eq!(rest, args);
}

#[test]
fn extract_as_pin_ignores_flag_after_passthrough_separator() {
    let args = vec!["cargo".into(), "--".into(), "--as".into(), "0.5.2".into()];
    let (version, rest) = extract_as_pin(&args).unwrap();
    assert_eq!(version, None);
    assert_eq!(rest, args);
}

#[test]
fn extract_as_pin_rejects_missing_value() {
    let err = extract_as_pin(&["--as".into()]).unwrap_err();
    assert!(err.to_string().contains("requires a version"));
}

#[test]
fn extract_as_pin_rejects_empty_value() {
    let err = extract_as_pin(&["--as".into(), "".into()]).unwrap_err();
    assert!(err.to_string().contains("must not be empty"));
    let err2 = extract_as_pin(&["--as=".into()]).unwrap_err();
    assert!(err2.to_string().contains("requires a version"));
}

#[test]
fn extract_as_pin_rejects_duplicate_flag() {
    let err = extract_as_pin(&["--as".into(), "0.5.2".into(), "--as=0.4.0".into()]).unwrap_err();
    assert!(err.to_string().contains("more than once"));
}

#[test]
fn normalize_version_strips_leading_v() {
    assert_eq!(normalize_version("0.5.2"), "0.5.2");
    assert_eq!(normalize_version("v0.5.2"), "0.5.2");
    assert_eq!(normalize_version("  v0.5.2 "), "0.5.2");
}

#[test]
fn should_trampoline_matches_current_version_as_no_op() {
    assert!(!should_trampoline(env!("CARGO_PKG_VERSION")));
    assert!(!should_trampoline(&format!(
        "v{}",
        env!("CARGO_PKG_VERSION")
    )));
    assert!(should_trampoline("0.0.0-not-this-version"));
}

#[test]
fn soldr_builtin_verbs_match_clap_subcommands_and_aliases() {
    // Issue #412: the SOLDR_BUILTIN_VERBS const drives the fuzzy
    // suggestion engine. If it drifts from the actual `Commands`
    // enum (someone adds a verb but forgets to update the list, or
    // renames one without updating the alias entry), users get
    // either bogus "did you mean?" hints or NO hint when one would
    // have helped. This test walks clap's discovered subcommands +
    // their `#[command(alias = ...)]` annotations and asserts every
    // discovered name is in the const, and vice versa.
    use clap::CommandFactory;

    let cmd = Cli::command();
    let mut discovered: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for sub in cmd.get_subcommands() {
        discovered.insert(sub.get_name().to_string());
        for alias in sub.get_all_aliases() {
            discovered.insert(alias.to_string());
        }
    }
    let declared: std::collections::BTreeSet<String> =
        SOLDR_BUILTIN_VERBS.iter().map(|s| s.to_string()).collect();

    let missing_from_const: Vec<_> = discovered.difference(&declared).collect();
    let extra_in_const: Vec<_> = declared.difference(&discovered).collect();
    assert!(
        missing_from_const.is_empty() && extra_in_const.is_empty(),
        "SOLDR_BUILTIN_VERBS is out of sync with the Commands enum.\n  \
         missing from const (clap knows, fuzzy-match doesn't): {missing_from_const:?}\n  \
         extra in const   (fuzzy-match knows, clap doesn't): {extra_in_const:?}\n\
         Update SOLDR_BUILTIN_VERBS in `main.rs` to match.",
    );
}

#[test]
fn fuzzy_match_with_soldr_verbs_recognizes_issue_412_examples() {
    // Black-box sanity check that the in-tree const list + the
    // shared fuzzy engine actually produce the suggestions the issue
    // documents as acceptance criteria. Cheap to keep alongside the
    // pure-logic tests in fuzzy_match.rs because it locks in the
    // const → suggestion contract end-to-end.
    use crate::fuzzy_match::suggest_close_match;

    // soldr#1368 removed `install-zccache`; use surviving multi-word
    // verbs to exercise the same fuzzy-match contract.
    let typo = "session-startt";
    let suggestion = suggest_close_match(typo, SOLDR_BUILTIN_VERBS);
    assert_eq!(
        suggestion,
        Some("session-start"),
        "expected session-start for {typo:?}; got {suggestion:?}",
    );

    let typo = "sessionend";
    assert_eq!(
        suggest_close_match(typo, SOLDR_BUILTIN_VERBS),
        Some("session-end"),
    );

    // Acceptance criterion: completely-different verb produces no
    // hint (no false suggestion).
    assert_eq!(
        suggest_close_match("completely-made-up-name", SOLDR_BUILTIN_VERBS),
        None,
    );

    // Acceptance criterion: a verb that IS a known built-in must
    // NOT produce a hint — the regular dispatch handles it. Tested
    // by suggest_close_match returning None on exact match.
    assert_eq!(suggest_close_match("doctor", SOLDR_BUILTIN_VERBS), None);
}

/// Issue #683 (parent #682, phase 1): bare cargo-subcommand shorthand.
/// `soldr nextest run` must resolve as `soldr cargo nextest run`, NOT
/// fall through to a crates.io fetch for a literally-named `nextest`
/// crate. The dispatch decision in `main.rs` keys on
/// `parse_tool_spec` + `lookup_by_cargo_subcommand`; these tests
/// exercise that contract directly so the surface stays observable
/// without spawning a full `soldr` process.
#[test]
fn bare_shorthand_recognizes_every_known_cargo_subcommand() {
    // Every entry in `KNOWN_TOOLS` with a `cargo_subcommand: Some(_)`
    // must be reachable via the bare verb (sans `@version`). The
    // list is intentionally hard-coded — adding a new
    // `cargo_subcommand` to the registry without also adding it here
    // is the kind of drift this test exists to catch.
    let expected = [
        "nextest",
        "deny",
        "audit",
        "llvm-cov",
        "udeps",
        "semver-checks",
        "expand",
        "watch",
        "chef",
        "zigbuild",
        "xwin",
        "binstall",
        "machete",
        "dylint",
    ];
    for verb in expected {
        let (crate_name, version) = parse_tool_spec(verb);
        assert!(
            matches!(version, VersionSpec::Latest),
            "bare verb {verb:?} must parse to VersionSpec::Latest"
        );
        assert!(
            crate::fetch::lookup_by_cargo_subcommand(&crate_name).is_some(),
            "bare verb {verb:?} must be matched by lookup_by_cargo_subcommand"
        );
    }
}

#[test]
fn bare_shorthand_skips_when_user_pinned_a_version() {
    // `soldr nextest@0.9.x` keeps the existing External fetch path
    // (which today errors with "no crate named nextest"). The
    // shorthand only fires for unversioned forms because the cargo
    // front door has no per-invocation version knob — pins are
    // managed in `KNOWN_TOOLS::pinned_version`.
    let (crate_name, version) = parse_tool_spec("nextest@0.9.x");
    assert_eq!(crate_name, "nextest");
    assert!(
        matches!(version, VersionSpec::Exact(_)),
        "pinned bare verb must parse to VersionSpec::Exact"
    );
    // Sanity: the lookup still matches the verb itself, so the gate
    // condition `matches!(version, VersionSpec::Latest) && lookup…`
    // depends entirely on the version arm. If a future refactor flips
    // this to use the lookup result alone, this test will surface
    // that mistake.
    assert!(crate::fetch::lookup_by_cargo_subcommand(&crate_name).is_some());
}

#[test]
fn bare_shorthand_does_not_capture_unrelated_verbs() {
    // Negative coverage for the phase-1 (cargo subcommand) hop: a
    // verb that ISN'T in `KNOWN_TOOLS::cargo_subcommand` must NOT
    // be matched by `lookup_by_cargo_subcommand`. Top-level fetch
    // tools (cross, mdbook, bacon) and made-up names go to External;
    // cargo built-ins (build, test) take the phase-2 hop instead.
    for verb in [
        "cross",
        "mdbook",
        "bacon",
        "completely-made-up-name",
        "build",
        "test",
    ] {
        let (crate_name, _) = parse_tool_spec(verb);
        assert!(
            crate::fetch::lookup_by_cargo_subcommand(&crate_name).is_none(),
            "verb {verb:?} must NOT be matched as a cargo subcommand in phase 1"
        );
    }
}

/// soldr#3637: `package` and `info` are cargo built-ins and must route to cargo.
#[test]
fn cargo_builtin_package_and_info_route_to_cargo() {
    for verb in ["package", "info"] {
        assert!(
            is_cargo_builtin_verb(verb),
            "{verb:?} must be a cargo built-in verb"
        );
        let (crate_name, version) = parse_tool_spec(verb);
        assert_eq!(crate_name, verb);
        assert!(matches!(version, VersionSpec::Latest));
    }
}

/// Issue #685 (parent #682, phase 2): bare cargo-built-in
/// shorthand. `soldr build` / `soldr test` / `soldr clippy` etc.
/// must resolve as `soldr cargo <verb>` via the External arm,
/// NOT fall through to a doomed crates.io fetch. The dispatch
/// decision keys on `parse_tool_spec` + `is_cargo_builtin_verb`;
/// these tests exercise that contract directly.
#[test]
fn cargo_builtin_shorthand_covers_every_verb_in_the_const() {
    // Every entry in `CARGO_BUILTIN_VERBS` must round-trip through
    // `is_cargo_builtin_verb` and through `parse_tool_spec` as a
    // bare, version-unpinned form. The list is intentionally
    // hard-coded — adding a new cargo verb to the const without
    // also adding it here is the kind of drift this test catches.
    let expected = [
        // `build` deliberately not in this list: soldr#1012 PR 1
        // promoted `build` to a soldr-native verb (clap-captured before
        // External). It joins clean/config/version as a collision verb.
        "test",
        "check",
        "run",
        "bench",
        "doc",
        "fmt",
        "clippy",
        "tree",
        "update",
        "fix",
        "add",
        "remove",
        "metadata",
        "pkgid",
        "search",
        "vendor",
        "yank",
        "owner",
        "login",
        "logout",
        "init",
        "new",
        "generate-lockfile",
        "verify-project",
        "locate-project",
        "report",
        // `install` deliberately not in this list: soldr#2310 promoted
        // `install` to a soldr-native verb (`Commands::Install`), clap-
        // captured before the External arm runs.
        "uninstall",
        "publish",
        // soldr#3637: cargo built-ins previously missing; without them they fell through to a crates.io tool fetch.
        "package",
        "info",
    ];
    for verb in expected {
        let (crate_name, version) = parse_tool_spec(verb);
        assert!(
            matches!(version, VersionSpec::Latest),
            "bare verb {verb:?} must parse to VersionSpec::Latest"
        );
        assert!(
            is_cargo_builtin_verb(&crate_name),
            "bare cargo built-in verb {verb:?} must be matched by is_cargo_builtin_verb"
        );
    }
    // And the reverse: every const entry is in the expected list.
    // Cheap, but if a future cargo gains a new verb and someone adds
    // it to the const without updating tests, this fails loudly.
    for verb in CARGO_BUILTIN_VERBS {
        assert!(
            expected.contains(verb),
            "CARGO_BUILTIN_VERBS gained {verb:?} without a test update"
        );
    }
}

#[test]
fn cargo_builtin_shorthand_excludes_soldr_native_collision_verbs() {
    // Collision verbs (`build`, `clean`, `config`, `version`) own a
    // soldr-native meaning and MUST NOT be remapped to cargo by the
    // phase-2 hop. Clap captures them before the External arm runs,
    // but we also assert the const itself excludes them — defense in
    // depth + intent documentation. `build` joined this list in
    // soldr#1012 PR 1 as the blessed-default surface for cross-compile.
    for collision_verb in ["build", "clean", "config", "version"] {
        assert!(
            !is_cargo_builtin_verb(collision_verb),
            "soldr-native verb {collision_verb:?} must NOT be in CARGO_BUILTIN_VERBS"
        );
    }
}

#[test]
fn cargo_builtin_shorthand_skips_when_user_pinned_a_version() {
    // `soldr test@1.0` keeps the existing External fetch path —
    // cargo built-ins have no per-invocation version dimension and
    // the parse-time `@version` form is reserved for the
    // crate-fetch path. Same shape as the phase-1
    // `bare_shorthand_skips_when_user_pinned_a_version` test.
    //
    // (Pre-soldr#1012 this test used `build@1.0`, but `build` is
    // now a soldr-native verb captured by clap before the External
    // arm runs, so the version-pin code path no longer applies to
    // it. `test` is the next-shortest cargo-builtin still in the
    // const and exercises the exact same logic.)
    let (crate_name, version) = parse_tool_spec("test@1.0");
    assert_eq!(crate_name, "test");
    assert!(
        matches!(version, VersionSpec::Exact(_)),
        "pinned bare verb must parse to VersionSpec::Exact"
    );
    // Sanity: the lookup still matches the verb itself, so the gate
    // condition `matches!(version, VersionSpec::Latest) && is_…`
    // depends entirely on the version arm.
    assert!(is_cargo_builtin_verb(&crate_name));
}

#[test]
fn install_is_soldr_native_not_a_cargo_builtin() {
    // soldr#2310 promoted `install` to Commands::Install (clap captures it
    // before the External arm), so it must NOT be a cargo built-in anymore.
    // `soldr install <github-url|path>` is the prebuilt-first tool installer;
    // `soldr cargo install <crate>` remains the crates.io passthrough.
    assert!(
        !is_cargo_builtin_verb("install"),
        "soldr-native `install` must NOT be in CARGO_BUILTIN_VERBS (soldr#2310)"
    );
}

#[test]
fn cargo_builtin_shorthand_does_not_capture_other_verbs() {
    // Negative coverage. None of:
    //   - phase-1 known cargo subcommands (handled by their own hop)
    //   - top-level fetch tools (cross, mdbook, ...)
    //   - made-up names
    //   - soldr-native verbs
    // should be captured as cargo built-ins.
    for verb in [
        "nextest",  // phase-1 known cargo subcommand
        "zigbuild", // phase-1 known cargo subcommand
        "cross",    // top-level fetch tool
        "mdbook",   // top-level fetch tool
        "bacon",    // top-level fetch tool
        "completely-made-up-name",
        "clean",   // soldr-native
        "config",  // soldr-native
        "version", // soldr-native
        "doctor",  // soldr-native
    ] {
        assert!(
            !is_cargo_builtin_verb(verb),
            "verb {verb:?} must NOT be classified as a cargo built-in"
        );
    }
}

// soldr#2519 deleted the soldr#882 dispatch tests along with the functions
// they covered (`pick_cross_subcommand`, `rewrite_build_args_for_subcommand`).
// Those only ever produced a subcommand when `SOLDR_USE_LEGACY_{XWIN,ZIGBUILD}`
// was set; cross builds now always use plain `cargo build` against the
// blessed sysroot, and `blessed_build_tests.rs` asserts the removed toggle
// no longer changes MSVC prep.

#[test]
fn insert_cargo_config_args_after_plain_build_verb() {
    let args = vec![
        "build".to_string(),
        "--target".to_string(),
        "x86_64-unknown-linux-gnu".to_string(),
    ];
    let config = vec!["--config".to_string(), "target.x.foo.bar=1".to_string()];
    let out = insert_cargo_config_args(args, &config);
    assert_eq!(
        out,
        vec![
            "build",
            "--config",
            "target.x.foo.bar=1",
            "--target",
            "x86_64-unknown-linux-gnu",
        ]
    );
}

#[test]
fn insert_cargo_config_args_after_zigbuild_verb() {
    let args = vec![
        "zigbuild".to_string(),
        "--target".to_string(),
        "x86_64-unknown-linux-musl".to_string(),
    ];
    let config = vec!["--config".to_string(), "target.x.foo.bar=1".to_string()];
    let out = insert_cargo_config_args(args, &config);
    assert_eq!(
        out,
        vec![
            "zigbuild",
            "--config",
            "target.x.foo.bar=1",
            "--target",
            "x86_64-unknown-linux-musl",
        ]
    );
}

#[test]
fn insert_cargo_config_args_after_xwin_build_pair() {
    let args = vec![
        "xwin".to_string(),
        "build".to_string(),
        "--target".to_string(),
        "x86_64-pc-windows-msvc".to_string(),
    ];
    let config = vec!["--config".to_string(), "target.x.foo.bar=1".to_string()];
    let out = insert_cargo_config_args(args, &config);
    assert_eq!(
        out,
        vec![
            "xwin",
            "build",
            "--config",
            "target.x.foo.bar=1",
            "--target",
            "x86_64-pc-windows-msvc",
        ]
    );
}

#[test]
fn insert_cargo_config_args_after_nextest_inner_command() {
    let args = vec![
        "nextest".to_string(),
        "--color".to_string(),
        "never".to_string(),
        "archive".to_string(),
        "--target".to_string(),
        "aarch64-unknown-linux-gnu".to_string(),
    ];
    let config = vec!["--config".to_string(), "target.x.foo.bar=1".to_string()];
    let out = insert_cargo_config_args(args, &config);
    assert_eq!(
        out,
        vec![
            "nextest",
            "--color",
            "never",
            "archive",
            "--config",
            "target.x.foo.bar=1",
            "--target",
            "aarch64-unknown-linux-gnu",
        ]
    );
}

#[test]
fn maturin_cargo_config_stays_before_rustc_separator() {
    let mut args = vec![
        "build".to_string(),
        "--".to_string(),
        "-C".to_string(),
        "target-cpu=native".to_string(),
    ];
    crate::target_lifecycle::insert_args_before_separator(
        &mut args,
        vec!["--config".to_string(), "target.demo=value".to_string()],
    );
    assert_eq!(
        args,
        [
            "build",
            "--config",
            "target.demo=value",
            "--",
            "-C",
            "target-cpu=native",
        ]
    );
}

// soldr#1878: the PEP 517 build log arrived empty of maturin output because
// the relay wrote to a block-buffered stdout and the maturin lane ends in
// `std::process::exit`, which never runs the destructor that would flush it.
//
// Asserting on the written bytes cannot catch that -- the bytes *were*
// written, they were just discarded at exit. So these assert the flush.

/// Writer that records whether it was flushed after its last write.
struct FlushRecorder {
    written: Vec<u8>,
    flushed_after_last_write: bool,
}

impl FlushRecorder {
    fn new() -> Self {
        Self {
            written: Vec::new(),
            flushed_after_last_write: false,
        }
    }
}

impl std::io::Write for FlushRecorder {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.written.extend_from_slice(buf);
        self.flushed_after_last_write = false;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.flushed_after_last_write = true;
        Ok(())
    }
}

#[test]
fn relaying_child_output_flushes_both_streams() {
    let mut out = FlushRecorder::new();
    let mut err = FlushRecorder::new();

    relay_child_output(
        b"compiled ok\n",
        b"error: could not compile\n",
        &mut out,
        &mut err,
    )
    .expect("relay must succeed");

    assert_eq!(out.written, b"compiled ok\n");
    assert_eq!(err.written, b"error: could not compile\n");
    assert!(
        out.flushed_after_last_write,
        "stdout must be flushed: the maturin lane exits via process::exit, \
         which skips the destructor that would otherwise flush it (soldr#1878)"
    );
    assert!(
        err.flushed_after_last_write,
        "stderr must be flushed for the same reason"
    );
}

#[test]
fn relaying_empty_child_output_still_flushes() {
    // A child that failed without writing anything is exactly the soldr#1878
    // case; the relay must not skip the flush just because there are no bytes.
    let mut out = FlushRecorder::new();
    let mut err = FlushRecorder::new();

    relay_child_output(b"", b"", &mut out, &mut err).expect("relay must succeed");

    assert!(out.flushed_after_last_write && err.flushed_after_last_write);
}

#[test]
fn utf8_args_are_collected_unchanged() {
    let args = ["soldr", "cargo", "build", "--release"]
        .into_iter()
        .map(std::ffi::OsString::from);
    assert_eq!(
        collect_utf8_args(args).expect("utf-8 argv"),
        vec!["soldr", "cargo", "build", "--release"]
    );
}

/// soldr#2883: the route budget must outlast the slowest lane's real work.
///
/// windows-gnu measured a 64s route start that passed at 65.2s overall, and
/// 60.2s+ on the runs a 60s budget cut off. Sizing to "observed plus epsilon"
/// is what put it at the cliff in the first place, so the bound is well clear
/// of the measurement rather than just past it.
///
/// The upper bound matters too: this is a wait a human is sitting through, and
/// an unbounded one would turn a genuinely wedged route into a hang instead of
/// a diagnostic.
#[test]
fn the_daemon_start_route_budget_clears_the_slowest_measured_lane() {
    let budget = DAEMON_START_ROUTE_BUDGET.as_secs();
    assert!(
        budget >= 120,
        "windows-gnu measured 64s of real route start; {budget}s leaves no room \
         for a slower host and reproduces soldr#2883"
    );
    assert!(
        budget <= 300,
        "an explicit `daemon start` still has to fail rather than hang: {budget}s"
    );
}
