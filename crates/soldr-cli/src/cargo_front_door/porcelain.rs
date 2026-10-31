//! `--porcelain` / `SOLDR_PORCELAIN` machine-readable cargo front door
//! (agent-harness context).
//!
//! An autonomous agent harness invokes soldr for builds and tests and
//! reads back *all* of soldr's stdout/stderr. The default front door
//! relays the child's entire cargo/nextest stream into that transcript,
//! which is the single largest source of wasted agent context and cost.
//!
//! Porcelain replaces that relay with exactly ONE line of JSON on
//! stdout, while the child's complete stdout+stderr is preserved in a
//! durable log under `<soldr root>/logs/porcelain/` (readable afterwards
//! via the emitted `log_path`).
//!
//! ## Where this sits in the existing architecture
//!
//! * Enabled by the `--porcelain` flag (published as
//!   [`PORCELAIN_ENV_VAR`] by `Cli::export_global_env`, the same
//!   mechanism `--debug` / `--timestamp-lines` use) OR by setting
//!   [`PORCELAIN_ENV_VAR`] directly -- the env var is what a harness
//!   sets, the flag is for humans.
//! * The capture itself reuses the pipe-tee primitives in
//!   [`super::output_capture`] (`CapturePipeMessage`, the pipe readers,
//!   `drain_capture_pipe_after_child_exit`, `wait_for_cargo_child_guarded`,
//!   `debug_trace::spawn_traced`, `finish_observation_after_wait`) --
//!   it is the cargo-JSON capture machinery with the terminal tee
//!   removed, not a parallel capture stack.
//! * The child's bytes land next to the always-on build logs, and their
//!   retention rides the same auto-GC sweeper
//!   (`gc::run_gc_auto_sweep_command`) that already prunes the build
//!   logs -- see [`prune_porcelain_logs`].
//! * The `summary` digest is derived from what soldr already parses out
//!   of the child stream: cargo's JSON message stream
//!   (`strip_diagnostics::merge_cargo_json_diagnostics` reads the same
//!   shape) for `--message-format=json` runs, and the existing
//!   `cargo_diagnostics` / `strip_diagnostics` classifiers for soldr's
//!   own failure attribution.
//!
//! ## Default behaviour is unchanged
//!
//! Nothing here is consulted unless [`enabled`] is true, so a CI run
//! without the flag or the variable streams exactly as it does today.
//!
//! ## `exit_code` always equals `$?`
//!
//! The emitted `exit_code` is the code this process exits with, including
//! the two cases that have no plain `ExitStatus::code`:
//!
//! * a child killed by a signal -- the process exits [`FAILURE_EXIT_CODE`]
//!   (see [`process_exit_code`]); the durable log keeps the signal evidence;
//! * soldr failing before a child status exists -- the report carries
//!   [`FAILURE_EXIT_CODE`], which is what `report_and_exit` exits with.
//!
//! A harness can therefore branch on either the process status or the JSON
//! field and get the same answer.
//!
//! ## Digest is colour-blind
//!
//! `summary` is scanned out of the child's captured bytes, including runs
//! where the caller forces ANSI colour (`CARGO_TERM_COLOR=always`,
//! `--color always`). Escape sequences are stripped per line before the
//! error scan ([`without_ansi`]), so a coloured `error[E0308]` still counts
//! and its identity carries no escape bytes.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::build_log_meta::{sanitize_cwd_slug, utc_compact_timestamp};
use crate::core::SoldrPaths;

/// Opt-in switch for the machine-readable front-door mode.
///
/// Owned (`SOLDR_*`) switch: read with the allowlist rule in
/// `soldr_core::core::env_flag`, so an unrecognised value stays OFF.
pub(crate) const PORCELAIN_ENV_VAR: &str = "SOLDR_PORCELAIN";

/// Schema version of the emitted JSON object. Additive field changes do
/// not bump it; a breaking shape change does.
pub(super) const SCHEMA_VERSION: u32 = 1;

