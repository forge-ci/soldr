//! Unit tests for the `--porcelain` / `SOLDR_PORCELAIN` machine-readable
//! front-door mode: the JSON shape, the durable log, the summary digest,
//! the retention sweep, and precedence against the other capture modes.

use super::porcelain::*;
use crate::build_log_meta::utc_compact_timestamp;
use crate::cargo_front_door::cache_states::DeferredCacheDrain;
use crate::core::SoldrPaths;
use crate::{EnvVarGuard, TEST_PROCESS_ENV_LOCK};

fn temp_paths() -> (tempfile::TempDir, SoldrPaths) {
    let dir = tempfile::tempdir().expect("temp dir");
    let paths = SoldrPaths::with_root(dir.path().to_path_buf());
    (dir, paths)
}

fn run_inputs<'a>(
    command: &'a str,
    exit_code: i32,
    log_path: Option<std::path::PathBuf>,
    output: Option<&'a str>,
) -> RunReportInputs<'a> {
    RunReportInputs {
        command,
        exit_code,
        started_at_ms: 1_000,
        ended_at_ms: 1_750,
        log_path,
        output,
        cache_lines: 0,
    }
}

#[test]
fn report_carries_every_required_field() {
    let (dir, paths) = temp_paths();
    let artifact = write_run_log(
        &paths,
        1_000,
        dir.path(),
        "cargo build --release",
        0,
        "   Compiling foo v0.1.0\n    Finished `release` profile\n",
    )
    .expect("write run log");

    let report = report_for_run(&run_inputs(
        "cargo build --release",
        0,
        Some(artifact.path.clone()),
        Some("   Compiling foo v0.1.0\n"),
    ));
    let line = serde_json::to_string(&report).expect("serialize");

    // The contract from the task: exactly these keys, at minimum.
    for key in [
        "schema_version",
        "command",
        "exit_code",
        "duration_ms",
        "log_path",
        "expires_at",
        "summary",
    ] {
        assert!(
            line.contains(&format!("\"{key}\"")),
            "missing {key}: {line}"
        );
    }
    assert_eq!(report.schema_version, SCHEMA_VERSION);
    assert_eq!(report.command, "cargo build --release");
    assert_eq!(report.exit_code, 0);
    assert_eq!(report.duration_ms, 750);
    assert_eq!(
        report.log_path.as_deref(),
        Some(artifact.path.display().to_string().as_str())
    );
    assert_eq!(report.expires_at, Some(1_000 + PORCELAIN_LOG_TTL_MS));
    assert_eq!(report.summary.status, "ok");
    assert_eq!(report.summary.exit_code, 0);
    assert_eq!(report.summary.cache_lines, 0);
}

/// The per-crate annotations a porcelain run moves out of stderr are reported
/// as a count, so a caller can tell the detail exists without reading it.
#[test]
fn the_report_counts_the_annotations_it_moved_into_the_log() {
    let report = report_for_run(&RunReportInputs {
        cache_lines: 16,
        ..run_inputs("cargo build", 0, None, Some("    Finished `dev` profile\n"))
    });
    assert_eq!(report.summary.cache_lines, 16);
    let line = serde_json::to_string(&report).expect("serialize");
    assert!(
        line.contains("\"cache_lines\":16"),
        "count must be on the wire: {line}"
    );
}

#[test]
fn report_without_a_log_has_null_path_and_expiry() {
    let report = report_for_run(&run_inputs("cargo test", 101, None, Some("")));
    let value = serde_json::to_value(&report).expect("serialize");
    assert!(value["log_path"].is_null());
    assert!(value["expires_at"].is_null());
}

#[test]
fn summarize_scans_plain_rustc_and_nextest_failures() {
    let output = "error[E0308]: mismatched types\nwarning: unused\nerror: aborting due to 1 previous error\nFAIL [   0.123s] my_crate::tests::it_works\n";
    let summary = summarize("cargo test", 101, output);
    assert_eq!(summary.status, "failed");
    assert_eq!(summary.exit_code, 101);
    // `error[..]`, `error:` and `FAIL ` all count.
    assert_eq!(summary.error_count, 3);
    assert!(summary.errors.iter().any(|e| e.starts_with("error[E0308]")));
    assert!(summary.errors.iter().any(|e| e.starts_with("FAIL ")));
}

