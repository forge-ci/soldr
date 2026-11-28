//! The environment base a synchronous daemon spawn starts from.

use std::ffi::OsStr;
use std::process::Command;

use kernal_api::platform::process::{
    spawn_sync_daemon, DaemonStdio, DaemonStdioSource, SyncEnvironment,
};

const HELPER_ENV: &str = "KERNAL_API_SYNC_ENVIRONMENT_HELPER";
const HELPER_TEST: &str = "sync_environment::environment_report_helper";
/// Set by Cargo (and nextest) on the test process, and never part of a
/// fresh login -- the ambient variable a baseline spawn must not inherit.
const AMBIENT: &str = "CARGO_MANIFEST_DIR";
const EXPLICIT: &str = "KERNAL_API_SYNC_ENVIRONMENT_EXPLICIT";

/// Re-executed by the tests below; a no-op otherwise.
#[test]
#[ignore = "helper process for the sync_environment tests"]
fn environment_report_helper() {
    if std::env::var_os(HELPER_ENV).is_none() {
        return;
    }
    let explicit = std::env::var(EXPLICIT).unwrap_or_default();
    let ambient = std::env::var_os(AMBIENT).is_some();
    println!("REPORT explicit={explicit} ambient={ambient} END");
}

/// Spawn the helper as a daemon and return `(explicit, ambient)` as it saw them.
fn report(environment: SyncEnvironment) -> (String, bool) {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("report.txt");
    let log = std::fs::File::create(&log_path).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([HELPER_TEST, "--exact", "--ignored", "--nocapture"])
        .env(HELPER_ENV, "1")
        .env(EXPLICIT, "kept");
    let mut child = spawn_sync_daemon(
        &mut command,
        DaemonStdio {
            stdout: DaemonStdioSource::File(&log),
            stderr: DaemonStdioSource::Null,
        },
        environment,
        false,
    )
    .expect("spawn helper daemon");
    assert_eq!(
        child
            .wait_within(std::time::Duration::from_secs(5))
            .expect("wait for helper"),
        0
    );
    drop(log);

    let output = std::fs::read_to_string(&log_path).unwrap();
    let start = output.find("REPORT ").expect("helper report") + "REPORT ".len();
    let line = &output[start..start + output[start..].find(" END").expect("report end")];
    let mut explicit = String::new();
    let mut ambient = false;
    for field in line.split(' ') {
        match field.split_once('=') {
            Some(("explicit", value)) => explicit = value.to_owned(),
            Some(("ambient", value)) => ambient = value == "true",
            _ => {}
        }
    }
    (explicit, ambient)
}

#[test]
fn a_user_baseline_daemon_does_not_inherit_ambient_variables() {
    let (explicit, _) = report(SyncEnvironment::UserBaseline);
    assert_eq!(explicit, "kept", "explicit command variables always apply");

    if std::env::var_os(AMBIENT).is_none() {
        return; // Not run under Cargo: there is no ambient marker to observe.
    }
    let login = kernal_api::platform::host::login_environment().expect("login environment");
    if login.iter().any(|(key, _)| key == OsStr::new(AMBIENT)) {
        return; // This host's login baseline legitimately defines it.
    }
    let (_, inherited) = report(SyncEnvironment::Inherit);
    assert!(inherited, "an inheriting spawn sees the ambient variable");
    let (_, baseline) = report(SyncEnvironment::UserBaseline);
    assert!(
        !baseline,
        "a user-baseline spawn starts from the login environment"
    );
}