/// Subdirectory of `<soldr root>/logs/` that holds porcelain run logs.
pub(super) const PORCELAIN_LOG_DIR: &str = "porcelain";

/// How long a porcelain log stays readable before it becomes eligible
/// for deletion. Advertised to the caller as `expires_at`.
///
/// Seven days is chosen to be comfortably longer than any single agent
/// session (so a harness can always re-read the detail of the run it
/// just made) while still bounding accumulation on a fleet running
/// soldr many times a day.
pub(super) const PORCELAIN_LOG_TTL_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// Hard cap on how many porcelain logs are kept regardless of age -- the
/// same shape as `build_log::BUILD_LOG_KEEP`. Whichever bound bites
/// first (age or count) wins.
pub(super) const PORCELAIN_LOG_KEEP: usize = 200;

/// Upper bound on the number of error identities carried in `summary`.
/// `summary.error_count` always reports the true count.
const MAX_ERROR_IDENTITIES: usize = 20;

/// Upper bound on the distinct warning identities carried in `summary`.
///
/// Warnings are not failures, so the digest only has to be enough for a
/// caller to decide whether to open `log_path` -- five distinct identities
/// covers the ordinary "one or two new lints" case without turning the
/// digest back into the relay this mode replaces.
const MAX_WARNING_IDENTITIES: usize = 5;

/// Upper bound on one warning identity's length (message plus location).
const MAX_WARNING_IDENTITY_CHARS: usize = 200;

/// Upper bound on the distinct error lines remembered for de-duplication.
/// Past it, repeated occurrences count as they arrive rather than growing the
/// set without limit.
const MAX_DEDUPE_ENTRIES: usize = 4096;

/// Exit code the process uses when soldr itself fails before a child status
/// exists (`soldr_main_helpers::report_and_exit`). The JSON reports this same
/// value so `exit_code` always equals `$?`.
pub(super) const FAILURE_EXIT_CODE: i32 = 1;

/// Is the porcelain mode requested for this process?
pub(super) fn enabled() -> bool {
    crate::core::flag(PORCELAIN_ENV_VAR)
}

/// `<soldr root>/logs/porcelain` -- where porcelain run logs live.
pub(crate) fn log_dir(paths: &SoldrPaths) -> PathBuf {
    paths.root.join("logs").join(PORCELAIN_LOG_DIR)
}

/// The durable log a porcelain run wrote, plus the run's start time.
///
/// Returned by the capture so the finish path can build the JSON report
/// without re-reading the file.
#[derive(Debug, Clone)]
pub(super) struct PorcelainLogArtifact {
    pub(super) path: PathBuf,
    pub(super) started_at_ms: i64,
}

/// A machine-readable digest of one run.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub(super) struct PorcelainSummary {
    pub(super) exit_code: i32,
    pub(super) command: String,
    /// `"ok"` or `"failed"` -- `failed` whenever `exit_code != 0`.
    pub(super) status: String,
    /// Number of DISTINCT failures/errors recognized in the child stream --
    /// one failure reported twice (nextest's progress line plus its summary)
    /// counts once.
    pub(super) error_count: usize,
    /// Identities of the first [`MAX_ERROR_IDENTITIES`] failures/errors.
    pub(super) errors: Vec<String>,
    /// soldr's own classification of the failure, when one of the
    /// existing `cargo_diagnostics` / strip classifiers matched.
    pub(super) diagnostic: Option<String>,
    /// Number of DISTINCT warnings recognized in the child stream. Present on
    /// a successful run too: a build that exits zero can still emit warnings,
    /// and a caller that cannot see them has to read the whole log to learn
    /// whether there is anything to fix.
    pub(super) warning_count: usize,
    /// Identities of the first [`MAX_WARNING_IDENTITIES`] warnings, each
    /// `warning: <message> [<file>:<line>:<col>]` where rustc printed a
    /// location. Empty when the run emitted none.
    pub(super) warnings: Vec<String>,
    /// Failing tests (first 10) with panic location and message lines
    /// (soldr#3961). Omitted when no test failed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) failed_tests: Vec<super::porcelain_failures::FailedTest>,
    /// Failing tests beyond the cap ("+N more"). Omitted when zero.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub(super) failed_tests_more: usize,
    /// Per-binary `test result: FAILED. ...` lines. Omitted when none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) test_results: Vec<String>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// The JSON object emitted on stdout in porcelain mode.
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub(super) struct PorcelainReport {
    pub(super) schema_version: u32,
    pub(super) command: String,
    pub(super) exit_code: i32,
    pub(super) duration_ms: u64,
    /// Path to the durable run log. `null` only when the log could not be
    /// written at all (e.g. an unmaterializable soldr root).
    pub(super) log_path: Option<String>,
    /// Unix-ms instant at which the log becomes eligible for deletion.
    pub(super) expires_at: Option<i64>,
    pub(super) summary: PorcelainSummary,
    /// Per-step outcomes for a multi-step run (`soldr lint`, soldr#3958).
    /// Omitted for single-command runs, so the `soldr cargo` shape is
    /// unchanged. Additive: does not bump [`SCHEMA_VERSION`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) steps: Vec<PorcelainStep>,
    /// Name of the step that failed, when a multi-step run stopped on one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) failed_step: Option<String>,
}

