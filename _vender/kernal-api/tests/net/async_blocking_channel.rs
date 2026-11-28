use kernal_api::async_engine::{channel, BlockingSendError, RuntimeBuilder, TryRecvError};
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(10);

#[test]
fn blocking_sender_works_without_a_runtime_and_preserves_order() {
    let (sender, mut receiver) = channel(1);
    sender.blocking_send(1, WAIT).unwrap();
    let (finished, completion) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        sender.blocking_send(2, WAIT).unwrap();
        finished.send(()).unwrap();
    });
    assert!(completion.recv_timeout(Duration::from_millis(50)).is_err());
    assert_eq!(receiver.try_recv(), Ok(1));
    completion.recv_timeout(Duration::from_secs(2)).unwrap();
    worker.join().unwrap();
    assert_eq!(receiver.try_recv(), Ok(2));
    assert_eq!(receiver.try_recv(), Err(TryRecvError::Disconnected));
}

#[test]
fn closing_or_dropping_receiver_wakes_blocked_sender_with_its_value() {
    for close in [true, false] {
        let (sender, mut receiver) = channel(1);
        sender.blocking_send(1, WAIT).unwrap();
        let (finished, completion) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            finished.send(sender.blocking_send(2, WAIT)).unwrap();
        });
        assert!(completion.recv_timeout(Duration::from_millis(50)).is_err());
        if close {
            receiver.close();
            assert_eq!(receiver.try_recv(), Ok(1));
        } else {
            drop(receiver);
        }
        assert!(matches!(
            completion.recv_timeout(Duration::from_secs(2)).unwrap(),
            Err(BlockingSendError::Closed(2))
        ));
        worker.join().unwrap();
    }
}

#[test]
fn blocking_sender_rejects_async_context_without_enqueuing() {
    let runtime = RuntimeBuilder::current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (sender, mut receiver) = channel(1);
    runtime
        .run_within(
            async {
                assert!(matches!(
                    sender.blocking_send(7, WAIT),
                    Err(BlockingSendError::AsyncContext(7))
                ));
                assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
            },
            std::time::Duration::from_secs(60),
        )
        .expect("test runtime budget");
    sender.blocking_send(8, WAIT).unwrap();
    assert_eq!(receiver.try_recv(), Ok(8));
}

#[test]
fn full_channel_send_times_out_returning_its_value_without_enqueuing() {
    let (sender, mut receiver) = channel(1);
    sender.blocking_send(1, WAIT).unwrap();
    let started = Instant::now();
    assert!(matches!(
        sender.blocking_send(2, Duration::from_millis(100)),
        Err(BlockingSendError::TimedOut(2))
    ));
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(100),
        "returned early after {waited:?}"
    );
    assert!(
        waited < Duration::from_secs(5),
        "overran the budget: {waited:?}"
    );
    assert_eq!(receiver.try_recv(), Ok(1));
    assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
    // The expired wait must not keep a reservation: capacity is usable again.
    sender.blocking_send(3, WAIT).unwrap();
    assert_eq!(receiver.try_recv(), Ok(3));
}

#[test]
fn capacity_freed_before_the_deadline_wakes_the_sender() {
    let (sender, mut receiver) = channel(1);
    sender.blocking_send(1, WAIT).unwrap();
    let worker = std::thread::spawn(move || sender.blocking_send(2, WAIT));
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(receiver.try_recv(), Ok(1));
    assert!(worker.join().unwrap().is_ok());
    assert_eq!(receiver.try_recv(), Ok(2));
}
