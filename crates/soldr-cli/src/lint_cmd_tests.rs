use super::*;

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).into()).collect()
}

#[test]
fn fmt_fix_formats_in_place_before_the_fmt_check() {
    let mut plan = LintPlan::parse(&[]).unwrap();
    assert!(!plan.check_only);
    plan.fmt_fix = true;
    let steps = plan.rust_step_args(false, &[]).unwrap();
    assert_eq!(
        steps[0],
        (
            LintStep::Fmt,
            "fmt-fix".to_string(),
            strings(&["fmt", "--all", "--", "-l"])
        )
    );
    assert_eq!(steps[1].2, strings(&["fmt", "--all", "--", "--check"]));
}

#[test]
fn check_flag_is_recorded_and_not_passed_to_cargo() {
    let plan = LintPlan::parse(&strings(&["--check", "-p", "x"])).unwrap();
    assert!(plan.check_only);
    assert_eq!(plan.scope, strings(&["-p", "x"]));
}

#[test]
fn fixed_files_keeps_only_rust_paths() {
    let output = "src/a.rs\n  soldr[cache] x [HIT!]\n\nsrc/b c.rs\nDiff in src/d.rs:3:\n";
    assert_eq!(
        lint_steps::fixed_files(output),
        strings(&["src/a.rs", "src/b c.rs"])
    );
}

#[test]
fn default_suite_has_one_clippy_scope_without_check() {
    let plan = LintPlan::parse(&[]).unwrap();
    assert_eq!(plan.mode, LintMode::Rust);
    assert_eq!(
        plan.rust_steps(false, &[]).unwrap(),
        vec![
            strings(&["fmt", "--all", "--", "--check"]),
            strings(&[
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings"
            ]),
            strings(&["dylint", "--all", "--", "--workspace", "--all-targets"]),
        ]
    );
}

#[test]
fn dylint_stage_passes_the_callers_target_scope_through() {
    // soldr#3930. A pre-expansion Dylint must not see test or bench
    // targets, so `--lib --bins` replaces the forced `--all-targets`.
    let plan = LintPlan::parse(&strings(&["rust", "--lib", "--bins"])).unwrap();
    let steps = plan.rust_steps(false, &[]).unwrap();
    assert_eq!(
        steps[2],
        strings(&["dylint", "--all", "--", "--workspace", "--lib", "--bins"])
    );
}

#[test]
fn dylint_stage_keeps_other_selectors_without_widening() {
    let plan = LintPlan::parse(&strings(&["rust", "--tests", "--package", "soldr-cli"])).unwrap();
    let steps = plan.rust_steps(false, &[]).unwrap();
    assert_eq!(
        steps[2],
        strings(&[
            "dylint",
            "--all",
            "--",
            "--workspace",
            "--tests",
            "--package",
            "soldr-cli"
        ])
    );
    assert!(!steps[2].contains(&"--all-targets".to_string()));
}

#[test]
fn target_selector_detection_covers_both_spellings() {
    for scope in [
        strings(&["--lib"]),
        strings(&["--bin=soldr"]),
        strings(&["--example", "demo"]),
        strings(&["--all-targets"]),
    ] {
        assert!(has_target_selector(&scope), "{scope:?}");
    }
    for scope in [
        Vec::new(),
        strings(&["--package", "soldr-cli"]),
        strings(&["--target", "x86_64-unknown-linux-musl"]),
        strings(&["--all-features"]),
    ] {
        assert!(!has_target_selector(&scope), "{scope:?}");
    }
}

#[test]
fn all_suite_uses_all_features_for_compiler_steps() {
    let plan = LintPlan::parse(&strings(&["all", "--package", "soldr-cli"])).unwrap();
    let rust = plan.rust_steps(true, &[]).unwrap();
    assert!(rust[1].contains(&"--all-features".into()));
    assert!(rust[2].contains(&"--all-features".into()));
    let exhaustive = plan.exhaustive_steps().unwrap();
    assert_eq!(exhaustive[0][0], "udeps");
    assert!(exhaustive[0].contains(&"--all-features".into()));
    assert_eq!(exhaustive[1], strings(&["semver-checks"]));
}

#[test]
fn dependency_suite_is_limited_to_shared_manifest_scope() {
    let plan = LintPlan::parse(&strings(&["deps", "--manifest-path", "Cargo.toml"])).unwrap();
    assert_eq!(
        plan.dependency_steps().unwrap(),
        vec![
            strings(&["--manifest-path", "Cargo.toml", "deny", "check"]),
            strings(&["--manifest-path", "Cargo.toml", "audit"]),
            strings(&["--manifest-path", "Cargo.toml", "machete"]),
        ]
    );
    let invalid = LintPlan::parse(&strings(&["deps", "--all-features"])).unwrap();
    assert!(invalid.dependency_steps().is_err());
}

