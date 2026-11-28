//! `Notify` futures created before they wait, `Task::detach_on_drop`,
//! payload-agnostic channel `Debug`, and the blocking-lane thread cap.

use kernal_api::async_engine::{self, Notify, RuntimeBuilder};
use std::pin::pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

const BOUND: Duration = Duration::from_secs(10);

#[tokio::test]
async fn an_enabled_notified_future_sees_a_later_broadcast() {
    let notify = Notify::new();
    let mut notified = pin!(notify.notified());
    assert!(!notified.as_mut().enable(), "nothing sent yet");
    // `notify_waiters` wakes only registered waiters and stores no permit:
    // registering through `enable` is what makes this broadcast count.
    notify.notify_waiters();
    async_engine::timeout(BOUND, notified)
        .await
        .expect("the registered waiter is woken");
}

#[tokio::test]
async fn enable_reports_a_stored_permit() {
    let notify = Notify::new();
    notify.notify_one();
    let mut notified = pin!(notify.notified());
    assert!(notified.as_mut().enable(), "the stored permit is received");
    async_engine::timeout(BOUND, notified)
        .await
        .expect("a received notification completes at once");
}

#[tokio::test]
async fn an_owned_notified_future_outlives_its_borrow() {
    let notify = Arc::new(Notify::new());
    let mut notified = Box::pin(Arc::clone(&notify).owned_notified());
    assert!(!notified.as_mut().enable());
    let waiter = async_engine::launch(notified);
    notify.notify_waiters();
    async_engine::timeout(BOUND, waiter)
        .await
        .expect("the owned waiter is woken")
        .unwrap();
    assert!(format!("{:?}", Arc::clone(&notify).owned_notified()).contains("OwnedNotified"));
}

#[tokio::test]
async fn a_detach_on_drop_task_survives_its_handle_and_stays_joinable() {
    let (release, released) = async_engine::oneshot_channel::<()>();
    let (done, mut finished) = async_engine::channel::<u32>(1);
    let task = async_engine::launch(async move {
        let _ = released.await;
        done.send_within(7, Duration::from_secs(1)).await.unwrap();
    })
    .detach_on_drop();
    drop(task);
    release.send(()).unwrap();
    let value = finished
        .recv_within(Duration::from_secs(1))
        .await
        .expect("the dropped handle did not cancel the task");
    assert_eq!(value, 7);

    let joinable = async_engine::launch(async { 11 }).detach_on_drop();
    assert_eq!(joinable.await.unwrap(), 11);

    let cancellable = async_engine::launch(std::future::pending::<()>()).detach_on_drop();
    cancellable.cancel();
    assert!(cancellable.await.unwrap_err().is_cancelled());
}

#[test]
fn channel_handles_debug_without_a_debug_payload() {
    struct Opaque;
    let (sender, receiver) = async_engine::channel::<Opaque>(1);
    let (unbounded, unbounded_receiver) = async_engine::unbounded_channel::<Opaque>();
    let (oneshot, oneshot_receiver) = async_engine::oneshot_channel::<Opaque>();
    for (debug, name) in [
        (format!("{sender:?}"), "Sender"),
        (format!("{receiver:?}"), "Receiver"),
        (format!("{unbounded:?}"), "UnboundedSender"),
        (format!("{unbounded_receiver:?}"), "UnboundedReceiver"),
        (format!("{oneshot:?}"), "OneshotSender"),
        (format!("{oneshot_receiver:?}"), "OneshotReceiver"),
    ] {
        assert!(debug.starts_with(name), "{debug}");
    }
}

