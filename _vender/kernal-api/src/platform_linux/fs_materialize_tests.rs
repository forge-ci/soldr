//! Linux-native behavior of `await_no_writers`: the inherited-descriptor
//! `ETXTBSY` race it exists to close (zackees/soldr#3350).

use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::time::Duration;

use super::await_no_writers;
use crate::platform::fs::WriterWait;

fn executable_script(dir: &std::path::Path) -> (std::path::PathBuf, std::fs::File) {
    let path = dir.join("build-script-build");
    let mut file = std::fs::File::create(&path).expect("create script");
    file.write_all(b"#!/bin/sh\nexit 0\n").expect("write script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    (path, file)
}

#[test]
fn an_unwritten_file_is_clear_at_once() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (path, file) = executable_script(dir.path());
    drop(file);
    match await_no_writers(&path, Duration::ZERO).expect("observe") {
        WriterWait::Clear { .. } => {}
        other => panic!("expected Clear, got {other:?}"),
    }
}

#[test]
fn an_open_writer_in_this_process_times_out() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (path, _writer) = executable_script(dir.path());
    assert_eq!(
        await_no_writers(&path, Duration::from_millis(20)).expect("observe"),
        WriterWait::TimedOut
    );
}

/// A child forked while the publisher's write descriptor is open keeps that
/// descriptor until its own exec. The publisher has closed its copy, yet
/// executing the file fails with `ETXTBSY` until the child execs; waiting for
/// writers makes the exec succeed.
#[test]
fn a_descriptor_inherited_across_fork_is_waited_out() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (path, writer) = executable_script(dir.path());

    let child = std::thread::spawn(|| {
        let mut command = std::process::Command::new("true");
        // SAFETY: the hook only sleeps, which is async-signal-safe.
        unsafe {
            command.pre_exec(|| {
                std::thread::sleep(Duration::from_millis(400));
                Ok(())
            });
        }
        command.status().expect("run lingering child")
    });
    // Let the fork happen while `writer` is open, then close the publisher's copy.
    std::thread::sleep(Duration::from_millis(100));
    drop(writer);

    let busy = std::process::Command::new(&path).status();
    assert_eq!(
        busy.as_ref().err().and_then(std::io::Error::raw_os_error),
        Some(libc::ETXTBSY),
        "the inherited descriptor must make exec fail: {busy:?}"
    );
    match await_no_writers(&path, Duration::from_secs(10)).expect("observe") {
        WriterWait::Clear { waited } => assert!(waited > Duration::ZERO),
        other => panic!("expected Clear after the child exec'd, got {other:?}"),
    }
    assert!(std::process::Command::new(&path).status().expect("exec after waiting").success());
    assert!(child.join().expect("child thread").success());
}