#[test]
fn unknown_suite_is_rejected() {
    let error = LintPlan::parse(&strings(&["everything"])).unwrap_err();
    assert!(error.to_string().contains("unknown suite"));
}

#[test]
fn ci_suite_is_parsed_with_default_human_format() {
    let plan = LintPlan::parse(&strings(&["ci"])).unwrap();
    assert_eq!(plan.mode, LintMode::Ci);
    assert_eq!(plan.ci_format, OutputFormat::Human);
    assert!(plan.scope.is_empty());
}

#[test]
fn ci_suite_parses_format_flag_both_spellings() {
    let split = LintPlan::parse(&strings(&["ci", "--format", "json"])).unwrap();
    assert_eq!(split.mode, LintMode::Ci);
    assert_eq!(split.ci_format, OutputFormat::Json);
    let joined = LintPlan::parse(&strings(&["ci", "--format=json"])).unwrap();
    assert_eq!(joined.ci_format, OutputFormat::Json);
}

#[test]
fn ci_suite_rejects_cargo_scope_flags() {
    // `--package` is a cargo scope flag; the ci suite must not accept it.
    let error = LintPlan::parse(&strings(&["ci", "--package", "soldr-cli"])).unwrap_err();
    assert!(error.to_string().contains("unexpected argument"));
    let bad_format = LintPlan::parse(&strings(&["ci", "--format", "yaml"])).unwrap_err();
    assert!(bad_format.to_string().contains("unknown --format"));
}

#[test]
fn all_suite_mode_is_all() {
    // `lint all` must reach LintMode::All, which now also runs the CI
    // suite before the compile/dep steps.
    let plan = LintPlan::parse(&strings(&["all"])).unwrap();
    assert_eq!(plan.mode, LintMode::All);
}

