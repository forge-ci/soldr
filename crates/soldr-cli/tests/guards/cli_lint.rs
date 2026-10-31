#![allow(unused_imports)]

use crate::common::*;
use std::{
    fs,
    io::Read,
    path::Path,
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant},
};

fn successful_tool_script() -> &'static str {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        "@echo off\nexit /b 0\n"
    } else {
        "#!/bin/sh\nexit 0\n"
    }
}

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).into()).collect()
}

/// How long an *uncancelled* sibling lint child would run for
/// (soldr#1876).
///
/// This is the negative signal: if cancellation regresses, the run takes at
/// least this long. It is deliberately far above
/// [`SIBLING_CANCEL_BUDGET_SECS`] so the two outcomes cannot be confused on a
/// loaded runner. Raising it costs nothing on the passing path — the siblings
/// are killed almost immediately — and only lengthens an already-failing run.
const SIBLING_SLEEP_SECS: u32 = 30;

/// Budget for `lint deps` spawn + fail + cancel, with front-door startup
/// already subtracted (soldr#2605).
///
/// It used to bound the *whole* invocation, startup included, and so had to
/// "leave room for soldr startup" -- but startup is the dominant and most
/// variable term. Measured on a Windows host: 5.9s warm, 13.7s on a cold
/// broker/daemon start, and over 15s when the run also paid a rustup
/// bootstrap, all with cancellation working correctly. The budget was mostly
/// reporting runner weather.
///
/// Excluding startup makes the number mean one thing. The work it now bounds
/// runs ~1-2s normally and would run ~[`SIBLING_SLEEP_SECS`] if cancellation
/// regressed, so this sits between two outcomes rather than inside the spread
/// of one. The previous 5 s against a 10 s sleep flaked on
/// `target-run x86_64-pc-windows-msvc` at 5.247 s.
const SIBLING_CANCEL_BUDGET_SECS: u64 = 15;

/// Front-door startup time, from the child's `SOLDR_STARTUP_TRACE` lines
/// (soldr#2605).
///
/// `soldr_exited` is the parent's *whole* runtime: getting soldr running,
/// plus the `lint deps` work this test actually bounds. Measured on a Windows
/// host, the first half alone ran 5.9s warm, 13.7s on a cold broker/daemon
/// start, and past 15s when the run also paid a rustup bootstrap — against a
/// 15s budget. So the budget was mostly measuring how busy the runner was,
/// and a slow-start failure was indistinguishable from the cancellation
/// regression this test exists to catch.
///
/// Subtracting this leaves spawn + fail + cancel, which is the quantity the
/// budget was always meant to bound. Returns zero when no trace line is
/// present — soldr died before the front door, and the assertion falls back
/// to bounding the whole run, which is the old behaviour.
///
/// Scope: the traced window ends at `clap_parse`, so this covers front-door
/// startup *including* broker bringup — `broker_image_hash` alone reached
/// 9.5s and a front-door total of 15.9s on CI job 97063065040 — but not a
/// rustup bootstrap, which the children pay later during toolchain
/// resolution. CI hosts arrive with rustup installed, so broker bringup is
/// the term that actually contaminates this budget there.
fn front_door_startup(stderr: &str) -> Duration {
    stderr
        .lines()
        .filter(|line| line.contains("soldr front-door:"))
        .filter_map(|line| line.rsplit_once("total_ms="))
        .filter_map(|(_, tail)| tail.split_whitespace().next())
        .filter_map(|value| value.parse::<u64>().ok())
        .max()
        .map(Duration::from_millis)
        .unwrap_or_default()
}

