use std::ffi::OsStr;

use crate::{shell_spec, ProcessCaptureError, StreamMode};

#[tokio::test]
async fn actor_facade_accepts_owner_death_cleanup_policy() {
    let output = shell_spec("printf macos-actor")
        .kill_when_owner_dies(true)
        .stdout(StreamMode::Piped)
        .stderr(StreamMode::Piped)
        .spawn()
        .await
        .expect("spawn through the actor facade")
        .wait_with_output_bounded_within(1024, std::time::Duration::from_secs(5))
        .await
        .expect("bounded capture from a naturally exiting child");

    assert!(output.status.success());
    assert_eq!(output.stdout, b"macos-actor");
    assert!(output.stderr.is_empty());
}

#[tokio::test]
async fn actor_facade_reports_finite_output_beyond_the_retention_limit() {
    // A finite child reaches EOF after its excess output is drained, so the
    // error below is the output limit, not the time budget.
    let error = shell_spec("printf 12345")
        .stdout(StreamMode::Piped)
        .stderr(StreamMode::Piped)
        .spawn()
        .await
        .expect("spawn finite output child")
        .wait_with_output_bounded_within(4, std::time::Duration::from_secs(2))
        .await
        .expect_err("retained output is bounded");

    assert!(matches!(
        error,
        ProcessCaptureError::OutputLimitExceeded { limit: 4 }
    ));
}

#[test]
fn shell_command_preserves_login_shell_contract_and_ignores_child_path() {
    let command_text = "printf '%s' 'alpha beta;\"gamma\"'";
    let mut command = super::shell_command(command_text);
    assert_eq!(command.get_program(), OsStr::new("/bin/sh"));
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        [OsStr::new("-lc"), OsStr::new(command_text)]
    );
    command
        .env_clear()
        .env("PATH", "/caller-supplied-path-override");
    let output = command
        .output()
        .expect("absolute shell command should execute independently of child PATH");
    assert!(output.status.success());
    assert_eq!(output.stdout, b"alpha beta;\"gamma\"");
}
