use kernal_api::async_engine::{self, channel, RecvTimeoutError, SendTimeoutError, TryRecvError};
use std::time::Duration;

#[tokio::test(start_paused = true)]
async fn full_channel_send_times_out_returning_its_value() {
    let (sender, mut receiver) = channel(1);
    sender.send_within(1, Duration::from_secs(1)).await.unwrap();
    let start = tokio::time::Instant::now();
    assert!(matches!(
        sender.send_within(2, Duration::from_secs(3)).await,
        Err(SendTimeoutError::TimedOut(2))
    ));
    assert_eq!(start.elapsed(), Duration::from_secs(3));
    assert_eq!(receiver.try_recv(), Ok(1));
    assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
    sender.send_within(3, Duration::ZERO).await.unwrap();
    drop(receiver);
    assert!(matches!(
        sender.send_within(4, Duration::from_secs(1)).await,
        Err(SendTimeoutError::Closed(4))
    ));
}

#[tokio::test(start_paused = true)]
async fn empty_channel_recv_times_out_then_sees_values_and_disconnection() {
    let (sender, mut receiver) = channel::<u8>(1);
    let start = tokio::time::Instant::now();
    assert_eq!(
        receiver.recv_within(Duration::from_secs(2)).await,
        Err(RecvTimeoutError::TimedOut)
    );
    assert_eq!(start.elapsed(), Duration::from_secs(2));
    let producer = tokio::spawn(async move {
        async_engine::sleep(Duration::from_secs(5)).await;
        sender.send_within(9, Duration::from_secs(1)).await.unwrap();
    });
    assert_eq!(receiver.recv_within(Duration::from_secs(60)).await, Ok(9));
    producer.await.unwrap();
    assert_eq!(
        receiver.recv_within(Duration::from_secs(1)).await,
        Err(RecvTimeoutError::Disconnected)
    );
}

#[tokio::test(start_paused = true)]
async fn unbounded_receive_times_out_then_sees_values_and_disconnection() {
    let (sender, mut receiver) = async_engine::unbounded_channel::<u8>();
    let start = tokio::time::Instant::now();
    assert_eq!(
        receiver.recv_within(Duration::from_secs(2)).await,
        Err(RecvTimeoutError::TimedOut)
    );
    assert_eq!(start.elapsed(), Duration::from_secs(2));
    sender.send(4).unwrap();
    drop(sender);
    assert_eq!(receiver.recv_within(Duration::ZERO).await, Ok(4));
    assert_eq!(
        receiver.recv_within(Duration::from_secs(1)).await,
        Err(RecvTimeoutError::Disconnected)
    );
}

#[tokio::test(start_paused = true)]
async fn broadcast_receive_times_out_and_keeps_lag_and_closure() {
    use kernal_api::async_engine::{broadcast_channel, BroadcastRecvTimeoutError};

    let (sender, mut receiver) = broadcast_channel::<u8>(1).unwrap();
    let start = tokio::time::Instant::now();
    assert_eq!(
        receiver.recv_within(Duration::from_secs(2)).await,
        Err(BroadcastRecvTimeoutError::TimedOut)
    );
    assert_eq!(start.elapsed(), Duration::from_secs(2));
    sender.send(1).unwrap();
    sender.send(2).unwrap();
    assert_eq!(
        receiver.recv_within(Duration::ZERO).await,
        Err(BroadcastRecvTimeoutError::Lagged(1))
    );
    assert_eq!(receiver.recv_within(Duration::ZERO).await, Ok(2));
    drop(sender);
    assert_eq!(
        receiver.recv_within(Duration::from_secs(1)).await,
        Err(BroadcastRecvTimeoutError::Closed)
    );
}

#[tokio::test(start_paused = true)]
async fn oneshot_receive_times_out_and_stays_usable() {
    let (sender, mut receiver) = async_engine::oneshot_channel::<u8>();
    let start = tokio::time::Instant::now();
    assert_eq!(
        receiver.recv_within(Duration::from_secs(2)).await,
        Err(RecvTimeoutError::TimedOut)
    );
    assert_eq!(start.elapsed(), Duration::from_secs(2));
    sender.send(7).unwrap();
    assert_eq!(receiver.recv_within(Duration::ZERO).await, Ok(7));

    let (sender, mut receiver) = async_engine::oneshot_channel::<u8>();
    drop(sender);
    assert_eq!(
        receiver.recv_within(Duration::from_secs(1)).await,
        Err(RecvTimeoutError::Disconnected)
    );
}
