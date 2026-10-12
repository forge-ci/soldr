use super::reap_tmpdir_holders;
use soldr_platform::process::test_child;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn reaps_a_detached_process_that_kept_the_private_tmpdir() {
    if !crate::memory::is_linux() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let tmpdir = dir.path().as_os_str();
    // Double-fork through `sh` + `setsid`, like a daemon: the survivor is
    // neither our child nor in our process group.
    let status = Command::new("sh")
        .args(["-c", "setsid sleep 300 >/dev/null 2>&1 &"])
        .env("TMPDIR", dir.path())
        .stdin(Stdio::null())
        .status()
        .expect("spawn detached sleep");
    assert!(status.success());
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut leaked = Vec::new();
    while Instant::now() < deadline {
        // Wait until the survivor has exec'd `sleep`: mid-exec, `environ`
        // is briefly unreadable and the transient `sh` fork also matches.
        leaked = test_child::pids_with_env("TMPDIR", tmpdir);
        let settled = leaked.len() == 1
            && std::fs::read_to_string(format!("/proc/{}/comm", leaked[0]))
                .is_ok_and(|comm| comm.trim() == "sleep");
        if settled {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        leaked.len(),
        1,
        "expected one detached sleep, found {leaked:?}"
    );

    assert_eq!(reap_tmpdir_holders(tmpdir), 1);
    assert!(
        test_child::pids_with_env("TMPDIR", tmpdir).is_empty(),
        "leaked process {leaked:?} survived the reap"
    );
}

#[test]
fn leaves_processes_with_another_tmpdir_alone() {
    if !crate::memory::is_linux() {
        return;
    }
    let mine = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let mut child = Command::new("sleep")
        .arg("300")
        .env("TMPDIR", other.path())
        .spawn()
        .expect("spawn sleep");

    assert_eq!(reap_tmpdir_holders(mine.path().as_os_str()), 0);
    assert!(test_child::pid_alive(child.id()));
    let _ = child.kill();
    let _ = child.wait();
}