fn dependency_failure_script() -> &'static str {
    // `ping -n <N>` waits N-1 intervals, so N = SIBLING_SLEEP_SECS + 1.
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        const _: () = assert!(SIBLING_SLEEP_SECS == 30);
        "@echo off\nif \"%~1\"==\"metadata\" exit /b 0\nif \"%~1\"==\"audit\" exit /b 9\nping -n 31 127.0.0.1 > nul\nexit /b 0\n"
    } else {
        const _: () = assert!(SIBLING_SLEEP_SECS == 30);
        "#!/bin/sh\nif [ \"$1\" = metadata ]; then exit 0; fi\nif [ \"$1\" = audit ]; then exit 9; fi\nsleep 30\n"
    }
}

#[test]
fn lint_deps_runs_all_tools_without_compiler_cache() {
    let root = unique_temp_dir("lint-deps");
    let log = root.join("cargo.log");
    let cargo = install_logging_fake_cargo(&log);
    let tools = root.join("tools");
    fs::create_dir_all(&tools).expect("create fake tool dir");
    for subcommand in ["deny", "audit", "machete"] {
        let tool = fake_script_path(&tools, &format!("cargo-{subcommand}"));
        write_fake_script(&tool, successful_tool_script());
    }

    let output = isolated_soldr_command()
        .args(["lint", "deps"])
        .env("SOLDR_CACHE_DIR", root.join("cache"))
        .env("SOLDR_TEST_CARGO_BIN", cargo)
        .env("PATH", prepend_to_path(&tools))
        .output()
        .expect("run soldr lint deps");

    assert!(
        output.status.success(),
        "lint deps failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let invocations = read_logged_cargo_invocations(&log);
    for expected in [
        vec!["deny".to_string(), "check".to_string()],
        vec!["audit".to_string()],
        vec!["machete".to_string()],
    ] {
        // soldr#2589: on a miss, include soldr's stderr — it carries the
        // per-leg `lint deps: `cargo <sub>` (pid N) exited with <status>`
        // telemetry that distinguishes a child that ran but whose log
        // vanished from a leg that never ran yet reported success.
        assert!(
            invocations.contains(&expected),
            "expected dependency check {expected:?}; got {invocations:?}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr),
        );
    }
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("cache daemon"),
        "dependency-only lint must not start the compiler cache"
    );
}

/// soldr#2589, reader half: a claimed slot whose `line.txt` is blank must be
/// a hard error, not a shrug.
///
/// The 2026-08-17 recurrence lost the `machete` invocation with every process
/// exiting 0 — the writer's `if not exist` guard passes for a 0-byte file and
/// `Ok("")` fell straight through to the terminal blank-line filter, so the
/// line vanished with no signal anywhere. Blank means the writer claimed the
/// slot and lost the content; that is a defect, not an empty invocation.
#[test]
#[should_panic(expected = "blank line.txt")]
fn blank_fake_cargo_slot_is_a_hard_error() {
    let root = unique_temp_dir("lint-blank-slot");
    let log = root.join("cargo.log");
    let slot = root.join("cargo.log.d").join("00000000_1_1");
    fs::create_dir_all(&slot).expect("create fake slot");
    fs::write(slot.join("line.txt"), "").expect("write blank slot line");

    let _ = read_logged_cargo_invocations(&log);
}

/// The companion to [`blank_fake_cargo_slot_is_a_hard_error`]: a populated
/// slot still reads back as exactly one invocation, split on the unit
/// separator.
#[test]
fn populated_fake_cargo_slot_reads_back_as_one_invocation() {
    let root = unique_temp_dir("lint-slot-readback");
    let log = root.join("cargo.log");
    let slot = root.join("cargo.log.d").join("00000000_1_1");
    fs::create_dir_all(&slot).expect("create fake slot");
    fs::write(slot.join("line.txt"), "deny\u{1f}check\r\n").expect("write slot line");

    assert_eq!(
        read_logged_cargo_invocations(&log),
        vec![strings(&["deny", "check"])],
    );
}