/// One step of a multi-step porcelain run (`soldr lint`, soldr#3958).
#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub(super) struct PorcelainStep {
    pub(super) name: String,
    pub(super) ok: bool,
    pub(super) exit_code: i32,
    pub(super) duration_ms: u64,
}

/// Inputs to [`report_for_run`].
pub(super) struct RunReportInputs<'a> {
    pub(super) command: &'a str,
    pub(super) exit_code: i32,
    pub(super) started_at_ms: i64,
    pub(super) ended_at_ms: i64,
    pub(super) log_path: Option<PathBuf>,
    /// The child's full captured stdout+stderr, when capture ran.
    pub(super) output: Option<&'a str>,
}

/// The exit code this process will actually report.
///
/// [`StripOutcome::effective_exit_code`] answers `-1` when the child died from a
/// signal, because an `ExitStatus` carries no numeric code then. The process
/// itself exits `status.code().unwrap_or(FAILURE_EXIT_CODE)`, so emitting `-1`
/// would make the JSON disagree with `$?` for the same run. Normalize to the code
/// the process uses; the durable log keeps the signal evidence.
///
/// Takes `status.code()` rather than the `ExitStatus` so the rule is testable on
/// every host without host-`cfg` selection (dylint: `ban_platform_cfg_outside_boundary`).
///
/// [`StripOutcome::effective_exit_code`]: super::strip_diagnostics::StripOutcome::effective_exit_code
pub(super) fn process_exit_code(effective: i32, status_code: Option<i32>) -> i32 {
    if effective >= 0 {
        return effective;
    }
    status_code.unwrap_or(FAILURE_EXIT_CODE)
}

/// Build the report for a run whose child completed, from its capture artifact.
///
/// Kept here (rather than inlined at the call site) so `complete_cargo_run`
/// stays inside clippy's `too_many_lines` ceiling.
pub(super) fn captured_run_report(
    artifact: Option<&PorcelainLogArtifact>,
    command: &str,
    status: &std::process::ExitStatus,
    effective_exit_code: i32,
    output: Option<&str>,
) -> Option<PorcelainReport> {
    let artifact = artifact?;
    Some(report_for_run(&RunReportInputs {
        command,
        exit_code: process_exit_code(effective_exit_code, status.code()),
        started_at_ms: artifact.started_at_ms,
        ended_at_ms: super::current_unix_ms(),
        log_path: Some(artifact.path.clone()),
        output,
    }))
}

