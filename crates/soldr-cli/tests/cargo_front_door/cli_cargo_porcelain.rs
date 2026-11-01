//! `--porcelain` / `SOLDR_PORCELAIN`: the machine-readable front-door mode.
//!
//! The contract under test, end to end through the real `soldr` binary:
//!
//! * exactly ONE line of JSON on stdout, and no relayed child output at all;
//! * the child's complete stdout+stderr readable afterwards from `log_path`;
//! * without the flag or the variable, output streams exactly as before;
//! * porcelain takes precedence over the other capture modes and still
//!   composes with a caller-supplied `--message-format=json`.

use crate::common::*;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

/// Markers the fake cargo writes so a test can tell "relayed" from
/// "captured to the log and suppressed".
const STDOUT_MARKER: &str = "PORCELAIN_CHILD_STDOUT_MARKER";
const STDERR_MARKER: &str = "PORCELAIN_CHILD_STDERR_MARKER";

fn success_cargo_script() -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\r\n\
             echo    Compiling porcelain_fixture v0.1.0\r\n\
             echo {STDOUT_MARKER}\r\n\
             echo {STDERR_MARKER} 1>&2\r\n\
             exit /b 0\r\n"
        )
    } else {
        format!(
            "#!/bin/sh\n\
             printf '%s\\n' '   Compiling porcelain_fixture v0.1.0'\n\
             printf '%s\\n' '{STDOUT_MARKER}'\n\
             printf '%s\\n' '{STDERR_MARKER}' >&2\n\
             exit 0\n"
        )
    }
}

fn failing_cargo_script() -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        format!(
            "@echo off\r\n\
             echo {STDOUT_MARKER}\r\n\
             echo error[E0308]: mismatched types 1>&2\r\n\
             exit /b 3\r\n"
        )
    } else {
        format!(
            "#!/bin/sh\n\
             printf '%s\\n' '{STDOUT_MARKER}'\n\
             printf '%s\\n' 'error[E0308]: mismatched types' >&2\n\
             exit 3\n"
        )
    }
}

/// A child that mimics a real rustc compile failure: a diagnostic on stderr
/// and Rust's canonical compile-failure exit code, 101.
///
/// This is the fixture for the soldr#2024 misattribution regression: 101 is
/// the exact status the real reproduction carried, and it is *not* a soldr
/// fault -- the child ran and explained itself.
fn compile_failure_cargo_script() -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        "@echo off\r\nexit /b 101\r\n".to_string()
    } else {
        "#!/bin/sh\nexit 101\n".to_string()
    }
}

/// Phrases from the `exit_guard` annotation (soldr#2024). Either appearing on
/// stderr of a run whose child *did* execute means soldr blamed itself for the
/// child's own failure.
const SOLD_FAULT_PHRASE: &str = "fault in soldr itself";
const EXIT_GUARD_ISSUE: &str = "soldr#2024";

/// Assert that a run whose child ran is not annotated as a soldr fault.
///
/// With `--porcelain` the child's bytes are relayed nowhere -- they live in
/// the `log_path` the single JSON line points at -- so the exit-guard's
/// process-wide `SPOKE` latch is the *only* thing standing between an ordinary
/// red build and this message. If it is not set, every normal compile failure
/// tells the caller (typically an autonomous harness) to file a bug against
/// soldr.
fn assert_not_misattributed_as_a_soldr_fault(stderr: &str, context: &str) {
    assert!(
        !stderr.contains(SOLD_FAULT_PHRASE) && !stderr.contains(EXIT_GUARD_ISSUE),
        "{context}: soldr claimed its own fault for a failure the child explained. \
         The child ran and its diagnostic is in the porcelain log, so neither \
         {SOLD_FAULT_PHRASE:?} nor {EXIT_GUARD_ISSUE:?} may appear.\nstderr:\n{stderr}"
    );
}

struct Fixture {
    workspace: PathBuf,
    cache_root: PathBuf,
    cargo: PathBuf,
}