/// soldr#2589, writer half: concurrent fake-cargo children must each land
/// exactly one readable line.
///
/// `lint deps` runs deny/audit/machete in parallel, and every sighting of this
/// flake has been one of those three lines going missing. This is the same
/// shape with more contenders — and it **reproduces the bug**, which nothing
/// else ever did: at this width it failed 2 runs in 12 against the unfixed
/// slot root, and 0 in 24 with the fix, turning a roughly-once-per-20-CI-runs
/// flake into a sub-second local experiment.
///
/// The width is load-bearing. The collision needs two children to reach
/// `mkdir` before *either* has created the shared parent, so the pair at risk
/// is the first two to start; more writers means more chances that two of them
/// land in that window, not merely more writes. Lowering this to make a lane
/// green would delete the only reliable repro this issue has had.
#[test]
fn concurrent_fake_cargo_writers_each_land_one_line() {
    const WRITERS: usize = 32;

    let root = unique_temp_dir("lint-slot-concurrency");
    let log = root.join("cargo.log");
    let cargo = install_logging_fake_cargo(&log);

    let children: Vec<_> = (0..WRITERS)
        .map(|index| {
            let tag = format!("--writer{index}");
            std::process::Command::new(&cargo)
                .args(["check", tag.as_str()])
                .spawn()
                .expect("spawn fake cargo writer")
        })
        .collect();
    for mut child in children {
        let status = child.wait().expect("wait for fake cargo writer");
        assert!(
            status.success(),
            "fake cargo writer exited with {status} (97 = the slot write never landed)"
        );
    }

    let invocations = read_logged_cargo_invocations(&log);
    assert_eq!(
        invocations.len(),
        WRITERS,
        "expected one line per writer; got {invocations:?}"
    );
    for index in 0..WRITERS {
        let tag = format!("--writer{index}");
        let expected = strings(&["check", tag.as_str()]);
        assert!(
            invocations.contains(&expected),
            "writer {index} lost its line; got {invocations:?}"
        );
    }
}

#[test]
fn lint_rust_uses_canonical_commands_without_a_redundant_check() {
    let root = unique_temp_dir("lint-rust");
    let log = root.join("cargo.log");
    let cargo = install_logging_fake_cargo(&log);
    let tools = root.join("tools");
    fs::create_dir_all(&tools).expect("create fake tool dir");
    let dylint = fake_script_path(&tools, "cargo-dylint");
    write_fake_script(&dylint, successful_tool_script());
    let dylint_link = fake_script_path(&tools, "dylint-link");
    write_fake_script(&dylint_link, successful_tool_script());
    let dylint_channel = format!(
        "nightly-2026-05-26-{}",
        soldr_cli::pyo3_detect::host_triple()
    );
    let dylint_release = "1.89.0-nightly";
    let dylint_commit = "0123456789abcdef0123456789abcdef01234567";
    let dylint_identity = format!("{dylint_channel}|{dylint_release}|{dylint_commit}");
    let driver_root = root.join("drivers");
    let driver = fake_script_path(&driver_root.join(&dylint_channel), "dylint-driver");
    fs::create_dir_all(driver.parent().expect("driver parent"))
        .expect("create prebuilt driver dir");
    let driver_script = if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        "@echo off\necho dylint-driver 6.2.0\nexit /b 0\n"
    } else {
        "#!/bin/sh\nprintf 'dylint-driver 6.2.0\\n'\n"
    };
    write_fake_script(&driver, driver_script);
    let rustc = install_versioned_fake_rustc(
        "rustc 1.89.0-nightly (0123456789abcdef0123456789abcdef01234567 2026-05-26)",
    );

    let output = isolated_soldr_command()
        .args([
            "--no-cache",
            "lint",
            "rust",
            "--check",
            "--package",
            "soldr-cli",
        ])
        .env("SOLDR_CACHE_DIR", root.join("cache"))
        // soldr#3203: a no-cache run prepares the target directory Cargo will
        // use, which from this crate is the repository's own `target/` -- the
        // tree this test suite is running out of. Give the fake build its own.
        .env("CARGO_TARGET_DIR", root.join("target"))
        .env("SOLDR_TEST_CARGO_BIN", cargo)
        .env("SOLDR_TEST_RUSTC_BIN", rustc)
        .env("SOLDR_DYLINT_CONFIGURED_TOOLCHAIN", dylint_channel)
        .env("SOLDR_DYLINT_CONFIGURED_RUSTC_RELEASE", dylint_release)
        .env("SOLDR_DYLINT_CONFIGURED_RUSTC_COMMIT_HASH", dylint_commit)
        .env("SOLDR_DYLINT_PREPARED_IDENTITY", dylint_identity)
        .env("DYLINT_DRIVER_PATH", driver_root)
        .env("PATH", prepend_to_path(&tools))
        .output()
        .expect("run soldr lint rust");

    assert!(
        output.status.success(),
        "lint rust failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(
        read_logged_cargo_invocations(&log)
            .into_iter()
            .filter(|args| {
                args.first()
                    .is_none_or(|subcommand| subcommand != "metadata")
            })
            .collect::<Vec<_>>(),
        vec![
            strings(&["fmt", "--all", "--package", "soldr-cli", "--", "--check",]),
            strings(&[
                "clippy",
                "--workspace",
                "--all-targets",
                "--package",
                "soldr-cli",
                "--",
                "-D",
                "warnings",
            ]),
            strings(&[
                "dylint",
                "--all",
                "--",
                "--workspace",
                "--all-targets",
                "--package",
                "soldr-cli",
            ]),
        ],
        "lint rust must use its canonical fmt/clippy/dylint pipeline"
    );
}