/// Write a minimal `Cargo.toml` fixture declaring `targets` (or none,
/// when `targets` is `None`) and return its path.
fn write_manifest_fixture(dir: &std::path::Path, targets: Option<&[&str]>) -> String {
    let body = match targets {
        Some(list) => format!(
            "[workspace]\n\n[workspace.metadata.soldr]\ntargets = [{}]\n",
            list.iter()
                .map(|t| format!("{t:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        None => "[package]\nname = \"thing\"\nversion = \"0.1.0\"\n".to_string(),
    };
    let path = dir.join("Cargo.toml");
    std::fs::write(&path, body).expect("write fixture");
    path.to_str().expect("utf8 path").to_string()
}

#[test]
fn declared_targets_produce_one_clippy_per_target() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let manifest = write_manifest_fixture(
        tmp.path(),
        Some(&["x86_64-pc-windows-msvc", "aarch64-apple-darwin"]),
    );
    let plan = LintPlan::parse(&strings(&["rust", "--manifest-path", &manifest])).unwrap();
    let cross = plan.resolve_cross_targets().unwrap();
    assert_eq!(
        cross,
        vec![
            "x86_64-pc-windows-msvc".to_string(),
            "aarch64-apple-darwin".to_string(),
        ]
    );
    let steps = plan.rust_steps(false, &cross).unwrap();
    // fmt, host clippy, two cross clippy steps, dylint.
    assert_eq!(steps.len(), 5);
    assert!(steps[2].contains(&"--target".to_string()));
    assert!(steps[2].contains(&"x86_64-pc-windows-msvc".to_string()));
    assert!(steps[3].contains(&"--target".to_string()));
    assert!(steps[3].contains(&"aarch64-apple-darwin".to_string()));
    assert_eq!(steps[4][0], "dylint", "Dylint stays host-only, last");
}

#[test]
fn cross_dylint_checks_each_selected_target_with_the_host_pass_first() {
    let plan = LintPlan::parse(&strings(&[
        "rust",
        "--cross-dylint",
        "--target",
        "win-x64",
        "--target=mac-x64",
    ]))
    .unwrap();
    assert!(plan.scope.is_empty());
    let cross = plan.resolve_cross_targets().unwrap();
    let steps = plan.rust_steps(false, &cross).unwrap();
    assert_eq!(steps.len(), 7);
    assert_eq!(
        steps[4],
        strings(&["dylint", "--all", "--", "--workspace", "--all-targets"])
    );
    assert_eq!(
        steps[5],
        strings(&[
            "dylint",
            "--all",
            "--",
            "--workspace",
            "--all-targets",
            "--target",
            "x86_64-pc-windows-msvc"
        ])
    );
    assert_eq!(
        steps[6],
        strings(&[
            "dylint",
            "--all",
            "--",
            "--workspace",
            "--all-targets",
            "--target",
            "x86_64-apple-darwin"
        ])
    );
    assert!(LintPlan::parse(&strings(&["rust", "--cross-dylint", "--host-only"])).is_err());
    assert!(LintPlan::parse(&strings(&["deps", "--cross-dylint"])).is_err());
}

#[test]
fn no_declared_targets_behaves_exactly_like_today() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let manifest = write_manifest_fixture(tmp.path(), None);
    let plan = LintPlan::parse(&strings(&["rust", "--manifest-path", &manifest])).unwrap();
    let cross = plan.resolve_cross_targets().unwrap();
    assert!(cross.is_empty());
    let steps = plan.rust_steps(false, &cross).unwrap();
    assert_eq!(steps.len(), 3, "fmt, clippy, dylint only");
}

#[test]
fn host_only_flag_gives_todays_steps_exactly() {
    let plan = LintPlan::parse(&strings(&["rust", "--host-only"])).unwrap();
    assert!(plan.host_only);
    assert!(plan.scope.is_empty(), "--host-only must be stripped");
    // No filesystem access even though no --manifest-path was given: a
    // declared workspace list must never be consulted under --host-only.
    let cross = plan.resolve_cross_targets().unwrap();
    assert!(cross.is_empty());
    assert_eq!(
        plan.rust_steps(false, &cross).unwrap(),
        vec![
            strings(&["fmt", "--all", "--", "--check"]),
            strings(&[
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings"
            ]),
            strings(&["dylint", "--all", "--", "--workspace", "--all-targets"]),
        ]
    );
}

#[test]
fn explicit_target_overrides_the_declared_list() {
    let tmp = tempfile::tempdir().expect("tmpdir");
    let manifest = write_manifest_fixture(tmp.path(), Some(&["x86_64-unknown-linux-musl"]));
    let plan = LintPlan::parse(&strings(&[
        "rust",
        "--manifest-path",
        &manifest,
        "--target",
        "win-x64",
    ]))
    .unwrap();
    assert_eq!(plan.explicit_targets, vec!["win-x64".to_string()]);
    let cross = plan.resolve_cross_targets().unwrap();
    // The declared musl target from the fixture must NOT appear -- only
    // the explicit override, resolved through the alias table.
    assert_eq!(cross, vec!["x86_64-pc-windows-msvc".to_string()]);
}

#[test]
fn unknown_target_triple_is_a_clear_error() {
    // A single word (no hyphens) doesn't even look like a Rust triple, so
    // it can't slip through as an unrecognized-but-plausible passthrough
    // the way a triple-shaped typo would.
    let plan = LintPlan::parse(&strings(&["rust", "--target", "bogus"])).unwrap();
    let error = plan.resolve_cross_targets().unwrap_err();
    assert!(
        error.to_string().contains("bogus"),
        "error must name the bad value: {error}"
    );
}

#[test]
fn host_triple_is_not_duplicated_as_a_cross_target() {
    let host = crate::pyo3_detect::host_triple().to_string();
    let plan = LintPlan::parse(&strings(&[
        "rust", "--target", &host, "--target", "win-x64",
    ]))
    .unwrap();
    let cross = plan.resolve_cross_targets().unwrap();
    assert_eq!(
        cross,
        vec!["x86_64-pc-windows-msvc".to_string()],
        "the host's own triple must be filtered out, not compiled twice"
    );
}

#[test]
fn target_and_host_only_flags_are_rejected_outside_rust_and_all() {
    let error = LintPlan::parse(&strings(&["deps", "--target", "win-x64"])).unwrap_err();
    assert!(error.to_string().contains("only valid for"));
    let error = LintPlan::parse(&strings(&["deps", "--host-only"])).unwrap_err();
    assert!(error.to_string().contains("only valid for"));
}

#[test]
fn host_only_cannot_combine_with_target() {
    let error =
        LintPlan::parse(&strings(&["rust", "--host-only", "--target", "win-x64"])).unwrap_err();
    assert!(error.to_string().contains("cannot be combined"));
}

// soldr#3390: `resolve_target_triples` used to prepend a hand-rolled
// `"lint: --target `{value}`: "` prefix on top of the resolver's own
// `"soldr build --target ..."`-worded message, doubling the input and
// naming the wrong command. RED on `main`.
#[test]
fn an_unknown_target_names_the_lint_surface_exactly_once() {
    let error = resolve_target_triples(&strings(&["win-armm"])).unwrap_err();
    let message = error.to_string();
    assert!(
        message.starts_with("soldr lint --target `win-armm`"),
        "{message}"
    );
    assert_eq!(message.matches("win-armm").count(), 1, "{message}");
    assert!(!message.contains("soldr build"), "{message}");
}
