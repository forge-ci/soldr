//! `soldr lint` under `SOLDR_PORCELAIN=1` (soldr#3958).
//!
//! `soldr lint` is several steps (fmt, Clippy, Dylint, plus any repo-declared
//! extras). Porcelain for a single `soldr cargo` call replaces the relayed
//! child stream with one JSON line; this module gives a *multi-step* run the
//! same treatment while reusing the same report ([`PorcelainReport`]), the
//! same durable log directory, and the same retention sweep -- not a second
//! format. The only additions to the JSON are the additive `steps` and
//! `failed_step` fields.
//!
//! * Every step's stdout+stderr is appended to ONE log under
//!   `<soldr root>/logs/porcelain/`.
//! * On success, the only output is the single JSON line on stdout.
//! * On failure, the failing step's output is printed in full on stderr
//!   (rustfmt diff, Clippy errors and warnings -- never folded), then the
//!   JSON line is printed on stdout. Output of steps that passed is folded
//!   into the log.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Instant;

use super::porcelain::{self, PorcelainReport, PorcelainStep};
use crate::build_log_meta::{sanitize_cwd_slug, utc_compact_timestamp};
use crate::core::{SoldrError, SoldrPaths};

/// The `command` carried in the lint report.
const LINT_COMMAND: &str = "soldr lint";

/// Is porcelain requested for this process?
pub(crate) fn requested() -> bool {
    porcelain::enabled()
}

/// Remove the porcelain switch from a child's environment so the child
/// `soldr cargo` runs the plain front door: the lint report is the only line.
pub(crate) fn scrub_child_env(command: &mut std::process::Command) {
    command.env_remove(porcelain::PORCELAIN_ENV_VAR);
}

/// One finished step, as the runner reports it.
#[derive(Debug, Clone)]
pub(crate) struct StepOutcome {
    pub(crate) name: String,
    pub(crate) exit_code: i32,
    pub(crate) duration_ms: u64,
}

/// Where a step's output starts in the run log.
#[derive(Clone, Copy, Debug)]
pub(crate) struct StepMark(u64);

/// stdout/stderr handles for one step, both appending to the run log.
pub(crate) struct StepSink {
    pub(crate) stdout: Stdio,
    pub(crate) stderr: Stdio,
    pub(crate) mark: StepMark,
}

/// The durable log of one `soldr lint` porcelain run.
pub(crate) struct LintLog {
    file: File,
    path: PathBuf,
    started_at_ms: i64,
    started: Instant,
}

impl LintLog {
    /// Create the run log. A log that cannot be created is an error: a
    /// porcelain run whose evidence is unreadable must not report green.
    pub(crate) fn create(paths: &SoldrPaths, cwd: &Path) -> Result<Self, SoldrError> {
        let started_at_ms = super::current_unix_ms();
        let dir = porcelain::log_dir(paths);
        std::fs::create_dir_all(&dir)?;
        let filename = format!(
            "{}-{}-lint.log",
            utc_compact_timestamp(started_at_ms),
            sanitize_cwd_slug(cwd)
        );
        let path = porcelain::unique_path(&dir, &filename);
        let mut file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .read(true)
            .open(&path)?;
        writeln!(file, "# soldr porcelain run log")?;
        writeln!(file, "# schema_version: {}", porcelain::SCHEMA_VERSION)?;
        writeln!(file, "# command: {LINT_COMMAND}")?;
        writeln!(file, "# started_at_ms: {started_at_ms}")?;
        Ok(Self {
            file,
            path,
            started_at_ms,
            started: Instant::now(),
        })
    }

    /// Write the step header and hand back handles the child writes through.
    /// Both handles share one file description, so stdout and stderr keep
    /// their relative order.
    pub(crate) fn begin_step(&mut self, name: &str, command: &str) -> Result<StepSink, SoldrError> {
        writeln!(self.file, "# --- step {name}: {command} ---")?;
        self.file.flush()?;
        let offset = self.file.metadata()?.len();
        Ok(StepSink {
            stdout: Stdio::from(self.file.try_clone()?),
            stderr: Stdio::from(self.file.try_clone()?),
            mark: StepMark(offset),
        })
    }

    /// Everything the step wrote, for printing in full on failure.
    pub(crate) fn step_output(&self, mark: StepMark) -> String {
        let bytes = std::fs::read(&self.path).unwrap_or_default();
        let start = usize::try_from(mark.0).unwrap_or(usize::MAX);
        bytes
            .get(start..)
            .map(|tail| String::from_utf8_lossy(tail).into_owned())
            .unwrap_or_default()
    }