fn fixture(label: &str, script: &str) -> Fixture {
    let root = unique_temp_dir(label);
    let workspace = root.join("workspace");
    let tool_dir = root.join("tool");
    let cache_root = root.join("cache");
    fs::create_dir_all(workspace.join("src")).expect("workspace source directory");
    fs::create_dir_all(&tool_dir).expect("tool directory");
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"porcelain_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("manifest");
    fs::write(workspace.join("src/lib.rs"), "pub fn fixture() {}\n").expect("source");
    let cargo = fake_script_path(&tool_dir, "cargo");
    write_fake_script(&cargo, script);
    Fixture {
        workspace,
        cache_root,
        cargo,
    }
}

fn run_soldr(fixture: &Fixture, args: &[&str], porcelain_env: bool) -> std::process::Output {
    let mut command = isolated_soldr_command();
    command
        .args(args)
        .current_dir(&fixture.workspace)
        .env("SOLDR_CACHE_DIR", &fixture.cache_root)
        .env("SOLDR_TEST_CARGO_BIN", &fixture.cargo)
        .env("ZCCACHE_DISABLE", "1");
    if porcelain_env {
        command.env("SOLDR_PORCELAIN", "1");
    } else {
        command.env_remove("SOLDR_PORCELAIN");
    }
    command.output().expect("run soldr")
}

/// The single JSON object porcelain mode must emit, or a readable failure.
fn sole_json_line(output: &std::process::Output) -> (Value, String) {
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.len(),
        1,
        "porcelain must emit exactly one line on stdout, got {}:\n{stdout}\nstderr:\n{}",
        lines.len(),
        String::from_utf8_lossy(&output.stderr),
    );
    let value: Value = serde_json::from_str(lines[0])
        .unwrap_or_else(|err| panic!("stdout must be one JSON object ({err}): {stdout}"));
    (value, stdout)
}

fn log_path_of(value: &Value) -> PathBuf {
    PathBuf::from(
        value["log_path"]
            .as_str()
            .expect("porcelain JSON must carry a log_path"),
    )
}

#[test]
fn porcelain_flag_emits_one_json_line_and_suppresses_the_relay() {
    let fixture = fixture("porcelain-flag", &success_cargo_script());
    let output = run_soldr(
        &fixture,
        &["--porcelain", "--no-cache", "cargo", "build"],
        false,
    );

    let (value, _stdout) = sole_json_line(&output);
    assert_eq!(value["schema_version"], 2);
    // soldr#4010: a passing line drops what the agent never acts on.
    assert!(value.get("command").is_none(), "{value}");
    assert_eq!(value["exit_code"], 0);
    assert!(
        value["duration_ms"].is_u64(),
        "duration_ms must be a number"
    );
    assert_eq!(value["status"], "ok");
    assert!(value.get("summary").is_none(), "{value}");

    // Point 2 of the contract: the child's bytes are NOT relayed.
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains(STDOUT_MARKER) && !stderr.contains(STDOUT_MARKER),
        "the child's stdout must not be relayed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        !stderr.contains(STDERR_MARKER),
        "the child's stderr must not be relayed\nstderr:\n{stderr}"
    );

    // Point 1: nothing of substance is lost -- the full detail is on disk.
    let log_path = log_path_of(&value);
    assert!(
        log_path.starts_with(&fixture.cache_root),
        "the run log must live under the soldr root: {}",
        log_path.display()
    );
    assert_eq!(
        log_path.parent().and_then(Path::file_name),
        Some(std::ffi::OsStr::new("porcelain")),
        "run logs live in logs/porcelain"
    );
    let body = fs::read_to_string(&log_path).expect("the emitted log_path must be readable");
    assert!(body.contains(STDOUT_MARKER), "child stdout lost: {body}");
    assert!(body.contains(STDERR_MARKER), "child stderr lost: {body}");
    assert!(body.contains("--- stdout ---"));
    assert!(body.contains("--- stderr ---"));
    assert!(body.contains("# command: cargo build"));
    assert!(body.contains("# exit_code: 0"));

    // soldr#2024: a green porcelain run has nothing to explain, so it is not
    // annotated either.
    assert_not_misattributed_as_a_soldr_fault(&stderr, "porcelain succeeding child");
}