/// Build the report for a run that reached the capture stage (success,
/// compiler failure, or post-spawn abort).
pub(super) fn report_for_run(inputs: &RunReportInputs<'_>) -> PorcelainReport {
    let output = inputs.output.unwrap_or_default();
    let summary = summarize(inputs.command, inputs.exit_code, output);
    PorcelainReport {
        schema_version: SCHEMA_VERSION,
        command: inputs.command.to_string(),
        exit_code: inputs.exit_code,
        duration_ms: (inputs.ended_at_ms - inputs.started_at_ms).max(0) as u64,
        log_path: inputs
            .log_path
            .as_ref()
            .map(|path| path.display().to_string()),
        expires_at: inputs
            .log_path
            .as_ref()
            .map(|_| inputs.started_at_ms + PORCELAIN_LOG_TTL_MS),
        summary,
        steps: Vec::new(),
        failed_step: None,
    }
}

/// Build the fallback report for a run that failed before (or without) a
/// capture -- e.g. a front-door configuration error, or a replayed
/// check. Still exactly one JSON line, with the failure in `summary`.
pub(super) fn report_for_failure(
    command: &str,
    exit_code: i32,
    log_path: Option<PathBuf>,
    message: Option<&str>,
) -> PorcelainReport {
    let detail = message.unwrap_or("soldr produced no run log for this invocation");
    let failed = exit_code != 0;
    let summary = PorcelainSummary {
        exit_code,
        command: command.to_string(),
        status: if failed { "failed" } else { "ok" }.to_string(),
        error_count: usize::from(failed),
        errors: if failed {
            vec![first_line(detail)]
        } else {
            Vec::new()
        },
        diagnostic: None,
        warning_count: 0,
        warnings: Vec::new(),
        failed_tests: Vec::new(),
        failed_tests_more: 0,
        test_results: Vec::new(),
    };
    let now = super::current_unix_ms();
    let log_path = log_path.map(|path| path.display().to_string());
    let expires_at = log_path.as_ref().map(|_| now + PORCELAIN_LOG_TTL_MS);
    PorcelainReport {
        schema_version: SCHEMA_VERSION,
        command: command.to_string(),
        exit_code,
        duration_ms: 0,
        log_path,
        expires_at,
        summary,
        steps: Vec::new(),
        failed_step: None,
    }
}

/// `"cargo <args>"` -- the canonical command string carried in the JSON.
pub(super) fn command_string(args: &[String]) -> String {
    if args.is_empty() {
        "cargo".to_string()
    } else {
        format!("cargo {}", args.join(" "))
    }
}

/// Derive a machine-readable digest from the child's captured stream.
///
/// Prefers cargo's JSON message stream when present (soldr already parses
/// that shape for diagnostics and the artifact closure); otherwise scans
/// the plain stream for rustc `error` lines and cargo-nextest `FAIL`
/// lines. A soldr-owned classifier result is attached as `diagnostic`
/// when one matches.
pub(super) fn summarize(command: &str, exit_code: i32, output: &str) -> PorcelainSummary {
    let (error_count, errors) = extract_errors(output);
    let (warning_count, warnings) = extract_warnings(output);
    let failures = if exit_code == 0 {
        super::porcelain_failures::FailureDigest::default()
    } else {
        let lines: Vec<String> = output
            .lines()
            .map(|l| without_ansi(l).into_owned())
            .collect();
        super::porcelain_failures::digest(&lines)
    };
    PorcelainSummary {
        failed_tests: failures.tests,
        failed_tests_more: failures.more,
        test_results: failures.results,
        exit_code,
        command: command.to_string(),
        status: if exit_code == 0 { "ok" } else { "failed" }.to_string(),
        error_count,
        errors,
        diagnostic: classify_diagnostic(output),
        warning_count,
        warnings,
    }
}

