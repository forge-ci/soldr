//! Synchronous child handles offer a bounded wait beside the unbounded one.

use std::io;
use std::process::Command;
use std::time::{Duration, Instant};

use kernal_api::platform::process::{
    spawn_sync, spawn_sync_daemon, DaemonStdio, DaemonStdioSource, SpawnStdio, StdioSource,
    SyncEnvironment,
};

const HELPER_ENV: &str = "KERNAL_API_BOUNDED_CHILD_WAIT_HELPER";
const HELPER_TEST: &str = "bounded_child_wait::slow_helper";

/// Re-executed by the tests below; a no-op otherwise.
#[test]
#[ignore = "helper process for the bounded_child_wait tests"]
fn slow_helper() {
    if std::env::var_os(HELPER_ENV).is_some() {
        std::thread::sleep(Duration::from_secs(60));
    }
}

fn slow_command() -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([HELPER_TEST, "--exact", "--ignored"])
        .env(HELPER_ENV, "1");
    command
}

fn assert_timed_out(result: io::Result<i32>, started: Instant) {
    let error = result.expect_err("a sleeping child must not exit within the budget");
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(30));
}

#[test]
fn a_bounded_daemon_wait_times_out_and_leaves_the_child_killable() {
    let mut child = spawn_sync_daemon(
        &mut slow_command(),
        DaemonStdio {
            stdout: DaemonStdioSource::Null,
            stderr: DaemonStdioSource::Null,
        },
        SyncEnvironment::Inherit,
        false,
    )
    .expect("spawn helper daemon");
    let started = Instant::now();
    assert_timed_out(child.wait_within(Duration::from_millis(200)), started);
    assert_eq!(
        child.try_wait().unwrap(),
        None,
        "an expired wait must not reap"
    );
    child.kill().unwrap();
    child
        .wait_within(Duration::from_secs(30))
        .expect("a killed child exits within the budget");
}

#[test]
fn a_bounded_contained_wait_times_out_and_leaves_the_child_killable() {
    let mut child = spawn_sync(
        &mut slow_command(),
        SpawnStdio {
            stdin: StdioSource::Null,
            stdout: StdioSource::Null,
            stderr: StdioSource::Null,
            drain_timeout: None,
            show_console: false,
        },
        SyncEnvironment::Inherit,
    )
    .expect("spawn contained helper");
    let started = Instant::now();
    assert_timed_out(child.wait_within(Duration::from_millis(200)), started);
    assert_eq!(
        child.try_wait().unwrap(),
        None,
        "an expired wait must not reap"
    );
    child.kill().unwrap();
    child
        .wait_within(Duration::from_secs(30))
        .expect("a killed child exits within the budget");
}
