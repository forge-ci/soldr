//! `soldr lint ci` — extensible CI/build-surface policy engine (soldr#2038).
//!
//! This suite statically validates executable CI surfaces (GitHub Actions
//! workflows, composite actions, and referenced helper scripts). It runs a
//! pure filesystem scan: it needs no Cargo manifest, no Rust toolchain, and
//! never starts the compiler cache, so it works in any repository.
//!
//! The engine is a registry of independent [`registry::CiRule`]s returning
//! structured [`model::Finding`]s. The first rule is
//! [`cross_compile_surface`], which enforces the blessed
//! `soldr build --target ...` surface for Apple Darwin and Windows MSVC.
//!
//! ## Inline suppression
//!
//! A finding can be suppressed with a comment on the offending line or the
//! line immediately above:
//!
//! ```text
//! soldr cargo xwin build --target x86_64-pc-windows-msvc  # soldr-lint-ci: allow cross-compile-surface -- intentional legacy-path regression test
//! ```
//!
//! `allow all` suppresses every rule on that line; `allow <rule-id>[,<id>...]`
//! suppresses the named rules. Text after `--` is a free-form reason.

mod canonical_invocation;
mod canonical_tokens;
pub mod cross_compile_surface;
pub mod model;
pub mod registry;
pub mod scan;

use std::collections::HashMap;
use std::path::Path;

use crate::core::SoldrError;
use model::{CiLintReport, Finding, OutputFormat, Severity};
use scan::ScannedFile;

/// Run the CI policy suite over `root` and render results to stdout.
///
/// Returns the process exit code: `0` when there are no error-severity
/// findings (warnings alone still return `0`), non-zero otherwise.
pub fn run(root: &Path, format: OutputFormat) -> Result<i32, SoldrError> {
    let report = analyze(root);
    match format {
        OutputFormat::Human => render_human(&report),
        OutputFormat::Json => render_json(&report)?,
    }
    Ok(report.exit_code())
}

/// Scan `root`, run every rule, apply suppressions, and build the report.
pub fn analyze(root: &Path) -> CiLintReport {
    let files: Vec<ScannedFile> = scan::discover(root)
        .iter()
        .filter_map(|path| scan::scan_file(root, path))
        .collect();
    analyze_with_rules(&files, registry::rules(Some(root)))
}

/// Rule execution + suppression, factored out for in-memory unit tests.
pub fn analyze_files(files: &[ScannedFile]) -> CiLintReport {
    analyze_with_rules(files, registry::rules(None))
}

fn analyze_with_rules(
    files: &[ScannedFile],
    rules: Vec<Box<dyn registry::CiRule>>,
) -> CiLintReport {
    let by_path: HashMap<&str, &ScannedFile> =
        files.iter().map(|f| (f.rel_path.as_str(), f)).collect();

    let mut findings = Vec::new();
    for rule in rules {
        for finding in rule.check(files) {
            if is_suppressed(&by_path, &finding) {
                continue;
            }
            findings.push(finding);
        }
    }
    findings.sort_by(|a, b| {
        (a.file.as_str(), a.line, a.rule.as_str()).cmp(&(b.file.as_str(), b.line, b.rule.as_str()))
    });
    CiLintReport::new(findings)
}

/// A finding is suppressed by a directive on its own line or the line above.
fn is_suppressed(by_path: &HashMap<&str, &ScannedFile>, finding: &Finding) -> bool {
    let Some(file) = by_path.get(finding.file.as_str()) else {
        return false;
    };
    let this = file
        .suppressions
        .get(&finding.line)
        .map(|s| s.allows(&finding.rule))
        .unwrap_or(false);
    let above = finding
        .line
        .checked_sub(1)
        .and_then(|l| file.suppressions.get(&l))
        .map(|s| s.allows(&finding.rule))
        .unwrap_or(false);
    this || above
}

fn render_human(report: &CiLintReport) {
    for finding in &report.findings {
        println!("{}", finding.render_human());
    }
    let errors = report
        .findings
        .iter()
        .filter(|f| f.severity == Severity::Error)
        .count();
    let warnings = report.findings.len() - errors;
    if report.findings.is_empty() {
        println!("soldr lint ci: clean — no CI policy violations found");
    } else {
        println!(
            "soldr lint ci: {errors} error(s), {warnings} warning(s){}",
            if report.ok {
                " (no errors — exit 0)"
            } else {
                ""
            }
        );
    }
}

fn render_json(report: &CiLintReport) -> Result<(), SoldrError> {
    let json = serde_json::to_string_pretty(report)
        .map_err(|e| SoldrError::Other(format!("lint ci: failed to serialize JSON: {e}")))?;
    println!("{json}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use scan::scan_text;

    #[test]
    fn suppression_on_offending_line_clears_finding() {
        let text = "        run: cargo xwin build --target x86_64-pc-windows-msvc  # soldr-lint-ci: allow cross-compile-surface -- legacy regression test";
        let report = analyze_files(&[scan_text("wf.yml".into(), text)]);
        assert!(report.findings.is_empty());
        assert!(report.ok);
        assert_eq!(report.exit_code(), 0);
    }

    #[test]
    fn suppression_on_line_above_clears_finding() {
        let text = "        # soldr-lint-ci: allow cross-compile-surface -- legacy\n        run: cargo xwin build --target x86_64-pc-windows-msvc";
        let report = analyze_files(&[scan_text("wf.yml".into(), text)]);
        assert!(report.findings.is_empty());
    }

    #[test]
    fn unrelated_suppression_does_not_clear_finding() {
        let text = "        run: cargo xwin build --target x86_64-pc-windows-msvc  # soldr-lint-ci: allow some-other-rule";
        let report = analyze_files(&[scan_text("wf.yml".into(), text)]);
        assert_eq!(report.findings.len(), 1);
        assert!(!report.ok);
        assert_eq!(report.exit_code(), 1);
    }

    #[test]
    fn json_schema_is_stable() {
        let text = "        run: cargo zigbuild --target aarch64-apple-darwin";
        let report = analyze_files(&[scan_text("wf.yml".into(), text)]);
        let value: serde_json::Value = serde_json::to_value(&report).unwrap();

        // Top-level shape.
        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["ok"], false);
        let findings = value["findings"].as_array().unwrap();
        assert_eq!(findings.len(), 1);

        // Exact stable field set on each finding.
        let obj = findings[0].as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            vec![
                "file",
                "line",
                "recommendation",
                "rule",
                "severity",
                "target",
                "tool",
            ]
        );
        assert_eq!(obj["rule"], "cross-compile-surface");
        assert_eq!(obj["severity"], "error");
        assert_eq!(obj["tool"], "cargo zigbuild");
        assert_eq!(obj["target"], "aarch64-apple-darwin");
    }

    #[test]
    fn clean_report_is_ok_and_exit_zero() {
        let text = "        run: soldr build --target aarch64-apple-darwin";
        let report = analyze_files(&[scan_text("wf.yml".into(), text)]);
        assert!(report.findings.is_empty());
        assert!(report.ok);
        assert_eq!(report.exit_code(), 0);
        assert_eq!(report.schema_version, model::SCHEMA_VERSION);
    }
}
