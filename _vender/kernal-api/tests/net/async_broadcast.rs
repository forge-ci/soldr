use kernal_api::async_engine::{
    self, broadcast_channel, BroadcastRecvError, BroadcastTryRecvError,
};
use std::time::Duration;

#[test]
fn bounded_fanout_reports_lag_and_resumes_in_order() {
    let (sender, mut first) = broadcast_channel(2).unwrap();
    assert_eq!(sender.send(1).unwrap(), 1);
    let mut second = sender.subscribe();
    let clone = sender.clone();
    assert_eq!(clone.send(2).unwrap(), 2);
    assert_eq!(sender.send(3).unwrap(), 2);
    assert_eq!(first.try_recv(), Err(BroadcastTryRecvError::Lagged(1)));
    assert_eq!(first.try_recv(), Ok(2));
    assert_eq!(first.try_recv(), Ok(3));
    assert_eq!(second.try_recv(), Ok(2));
    assert_eq!(second.try_recv(), Ok(3));
    assert_eq!(second.try_recv(), Err(BroadcastTryRecvError::Empty));
    drop(sender);
    drop(clone);
    assert_eq!(first.try_recv(), Err(BroadcastTryRecvError::Closed));
}

#[test]
fn unsubscribed_send_returns_value_and_invalid_capacity_is_rejected() {
    for capacity in [0, 3, 1_048_577, usize::MAX] {
        assert!(broadcast_channel::<String>(capacity).is_err());
    }
    let (sender, receiver) = broadcast_channel(1).unwrap();
    drop(receiver);
    assert_eq!(
        sender.send(String::from("retained")).unwrap_err().0,
        "retained"
    );
    let mut receiver = sender.subscribe();
    sender.send(String::from("new")).unwrap();
    drop(sender);
    assert_eq!(receiver.try_recv().unwrap(), "new");
    assert_eq!(receiver.try_recv(), Err(BroadcastTryRecvError::Closed));
}

// Contract test of the unbounded receive deprecated by #433.
#[allow(deprecated)]
#[tokio::test(start_paused = true)]
async fn cancelled_receive_does_not_consume_and_closed_queue_drains() {
    let (sender, mut receiver) = broadcast_channel(1).unwrap();
    assert!(
        async_engine::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .is_err()
    );
    sender.send(1).unwrap();
    sender.send(2).unwrap();
    drop(sender);
    assert_eq!(receiver.recv().await, Err(BroadcastRecvError::Lagged(1)));
    assert_eq!(receiver.recv().await, Ok(2));
    assert_eq!(receiver.recv().await, Err(BroadcastRecvError::Closed));
}

#[cfg(feature = "event-stream")]
#[tokio::test]
async fn stream_adapter_maps_lag_without_a_pump_and_ends_on_close() {
    use futures_core::Stream;
    use std::{future::poll_fn, pin::Pin};
    let (sender, receiver) = broadcast_channel(1).unwrap();
    let mut stream = receiver.into_stream_with(|result| match result {
        Ok(value) => Some(value),
        Err(lag) => Some(-(lag.skipped as i32)),
    });
    sender.send(10).unwrap();
    sender.send(20).unwrap();
    drop(sender);
    assert_eq!(
        poll_fn(|cx| Pin::new(&mut stream).poll_next(cx)).await,
        Some(-1)
    );
    assert_eq!(
        poll_fn(|cx| Pin::new(&mut stream).poll_next(cx)).await,
        Some(20)
    );
    assert_eq!(
        poll_fn(|cx| Pin::new(&mut stream).poll_next(cx)).await,
        None
    );
}

#[cfg(feature = "event-stream")]
#[tokio::test(start_paused = true)]
async fn pending_stream_is_cancellable_and_filters_only_by_caller_choice() {
    use futures_core::Stream;
    use std::{future::poll_fn, pin::Pin};
    let (sender, receiver) = broadcast_channel(1).unwrap();
    let mut stream = receiver.into_stream_with(Result::ok);
    assert!(async_engine::timeout(
        Duration::from_secs(1),
        poll_fn(|cx| Pin::new(&mut stream).poll_next(cx)),
    )
    .await
    .is_err());
    sender.send(1).unwrap();
    sender.send(2).unwrap();
    assert_eq!(
        poll_fn(|cx| Pin::new(&mut stream).poll_next(cx)).await,
        Some(2)
    );
    drop(stream);
    assert_eq!(sender.send(3).unwrap_err().0, 3);
}