/// Count and name the failures/errors in a child stream.
fn extract_errors(output: &str) -> (usize, Vec<String>) {
    if let Some(result) = extract_cargo_json_errors(output) {
        return result;
    }
    let mut errors: Vec<String> = Vec::new();
    // A single failure is often reported more than once: cargo-nextest prints
    // the `FAIL ...` line in the progress stream AND again in its summary, so a
    // raw occurrence count told a caller "3 errors" for ONE failing test. Count
    // and list each distinct line once, in first-seen order.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut count = 0usize;
    for line in output.lines() {
        let line = without_ansi(line);
        let trimmed = line.trim();
        if !is_error_line(trimmed) {
            continue;
        }
        // Bound the dedupe set so a pathological stream cannot grow it without
        // limit; past the cap, occurrences are counted as they arrive.
        if seen.len() < MAX_DEDUPE_ENTRIES && !seen.insert(trimmed.to_string()) {
            continue;
        }
        count += 1;
        if errors.len() < MAX_ERROR_IDENTITIES {
            errors.push(trimmed.to_string());
        }
    }
    (count, errors)
}

/// Count and name the DISTINCT warnings in a child stream.
///
/// A porcelain success digest used to say only `status: "ok", error_count: 0`,
/// so a build that exited zero *with* warnings looked clean and the caller had
/// to open `log_path` to find out. The identity here is rustc's header line
/// plus the ` --> file:line:col` location when one is printed; the caret,
/// help and note lines below it stay in the durable log.
///
/// Cargo's trailing tally (``warning: `crate` (lib) generated 2 warnings``) is
/// not a warning, so it is not counted. Returns `(distinct_count,
/// capped_identities)`.
fn extract_warnings(output: &str) -> (usize, Vec<String>) {
    if let Some(result) = extract_cargo_json_warnings(output) {
        return result;
    }
    let lines: Vec<&str> = output.lines().collect();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut count = 0usize;
    for (index, line) in lines.iter().enumerate() {
        let line = without_ansi(line);
        let trimmed = line.trim();
        if !is_warning_header(trimmed) || is_warning_summary(trimmed) {
            continue;
        }
        let identity = bounded_identity(&with_location(trimmed, &lines[index + 1..]));
        if seen.len() < MAX_DEDUPE_ENTRIES && !seen.insert(identity.clone()) {
            continue;
        }
        count += 1;
        if warnings.len() < MAX_WARNING_IDENTITIES {
            warnings.push(identity);
        }
    }
    (count, warnings)
}

/// Does this captured line open a rustc warning block?
///
/// `warning: <message>` is the common form; `warning[<lint>]: <message>` is the
/// structured spelling. `error: warnings emitted` is excluded because it is
/// built from the same word and is a tally.
fn is_warning_header(trimmed: &str) -> bool {
    trimmed.starts_with("warning:") || trimmed.starts_with("warning[")
}

/// Cargo's per-crate tally, in any of the shapes it prints:
///
/// * ``warning: `warny` (lib) generated 2 warnings``
/// * ``warning: `warny` (lib) generated 1 warning (run `cargo fix ...`)``
/// * ``warning: 3 warnings emitted``
///
/// All of them are built from the word `warning` and a count, so they are
/// matched by shape rather than by a suffix test (the suffix test missed the
/// `cargo fix` suggestion cargo appends to a single-warning tally).
fn is_warning_summary(trimmed: &str) -> bool {
    if let Some((_, rest)) = trimmed.split_once(" generated ") {
        let count_leads = rest.starts_with(|ch: char| ch.is_ascii_digit());
        return count_leads && rest.contains("warning");
    }
    trimmed.starts_with("warning: ") && trimmed.ends_with("emitted")
}

/// Append rustc's `--> file:line:col` location, when the block prints one
/// within the next few lines.
fn with_location(header: &str, following: &[&str]) -> String {
    for line in following.iter().take(4) {
        let line = without_ansi(line);
        let trimmed = line.trim();
        if let Some(location) = trimmed.strip_prefix("--> ") {
            return format!("{header} [{}]", location.trim());
        }
        // The caret block before the location would mean rustc printed no
        // location at all; stop rather than scanning unrelated output.
        if trimmed.starts_with('|') || trimmed.starts_with("= ") {
            break;
        }
    }
    header.to_string()
}

