//! Process-level coverage for offline nextest archive replay.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::too_many_lines)]

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::process::Command;

use crate::common;

#[test]
fn replay_forwards_exit_and_signal_and_rejects_before_spawning() {
    let tmp = tempfile::tempdir().expect("temporary test directory");
    let runner = tmp.path().join("cargo-nextest");
    let marker = tmp.path().join("invoked");
    std::fs::write(
        &runner,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$REPLAY_MARKER\"\ncase \"$REPLAY_ACTION\" in exit) exit 37;; signal) kill -TERM $$;; esac\n",
    )
    .expect("write runner");
    let mut permissions = std::fs::metadata(&runner)
        .expect("runner metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&runner, permissions).expect("make runner executable");
    let invoke = |args: &[&str], action: &str, version_pin: Option<&str>| {
        let mut cmd = Command::new(common::soldr_bin());
        cmd.current_dir(tmp.path())
            .env("REPLAY_MARKER", &marker)
            .env("REPLAY_ACTION", action)
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
    assert_eq!(
        std::fs::read_to_string(&marker).expect("marker").trim(),
        "nextest --version"
    );
    let output = invoke(
        &["run", "--archive-file", "fixture.tar.zst"],
        "signal",
        None,
    );
    assert_eq!(output.status.signal(), Some(15));
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
    for mode_option in [vec!["--zccache-mode", "copy"], vec!["--zccache-mode=copy"]] {
        for pin_by_argument in [false, true] {
            let mut cmd = Command::new(common::soldr_bin());
            cmd.current_dir(tmp.path())
                .env("REPLAY_MARKER", &marker)
                .env("RUSTUP_HOME", tmp.path().join("missing-rustup"))
                .env("CARGO_HOME", tmp.path().join("missing-cargo"));
            if pin_by_argument {
                cmd.env_remove("SOLDR_AS").args(["--as", "0.0.1"]);
            } else {
                cmd.env("SOLDR_AS", "0.0.1");
            }
            cmd.args(&mode_option);
            let output = cmd
                .args(["replay-nextest", "--runner"])
                .arg(&runner)
                .args(["--", "nextest", "--version"])
                .output()
                .expect("run with global mode");
            assert!(!output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("version overrides"),
                "stderr: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(std::fs::read_to_string(&marker).expect("marker"), previous);
        }
    }
    for mode_option in [vec!["--zccache-mode", "copy"], vec!["--zccache-mode=copy"]] {
        for pin_by_argument in [false, true] {
            let mut cmd = Command::new(common::soldr_bin());
            cmd.current_dir(tmp.path()).env("REPLAY_MARKER", &marker);
            if pin_by_argument {
                cmd.env_remove("SOLDR_AS").args(["--as", "0.0.1"]);
            } else {
                cmd.env("SOLDR_AS", "0.0.1");
            }
            cmd.args(&mode_option);
            let output = cmd
                .args(["replay-nextest", "--", "nextest", "--version"])
                .output()
                .expect("run malformed replay with version pin");
            assert!(!output.status.success());
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("version overrides"),
                "stderr: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(std::fs::read_to_string(&marker).expect("marker"), previous);
        }
    }
    assert!(!tmp.path().join("missing-rustup").exists());
    assert!(!tmp.path().join("missing-cargo").exists());
}
