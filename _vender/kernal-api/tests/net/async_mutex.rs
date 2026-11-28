//! `async_engine::Mutex`: borrowed, owned, non-waiting, and blocking guards,
//! and an owned guard that keeps its facade handle alive.
//!
//! These are the contract tests of the unbounded acquisitions deprecated by
//! #433; they keep exercising them until the forms are removed.
#![allow(deprecated)]

use kernal_api::async_engine::{self, Mutex, MutexTryLockError};
use std::future::Future;
use std::sync::{Arc, Weak};
use std::task::Poll;

/// Poll `future` exactly once and report whether it is still pending.
async fn poll_once<F: Future + Unpin>(future: &mut F) -> bool {
    std::future::poll_fn(|context| {
        Poll::Ready(std::pin::Pin::new(&mut *future).poll(context).is_pending())
    })
    .await
}

#[tokio::test]
async fn borrowed_guards_mutate_exclusively() {
    let lock = Mutex::new(vec![1_u8]);
    {
        let mut guard = lock.lock().await;
        guard.push(2);
        assert_eq!(format!("{guard:?}"), "[1, 2]");
        assert_eq!(lock.try_lock().err(), Some(MutexTryLockError));
    }
    assert_eq!(*lock.try_lock().expect("released"), [1, 2]);
    assert_eq!(*Mutex::<u8>::default().lock().await, 0);
    assert_eq!(MutexTryLockError.to_string(), "mutex is busy");
}

#[tokio::test]
async fn owned_guards_move_into_launched_work() {
    let lock = Arc::new(Mutex::new(0_u32));
    let mut guard = Arc::clone(&lock).lock_owned().await;
    *guard += 1;
    assert!(Arc::clone(&lock).try_lock_owned().is_err());
    async_engine::launch(async move {
        *guard += 1;
    })
    .await
    .unwrap();
    let guard = Arc::clone(&lock)
        .try_lock_owned()
        .expect("released by the task");
    assert_eq!(*guard, 2);
}

/// The owned guard retains the very `Arc<Mutex<T>>` it came from.
///
/// A registry keyed by `Weak<Mutex<T>>` -- zccache's per-output link locks --
/// hands a second caller the *same* lock by upgrading the weak reference.
/// If the guard kept only private storage alive, dropping the last caller
/// handle would let the upgrade fail while the lock was still held, and the
/// second caller would mint a fresh, uncontended lock: two holders of what
/// was meant to be one exclusive section.
#[tokio::test]
async fn an_owned_guard_keeps_weak_registry_entries_upgradable() {
    let lock = Arc::new(Mutex::new(()));
    let registry: Weak<Mutex<()>> = Arc::downgrade(&lock);

    let guard = Arc::clone(&lock).lock_owned().await;
    drop(lock);
    let same = registry
        .upgrade()
        .expect("a held owned guard keeps the facade Arc alive");
    assert!(
        Arc::clone(&same).try_lock_owned().is_err(),
        "the upgraded handle is the held lock, not a new one"
    );
    drop(same);
    drop(guard);
    assert!(
        registry.upgrade().is_none(),
        "releasing the guard releases the handle"
    );

    let lock = Arc::new(Mutex::new(()));
    let registry = Arc::downgrade(&lock);
    let guard = Arc::clone(&lock).try_lock_owned().expect("uncontended");
    drop(lock);
    assert!(
        registry.upgrade().is_some(),
        "try_lock_owned retains it too"
    );
    drop(guard);
    assert!(registry.upgrade().is_none());
}

#[tokio::test]
async fn a_cancelled_waiter_does_not_strand_the_next() {
    let lock = Arc::new(Mutex::new(Vec::new()));
    let held = Arc::clone(&lock).lock_owned().await;

    let mut abandoned = Box::pin(Arc::clone(&lock).lock_owned());
    assert!(poll_once(&mut abandoned).await, "queued behind the holder");
    let first = async_engine::launch({
        let lock = Arc::clone(&lock);
        async move { lock.lock().await.push(1) }
    });
    async_engine::yield_now().await;
    // Cancelling the earlier waiter must not strand the one behind it.
    drop(abandoned);

    drop(held);
    first.await.unwrap();
    lock.lock().await.push(2);
    assert_eq!(*lock.lock().await, [1, 2]);
}

#[test]
fn blocking_lock_serves_synchronous_threads() {
    let lock = Arc::new(Mutex::new(0_u32));
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let lock = Arc::clone(&lock);
            std::thread::spawn(move || *lock.blocking_lock() += 1)
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(*lock.blocking_lock(), 4);
}

#[tokio::test(start_paused = true)]
async fn bounded_lock_times_out_while_held_and_succeeds_once_released() {
    use kernal_api::async_engine::LockTimedOut;
    use std::time::Duration;

    let lock = Arc::new(Mutex::new(0_u32));
    let held = lock.lock().await;
    let start = tokio::time::Instant::now();
    assert!(matches!(
        lock.lock_within(Duration::from_secs(2)).await,
        Err(LockTimedOut)
    ));
    assert!(matches!(
        Arc::clone(&lock)
            .lock_owned_within(Duration::from_secs(1))
            .await,
        Err(LockTimedOut)
    ));
    assert_eq!(start.elapsed(), Duration::from_secs(3));
    drop(held);
    *lock.lock_within(Duration::ZERO).await.unwrap() += 1;
    let owned = Arc::clone(&lock)
        .lock_owned_within(Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(*owned, 1);
}

#[test]
fn a_bounded_blocking_lock_times_out_then_acquires_after_release() {
    use kernal_api::async_engine::LockTimedOut;
    use std::time::{Duration, Instant};

    let mutex = Arc::new(Mutex::new(0_u8));
    let held = mutex.try_lock().unwrap();
    let started = Instant::now();
    assert!(matches!(
        mutex.blocking_lock_within(Duration::from_millis(100)),
        Err(LockTimedOut)
    ));
    assert!(started.elapsed() >= Duration::from_millis(100));

    let waiter = {
        let mutex = Arc::clone(&mutex);
        std::thread::spawn(move || {
            *mutex
                .blocking_lock_within(Duration::from_secs(30))
                .expect("acquired once the holder releases") += 1;
        })
    };
    std::thread::sleep(Duration::from_millis(50));
    drop(held);
    waiter.join().unwrap();
    assert_eq!(*mutex.try_lock().unwrap(), 1);
}

#[tokio::test]
async fn an_expired_wait_can_name_the_holder_and_its_hold_time() {
    let mutex = Arc::new(Mutex::new(0_u8));
    let line = line!() + 1;
    let guard = Arc::clone(&mutex).try_lock_owned().unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    let expired = mutex
        .lock_within(std::time::Duration::from_millis(10))
        .await
        .err();
    assert_eq!(expired, Some(async_engine::LockTimedOut));
    let holder = mutex.holder().expect("the lock is held");
    assert_eq!(holder.location().file(), file!());
    assert_eq!(holder.location().line(), line);
    assert!(holder.held_for() >= std::time::Duration::from_millis(20));

    drop(guard);
    assert_eq!(mutex.holder(), None, "release clears the holder");
    // The location is the method name's line, one below the `let`.
    let relock_line = line!() + 2;
    let reacquired = mutex
        .lock_within(std::time::Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(
        mutex.holder().map(|holder| holder.location().line()),
        Some(relock_line),
        "the new holder replaces the record"
    );
    drop(reacquired);
}
