use super::*;
use std::sync::mpsc::{channel, sync_channel};
use std::sync::Arc;

#[test]
fn recv_for_times_out_and_reports_disconnect() {
    let (tx, rx) = channel::<u8>();
    assert!(matches!(
        recv_for(&rx, Duration::from_millis(20)),
        Err(WaitError::TimedOut(_))
    ));
    tx.send(7).expect("send");
    assert_eq!(recv_for(&rx, Duration::from_millis(20)).expect("value"), 7);
    drop(tx);
    assert!(matches!(
        recv_for(&rx, Duration::from_millis(20)),
        Err(WaitError::Disconnected)
    ));
}

#[test]
fn send_for_gives_the_value_back_when_full() {
    let (tx, rx) = sync_channel::<u8>(1);
    send_for(&tx, 1, Duration::from_millis(20)).expect("room");
    let (back, error) = send_for(&tx, 2, Duration::from_millis(20)).expect_err("full");
    assert_eq!(back, 2);
    assert!(matches!(error, WaitError::TimedOut(_)));
    drop(rx);
}

#[test]
fn join_for_returns_the_handle_on_timeout() {
    let (release_tx, release_rx) = channel::<()>();
    let handle = std::thread::spawn(move || {
        let _ = release_rx.recv_timeout(Duration::from_secs(10));
        5
    });
    let (handle, error) = join_for(handle, Duration::from_millis(20)).expect_err("still running");
    assert!(matches!(error, WaitError::TimedOut(_)));
    release_tx.send(()).expect("release");
    let value = join_for(handle.expect("handle back"), Duration::from_secs(10)).expect("joined");
    assert_eq!(value, 5);
}

#[test]
fn barrier_releases_a_full_generation_and_times_out_a_short_one() {
    let barrier = Arc::new(Barrier::new(2));
    let peer = Arc::clone(&barrier);
    let other = std::thread::spawn(move || peer.wait_for(Duration::from_secs(10)).is_ok());
    assert!(barrier.wait_for(Duration::from_secs(10)).is_ok());
    assert!(join_for(other, Duration::from_secs(10)).expect("joined"));
    assert!(matches!(
        barrier.wait_for(Duration::from_millis(20)),
        Err(WaitError::TimedOut(_))
    ));
}

#[test]
fn file_lock_times_out_while_another_handle_holds_it() {
    let dir = std::env::temp_dir().join(format!("soldr-wait-lock-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let path = dir.join("lock");
    let holder = File::create(&path).expect("create");
    lock_exclusive_for(&holder, Duration::from_millis(20)).expect("free");
    let contender = File::open(&path).expect("open");
    let error = lock_exclusive_for(&contender, Duration::from_millis(30)).expect_err("held");
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    drop(holder);
    lock_shared_for(&contender, Duration::from_secs(5)).expect("released");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
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
        let mut backoff = Duration::from_micros(50);
        while super::super::process::inspect::is_alive(pid) {
            assert!(Instant::now() < deadline, "worker {pid} survived timeout");
            backoff_sleep(&mut backoff, deadline);
        }
    }
}
