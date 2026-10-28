//! Windows replay reports the child failure without inventing a Soldr fault.

use std::process::Command;

use crate::common;

#[test]
fn windows_replay_preserves_child_failure_without_claiming_a_soldr_fault() {
    if !matches!(
        soldr_platform::host::facts::os(),
        soldr_platform::host::facts::HostOs::Windows
    ) {
        return;
    }
    let tmp = tempfile::tempdir().expect("temporary test directory");
    let runner = tmp.path().join("cargo-nextest.cmd");
    std::fs::write(
        &runner,
        "@echo off\r\necho nextest test failed 1>&2\r\nexit /b 100\r\n",
    )
    .expect("write runner");
    let output = Command::new(common::soldr_bin())
        .current_dir(tmp.path())
        .env_remove("IN_SOLDR_PID")
        .env_remove("IN_SOLDR_ROLE")
        .env_remove("SOLDR_AS")
        .args(["replay-nextest", "--runner"])
        .arg(&runner)
        .args(["--", "nextest", "--version"])
        .output()
        .expect("run replay");
    assert_eq!(output.status.code(), Some(100));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("nextest test failed"), "stderr: {stderr}");
    assert!(
        !stderr.contains("this is a fault in soldr itself"),
        "stderr: {stderr}"
    );
}