#[test]
fn summarize_sees_coloured_failures() {
    // Cargo emits SGR escapes around `error[..]` whenever colour is forced.
    // The digest must not go empty just because the caller asked for colour.
    let output = "\u{1b}[1m\u{1b}[31merror[E0308]\u{1b}[0m\u{1b}[1m: mismatched types\u{1b}[0m\n\
                  \u{1b}[1m\u{1b}[31merror\u{1b}[0m\u{1b}[1m: could not compile `demo` (lib)\u{1b}[0m\n";
    let summary = summarize("cargo build", 101, output);
    assert_eq!(summary.status, "failed");
    assert_eq!(summary.error_count, 2, "colour must not hide the digest");
    assert!(
        summary.errors[0].starts_with("error[E0308]"),
        "{:?}",
        summary.errors
    );
    assert!(
        !summary.errors.iter().any(|e| e.contains('\u{1b}')),
        "escape bytes must not survive into the identities: {:?}",
        summary.errors
    );
}

#[test]
fn summarize_sees_nextest_failures_through_colour() {
    let output = "\u{1b}[31mFAIL\u{1b}[0m [   0.123s] my_crate::tests::it_works\n";
    let summary = summarize("cargo nextest run", 100, output);
    assert_eq!(summary.error_count, 1);
    assert_eq!(
        summary.errors,
        vec!["FAIL [   0.123s] my_crate::tests::it_works"]
    );
}

#[test]
fn summarize_counts_a_repeated_failure_once() {
    // cargo-nextest prints the same FAIL line in the progress stream and again
    // in its summary; one failing test must not read as two failures.
    let output = concat!(
        "FAIL [   0.008s] (  1/302) demo_big::late_failure the_one_that_fails\n",
        "FAIL [   0.008s] (  1/302) demo_big::late_failure the_one_that_fails\n",
        "error: test run failed\n",
    );
    let summary = summarize("cargo nextest run", 100, output);
    assert_eq!(summary.error_count, 2, "one FAIL + one error line");
    assert_eq!(
        summary.errors,
        vec![
            "FAIL [   0.008s] (  1/302) demo_big::late_failure the_one_that_fails".to_string(),
            "error: test run failed".to_string(),
        ]
    );
}

#[test]
fn failure_report_reports_the_process_exit_code() {
    // soldr's own pre-status failures reach `report_and_exit`, which exits 1.
    let report = report_for_failure(
        "cargo build",
        FAILURE_EXIT_CODE,
        None,
        Some("soldr: could not start cargo"),
    );
    assert_eq!(report.exit_code, FAILURE_EXIT_CODE);
    assert_eq!(report.summary.exit_code, FAILURE_EXIT_CODE);
    assert_eq!(report.summary.status, "failed");
    assert_eq!(report.summary.error_count, 1);
}

#[test]
fn signal_killed_child_reports_the_code_the_process_exits_with() {
    // A signalled child has no `ExitStatus::code`, so `effective_exit_code`
    // answers -1 while the process exits FAILURE_EXIT_CODE. Expressed on
    // `status.code()` so it needs no host-cfg selection.
    assert_eq!(process_exit_code(-1, None), FAILURE_EXIT_CODE);
    assert_eq!(
        process_exit_code(-1, Some(101)),
        101,
        "a real code must never be replaced by the -1 sentinel"
    );
    assert_eq!(process_exit_code(101, Some(101)), 101);
    assert_eq!(process_exit_code(0, Some(0)), 0);
    assert_eq!(process_exit_code(1, None), 1, "an explicit code wins");
}

#[test]
fn summarize_prefers_the_cargo_json_stream_when_present() {
    let output = concat!(
        r#"{"reason":"compiler-message","message":{"level":"error","code":{"code":"E0432"},"message":"unresolved import `foo`\nsecond line"}}"#,
        "\n",
        r#"{"reason":"compiler-message","message":{"level":"warning","code":null,"message":"unused"}}"#,
        "\n",
        r#"{"reason":"build-finished","success":false}"#,
        "\n",
    );
    let summary = summarize("cargo build", 101, output);
    assert_eq!(summary.error_count, 1, "only error-level messages count");
    assert_eq!(
        summary.errors,
        vec!["E0432: unresolved import `foo`".to_string()]
    );
}