#[test]
fn dependency_failure_cancels_sibling_lint_children() {
    let root = unique_temp_dir("lint-deps-cancel");
    let tools = root.join("tools");
    fs::create_dir_all(&tools).expect("create fake tool dir");
    for subcommand in ["deny", "audit", "machete"] {
        let tool = fake_script_path(&tools, &format!("cargo-{subcommand}"));
        write_fake_script(&tool, successful_tool_script());
    }
    let cargo = fake_script_path(&tools, "cargo");
    write_fake_script(&cargo, dependency_failure_script());

    let mut child = isolated_soldr_command()
        .args(["lint", "deps"])
        .env("SOLDR_CACHE_DIR", root.join("cache"))
        .env("SOLDR_TEST_CARGO_BIN", cargo)
        .env("PATH", prepend_to_path(&tools))
        // soldr#2605: lets the assertions below subtract startup from
        // the measured window. Same mechanism cli_daemon_lifecycle.rs
        // already uses; costs a few stderr lines this test captures anyway.
        .env(soldr_cli::startup_trace::STARTUP_TRACE_ENV_VAR, "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn failing soldr lint deps");

    // Drain both pipes on their own threads. A single-threaded read would
    // deadlock against a child that fills the other pipe's buffer, and the
    // EOF instant is a measurement here, not just cleanup.
    let started = Instant::now();
    let mut stdout_pipe = child.stdout.take().expect("piped stdout");
    let mut stderr_pipe = child.stderr.take().expect("piped stderr");
    let stdout_drain = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buffer);
        (buffer, Instant::now())
    });
    let stderr_drain = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buffer);
        (buffer, Instant::now())
    });

    let status = child.wait().expect("wait for failing soldr lint deps");
    let soldr_exited = started.elapsed();
    let (stdout_bytes, stdout_eof) = stdout_drain.join().expect("join stdout drain");
    let (stderr_bytes, stderr_eof) = stderr_drain.join().expect("join stderr drain");
    let last_holder = stdout_eof.max(stderr_eof).duration_since(started);
    let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();

    assert_eq!(
        status.code(),
        Some(9),
        "the failing dependency check's exit code must propagate\nstdout:\n{}\nstderr:\n{stderr}",
        String::from_utf8_lossy(&stdout_bytes),
    );

    // soldr#2605: these two budgets used to be one. `Command::output()` reads
    // both pipes to EOF *before* reaping, so its elapsed time is
    // `max(soldr exit, last descendant holding the inherited stdio)` — and a
    // 31 s failure could mean either "cancellation never interrupted the
    // sleeping sibling" or "cancellation worked and something outlived it
    // still holding the pipe". Both produced the same panic, so five
    // sightings could not be told apart. Measured separately, the failure
    // names itself.
    // soldr#2605: bound the work, not the startup. `soldr_exited` includes
    // getting soldr running, which measured 5.9s warm and 13.7s cold on a
    // Windows host — up to 91% of the budget with cancellation working
    // perfectly. Subtracting it makes the gap real in both directions:
    // spawn+fail+cancel runs ~1-2s normally and ~30s if cancellation
    // regresses, so a 15s bound now sits between two outcomes instead of
    // inside the noise of one.
    let startup = front_door_startup(&stderr);
    let cancel_window = soldr_exited.saturating_sub(startup);
    assert!(
        cancel_window < Duration::from_secs(SIBLING_CANCEL_BUDGET_SECS),
        "dependency failure must cancel sibling lint children promptly: \
         spawn+fail+cancel took {cancel_window:?} (soldr ran {soldr_exited:?} \
         total, of which {startup:?} was front-door startup), and an \
         uncancelled sibling would have slept {SIBLING_SLEEP_SECS}s \
         (soldr#1876). Startup is already excluded, so this is the \
         cancellation path failing, not a slow runner and not a leaked \
         descendant.\nstderr:\n{stderr}"
    );
    assert!(
        last_holder.saturating_sub(startup) < Duration::from_secs(SIBLING_CANCEL_BUDGET_SECS),
        "soldr exited in {soldr_exited:?} but its inherited stdio stayed open \
         for {last_holder:?} ({startup:?} of that was front-door startup): \
         cancellation returned while a descendant outlived it holding the \
         pipe (soldr#2605). The sibling's fake cargo redirects only stdout, \
         so its `sleep`/`ping` grandchild inherits this stderr for the full \
         {SIBLING_SLEEP_SECS}s. Look for a descendant outside the killed \
         tree, not at the cancellation logic.\nstderr:\n{stderr}"
    );
}

