//! Process-level coverage for offline nextest archive replay.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;

use crate::common;

#[test]
fn replay_forwards_exit_and_signal_and_rejects_before_spawning() {
    // The fixture is a POSIX shell script. Windows replay gets native coverage
    // in the target workflow; this guard runs once on the Linux host.
    if matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let tmp = tempfile::tempdir().expect("temporary test directory");
    let runner = tmp.path().join("cargo-nextest");
    let marker = tmp.path().join("invoked");
    std::fs::write(
        &runner,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$REPLAY_MARKER\"\ncase \"$REPLAY_ACTION\" in exit) echo 'nextest test failed' >&2; exit 37;; signal) kill -TERM $$;; nested) exec \"$REPLAY_SOLDR\" --version;; esac\n",
    )
    .expect("write runner");
    soldr_platform::fs::permissions::make_executable(&runner).expect("make runner executable");
    let invoke = |args: &[&str], action: &str, version_pin: Option<&str>| {
        let mut cmd = Command::new(common::soldr_bin());
        cmd.current_dir(tmp.path())
            .env("REPLAY_MARKER", &marker)
            .env("REPLAY_ACTION", action)
            .env("REPLAY_SOLDR", common::soldr_bin())
            .env_remove("IN_SOLDR_PID")
            .env_remove("IN_SOLDR_ROLE")
            .env("RUSTUP_HOME", tmp.path().join("missing-rustup"))
            .env("CARGO_HOME", tmp.path().join("missing-cargo"))
            .env_remove("SOLDR_AS")
            .arg("replay-nextest")
            .arg("--runner")
            .arg(&runner)
            .arg("--")
            .arg("nextest")
            .args(args);
        if let Some(pin) = version_pin {
            cmd.env("SOLDR_AS", pin);
        }
        cmd.output().expect("run soldr")
    };
    for args in [
        vec!["run", "--", "--archive-file", "fake"],
        vec!["run", "--profile", "--archive-file", "fake"],
        vec!["run"],
    ] {
        assert!(!invoke(&args, "", None).status.success());
        assert!(!marker.exists(), "invalid replay started the runner");
    }
    let output = invoke(&["--version"], "exit", None);
    assert_eq!(output.status.code(), Some(37));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("nextest test failed"), "stderr: {stderr}");
    assert!(
        !stderr.contains("this is a fault in soldr itself"),
        "stderr: {stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(&marker).expect("marker").trim(),
        "nextest --version"
    );
    let output = invoke(
        &["run", "--archive-file", "fixture.tar.zst"],
        "signal",
        None,
    );
    assert_eq!(
        soldr_platform::process::test_child::returncode(&output.status),
        -15
    );

    let nested = invoke(&["--version"], "nested", None);
    assert!(
        nested.status.success(),
        "nested Soldr failed: {}",
        String::from_utf8_lossy(&nested.stderr)
    );
    let previous = std::fs::read_to_string(&marker).expect("marker");
    let output = invoke(&["--version"], "", Some("0.0.1"));
    assert!(!output.status.success());
    assert_eq!(std::fs::read_to_string(&marker).expect("marker"), previous);
    let explicit_pin = Command::new(common::soldr_bin())
        .current_dir(tmp.path())
        .env("REPLAY_MARKER", &marker)
        .env_remove("SOLDR_AS")
        .args(["--as", "0.0.1", "replay-nextest", "--runner"])
        .arg(&runner)
        .args(["--", "nextest", "--version"])
        .output()
        .expect("run soldr with explicit version pin");
    assert!(!explicit_pin.status.success());
    assert_eq!(std::fs::read_to_string(&marker).expect("marker"), previous);
    assert_mode_pins_rejected(tmp.path(), &runner, &marker, &previous, false);
    assert_mode_pins_rejected(tmp.path(), &runner, &marker, &previous, true);
    assert!(!tmp.path().join("missing-rustup").exists());
    assert!(!tmp.path().join("missing-cargo").exists());
}

fn assert_mode_pins_rejected(
    root: &Path,
    runner: &Path,
    marker: &Path,
    previous: &str,
    malformed: bool,
) {
    for mode_option in [vec!["--zccache-mode", "copy"], vec!["--zccache-mode=copy"]] {
        for pin_by_argument in [false, true] {
            let mut cmd = Command::new(common::soldr_bin());
            cmd.current_dir(root)
                .env("REPLAY_MARKER", marker)
                .env("RUSTUP_HOME", root.join("missing-rustup"))
                .env("CARGO_HOME", root.join("missing-cargo"));
            if pin_by_argument {
                cmd.env_remove("SOLDR_AS").args(["--as", "0.0.1"]);
            } else {
                cmd.env("SOLDR_AS", "0.0.1");
            }
            cmd.args(&mode_option).arg("replay-nextest");
            if !malformed {
                cmd.arg("--runner").arg(runner);
            }
            let output = cmd
                .args(["--", "nextest", "--version"])
                .output()
                .expect("run replay with global mode and version pin");
            assert!(!output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("version overrides"),
                "stderr: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(std::fs::read_to_string(marker).expect("marker"), previous);
        }
    }
}