    /// Print the failing step's diagnostics (stderr) and the one JSON line
    /// (stdout). Returns the process exit code, which equals the report's
    /// `exit_code`.
    pub(crate) fn finish(
        self,
        outcomes: &[StepOutcome],
        failure_output: Option<&str>,
        autofixed: Vec<String>,
    ) -> i32 {
        let failed = outcomes.iter().find(|outcome| outcome.exit_code != 0);
        let exit_code = failed.map_or(0, |outcome| outcome.exit_code);
        if let Some(output) = failure_output {
            let mut stderr = std::io::stderr().lock();
            let _ = stderr.write_all(output.as_bytes());
            if !output.ends_with('\n') {
                let _ = stderr.write_all(b"\n");
            }
            let _ = stderr.flush();
        }
        let mut report = self.report(outcomes, failed, exit_code, failure_output.unwrap_or(""));
        report.autofixed = autofixed;
        porcelain::emit(&report);
        exit_code
    }

    fn report(
        &self,
        outcomes: &[StepOutcome],
        failed: Option<&StepOutcome>,
        exit_code: i32,
        failure_output: &str,
    ) -> PorcelainReport {
        let mut summary = porcelain::summarize(LINT_COMMAND, exit_code, failure_output);
        if let Some(step) = failed {
            if summary.error_count == 0 {
                // A rustfmt diff has no `error:` line; the step failure is
                // still the digest's identity.
                summary.error_count = 1;
                summary.errors = vec![format!("{} failed (exit {})", step.name, step.exit_code)];
            }
        }
        PorcelainReport {
            schema_version: porcelain::SCHEMA_VERSION,
            command: LINT_COMMAND.to_string(),
            exit_code,
            duration_ms: u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX),
            log_path: Some(self.path.display().to_string()),
            expires_at: Some(self.started_at_ms + porcelain::PORCELAIN_LOG_TTL_MS),
            summary,
            steps: outcomes
                .iter()
                .map(|outcome| PorcelainStep {
                    name: outcome.name.clone(),
                    ok: outcome.exit_code == 0,
                    exit_code: outcome.exit_code,
                    duration_ms: outcome.duration_ms,
                })
                .collect(),
            failed_step: failed.map(|outcome| outcome.name.clone()),
            autofixed: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome(name: &str, exit_code: i32) -> StepOutcome {
        StepOutcome {
            name: name.into(),
            exit_code,
            duration_ms: 7,
        }
    }

    fn log_in(dir: &Path) -> LintLog {
        let path = dir.join("lint.log");
        LintLog {
            file: File::create_new(&path).expect("log file"),
            path,
            started_at_ms: 1_000,
            started: Instant::now(),
        }
    }

    #[test]
    fn passing_run_reports_every_step_and_no_failed_step() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let log = log_in(tmp.path());
        let report = log.report(&[outcome("fmt", 0), outcome("clippy", 0)], None, 0, "");
        let json = serde_json::to_value(&report).expect("serialize");
        assert_eq!(json["summary"]["status"], "ok");
        assert_eq!(json["exit_code"], 0);
        assert_eq!(json["steps"][0]["name"], "fmt");
        assert_eq!(json["steps"][1]["ok"], true);
        assert!(json.get("failed_step").is_none());
        assert!(json["log_path"].is_string());
    }

    #[test]
    fn failing_fmt_names_the_step_even_without_an_error_line() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let log = log_in(tmp.path());
        let fmt = outcome("fmt", 1);
        let report = log.report(
            std::slice::from_ref(&fmt),
            Some(&fmt),
            1,
            "Diff in src/lib.rs:1:\n-a\n+b\n",
        );
        let json = serde_json::to_value(&report).expect("serialize");
        assert_eq!(json["summary"]["status"], "failed");
        assert_eq!(json["failed_step"], "fmt");
        assert_eq!(json["summary"]["errors"][0], "fmt failed (exit 1)");
    }

    #[test]
    fn step_output_returns_only_that_steps_bytes() {
        let tmp = tempfile::tempdir().expect("tmpdir");
        let mut log = log_in(tmp.path());
        let _first = log.begin_step("fmt", "cargo fmt").expect("first");
        writeln!(log.file, "first-step-output").expect("write");
        let second = log.begin_step("clippy", "cargo clippy").expect("second");
        writeln!(log.file, "second-step-output").expect("write");
        let output = log.step_output(second.mark);
        assert!(output.contains("second-step-output"), "{output}");
        assert!(!output.contains("first-step-output"), "{output}");
    }
}
