use super::*;

fn entry(id: u64, phase: Phase, age: Duration, now: Instant) -> Entry {
    Entry {
        id,
        kind: "Mutex",
        phase,
        site: Location::caller(),
        thread: "t".to_string(),
        since: now - age,
    }
}

#[test]
fn nothing_overdue_reports_nothing() {
    let now = Instant::now();
    let entries = [entry(1, Phase::Holding, Duration::from_secs(2), now)];
    assert!(overdue_report(&entries, now, Duration::from_secs(15), &[]).is_none());
}

/// soldr#3836: an overrun must name every registered site, including the
/// waiters the overdue holder is blocking.
#[test]
fn an_overdue_hold_names_every_registered_site() {
    let now = Instant::now();
    let entries = [
        entry(1, Phase::Holding, Duration::from_secs(20), now),
        entry(2, Phase::Waiting, Duration::from_secs(3), now),
    ];
    let Some(Overdue::Abort(report)) = overdue_report(&entries, now, Duration::from_secs(15), &[])
    else {
        panic!("an overdue hold must abort");
    };
    assert!(report.contains("Holding Mutex for 20."), "{report}");
    assert!(report.contains("Waiting Mutex for 3."), "{report}");
    assert!(report.contains("sync_tests.rs"), "{report}");
}

/// A long wait alone can be a queue of brief holds under load; a deadlock
/// always shows up as an overdue hold. So a wait is reported, once, and never
/// aborts the process by itself.
#[test]
fn an_overdue_wait_is_reported_once_and_does_not_abort() {
    let now = Instant::now();
    let entries = [
        entry(1, Phase::Waiting, Duration::from_secs(16), now),
        entry(2, Phase::Holding, Duration::from_secs(1), now),
    ];
    let limit = Duration::from_secs(15);
    let Some(Overdue::Report(ids, report)) = overdue_report(&entries, now, limit, &[]) else {
        panic!("an overdue wait must be reported, not abort");
    };
    assert_eq!(ids, vec![1]);
    assert!(report.contains("lock wait exceeded"), "{report}");
    assert!(overdue_report(&entries, now, limit, &ids).is_none());
}

#[test]
fn critical_section_guards_unregister_on_drop() {
    let mutex = Mutex::new(1);
    let lock = RwLock::new(1);
    // Other tests hold locks concurrently in the same registry; count only
    // this test's thread (libtest names it after the test).
    let me = current_thread_name();
    let count = || with_registry(|entries| entries.iter().filter(|e| e.thread == me).count());
    let before = count();
    {
        let mut guard = mutex.lock();
        *guard += 1;
        let _read = lock.read();
        assert!(count() >= before + 2);
    }
    {
        let _write = lock.write();
    }
    assert_eq!(count(), before);
    assert_eq!(mutex.into_inner(), 2);
}

#[test]
fn timeout_flavor_reports_a_held_lock() {
    let mutex = Mutex::new(());
    let _held = mutex.lock();
    let start = Instant::now();
    let err = std::thread::scope(|scope| {
        scope
            .spawn(|| mutex.lock_for(Duration::from_millis(50)).err())
            .join()
            .expect("join")
    })
    .expect("a held mutex must time out");
    assert!(err.waited >= Duration::from_millis(50));
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn timeout_flavor_acquires_a_free_lock() {
    let lock = RwLock::new(5);
    assert_eq!(*lock.read_for(Duration::from_millis(10)).expect("free"), 5);
    *lock.write_for(Duration::from_millis(10)).expect("free") = 6;
    assert_eq!(lock.into_inner(), 6);
}

#[test]
fn poisoned_locks_are_recovered() {
    let mutex = std::sync::Arc::new(Mutex::new(7));
    let poisoner = std::sync::Arc::clone(&mutex);
    let _ = std::thread::spawn(move || {
        let _guard = poisoner.lock();
        panic!("poison");
    })
    .join();
    assert_eq!(*mutex.lock(), 7);
}