// ---- soldr#3958: `SOLDR_PORCELAIN=1 soldr lint` and the declared step list ----

const LINT_NOISE_MARKER: &str = "LINT_PASSING_STEP_NOISE_3958";
const LINT_FMT_DIFF: &str = "Diff in src/lib.rs:1:";

/// A fake cargo whose `fmt` step fails with a rustfmt-style diff when
/// `fail_fmt`, and whose every other step prints a noise line and passes.
fn porcelain_lint_cargo_script(fail_fmt: bool) -> String {
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        let fmt = if fail_fmt {
            format!("echo {LINT_FMT_DIFF}\r\necho -fn  a(){{}}\r\necho +fn a() {{}}\r\nexit /b 1")
        } else {
            "exit /b 0".to_string()
        };
        format!(
            "@echo off\r\nif \"%~1\"==\"metadata\" exit /b 0\r\nif \"%~1\"==\"fmt\" (\r\n{fmt}\r\n)\r\necho {LINT_NOISE_MARKER}\r\nexit /b 0\r\n"
        )
    } else {
        let fmt = if fail_fmt {
            format!("printf '%s\\n' '{LINT_FMT_DIFF}' '-fn  a(){{}}' '+fn a() {{}}'; exit 1")
        } else {
            "printf '%s\\n' '{NOISE}'; exit 0".replace("{NOISE}", LINT_NOISE_MARKER)
        };
        format!(
            "#!/bin/sh\nif [ \"$1\" = metadata ]; then exit 0; fi\nif [ \"$1\" = fmt ]; then {fmt}; fi\nprintf '%s\\n' '{LINT_NOISE_MARKER}'\nexit 0\n"
        )
    }
}

