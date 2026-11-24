use crate::platform::wait::{join_for, output_for, spawn_for_output, wait_for, PROBE};
use std::io;
use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn captured_command_dies_when_spawning_thread_exits() {
    use std::os::unix::process::ExitStatusExt;

    let owner = std::thread::spawn(|| {
        spawn_for_output(Command::new("sh").args(["-c", "exec sleep 120"])).expect("spawn worker")
    });
    let mut child = join_for(owner, PROBE).expect("owner exited");
    let status = wait_for(&mut child, PROBE).expect("worker died with its owner");
    assert_eq!(status.signal(), Some(libc::SIGKILL));
}

#[test]
fn captured_command_deadline_closes_descendant_pipes() {
    for leader in ["wait", "exit 0"] {
        let directory = tempfile::tempdir().expect("create readiness directory");
        let pid_file = directory.path().join("pid");
        let script = format!("sleep 120 & printf '%s' \"$!\" > \"$1\"; {leader}");
        let error = output_for(
            Command::new("sh")
                .args(["-c", &script, "worker"])
                .arg(&pid_file),
            Duration::from_secs(1),
        )
        .expect_err("a descendant keeps the capture open");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        let pid = std::fs::read_to_string(pid_file).expect("worker started");
        let pid = pid.parse().expect("worker PID");
        let deadline = Instant::now() + PROBE;
        while super::super::inspect::is_alive(pid) {
            assert!(Instant::now() < deadline, "worker {pid} survived timeout");
            std::thread::yield_now();
        }
    }
}