/// Cap one identity so a pathological lint message cannot re-inflate the digest.
fn bounded_identity(identity: &str) -> String {
    if identity.chars().count() <= MAX_WARNING_IDENTITY_CHARS {
        return identity.to_string();
    }
    let mut capped: String = identity.chars().take(MAX_WARNING_IDENTITY_CHARS).collect();
    capped.push_str("...");
    capped
}

/// Warnings from cargo's `--message-format=json` stream, when this stream is
/// one. `None` means "not a JSON stream" so the plain scan takes over.
fn extract_cargo_json_warnings(output: &str) -> Option<(usize, Vec<String>)> {
    let mut saw_message = false;
    let mut warnings: Vec<String> = Vec::new();
    let mut total = 0usize;
    for line in output.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(reason) = value.get("reason").and_then(serde_json::Value::as_str) else {
            continue;
        };
        saw_message = true;
        if reason != "compiler-message" {
            continue;
        }
        if value
            .pointer("/message/level")
            .and_then(serde_json::Value::as_str)
            != Some("warning")
        {
            continue;
        }
        total += 1;
        if warnings.len() < MAX_WARNING_IDENTITIES {
            warnings.push(bounded_identity(&json_warning_identity(&value)));
        }
    }
    saw_message.then_some((total, warnings))
}