#[test]
fn max_blocking_threads_caps_the_blocking_lane() {
    let runtime = RuntimeBuilder::multi_thread()
        .worker_threads(1)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let running = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    runtime
        .run_within(
            async {
                let tasks: Vec<_> = (0..4)
                    .map(|_| {
                        let running = Arc::clone(&running);
                        let peak = Arc::clone(&peak);
                        async_engine::launch_blocking(move || {
                            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                            peak.fetch_max(now, Ordering::SeqCst);
                            std::thread::sleep(Duration::from_millis(20));
                            running.fetch_sub(1, Ordering::SeqCst);
                        })
                    })
                    .collect();
                for task in tasks {
                    task.await.unwrap();
                }
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
    assert_eq!(
        peak.load(Ordering::SeqCst),
        1,
        "one blocking thread at a time"
    );
}

#[test]
#[should_panic(expected = "max_blocking_threads must be greater than zero")]
fn max_blocking_threads_rejects_zero() {
    let _ = RuntimeBuilder::multi_thread().max_blocking_threads(0);
}

#[tokio::test(start_paused = true)]
async fn bounded_notification_waits_time_out_and_still_see_an_enabled_permit() {
    use kernal_api::async_engine::NotifyTimedOut;

    let notify = Arc::new(Notify::new());
    let start = tokio::time::Instant::now();
    assert!(matches!(
        notify.notified().wait_within(Duration::from_secs(2)).await,
        Err(NotifyTimedOut)
    ));
    assert!(matches!(
        Arc::clone(&notify)
            .owned_notified()
            .wait_within(Duration::from_secs(1))
            .await,
        Err(NotifyTimedOut)
    ));
    assert_eq!(start.elapsed(), Duration::from_secs(3));

    // A stored notify_one permit satisfies even a zero budget.
    notify.notify_one();
    assert!(notify.notified().wait_within(Duration::ZERO).await.is_ok());

    let owned = Arc::clone(&notify).owned_notified();
    let notifier = Arc::clone(&notify);
    let sender = tokio::spawn(async move {
        async_engine::sleep(Duration::from_secs(5)).await;
        notifier.notify_one();
    });
    assert!(owned.wait_within(Duration::from_secs(60)).await.is_ok());
    sender.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn bounded_joins_time_out_and_keep_the_task() {
    use kernal_api::async_engine::{JoinTimedOut, TaskGroup};

    let mut task = async_engine::launch(async {
        async_engine::sleep(Duration::from_secs(10)).await;
        7_u8
    });
    let start = tokio::time::Instant::now();
    assert!(matches!(
        task.join_within(Duration::from_secs(2)).await,
        Err(JoinTimedOut)
    ));
    assert_eq!(start.elapsed(), Duration::from_secs(2));
    assert!(
        !task.is_finished(),
        "an expired join must not cancel the task"
    );
    assert_eq!(
        task.join_within(Duration::from_secs(60))
            .await
            .unwrap()
            .unwrap(),
        7
    );

    let mut group = TaskGroup::new();
    group.spawn(async {
        async_engine::sleep(Duration::from_secs(5)).await;
        1_u8
    });
    assert!(matches!(
        group.join_next_within(Duration::from_secs(1)).await,
        Err(JoinTimedOut)
    ));
    assert_eq!(group.len(), 1);
    assert_eq!(
        group
            .join_next_within(Duration::from_secs(60))
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        1
    );
    assert!(group
        .join_next_within(Duration::ZERO)
        .await
        .unwrap()
        .is_none());
}

#[tokio::test(start_paused = true)]
async fn bounded_pair_join_times_out_or_returns_both_outputs() {
    use kernal_api::async_engine::JoinTimedOut;

    let start = tokio::time::Instant::now();
    let fast = async { 1_u8 };
    let slow = async_engine::sleep(Duration::from_secs(10));
    assert!(matches!(
        async_engine::join_within(fast, slow, Duration::from_secs(3)).await,
        Err(JoinTimedOut)
    ));
    assert_eq!(start.elapsed(), Duration::from_secs(3));

    let both = async_engine::join_within(
        async { 2_u8 },
        async {
            async_engine::sleep(Duration::from_secs(1)).await;
            "done"
        },
        Duration::from_secs(60),
    )
    .await;
    assert_eq!(both, Ok((2, "done")));
}

#[test]
fn bounded_runtime_run_times_out_or_returns_the_output() {
    use kernal_api::async_engine::DeadlineElapsed;

    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    let start = std::time::Instant::now();
    assert!(matches!(
        runtime.run_within(std::future::pending::<()>(), Duration::from_millis(50)),
        Err(DeadlineElapsed)
    ));
    assert!(start.elapsed() < Duration::from_secs(30));
    assert!(matches!(
        runtime.run_within(async { 3_u8 }, Duration::from_secs(60)),
        Ok(3)
    ));
}
