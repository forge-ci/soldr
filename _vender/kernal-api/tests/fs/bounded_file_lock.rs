use std::fs::{File, OpenOptions};
use std::io;
use std::time::{Duration, Instant};

use kernal_api::platform::fs::{
    lock_exclusive_for, lock_exclusive_owned_for, lock_shared_for, lock_shared_owned_for,
    try_lock_exclusive,
};

const WAIT: Duration = Duration::from_secs(10);

fn open(path: &std::path::Path) -> File {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .unwrap()
}

fn assert_times_out<T>(result: io::Result<T>, started: Instant, budget: Duration) {
    let waited = started.elapsed();
    match result {
        Err(error) => assert_eq!(error.kind(), io::ErrorKind::TimedOut, "{error}"),
        Ok(_) => panic!("lock acquired while another handle held it"),
    }
    assert!(waited >= budget, "returned early after {waited:?}");
    assert!(
        waited < Duration::from_secs(5),
        "overran the budget: {waited:?}"
    );
}

#[test]
fn held_exclusive_lock_makes_bounded_acquisition_time_out() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lock");
    let holder = open(&path);
    let held = try_lock_exclusive(&holder).unwrap();
    let budget = Duration::from_millis(100);

    let contender = open(&path);
    let started = Instant::now();
    assert_times_out(lock_exclusive_for(&contender, budget), started, budget);
    let started = Instant::now();
    assert_times_out(lock_shared_for(&contender, budget), started, budget);
    let started = Instant::now();
    assert_times_out(
        lock_exclusive_owned_for(open(&path), budget),
        started,
        budget,
    );
    let started = Instant::now();
    assert_times_out(lock_shared_owned_for(open(&path), budget), started, budget);

    drop(held);
    drop(lock_exclusive_for(&contender, WAIT).unwrap());
}

#[test]
fn shared_holders_coexist_and_a_released_lock_is_acquired_before_the_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lock");
    let first = open(&path);
    let second = open(&path);
    let shared = lock_shared_for(&first, WAIT).unwrap();
    let also_shared = lock_shared_owned_for(open(&path), WAIT).unwrap();
    drop(also_shared);

    let worker = std::thread::spawn(move || {
        let started = Instant::now();
        let lock = lock_exclusive_for(&second, WAIT).map(drop);
        (lock, started.elapsed())
    });
    std::thread::sleep(Duration::from_millis(50));
    drop(shared);
    let (lock, waited) = worker.join().unwrap();
    lock.unwrap();
    assert!(waited < WAIT, "waited the whole budget: {waited:?}");
}