/// Identity for one cargo JSON `compiler-message` warning: the message's first
/// line, with the primary span appended when cargo reports one.
fn json_warning_identity(value: &serde_json::Value) -> String {
    let message = value
        .pointer("/message/message")
        .and_then(serde_json::Value::as_str)
        .map(first_line)
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| "warning".to_string());
    let span = value.pointer("/message/spans/0");
    let location = span
        .and_then(|span| span.get("file_name"))
        .and_then(serde_json::Value::as_str)
        .map(|file| {
            let line = span
                .and_then(|span| span.get("line_start"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_default();
            let column = span
                .and_then(|span| span.get("column_start"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or_default();
            format!("{file}:{line}:{column}")
        });
    match location {
        Some(location) => format!("warning: {message} [{location}]"),
        None => format!("warning: {message}"),
    }
}

/// Strip ANSI escapes from one captured line.
///
/// Cargo colours `error[E0308]: ...` whenever the caller forces colour
/// (`CARGO_TERM_COLOR=always`, `--color always`, `CLICOLOR_FORCE`). The escape
/// bytes then sit in front of `error[`, so a plain `starts_with` scan reports
/// `status: "failed"` with `error_count: 0, errors: []` -- the empty digest this
/// mode exists to prevent. Borrow when there is nothing to strip, which is the
/// common (piped, uncoloured) case.
fn without_ansi(line: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    if !line.contains('\u{1b}') {
        return Cow::Borrowed(line);
    }
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            // CSI: parameters/intermediates, then one final byte in `@..=~`.
            Some('[') => {
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            // OSC: consume until BEL or ST (`ESC \`).
            Some(']') => {
                for next in chars.by_ref() {
                    if next == '\u{7}' {
                        break;
                    }
                    if next == '\u{1b}' {
                        let _ = chars.next();
                        break;
                    }
                }
            }
            // Two-byte escape, or a truncated sequence at end of line.
            Some(_) | None => {}
        }
    }
    Cow::Owned(out)
}

fn is_error_line(trimmed: &str) -> bool {
    trimmed.starts_with("error[")
        || trimmed.starts_with("error:")
        // cargo-nextest's per-test failure line, e.g. `FAIL [ 0.1s] pkg::t`.
        || trimmed.starts_with("FAIL ")
        || trimmed.starts_with("FAILED ")
}

/// Errors from cargo's `--message-format=json` stream, when this stream is
/// one. `None` means "not a JSON stream" so the plain scan takes over.
///
/// The shape mirrors `strip_diagnostics::merge_cargo_json_diagnostics`
/// (`reason == "compiler-message"`, `/message/rendered`), extended to the
/// error level so a failure has an identity rather than just a count.
/// Returns `(true_count, capped_identities)`.
fn extract_cargo_json_errors(output: &str) -> Option<(usize, Vec<String>)> {
    let mut saw_message = false;
    let mut errors: Vec<String> = Vec::new();
    let mut total = 0usize;
    for line in output.lines() {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let Some(reason) = value.get("reason").and_then(serde_json::Value::as_str) else {
            continue;
        };
        saw_message = true;
        if reason != "compiler-message" {
            continue;
        }
        if value
            .pointer("/message/level")
            .and_then(serde_json::Value::as_str)
            != Some("error")
        {
            continue;
        }
        total += 1;
        if errors.len() < MAX_ERROR_IDENTITIES {
            errors.push(json_error_identity(&value));
        }
    }
    saw_message.then_some((total, errors))
}

/// Short identity for one cargo JSON `compiler-message` error: its rustc
/// error code when present (`E0308`), else the first line of the message.
fn json_error_identity(value: &serde_json::Value) -> String {
    let code = value
        .pointer("/message/code/code")
        .and_then(serde_json::Value::as_str);
    let message = value
        .pointer("/message/message")
        .and_then(serde_json::Value::as_str)
        .map(first_line)
        .unwrap_or_else(|| "error".to_string());
    match code {
        Some(code) => format!("{code}: {message}"),
        None => message,
    }
}

/// soldr's own classification of a failure, when an existing classifier
/// recognizes the stream. Consulted only for a failing run.
fn classify_diagnostic(output: &str) -> Option<String> {
    if let Some(failure) = super::strip_diagnostics::detect_requested_strip_failure(output) {
        return Some(format!("strip-failure: {}", failure.utility));
    }
    let diagnosis = crate::cargo_diagnostics::detect_build_script_failure(output)?;
    let category = match diagnosis.category {
        crate::cargo_diagnostics::DiagnosisCategory::MissingHostTool => "missing-host-tool",
        crate::cargo_diagnostics::DiagnosisCategory::WindowsMsvcOpenSslNotFound => {
            "windows-msvc-openssl-not-found"
        }
    };
    Some(format!("build-script-{category}: {}", diagnosis.crate_name))
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_string()
}

/// Write the child's full captured stdout+stderr to a durable log and
/// return its path plus the run start time.
///
/// The log's filename mirrors the always-on build-log convention
/// (`<timestamp>-<cwd-slug>`) so a directory listing sorts
/// chronologically, and the body carries a short header so the raw child
/// bytes are unambiguous.
pub(super) fn write_run_log(
    paths: &SoldrPaths,
    started_at_ms: i64,
    cwd: &Path,
    command: &str,
    exit_code: i32,
    output: &str,
) -> Result<PorcelainLogArtifact, crate::core::SoldrError> {
    let dir = log_dir(paths);
    std::fs::create_dir_all(&dir)?;
    let filename = format!(
        "{}-{}.log",
        utc_compact_timestamp(started_at_ms),
        sanitize_cwd_slug(cwd)
    );
    let path = unique_path(&dir, &filename);
    let mut body = String::with_capacity(output.len() + 256);
    body.push_str("# soldr porcelain run log\n");
    body.push_str(&format!("# schema_version: {SCHEMA_VERSION}\n"));
    body.push_str(&format!("# command: {command}\n"));
    body.push_str(&format!("# started_at_ms: {started_at_ms}\n"));
    body.push_str(&format!("# exit_code: {exit_code}\n"));
    body.push_str("# --- child cargo stdout+stderr ---\n");
    body.push_str(output);
    if !output.ends_with('\n') {
        body.push('\n');
    }
    std::fs::write(&path, body)?;
    Ok(PorcelainLogArtifact {
        path,
        started_at_ms,
    })
}

/// Best-effort durable log for a run that failed before capture. Returns
/// `None` when even the soldr root cannot be resolved.
pub(super) fn write_failure_log(
    paths: &SoldrPaths,
    command: &str,
    message: &str,
) -> Option<PathBuf> {
    let now = super::current_unix_ms();
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    write_run_log(paths, now, &cwd, command, -1, message)
        .ok()
        .map(|artifact| artifact.path)
}

pub(super) fn unique_path(dir: &Path, filename: &str) -> PathBuf {
    let mut candidate = dir.join(filename);
    if !candidate.exists() {
        return candidate;
    }
    let stem = filename.strip_suffix(".log").unwrap_or(filename);
    let mut suffix = 2;
    while candidate.exists() {
        candidate = dir.join(format!("{stem}-{suffix}.log"));
        suffix += 1;
    }
    candidate
}

/// Retention sweep entry point for the existing auto-GC pass
/// (`gc::run_gc_auto_sweep_command`), beside `build_log::prune_build_logs`.
///
/// This is the enforcing half of the `expires_at` contract: a log past its
/// [`PORCELAIN_LOG_TTL_MS`] age, or beyond the newest
/// [`PORCELAIN_LOG_KEEP`] entries, is deleted here.
pub(crate) fn prune(paths: &SoldrPaths) -> usize {
    prune_porcelain_logs(&log_dir(paths), PORCELAIN_LOG_KEEP, PORCELAIN_LOG_TTL_MS)
}

/// Delete porcelain logs that are past their TTL or beyond the newest
/// `keep` entries. Best-effort: per-file delete errors are swallowed.
/// Returns the number of files actually deleted.
///
/// This is the retention half of the `expires_at` contract, and it is
/// called from the existing auto-GC sweeper (`gc::run_gc_auto_sweep_command`)
/// beside `build_log::prune_build_logs`, so porcelain logs ride the same
/// throttle and detached-process design as the always-on build logs.
pub(super) fn prune_porcelain_logs(dir: &Path, keep: usize, ttl_ms: i64) -> usize {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return 0;
    };
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0);
    let mut entries: Vec<(PathBuf, Option<i64>)> = read_dir
        .filter_map(|entry| entry.ok())
        .map(|entry| {
            let path = entry.path();
            let modified_ms = entry
                .metadata()
                .ok()
                .and_then(|meta| meta.modified().ok())
                .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis() as i64);
            (path, modified_ms)
        })
        .filter(|(path, _)| path.extension().and_then(|ext| ext.to_str()) == Some("log"))
        .collect();
    // Newest-first by filename (timestamp prefix sorts lexically).
    entries.sort_by(|a, b| b.0.file_name().cmp(&a.0.file_name()));

    let mut deleted = 0;
    for (index, (path, modified_ms)) in entries.into_iter().enumerate() {
        let beyond_keep = index >= keep;
        let expired = modified_ms.is_some_and(|modified| now_ms.saturating_sub(modified) > ttl_ms);
        if !beyond_keep && !expired {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            deleted += 1;
        }
    }
    deleted
}

/// Emit the single JSON line on stdout. Infallible: a serialization
/// failure (impossible for this fixed shape) degrades to a minimal
/// object rather than printing a second line or panicking.
pub(super) fn emit(report: &PorcelainReport) {
    let line = serde_json::to_string(report).unwrap_or_else(|_| {
        format!(
            "{{\"schema_version\":{SCHEMA_VERSION},\"command\":{:?},\"exit_code\":{}}}",
            report.command, report.exit_code
        )
    });
    use std::io::Write as _;
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = writeln!(lock, "{line}");
    let _ = lock.flush();
    mark_emitted();
}

static EMITTED: AtomicBool = AtomicBool::new(false);

/// Record that the one-and-only porcelain line has been written.
fn mark_emitted() {
    EMITTED.store(true, Ordering::SeqCst);
}

/// Whether a porcelain line has already been written this process.
///
/// The catch-all emitter in `run_cargo_front_door` uses this so a run
/// that failed *before* the capture stage still produces exactly one
/// line, and a run that already emitted its rich report is not repeated.
pub(super) fn already_emitted() -> bool {
    EMITTED.load(Ordering::SeqCst)
}