/// `extra` follows `lint rust`; the verify-only contract passes `--check`.
fn run_porcelain_lint(
    label: &str,
    fail_fmt: bool,
    extra: &[&str],
) -> (std::process::Output, PathBuf) {
    let root = unique_temp_dir(label);
    let workspace = root.join("workspace");
    fs::create_dir_all(workspace.join("src")).expect("workspace source dir");
    fs::write(
        workspace.join("Cargo.toml"),
        "[package]\nname = \"lint_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
         [workspace]\n\n[workspace.metadata.soldr.lint]\nsteps = [\"fmt\", \"clippy\"]\n",
    )
    .expect("manifest");
    fs::write(workspace.join("src/lib.rs"), "pub fn a() {}\n").expect("source");
    let tool_dir = root.join("tool");
    fs::create_dir_all(&tool_dir).expect("tool dir");
    let cargo = fake_script_path(&tool_dir, "cargo");
    write_fake_script(&cargo, &porcelain_lint_cargo_script(fail_fmt));
    let output = isolated_soldr_command()
        .args(["--no-cache", "lint", "rust"])
        .args(extra)
        .current_dir(&workspace)
        .env_remove("CI")
        .env("SOLDR_PORCELAIN", "1")
        .env("SOLDR_CACHE_DIR", root.join("cache"))
        .env("CARGO_TARGET_DIR", root.join("target"))
        .env("SOLDR_TEST_CARGO_BIN", &cargo)
        .output()
        .expect("run soldr lint");
    (output, root)
}

#[test]
fn porcelain_lint_success_prints_exactly_one_summary_line() {
    let (output, _root) = run_porcelain_lint("lint-porcelain-ok", false, &["--check"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(stdout.lines().count(), 1, "stdout:\n{stdout}");
    assert!(!stdout.contains(LINT_NOISE_MARKER) && !stderr.contains(LINT_NOISE_MARKER));
    let report: serde_json::Value = serde_json::from_str(stdout.trim()).expect("one JSON line");
    assert_eq!(report["summary"]["status"], "ok");
    assert_eq!(report["exit_code"], 0);
    let names: Vec<_> = report["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .map(|step| {
            (
                step["name"].as_str().unwrap_or_default().to_string(),
                step["ok"].clone(),
            )
        })
        .collect();
    assert_eq!(
        names,
        vec![
            ("fmt".to_string(), serde_json::json!(true)),
            ("clippy".to_string(), serde_json::json!(true)),
        ]
    );
    let log = fs::read_to_string(report["log_path"].as_str().expect("log_path")).expect("log");
    assert!(
        log.contains(LINT_NOISE_MARKER),
        "the folded output lives in the log:\n{log}"
    );
}

#[test]
fn porcelain_lint_failure_shows_the_fmt_diff_in_full() {
    let (output, _root) = run_porcelain_lint("lint-porcelain-fmt", true, &["--check"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains(LINT_FMT_DIFF),
        "the diff is never folded:\n{stderr}"
    );
    assert_eq!(stdout.lines().count(), 1, "stdout:\n{stdout}");
    let report: serde_json::Value = serde_json::from_str(stdout.trim()).expect("one JSON line");
    assert_eq!(report["summary"]["status"], "failed");
    assert_eq!(report["failed_step"], "fmt");
    assert_eq!(report["exit_code"], 1);
    assert_eq!(
        report["steps"].as_array().map(Vec::len),
        Some(1),
        "lint stops at the failing step"
    );
}

#[test]
fn porcelain_lint_formats_in_place_before_the_fmt_check() {
    // soldr#4006: outside CI and without `--check`, a formatting pass runs
    // first, so formatting alone never fails the lint.
    let (output, _root) = run_porcelain_lint("lint-porcelain-fix", false, &[]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "stdout:\n{stdout}");
    let report: serde_json::Value = serde_json::from_str(stdout.trim()).expect("one JSON line");
    let names: Vec<_> = report["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .map(|step| step["name"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(names, vec!["fmt-fix", "fmt", "clippy"]);
}
