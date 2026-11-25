//! Shared data model for the `soldr lint ci` policy engine (soldr#2038).
//!
//! # JSON schema (`schema_version: 1`)
//!
//! `soldr lint ci --format json` emits a single JSON object:
//!
//! ```json
//! {
//!   "schema_version": 1,
//!   "ok": true,
//!   "findings": [
//!     {
//!       "rule": "cross-compile-surface",
//!       "severity": "error",
//!       "file": ".github/workflows/release.yml",
//!       "line": 42,
//!       "tool": "cargo zigbuild",
//!       "target": "aarch64-apple-darwin",
//!       "recommendation": "use `soldr build --target aarch64-apple-darwin` ..."
//!     }
//!   ]
//! }
//! ```
//!
//! Field contract (stable across `schema_version: 1`):
//! - `schema_version` (u32): bumped only on a breaking shape change.
//! - `ok` (bool): `true` when there are zero **error**-severity findings.
//!   Warning-only runs are still `ok: true` and exit `0`.
//! - `findings` (array): every finding, error and warning, after inline
//!   suppressions have been applied.
//!   - `rule` (string): stable rule identifier, e.g. `cross-compile-surface`.
//!   - `severity` (string): `"error"` or `"warning"`.
//!   - `file` (string): repo-root-relative path with `/` separators.
//!   - `line` (u32): 1-based line where the offending command begins.
//!   - `tool` (string): the detected non-blessed tool, e.g. `cargo xwin`.
//!   - `target` (string): the resolved Apple/Windows target triple, or a
//!     representative such as `*-pc-windows-msvc` when only the tool class is
//!     known, or `<unresolved>` for a lower-severity warning.
//!   - `recommendation` (string): the exact blessed replacement command.

use serde::Serialize;

/// Severity of a CI policy finding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// A hard policy violation. Any error-severity finding fails the run.
    Error,
    /// A best-effort concern (e.g. a target that could not be statically
    /// resolved on a surface that is capable of Apple/Windows builds). Does
    /// not fail the run.
    Warning,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

/// One structured policy finding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Finding {
    /// Stable rule identifier.
    pub rule: String,
    pub severity: Severity,
    /// Repo-root-relative path, `/`-separated.
    pub file: String,
    /// 1-based line number where the offending command begins.
    pub line: u32,
    /// The detected tool or invocation.
    pub tool: String,
    /// Resolved target, or differing selection fields for canonical invocations.
    pub target: String,
    /// The rule-specific replacement command and remedy.
    pub recommendation: String,
}

impl Finding {
    /// Human-readable one-line rendering.
    ///
    /// `<severity>: <RULE-ID> <path>:<line> — <tool> for <target>; <recommendation>`
    pub fn render_human(&self) -> String {
        format!(
            "{}: {} {}:{} — {} for {}; {}",
            self.severity.label(),
            self.rule,
            self.file,
            self.line,
            self.tool,
            self.target,
            self.recommendation,
        )
    }
}

/// Top-level machine-readable report (`schema_version: 1`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CiLintReport {
    pub schema_version: u32,
    pub ok: bool,
    pub findings: Vec<Finding>,
}

/// Current JSON schema version.
pub const SCHEMA_VERSION: u32 = 1;

impl CiLintReport {
    pub fn new(findings: Vec<Finding>) -> Self {
        let ok = !findings.iter().any(|f| f.severity == Severity::Error);
        Self {
            schema_version: SCHEMA_VERSION,
            ok,
            findings,
        }
    }

    /// Process exit code: `0` when there are no error-severity findings.
    pub fn exit_code(&self) -> i32 {
        if self.ok {
            0
        } else {
            1
        }
    }
}

/// Output rendering selector for `soldr lint ci`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputFormat {
    Human,
    Json,
}

impl OutputFormat {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "human" | "text" => Some(OutputFormat::Human),
            "json" => Some(OutputFormat::Json),
            _ => None,
        }
    }
}