#[test]
fn porcelain_env_var_emits_one_json_line() {
    let fixture = fixture("porcelain-env", &success_cargo_script());
    let output = run_soldr(&fixture, &["--no-cache", "cargo", "build"], true);
    let (value, _stdout) = sole_json_line(&output);
    assert_eq!(value["exit_code"], 0);
    assert!(value["log_path"].is_string());
    // The env-var entry point shares the flag's spawn path, so it must not
    // annotate a success either (soldr#2024).
    assert_not_misattributed_as_a_soldr_fault(
        &String::from_utf8_lossy(&output.stderr),
        "SOLDR_PORCELAIN=1 succeeding child",
    );
}

#[test]
fn a_failing_run_reports_its_exit_code_and_error_digest() {
    let fixture = fixture("porcelain-failure", &failing_cargo_script());
    let output = run_soldr(
        &fixture,
        &["--porcelain", "--no-cache", "cargo", "build"],
        false,
    );

    assert!(
        !output.status.success(),
        "a failing child must fail soldr\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let (value, _stdout) = sole_json_line(&output);
    assert_eq!(value["exit_code"], 3, "the child's exit code is preserved");
    assert_eq!(value["summary"]["status"], "failed");
    assert!(
        value["summary"]["error_count"].as_u64().unwrap_or(0) >= 1,
        "the failing run must report at least one error: {value}"
    );
    let errors = value["summary"]["errors"]
        .as_array()
        .expect("errors is an array");
    assert!(
        errors
            .iter()
            .any(|entry| entry.as_str().is_some_and(|e| e.contains("E0308"))),
        "the error identity must be carried: {value}"
    );
    // The detail still survives in the log.
    let body = fs::read_to_string(log_path_of(&value)).expect("readable run log");
    assert!(body.contains("E0308"), "failure detail lost: {body}");

    // soldr#2024 regression guard: the child ran and emitted the diagnostic
    // above, so soldr must not tell the caller the failure was its own fault.
    assert_not_misattributed_as_a_soldr_fault(
        &String::from_utf8_lossy(&output.stderr),
        "porcelain failing child (exit 3)",
    );
}

/// The soldr#2024 misattribution regression at the exact exit code the real
/// reproduction carried.
///
/// Before the fix, `SPOKE` was never set on the porcelain spawn path (it
/// pipes the child into the log rather than inheriting stdio, and it skips
/// the log-summary call that marks on every other path), so an ordinary
/// compile failure carried this on stderr:
///
/// ```text
/// soldr: exiting 101 — soldr emitted no diagnostic and ran no child process,
/// so nothing has explained this failure.
/// soldr: this is a fault in soldr itself, not a compile error in your project;
/// please report it with the command you ran (soldr#2024).
/// ```
///
/// That is false: the child ran, and its diagnostic is in the `log_path` the
/// single JSON line names. A harness believed it and filed bugs against soldr
/// for its users' broken code.
#[test]
fn porcelain_preserves_a_child_compile_failure_without_blaming_soldr() {
    let fixture = fixture("porcelain-exit-101", &compile_failure_cargo_script());
    let output = run_soldr(
        &fixture,
        &["--porcelain", "--no-cache", "cargo", "build"],
        false,
    );

    assert_eq!(
        output.status.code(),
        Some(101),
        "the child's real exit code must survive porcelain\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let (value, _stdout) = sole_json_line(&output);
    assert_eq!(value["exit_code"], 101);
    assert_eq!(value["summary"]["status"], "failed");
    assert_not_misattributed_as_a_soldr_fault(
        &String::from_utf8_lossy(&output.stderr),
        "porcelain child exiting 101",
    );
}

/// The default (non-porcelain) path is unchanged: a failing child still has
/// its diagnostic relayed through to the caller, and soldr still does not
/// annotate. This pins the behaviour the porcelain fix must not disturb.
#[test]
fn default_path_failure_relays_the_child_and_is_not_annotated() {
    let fixture = fixture("porcelain-default-failure", &failing_cargo_script());
    let output = run_soldr(&fixture, &["--no-cache", "cargo", "build"], false);

    assert_eq!(output.status.code(), Some(3));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // The default path relays the child: its diagnostic reaches the caller
    // (stdout is inherited; stderr is captured and tee'd back).
    assert!(
        format!("{stdout}{stderr}").contains("E0308"),
        "the default path must keep relaying the child's diagnostic\n\
         stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_not_misattributed_as_a_soldr_fault(&stderr, "default path failing child");
}

#[test]
fn without_the_flag_or_variable_behaviour_is_unchanged() {
    let fixture = fixture("porcelain-default", &success_cargo_script());
    let output = run_soldr(&fixture, &["--no-cache", "cargo", "build"], false);

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // The default path still relays the child output (inherited stdio).
    assert!(
        stdout.contains(STDOUT_MARKER) || stderr.contains(STDOUT_MARKER),
        "the default path must keep relaying\nerror:\n{}\nstdout:\n{stdout}\nstderr:\n{stderr}",
        output.status
    );
    // ... and it is emphatically not the single porcelain JSON object.
    assert!(
        serde_json::from_str::<Value>(stdout.trim()).is_err(),
        "the default path must not emit a porcelain payload:\n{stdout}"
    );
}

/// Precedence: porcelain wins over the cargo-JSON capture mode (whose
/// terminal tee is exactly the relay porcelain replaces), and a caller's
/// own `--message-format=json` still leaves the payload parseable.
#[test]
fn porcelain_takes_precedence_and_composes_with_cargo_json_capture() {
    let fixture = fixture("porcelain-precedence", &success_cargo_script());
    let output = run_soldr(
        &fixture,
        &[
            "--porcelain",
            "--no-cache",
            "cargo",
            "build",
            "--message-format=json",
        ],
        false,
    );

    // One line, and it is the porcelain object -- not cargo's JSON stream.
    let (value, stdout) = sole_json_line(&output);
    assert_eq!(value["schema_version"], 2);
    assert_eq!(
        value["status"], "ok",
        "the porcelain envelope must win: {stdout}"
    );
}

/// The "exactly one line" invariant also holds when the front door fails
/// before it can spawn Cargo at all: the catch-all emitter still writes the
/// porcelain object, so a harness never has to parse an empty stdout.
#[test]
fn an_early_front_door_error_still_emits_one_json_line() {
    let fixture = fixture("porcelain-early-error", &success_cargo_script());
    // `--no-cache` after `cargo` is a reserved-argument misuse: the front door
    // rejects it before any spawn.
    let output = run_soldr(
        &fixture,
        &["--porcelain", "cargo", "build", "--no-cache"],
        false,
    );

    assert!(
        !output.status.success(),
        "the misuse must fail the run\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let (value, _stdout) = sole_json_line(&output);
    assert_eq!(value["exit_code"], 1);
    assert_eq!(value["summary"]["status"], "failed");
    assert!(
        value["summary"]["error_count"].as_u64().unwrap_or(0) >= 1,
        "the failure must be described: {value}"
    );
}

/// Composition with an existing `--json` payload verb. Porcelain is a
/// front-door mode; with the variable set, another verb's own JSON payload
/// must still be exactly what that verb documents -- one object, none of the
/// porcelain envelope.
#[test]
fn porcelain_leaves_another_verbs_json_payload_intact() {
    let fixture = fixture("porcelain-json-verb", &success_cargo_script());
    let output = run_soldr(&fixture, &["logs", "paths", "--json"], true);

    assert!(
        output.status.success(),
        "logs paths --json must succeed\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value: Value =
        serde_json::from_str(&stdout).expect("the other verb's JSON payload must stay parseable");
    assert_eq!(value["schema_version"], 1);
    assert!(
        value["paths"].is_array(),
        "the verb's own payload must be intact: {stdout}"
    );
    assert!(
        value.get("summary").is_none() && value.get("log_path").is_none(),
        "the porcelain envelope must not leak into another verb: {stdout}"
    );
}