#[test]
fn summarize_is_ok_for_a_clean_run() {
    let summary = summarize("cargo build", 0, "    Finished `dev` profile\n");
    assert_eq!(summary.status, "ok");
    assert_eq!(summary.error_count, 0);
    assert!(summary.errors.is_empty());
}

#[test]
fn run_log_is_durable_and_readable_afterwards() {
    let (dir, paths) = temp_paths();
    let marker = "   Compiling soldr-cli v0.9.43\nsrc/lib.rs:1:1: error[E0308]: boom\n";
    let artifact = write_run_log(
        &paths,
        1_700_000_000_000,
        dir.path(),
        "cargo build",
        101,
        marker,
    )
    .expect("write run log");

    assert!(artifact.path.starts_with(log_dir(&paths)));
    assert_eq!(artifact.started_at_ms, 1_700_000_000_000);
    let body = std::fs::read_to_string(&artifact.path).expect("read back the run log");
    // Nothing of substance may be lost: the raw child bytes are all there.
    assert!(body.contains(marker.trim_end()), "child bytes must survive");
    assert!(body.contains("# command: cargo build"));
    assert!(body.contains("# exit_code: 101"));
    assert!(body.contains(&format!("# schema_version: {SCHEMA_VERSION}")));
}

/// The annotations are preserved, not dropped: they land in the log under a
/// header that names them as a separate source, and the child's bytes above
/// them are untouched.
#[test]
fn deferred_annotations_are_appended_to_the_log_as_their_own_section() {
    let (dir, paths) = temp_paths();
    let child = "   Compiling demo v0.1.0\n    Finished `dev` profile\n";
    let artifact =
        write_run_log(&paths, 1_000, dir.path(), "cargo build", 0, child).expect("write run log");
    let drained = DeferredCacheDrain {
        lines: vec![
            "    0.92 soldr[cache] unicode_ident [MISS]".to_string(),
            "   15.35 soldr[cache] demo [MISS]".to_string(),
        ],
        omitted: 4,
    };

    assert_eq!(
        append_deferred_annotations(Some(&artifact), Some(&drained)),
        2
    );

    let body = std::fs::read_to_string(&artifact.path).expect("read log");
    assert!(body.contains(child.trim_end()), "child bytes must survive");
    assert!(
        body.contains(DEFERRED_SECTION_HEADER.trim_end()),
        "section must be labelled: {body}"
    );
    for line in &drained.lines {
        assert!(body.contains(line.as_str()), "missing {line} in {body}");
    }
    assert!(
        body.contains("# 4 further annotation line(s) omitted"),
        "an omission must be stated, not hidden: {body}"
    );
    // Appended after the child section, never spliced into it.
    let child_at = body.find(child.trim_end()).expect("child section");
    let section_at = body
        .find(DEFERRED_SECTION_HEADER.trim_end())
        .expect("section");
    assert!(child_at < section_at);
}

#[test]
fn nothing_deferred_leaves_the_log_alone() {
    let (dir, paths) = temp_paths();
    let artifact =
        write_run_log(&paths, 1_000, dir.path(), "cargo build", 0, "a\n").expect("write run log");
    let before = std::fs::read_to_string(&artifact.path).expect("read log");
    assert_eq!(
        append_deferred_annotations(Some(&artifact), Some(&DeferredCacheDrain::default())),
        0
    );
    assert_eq!(
        std::fs::read_to_string(&artifact.path).expect("read log"),
        before,
        "an empty drain must not touch the log"
    );
    assert_eq!(
        append_deferred_annotations(None, Some(&DeferredCacheDrain::default())),
        0,
        "no log means nothing to append to"
    );
}

