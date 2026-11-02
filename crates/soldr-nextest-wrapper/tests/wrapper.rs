//! Black-box checks of the Nextest wrapper (soldr#3453). The fuller
//! black-box suites are `tests/test_nextest_timeout_wrapper.py` and
//! `tests/test_nextest_memory_guard.py`, which drive this binary through
//! `SOLDR_NEXTEST_WRAPPER_UNDER_TEST` (soldr#3454).

use soldr_platform::host::facts::{os, HostOs};
use soldr_platform::process::test_child::{self, TestSignal};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Cargo defines this for integration tests that build the bin; Dylint's
/// check pass does not, and Nextest also exports it at run time.
fn wrapper() -> String {
    option_env!("CARGO_BIN_EXE_soldr-nextest-wrapper")
        .map(str::to_owned)
        .or_else(|| std::env::var("CARGO_BIN_EXE_soldr-nextest-wrapper").ok())
        .expect("CARGO_BIN_EXE_soldr-nextest-wrapper")
}

fn linux() -> bool {
    os() == HostOs::Linux
}

fn wrapped(script: &str, configure: impl FnOnce(&mut Command)) -> Output {
    let mut command = Command::new(wrapper());
    command.args(["/bin/sh", "-c", script]);
    for name in [
        "SOLDR_TEST_FORBID_TOOLCHAIN_INSTALL",
        "RUSTUP_AUTO_INSTALL",
        "SOLDR_TEST_FORBID_TARGET_CONTAINING",
        "SOLDR_NEXTEST_KEEP_TMPDIR",
        "SOLDR_NEXTEST_ADMISSION_DIR",
        "SOLDR_NEXTEST_TEST_MEMORY_CEILING_BYTES",
    ] {
        command.env_remove(name);
    }
    configure(&mut command);
    command.output().expect("run the native wrapper")
}

#[test]
fn output_and_exit_status_pass_through() {
    if !linux() {
        return;
    }
    let output = wrapped("echo out; echo err >&2; exit 7", |_| {});
    assert_eq!(output.status.code(), Some(7));
    assert_eq!(output.stdout, b"out\n");
    assert_eq!(output.stderr, b"err\n");
}

#[test]
fn the_test_gets_guards_and_a_private_tmpdir_that_is_removed() {
    if !linux() {
        return;
    }
    let base = tempfile::tempdir().expect("tmp base");
    let output = wrapped(
        "echo \"$TMPDIR|$SOLDR_TEST_FORBID_TOOLCHAIN_INSTALL|$RUSTUP_AUTO_INSTALL|$SOLDR_TEST_FORBID_TARGET_CONTAINING|${SOLDR_NEXTEST_NATIVE_WRAPPER:-unset}|${SOLDR_NEXTEST_ADMISSION_DIR:-unset}\"; touch \"$TMPDIR/leak\"",
        |command| {
            command
                .env("TMPDIR", base.path())
                .env("SOLDR_NEXTEST_NATIVE_WRAPPER", wrapper())
                .env("SOLDR_NEXTEST_ADMISSION_DIR", base.path().join("admission"));
        },
    );
    assert!(output.status.success(), "{output:?}");
    let line = String::from_utf8(output.stdout).expect("utf8");
    let fields: Vec<&str> = line.trim().split('|').collect();
    let private = Path::new(fields[0]);
    assert_eq!(private.parent(), Some(base.path()));
    assert!(
        !private.exists(),
        "the private TMPDIR is removed afterwards"
    );
    assert_eq!(&fields[1..], ["1", "0", "/bin/sh", "unset", "unset"]);
}

/// soldr#3516: whatever HOME and SOLDR_CACHE_DIR the caller has, the test
/// resolves its soldr state inside its private directory, which is removed.
#[test]
fn the_test_cannot_see_the_callers_soldr_state() {
    if !linux() {
        return;
    }
    let base = tempfile::tempdir().expect("tmp base");
    let caller_home = base.path().join("caller-home");
    let output = wrapped(
        "echo \"$TMPDIR|$HOME|$SOLDR_CACHE_DIR|$XDG_RUNTIME_DIR|$XDG_CONFIG_HOME|$SOLDR_TEST_ISOLATED|$CARGO_HOME\"; test -O \"$XDG_RUNTIME_DIR\" && touch \"$SOLDR_CACHE_DIR/state\"",
        |command| {
            command
                .env("TMPDIR", base.path())
                .env("HOME", &caller_home)
                .env("SOLDR_CACHE_DIR", caller_home.join(".soldr"))
                .env("XDG_RUNTIME_DIR", base.path().join("caller-run"))
                .env_remove("CARGO_HOME")
                .env_remove("SOLDR_TEST_ISOLATED");
        },
    );
    assert!(output.status.success(), "{output:?}");
    let line = String::from_utf8(output.stdout).expect("utf8");
    let fields: Vec<&str> = line.trim().split('|').collect();
    let private = Path::new(fields[0]);
    for value in &fields[1..5] {
        assert!(
            Path::new(value).starts_with(private),
            "{value} is outside the private root {}",
            private.display()
        );
    }
    assert_eq!(fields[5], "1");
    assert_eq!(fields[6], "", "CARGO_HOME is not exported");
    assert!(!caller_home.exists(), "the caller's home was never touched");
    assert!(!private.exists(), "the private root is removed afterwards");
}

#[test]
fn sigterm_dumps_threads_and_drains_the_test_output() {
    if !linux() {
        return;
    }
    let mut child = Command::new(wrapper())
        .args([
            "/bin/sh",
            "-c",
            "trap 'echo drained-after-term; exit 3' TERM; echo ready; while :; do sleep 0.05; done",
        ])
        .env("SOLDR_NEXTEST_DISABLE_DEBUGGER", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wrapper");
    let mut stdout = child.stdout.take().expect("stdout");
    let mut first = [0u8; 6];
    stdout.read_exact(&mut first).expect("ready line");
    assert_eq!(&first, b"ready\n");
    test_child::signal_process(child.id(), TestSignal::Terminate);
    let started = Instant::now();
    let mut rest = String::new();
    stdout.read_to_string(&mut rest).expect("drain stdout");
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut stderr)
        .expect("drain stderr");
    let status = child.wait().expect("wrapper exit");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(status.code(), Some(3));
    assert!(rest.contains("drained-after-term"), "{rest}");
    assert!(
        stderr.contains("=== nextest timeout: thread dump for pid"),
        "{stderr}"
    );
    assert!(
        stderr.contains("=== nextest timeout: stdout/stderr drained ==="),
        "{stderr}"
    );
}

#[test]
fn a_missing_program_is_an_infrastructure_failure() {
    if !linux() {
        return;
    }
    let output = Command::new(wrapper())
        .arg("/definitely/not/a/test-binary")
        .output()
        .expect("run wrapper");
    assert_eq!(output.status.code(), Some(75));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("could not start the test process"),
        "{stderr}"
    );
}
