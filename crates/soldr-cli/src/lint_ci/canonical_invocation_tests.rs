use super::*;
use crate::lint_ci;

fn fixture(profile: bool) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let mut manifest = "[workspace]\nmembers = ['crates/*']\n".to_owned();
    if profile {
        manifest.push_str("[workspace.metadata.soldr.cargo]\ntarget = 'implicit'\nunify = 'off'\nfeatures = ['a', 'b']\n[workspace.metadata.soldr.cargo.alternates.narrow]\npackages = ['a']\nfeatures = ['other']\n");
    }
    std::fs::write(dir.path().join("Cargo.toml"), manifest).unwrap();
    for name in ["a", "b"] {
        let member = dir.path().join("crates").join(name);
        std::fs::create_dir_all(&member).unwrap();
        std::fs::write(
            member.join("Cargo.toml"),
            format!("[package]\nname = '{name}'\nversion = '0.1.0'\n"),
        )
        .unwrap();
    }
    std::fs::create_dir(dir.path().join("ci")).unwrap();
    dir
}

fn write(dir: &Path, path: &str, text: &str) {
    let path = dir.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

#[test]
fn canonical_ci_reports_drift_with_location_and_execution_remedy() {
    let dir = fixture(true);
    write(
        dir.path(),
        "ci/test.py",
        "# fixture\nrun(\"soldr cargo test -p a\")\n",
    );
    let report = lint_ci::analyze(dir.path());
    assert_eq!(report.findings.len(), 1);
    let finding = &report.findings[0];
    assert_eq!(finding.file, "ci/test.py");
    assert_eq!(finding.line, 2);
    assert_eq!(finding.rule, RULE);
    assert_eq!(finding.tool, "soldr cargo test");
    assert_eq!(finding.target, "packages, features");
    assert!(finding
        .recommendation
        .contains("soldr cargo test --workspace --features a,b --all-targets"));
    assert!(finding.recommendation.contains("-E 'package(a)'"));
    assert!(finding.render_human().contains(&finding.recommendation));
    assert_eq!(report.exit_code(), 1);
}

#[test]
fn canonical_ci_accepts_profile_alternate_and_all_member_selection() {
    let dir = fixture(true);
    for line in [
        "soldr cargo test --workspace --features a,b",
        "soldr cargo test -p a -p b --features 'b a'",
        "soldr cargo test -p a --features other",
        "soldr test --workspace --features a,b",
    ] {
        write(dir.path(), "ci/test.py", line);
        assert!(lint_ci::analyze(dir.path()).findings.is_empty(), "{line}");
    }
}

#[test]
fn canonical_ci_uses_existing_inline_and_preceding_suppressions() {
    let dir = fixture(true);
    for text in [
        "soldr cargo test -p a # soldr-lint-ci: allow canonical-cargo-invocation",
        "# soldr-lint-ci: allow canonical-cargo-invocation\nsoldr cargo test -p a",
    ] {
        write(dir.path(), "ci/test.py", text);
        assert!(lint_ci::analyze(dir.path()).findings.is_empty());
    }
}

#[test]
fn canonical_ci_is_inert_without_a_profile() {
    let dir = fixture(false);
    write(
        dir.path(),
        ".github/workflows/test.yml",
        "steps:\n  - run: soldr cargo test -p wild --all-features --no-default-features",
    );
    assert!(lint_ci::analyze(dir.path()).findings.is_empty());
}

#[test]
fn canonical_ci_discovers_every_entrypoint_and_normalizes_continuations() {
    let dir = fixture(true);
    for path in [
        "lint",
        "test",
        "push",
        "ci/nested/test.py",
        "justfile",
        "Makefile",
        "local-gate.toml",
        "bosn.toml",
        ".github/scripts/test.sh",
        ".github/actions/build/action.yml",
        ".github/workflows/test.yml",
    ] {
        let command = if path.ends_with("yml") {
            "run: soldr cargo test -p a"
        } else {
            "soldr cargo test \\\n  -p a"
        };
        write(dir.path(), path, command);
    }
    let findings = lint_ci::analyze(dir.path()).findings;
    assert_eq!(findings.len(), 11);
    assert!(findings.iter().all(|f| f.line == 1 && f.rule == RULE));
}

#[test]
fn canonical_ci_clippy_tail_and_target_use_the_runtime_normalizer() {
    let dir = fixture(true);
    write(
        dir.path(),
        "lint",
        "soldr cargo clippy --workspace --features a,b --all-targets --target host -- -A warnings",
    );
    let report = lint_ci::analyze(dir.path());
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].target, "target, clippy args");
}

#[test]
fn canonical_ci_invalid_profile_fails_loudly() {
    let dir = fixture(true);
    write(
        dir.path(),
        "Cargo.toml",
        "[workspace.metadata.soldr.cargo]\nfeatuers = []",
    );
    let report = lint_ci::analyze(dir.path());
    assert_eq!(report.findings.len(), 1);
    assert!(report.findings[0].recommendation.contains("featuers"));
    assert!(!report.ok);
}