#[test]
fn run_log_paths_are_unique_within_the_same_millisecond() {
    let (dir, paths) = temp_paths();
    let first = write_run_log(&paths, 1_000, dir.path(), "cargo build", 0, "a\n")
        .expect("first log")
        .path;
    let second = write_run_log(&paths, 1_000, dir.path(), "cargo build", 0, "b\n")
        .expect("second log")
        .path;
    assert_ne!(first, second, "colliding starts must not overwrite");
    assert!(std::fs::read_to_string(&first)
        .expect("first")
        .contains('a'));
    assert!(std::fs::read_to_string(&second)
        .expect("second")
        .contains('b'));
}

#[test]
fn retention_sweeps_expired_logs() {
    let (dir, paths) = temp_paths();
    let logs = log_dir(&paths);
    std::fs::create_dir_all(&logs).expect("logs dir");
    // One second apart, so the names do not collide.
    let base = 1_700_000_000_000i64;
    for index in 0..3 {
        write_run_log(
            &paths,
            base + index * 1000,
            dir.path(),
            "cargo build",
            0,
            "x\n",
        )
        .expect("seed log");
    }
    // A negative TTL puts every entry past its expiry, deterministically even
    // when the log was written inside the same millisecond as the sweep.
    let deleted = prune_porcelain_logs(&logs, PORCELAIN_LOG_KEEP, -1);
    assert_eq!(deleted, 3, "an expired log must be swept");
    assert_eq!(std::fs::read_dir(&logs).expect("read").count(), 0);
}

#[test]
fn retention_keeps_the_newest_entries_within_the_cap() {
    let (dir, paths) = temp_paths();
    let logs = log_dir(&paths);
    std::fs::create_dir_all(&logs).expect("logs dir");
    // One second apart: the filenames' timestamp prefixes sort lexically.
    let base = 1_700_000_000_000i64;
    for index in 0..5 {
        write_run_log(
            &paths,
            base + index * 1000,
            dir.path(),
            "cargo build",
            0,
            "x\n",
        )
        .expect("seed log");
    }
    let keep = 2;
    let deleted = prune_porcelain_logs(&logs, keep, i64::MAX);
    assert_eq!(deleted, 3);
    let mut remaining: Vec<_> = std::fs::read_dir(&logs)
        .expect("read")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    remaining.sort();
    assert_eq!(remaining.len(), keep);
    // The two newest survived.
    assert!(remaining[0].contains(&utc_compact_timestamp(base + 3 * 1000)));
    assert!(remaining[1].contains(&utc_compact_timestamp(base + 4 * 1000)));
}

#[test]
fn retention_is_a_no_op_for_a_missing_directory() {
    let (dir, _paths) = temp_paths();
    assert_eq!(prune_porcelain_logs(&dir.path().join("nope"), 1, 0), 0);
}

#[test]
fn enabled_follows_the_env_var_only_on_a_truthy_value() {
    let _lock = TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Restores whatever the outer process had on drop.
    let _restore = EnvVarGuard::remove(PORCELAIN_ENV_VAR);
    assert!(!enabled(), "absent means off");
    std::env::set_var(PORCELAIN_ENV_VAR, "0");
    assert!(!enabled(), "=0 stays off");
    std::env::set_var(PORCELAIN_ENV_VAR, "false");
    assert!(!enabled(), "=false stays off");
    std::env::set_var(PORCELAIN_ENV_VAR, "1");
    assert!(enabled(), "=1 turns it on");
}

#[test]
fn failure_report_is_a_well_formed_single_object() {
    let report = report_for_failure("cargo build", -1, None, Some("soldr: cargo timed out"));
    let value = serde_json::to_value(&report).expect("serialize");
    assert_eq!(value["schema_version"], SCHEMA_VERSION);
    assert_eq!(value["command"], "cargo build");
    assert_eq!(value["exit_code"], -1);
    assert_eq!(value["summary"]["status"], "failed");
    assert_eq!(value["summary"]["error_count"], 1);
}

#[test]
fn command_string_labels_the_run() {
    assert_eq!(command_string(&[]), "cargo");
    assert_eq!(
        command_string(&["nextest".to_string(), "run".to_string()]),
        "cargo nextest run"
    );
}
