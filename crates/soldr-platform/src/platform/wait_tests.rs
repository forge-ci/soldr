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
