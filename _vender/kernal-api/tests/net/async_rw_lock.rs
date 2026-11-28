//! `async_engine::RwLock`: fair write preference, owned/borrowed/blocking
//! guards, and cancellation that releases a queued waiter.
//!
//! These are the contract tests of the unbounded acquisitions deprecated by
//! #433; they keep exercising them until the forms are removed.
#![allow(deprecated)]

use kernal_api::async_engine::{self, RwLock, RwLockTryLockError};
use std::future::Future;
use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

/// Poll `future` exactly once and report whether it is still pending.
async fn poll_once<F: Future + Unpin>(future: &mut F) -> bool {
    std::future::poll_fn(|context| {
        Poll::Ready(std::pin::Pin::new(&mut *future).poll(context).is_pending())
    })
    .await
}

#[tokio::test]
async fn borrowed_guards_share_reads_and_mutate_exclusively() {
    let lock = RwLock::new(vec![1_u8]);
    {
        let first = lock.read().await;
        let second = lock.read().await;
        assert_eq!(*first, *second);
        assert_eq!(format!("{first:?}"), "[1]");
    }
    lock.write().await.push(2);
    assert_eq!(*lock.read().await, [1, 2]);
    assert_eq!(lock.into_inner(), Some(vec![1, 2]));
    assert_eq!(*RwLock::<u8>::default().read().await, 0);
}

#[tokio::test]
async fn owned_guards_move_into_launched_work() {
    let lock = Arc::new(RwLock::new(0_u32));
    let mut writer = Arc::clone(&lock).write_owned().await;
    *writer += 1;
    let task = async_engine::launch(async move {
        *writer += 1;
        drop(writer);
    });
    task.await.unwrap();
    let reader = Arc::clone(&lock).read_owned().await;
    assert_eq!(*reader, 2);
    let reader = async_engine::launch(async move { *reader }).await.unwrap();
    assert_eq!(reader, 2);
}

#[tokio::test]
async fn owned_guard_keeps_the_value_alive_past_the_lock() {
    let lock = Arc::new(RwLock::new(String::from("kept")));
    let guard = Arc::clone(&lock).read_owned().await;
    let lock = Arc::into_inner(lock).expect("only the guard's private lease remains");
    assert_eq!(
        lock.into_inner(),
        None,
        "the owned guard still reaches the value"
    );
    assert_eq!(&*guard, "kept");
}

#[tokio::test]
async fn a_queued_writer_blocks_later_readers() {
    let lock = Arc::new(RwLock::new(()));
    let reader = Arc::clone(&lock).read_owned().await;
    let mut writer = Box::pin(Arc::clone(&lock).write_owned());
    assert!(poll_once(&mut writer).await, "writer waits for the reader");

    // Write preference: a later reader must not overtake the queued writer.
    assert_eq!(
        Arc::clone(&lock).try_read_owned().err(),
        Some(RwLockTryLockError)
    );
    let mut late_reader = Box::pin(lock.read());
    assert!(poll_once(&mut late_reader).await);

    drop(reader);
    let writer = async_engine::timeout(Duration::from_secs(5), writer)
        .await
        .expect("writer acquires once the earlier reader leaves");
    assert!(poll_once(&mut late_reader).await, "reader still queued");
    drop(writer);
    async_engine::timeout(Duration::from_secs(5), late_reader)
        .await
        .expect("reader acquires after the writer");
    assert!(Arc::clone(&lock).try_read_owned().is_ok());
}

#[tokio::test]
async fn cancelling_a_queued_writer_releases_later_readers() {
    let lock = Arc::new(RwLock::new(()));
    let reader = lock.read().await;
    let mut writer = Box::pin(lock.write());
    assert!(poll_once(&mut writer).await);
    assert!(Arc::clone(&lock).try_read_owned().is_err());
    drop(writer);
    assert!(Arc::clone(&lock).try_read_owned().is_ok());
    drop(reader);
}

#[test]
fn blocking_guards_work_from_synchronous_threads() {
    let lock = Arc::new(RwLock::new(1_u8));
    *lock.blocking_write() = 2;
    assert_eq!(*lock.blocking_read(), 2);
    let runtime = async_engine::RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    let shared = Arc::clone(&lock);
    runtime.run(async move {
        async_engine::launch_blocking(move || *shared.blocking_write() = 3)
            .await
            .unwrap();
    });
    assert_eq!(*lock.blocking_read(), 3);
}

#[tokio::test(start_paused = true)]
async fn bounded_read_and_write_time_out_against_the_opposite_holder() {
    use kernal_api::async_engine::{LockTimedOut, RwLock};
    use std::time::Duration;

    let lock = RwLock::new(5_u32);
    let reader = lock.read_within(Duration::from_secs(1)).await.unwrap();
    let also = lock.read_within(Duration::ZERO).await.unwrap();
    let start = tokio::time::Instant::now();
    assert!(matches!(
        lock.write_within(Duration::from_secs(2)).await,
        Err(LockTimedOut)
    ));
    assert_eq!(start.elapsed(), Duration::from_secs(2));
    drop((reader, also));
    let writer = lock.write_within(Duration::ZERO).await.unwrap();
    assert!(matches!(
        lock.read_within(Duration::from_secs(1)).await,
        Err(LockTimedOut)
    ));
    drop(writer);
    assert_eq!(*lock.read_within(Duration::ZERO).await.unwrap(), 5);
}

#[tokio::test(start_paused = true)]
async fn bounded_owned_guards_time_out_against_the_opposite_holder() {
    use kernal_api::async_engine::LockTimedOut;

    let lock = Arc::new(RwLock::new(1_u32));
    let reader = Arc::clone(&lock)
        .read_owned_within(Duration::ZERO)
        .await
        .unwrap();
    let start = tokio::time::Instant::now();
    assert!(matches!(
        Arc::clone(&lock)
            .write_owned_within(Duration::from_secs(2))
            .await,
        Err(LockTimedOut)
    ));
    assert_eq!(start.elapsed(), Duration::from_secs(2));
    drop(reader);
    let mut writer = Arc::clone(&lock)
        .write_owned_within(Duration::ZERO)
        .await
        .unwrap();
    *writer += 1;
    assert!(matches!(
        Arc::clone(&lock)
            .read_owned_within(Duration::from_secs(1))
            .await,
        Err(LockTimedOut)
    ));
    drop(writer);
    assert_eq!(
        *Arc::clone(&lock)
            .read_owned_within(Duration::ZERO)
            .await
            .unwrap(),
        2
    );
}

#[test]
fn bounded_blocking_rw_guards_time_out_then_acquire_after_release() {
    use kernal_api::async_engine::LockTimedOut;
    use std::time::Instant;

    let lock = Arc::new(RwLock::new(0_u8));
    let writer = lock.blocking_write();
    let started = Instant::now();
    assert!(matches!(
        lock.blocking_read_within(Duration::from_millis(100)),
        Err(LockTimedOut)
    ));
    assert!(started.elapsed() >= Duration::from_millis(100));
    drop(writer);

    let reader = lock.blocking_read();
    assert!(matches!(
        lock.blocking_write_within(Duration::from_millis(100)),
        Err(LockTimedOut)
    ));
    let waiter = {
        let lock = Arc::clone(&lock);
        std::thread::spawn(move || {
            *lock
                .blocking_write_within(Duration::from_secs(30))
                .expect("acquired once the reader releases") += 1;
        })
    };
    std::thread::sleep(Duration::from_millis(50));
    drop(reader);
    waiter.join().unwrap();
    assert_eq!(
        *lock.blocking_read_within(Duration::from_secs(30)).unwrap(),
        1
    );
}
