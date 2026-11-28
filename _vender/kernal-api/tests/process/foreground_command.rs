//! The foreground operation runs a caller-owned command and changes nothing
//! else about it.
//!
//! Written as a client would use it: through `kernal_api::platform::process`,
//! naming only `std::process::Command`. The point of the capability is that it
//! adds no policy, so every assertion here is about something the caller
//! configured surviving the call.
//!
//! The child is this test binary re-executed with `--exact child_probe`, so
//! the probe is the same program on every host and the suite depends on no
//! interpreter or shell being installed.

use kernal_api::platform::process::{foreground_output, foreground_status};
use std::process::{Command, Stdio};

/// Environment switch that turns the helper below into the probe.
const PROBE: &str = "KERNAL_FOREGROUND_PROBE";

/// The child half of these tests; an ordinary no-op unless [`PROBE`] is set.
///
/// When set it asserts on what the parent configured -- the trailing argument,
/// the working directory, and this variable itself -- and exits with a code
/// the parent can tell apart from a panic.
#[test]
fn child_probe() {
    let Ok(mode) = std::env::var(PROBE) else {
        return;
    };
    if mode == "exit-3" {
        std::process::exit(3);
    }
    if mode == "speak" {
        use std::io::Write as _;
        // Written to the streams themselves, not through `println!`: the test
        // harness intercepts the macros, so their bytes would never reach the
        // pipes this test is about.
        let mut stdout = std::io::stdout();
        let mut stderr = std::io::stderr();
        stdout.write_all(b"to stdout\n").expect("write stdout");
        stderr.write_all(b"to stderr\n").expect("write stderr");
        stdout.flush().expect("flush stdout");
        stderr.flush().expect("flush stderr");
        std::process::exit(0);
    }

    let expected = std::path::PathBuf::from(mode);
    let sentinel_seen = std::env::args().any(|argument| argument == "kernal-sentinel");
    let working_directory_matches = std::env::current_dir()
        .and_then(|directory| directory.canonicalize())
        .map(|directory| directory == expected)
        .unwrap_or(false);

    std::process::exit(if sentinel_seen && working_directory_matches {
        0
    } else {
        9
    });
}

/// A command that succeeds reports success, and one that fails reports its own
/// exit code rather than a generic failure.
#[test]
fn the_command_s_own_exit_status_is_reported() {
    let mut fails = probe();
    fails.env(PROBE, "exit-3");

    // An unset probe variable leaves the helper an ordinary passing test.
    let mut plain = probe();
    plain.env_remove(PROBE);

    let success = foreground_status(&mut plain).expect("run the passing helper");
    let failure = foreground_status(&mut fails).expect("run the failing helper");

    assert!(success.success());
    assert!(!failure.success());
    assert_eq!(failure.code(), Some(3));
}

/// Arguments, working directory and environment reach the child unchanged.
///
/// The child asserts on all three itself and exits `9` if any is wrong, so a
/// green run means the facade forwarded the caller's whole contract.
#[test]
fn the_caller_s_command_configuration_is_left_alone() {
    let directory = tempfile::tempdir().expect("probe directory");
    let expected = directory
        .path()
        .canonicalize()
        .expect("canonical probe directory");

    let mut command = probe();
    command
        .arg("kernal-sentinel")
        .current_dir(&expected)
        .env(PROBE, &expected);

    let status = foreground_status(&mut command).expect("run the probe");

    assert!(
        status.success(),
        "argv, cwd and env reached the child unchanged (exit {:?})",
        status.code()
    );
}

/// The capture variant returns what the child wrote, and its status with it.
///
/// Streams are left at the operation's own default here -- the caller sets
/// nothing -- which is the case where `Command::output` captures rather than
/// inherits.
#[test]
fn captured_output_carries_both_streams_and_the_status() {
    let mut command = Command::new(std::env::current_exe().expect("test binary path"));
    command
        .arg("--exact")
        .arg("foreground_command::child_probe")
        .env(PROBE, "speak");

    let output = foreground_output(&mut command).expect("run the speaking helper");

    // Asserted by containment: the child is a test binary, so its own harness
    // writes a banner to each stream around what the probe wrote.
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("to stdout"),
        "stdout was captured"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("to stderr"),
        "stderr was captured separately from stdout"
    );
}

/// A command that cannot start is an error, not an exit status.
#[test]
fn a_command_that_cannot_start_reports_an_error() {
    let mut missing = Command::new("kernal-api-no-such-program-xyz");
    missing.stdout(Stdio::null()).stderr(Stdio::null());

    let error = foreground_status(&mut missing).expect_err("no such program");

    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}

/// This test binary, aimed at [`child_probe`], with its streams silenced.
fn probe() -> Command {
    let mut command = Command::new(std::env::current_exe().expect("test binary path"));
    command
        .arg("--exact")
        .arg("foreground_command::child_probe")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}
