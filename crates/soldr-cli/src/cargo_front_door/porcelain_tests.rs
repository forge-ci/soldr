//! Unit tests for the `--porcelain` / `SOLDR_PORCELAIN` machine-readable
//! front-door mode: the JSON shape, the durable log, the summary digest,
//! the retention sweep, and precedence against the other capture modes.

use super::porcelain::*;
use crate::build_log_meta::utc_compact_timestamp;
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
fn porcelain_silences_the_cache_state_lines() {
    let _lock = TEST_PROCESS_ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _restore = EnvVarGuard::remove(PORCELAIN_ENV_VAR);
    let _cache = EnvVarGuard::remove("SOLDR_NO_CACHE_STATES");
    assert!(
        crate::cargo_front_door::cache_states::enabled(),
        "on outside porcelain"
    );
    std::env::set_var(PORCELAIN_ENV_VAR, "1");
    assert!(
        !crate::cargo_front_door::cache_states::enabled(),
        "soldr#4005: porcelain keeps stderr free of HIT/MISS lines"
    );
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

#[test]
fn test_failure_uses_the_structured_digest_not_a_duplicate_excerpt() {
    // soldr#3961 landed `failed_tests` / `test_results` on main while this
    // branch was open: a named test failure is now described field-by-field
    // (name, panic `file:line:col`, message) and the count lines live in
    // `test_results`. Repeating that in `excerpt` would double the digest for
    // no new evidence, so the excerpt stays empty here -- and the caller still
    // never has to open `log_path`.
    let output = concat!(
        "   Compiling pfail v0.1.0\n",
        "    Finished `test` profile\n",
        "     Running unittests src/lib.rs\n",
        "test t::wrong_sum ... FAILED\n",
        "\n",
        "failures:\n",
        "\n",
        "---- t::wrong_sum stdout ----\n",
        "\n",
        "thread 't::wrong_sum' (1913010) panicked at src/lib.rs:6:9:\n",
        "assertion `left == right` failed: two plus two should be five\n",
        "note: run with `RUST_BACKTRACE=1` for a backtrace\n",
        "\n",
        "failures:\n",
        "    t::wrong_sum\n",
        "\n",
        "test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n",
        "error: test failed, to rerun pass `--lib`\n",
    );
    let summary = summarize("cargo test", 101, output);
    assert!(
        summary.excerpt.is_empty(),
        "the structured digest already carries this failure: {:?}",
        summary.excerpt
    );
    assert!(!summary.excerpt_truncated);
    assert_eq!(summary.failed_tests.len(), 1, "{:?}", summary.failed_tests);
    let failed = &summary.failed_tests[0];
    assert_eq!(failed.name, "t::wrong_sum");
    assert_eq!(failed.location.as_deref(), Some("src/lib.rs:6:9"));
    assert!(
        failed
            .message
            .iter()
            .any(|m| m.contains("two plus two should be five")),
        "the assertion message is the actionable line: {:?}",
        failed.message
    );
    assert!(
        summary
            .test_results
            .iter()
            .any(|r| r.contains("test result: FAILED")),
        "{:?}",
        summary.test_results
    );
}

#[test]
fn failure_excerpt_carries_the_compiler_location_for_a_build_error() {
    let output = concat!(
        "   Compiling demo v0.1.0\n",
        "error[E0308]: mismatched types\n",
        " --> src/main.rs:12:5\n",
        "  |\n",
        "12 |     let x: u32 = \"nope\";\n",
        "  |            ---   ^^^^^^ expected `u32`, found `&str`\n",
        "  |            |\n",
        "  |            expected due to this\n",
        "\n",
        "error: could not compile `demo` (bin \"demo\") due to 1 previous error\n",
    );
    let summary = summarize("cargo build", 101, output);
    assert!(
        summary
            .excerpt
            .first()
            .is_some_and(|l| l.starts_with("error[E0308]")),
        "{:?}",
        summary.excerpt
    );
    assert!(
        summary
            .excerpt
            .iter()
            .any(|l| l.contains("--> src/main.rs:12:5")),
        "the location line is what makes the error actionable: {:?}",
        summary.excerpt
    );
    assert!(
        !summary
            .excerpt
            .iter()
            .any(|l| l.trim_start().starts_with('|')),
        "the caret art is noise in a digest: {:?}",
        summary.excerpt
    );
}

#[test]
fn failure_excerpt_is_bounded_on_a_pathological_stream() {
    let mut output = String::new();
    for i in 0..5_000 {
        output.push_str(&format!(
            "error[E0308]: mismatched types {i} {}\n  --> src/lib.rs:{i}:1\n",
            "x".repeat(500)
        ));
    }
    let summary = summarize("cargo build", 101, &output);
    assert_eq!(summary.error_count, 5_000, "the count stays truthful");
    assert!(summary.excerpt.len() <= 8, "{}", summary.excerpt.len());
    let bytes: usize = summary.excerpt.iter().map(|l| l.len() + 1).sum();
    assert!(bytes <= 1_200, "{bytes} bytes");
    assert!(
        summary.excerpt.iter().all(|l| l.chars().count() <= 203),
        "per-line cap: {:?}",
        summary.excerpt.first().map(|l| l.chars().count())
    );
    assert!(
        summary.excerpt_truncated,
        "the caller must know it is partial"
    );
    // The excerpt must remain valid, single-line JSON -- no invented bytes.
    let line = serde_json::to_string(&summary).expect("serialize");
    assert!(!line.contains('\n'), "one JSON line");
}

#[test]
fn failure_excerpt_uses_the_json_streams_rendered_message() {
    let rendered = "error[E0432]: unresolved import `foo`\n --> src/lib.rs:3:5\n  |\n3 | use foo::Bar;\n  |     ^^^ no `foo` in the crate root\n";
    let output = format!(
        "{}\n",
        serde_json::json!({
            "reason": "compiler-message",
            "message": {
                "level": "error",
                "code": {"code": "E0432"},
                "message": "unresolved import `foo`",
                "rendered": rendered,
            }
        })
    );
    let summary = summarize("cargo build", 101, &output);
    let excerpt = summary.excerpt.join("\n");
    assert!(excerpt.contains("error[E0432]"), "{excerpt}");
    assert!(excerpt.contains("--> src/lib.rs:3:5"), "{excerpt}");
    assert!(excerpt.contains("no `foo` in the crate root"), "{excerpt}");
}

#[test]
fn failure_excerpt_is_colour_blind() {
    // CI forces colour (soldr#3437); the panic header then carries escapes in
    // front of `thread '`, which is exactly how a naive scan loses the message.
    let output = "\u{1b}[31mthread 't::x' panicked at src/lib.rs:2:5:\u{1b}[0m\n\u{1b}[31massertion failed: false\u{1b}[0m\ntest result: FAILED. 0 passed; 1 failed\n";
    let summary = summarize("cargo test", 101, output);
    let excerpt = summary.excerpt.join("\n");
    assert!(excerpt.contains("panicked at src/lib.rs:2:5:"), "{excerpt}");
    assert!(excerpt.contains("assertion failed: false"), "{excerpt}");
    assert!(!excerpt.contains('\u{1b}'), "no escape bytes in the digest");
}

#[test]
fn success_carries_no_excerpt() {
    let summary = summarize(
        "cargo build",
        0,
        "   Compiling foo v0.1.0\n    Finished `dev` profile\n",
    );
    assert!(summary.excerpt.is_empty());
    assert!(!summary.excerpt_truncated);
}

#[test]
fn pre_capture_failure_reports_its_message_as_the_excerpt() {
    let report = report_for_failure(
        "cargo build",
        FAILURE_EXIT_CODE,
        None,
        Some("soldr: could not start cargo\nmore detail"),
    );
    assert_eq!(
        report.summary.excerpt,
        vec!["soldr: could not start cargo".to_string()]
    );
    assert!(!report.summary.excerpt_truncated);
}

fn libtest_failure(name: &str, line: u32) -> String {
    format!(
        "---- {name} stdout ----\n\nthread '{name}' panicked at crates/x/src/lib.rs:{line}:5:\n\
         assertion `left == right` failed: contract broke\n  left: 1\n right: 2\n\
         note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n\n"
    )
}

#[test]
fn summarize_names_the_failing_test_its_location_and_assertion() {
    let output = format!(
        "running 2 tests\ntest shim::ok ... ok\ntest shim::broken ... FAILED\n\nfailures:\n\n{}\
         failures:\n    shim::broken\n\ntest result: FAILED. 1 passed; 1 failed; 0 ignored\n\
         error: test failed, to rerun pass `-p clud --lib`\n",
        libtest_failure("shim::broken", 410)
    );
    let summary = summarize("cargo test", 101, &output);
    assert_eq!(summary.failed_tests.len(), 1);
    let test = &summary.failed_tests[0];
    assert_eq!(test.name, "shim::broken");
    assert_eq!(test.location.as_deref(), Some("crates/x/src/lib.rs:410:5"));
    assert!(
        test.message[0].contains("contract broke"),
        "{:?}",
        test.message
    );
    assert!(test.message.iter().any(|l| l.contains("right: 2")));
    assert!(test.message.iter().all(|l| !l.contains("RUST_BACKTRACE")));
    assert_eq!(summary.failed_tests_more, 0);
    assert_eq!(summary.test_results.len(), 1);
    assert!(summary.test_results[0].starts_with("test result: FAILED. 1 passed; 1 failed"));
    assert_eq!(summary.errors.len(), 1, "compiler/cargo errors are kept");
}

#[test]
fn many_failing_tests_are_capped_with_a_more_count() {
    let mut output = String::from("failures:\n\n");
    for n in 0..25 {
        output.push_str(&libtest_failure(&format!("t::case_{n}"), 10 + n));
    }
    output.push_str("test result: FAILED. 0 passed; 25 failed\n");
    let summary = summarize("cargo test", 101, &output);
    assert_eq!(summary.failed_tests.len(), 10);
    assert_eq!(summary.failed_tests_more, 15);
    assert_eq!(summary.failed_tests[9].name, "t::case_9");
}

#[test]
fn a_long_panic_message_is_capped_per_failure() {
    let mut output =
        String::from("---- t::big stdout ----\nthread 't::big' panicked at a.rs:1:1:\n");
    for n in 0..100 {
        output.push_str(&format!("line {n}\n"));
    }
    output.push_str("\nfailures:\n");
    let summary = summarize("cargo test", 101, &output);
    assert_eq!(summary.failed_tests[0].message.len(), 20);
}

#[test]
fn nextest_stderr_block_yields_the_panic_detail() {
    let output = "        FAIL [   0.010s] clud shim::broken\n\
        --- STDERR:              clud shim::broken ---\n\
        thread 'shim::broken' panicked at src/shim.rs:410:5:\n\
        boom\n\
        \n\
        ------------\n";
    let summary = summarize("cargo nextest run", 100, output);
    assert_eq!(summary.failed_tests[0].name, "shim::broken");
    assert_eq!(
        summary.failed_tests[0].location.as_deref(),
        Some("src/shim.rs:410:5")
    );
    assert_eq!(summary.failed_tests[0].message, vec!["boom".to_string()]);
}

#[test]
fn success_summary_stays_free_of_failure_fields() {
    let summary = summarize("cargo test", 0, &libtest_failure("a::b", 1));
    let json = serde_json::to_string(&summary).expect("serialize");
    assert!(!json.contains("failed_tests"), "{json}");
    assert!(!json.contains("test_results"), "{json}");
}

/// soldr#3985: porcelain carries the broker skew as `broker_skew` in the JSON.
#[test]
fn serialize_report_adds_broker_skew_only_when_observed() {
    let report = report_for_failure("soldr cargo test", 1, None, Some("boom"));
    let plain = serialize_report(&report, None, &[]).expect("serialize");
    assert!(!plain.contains("broker_skew"), "{plain}");
    let skew = crate::broker_spawn::BrokerSkew {
        running_broker: "soldr-0.9.44-aaaa".into(),
        this_soldr: "soldr-0.9.46-bbbb".into(),
        remedy: crate::broker_spawn::BROKER_REMOVE_COMMAND,
    };
    let line = serialize_report(&report, Some(&skew), &[]).expect("serialize");
    let value: serde_json::Value = serde_json::from_str(&line).expect("json");
    assert_eq!(value["broker_skew"]["running_broker"], "soldr-0.9.44-aaaa");
    assert_eq!(value["broker_skew"]["remedy"], "soldr broker remove");
    assert_eq!(line.lines().count(), 1);
}

/// soldr#3960: porcelain lists installed binaries as `installed`.
#[test]
fn serialize_report_adds_installed_paths() {
    let report = report_for_failure("soldr cargo install", 0, None, None);
    let line = serialize_report(&report, None, &["/x/bin/ghd".to_string()]).expect("serialize");
    let value: serde_json::Value = serde_json::from_str(&line).expect("json");
    assert_eq!(value["installed"][0], "/x/bin/ghd");
    let none = serialize_report(&report, None, &[]).expect("serialize");
    assert!(!none.contains("installed"), "{none}");
}
