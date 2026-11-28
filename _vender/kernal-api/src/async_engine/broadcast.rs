//! Bounded fanout with explicit loss notification, not a lossless work queue.

use super::SendError;

/// Create bounded event delivery to independently advancing subscribers.
///
/// Sending never waits for a slow subscriber: the oldest retained value is
/// replaced and that subscriber receives a lag count. Use [`super::channel`]
/// when a producer must wait instead. Capacity bounds entries, not payload
/// bytes; callers must bound message sizes separately.
///
/// # Errors
///
/// Capacity must be a power of two from 1 through 1,048,576. This makes the
/// configured capacity exact instead of silently rounding up storage.
pub fn broadcast_channel<T: Clone>(
    capacity: usize,
) -> std::io::Result<(BroadcastSender<T>, BroadcastReceiver<T>)> {
    if !capacity.is_power_of_two() || capacity > 1_048_576 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "broadcast capacity must be a power of two from 1 through 1048576",
        ));
    }
    let (sender, receiver) = tokio::sync::broadcast::channel(capacity);
    Ok((
        BroadcastSender { inner: sender },
        BroadcastReceiver { inner: receiver },
    ))
}

/// Cloneable producer for bounded event fanout.
#[derive(Debug)]
pub struct BroadcastSender<T> {
    inner: tokio::sync::broadcast::Sender<T>,
}

impl<T> Clone for BroadcastSender<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T: Clone> BroadcastSender<T> {
    /// Publish a value and return the current subscriber count. This does not
    /// guarantee every subscriber consumes it before later values displace it.
    ///
    /// # Errors
    /// Returns the original value when no subscribers remain.
    pub fn send(&self, value: T) -> Result<usize, SendError<T>> {
        self.inner.send(value).map_err(|error| SendError(error.0))
    }

    /// Subscribe to values sent after this call, not retained older values.
    pub fn subscribe(&self) -> BroadcastReceiver<T> {
        BroadcastReceiver {
            inner: self.inner.subscribe(),
        }
    }
}

/// One independently advancing subscription. Dropping it unsubscribes.
#[derive(Debug)]
pub struct BroadcastReceiver<T> {
    inner: tokio::sync::broadcast::Receiver<T>,
}

impl<T: Clone> BroadcastReceiver<T> {
    /// Await the next value. Cancelling a pending receive consumes nothing.
    ///
    /// # Errors
    /// Lag advances to the oldest retained value; retry to receive it. Closure
    /// is reported only after all senders are dropped and retained values drain.
    #[deprecated(note = "can wait forever; use `recv_within` (#433)")]
    pub async fn recv(&mut self) -> Result<T, BroadcastRecvError> {
        self.inner.recv().await.map_err(|error| match error {
            tokio::sync::broadcast::error::RecvError::Lagged(n) => BroadcastRecvError::Lagged(n),
            tokio::sync::broadcast::error::RecvError::Closed => BroadcastRecvError::Closed,
        })
    }

    /// Wait at most `timeout` for the next value, with the same lag and
    /// closure rules as [`Self::recv`]. An expired wait consumes nothing.
    ///
    /// # Errors
    /// Reports lag, a drained and closed subscription, or the timeout.
    /// Requires a runtime with its timer enabled.
    pub async fn recv_within(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<T, BroadcastRecvTimeoutError> {
        // Bounds the deprecated receive; inline it when that is removed (#433).
        #[allow(deprecated)]
        let receive = self.recv();
        match super::timeout(timeout, receive).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(BroadcastRecvError::Lagged(n))) => Err(BroadcastRecvTimeoutError::Lagged(n)),
            Ok(Err(BroadcastRecvError::Closed)) => Err(BroadcastRecvTimeoutError::Closed),
            Err(_) => Err(BroadcastRecvTimeoutError::TimedOut),
        }
    }

    /// Receive without waiting, with the same lag and closure rules as [`Self::recv`].
    ///
    /// # Errors
    /// Reports an empty queue, lag, or a drained and closed subscription.
    pub fn try_recv(&mut self) -> Result<T, BroadcastTryRecvError> {
        self.inner.try_recv().map_err(|error| match error {
            tokio::sync::broadcast::error::TryRecvError::Empty => BroadcastTryRecvError::Empty,
            tokio::sync::broadcast::error::TryRecvError::Lagged(n) => {
                BroadcastTryRecvError::Lagged(n)
            }
            tokio::sync::broadcast::error::TryRecvError::Closed => BroadcastTryRecvError::Closed,
        })
    }
}

/// A broadcast receive could not deliver the next value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BroadcastRecvError {
    /// This many older values were overwritten; retry from the oldest retained.
    #[error("broadcast receiver skipped {0} values")]
    Lagged(u64),
    /// No senders or retained values remain.
    #[error("broadcast channel is closed")]
    Closed,
}

/// A bounded-time broadcast receive could not deliver a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BroadcastRecvTimeoutError {
    /// This many older values were overwritten; retry from the oldest retained.
    #[error("broadcast receiver skipped {0} values")]
    Lagged(u64),
    /// No senders or retained values remain.
    #[error("broadcast channel is closed")]
    Closed,
    /// No value arrived before the timeout.
    #[error("no broadcast value arrived before the timeout")]
    TimedOut,
}

/// A non-waiting broadcast receive could not deliver a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BroadcastTryRecvError {
    /// Senders remain but no new value is available.
    #[error("broadcast channel is empty")]
    Empty,
    /// This many older values were overwritten; retry from the oldest retained.
    #[error("broadcast receiver skipped {0} values")]
    Lagged(u64),
    /// No senders or retained values remain.
    #[error("broadcast channel is closed")]
    Closed,
}

/// Loss reported by a stream subscription; closure is represented by stream EOF.
#[cfg(feature = "event-stream")]
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("broadcast receiver skipped {skipped} values")]
pub struct BroadcastLagged {
    /// Number of overwritten values.
    pub skipped: u64,
}

/// Pull-driven interoperability stream with application-owned item mapping.
/// Implements the ecosystem `futures_core::Stream` trait without exposing a
/// runtime-specific stream type. No pump task or additional queue is created.
#[cfg(feature = "event-stream")]
pub struct BroadcastStream<T, F> {
    inner: tokio_stream::wrappers::BroadcastStream<T>,
    map: F,
}

#[cfg(feature = "event-stream")]
impl<T: Clone + Send + 'static> BroadcastReceiver<T> {
    /// Convert this subscription into a stream, explicitly handling values and
    /// lag through `map`. Returning `None` filters a delivery notification.
    /// Dropping the stream unsubscribes and cancels a pending receive.
    pub fn into_stream_with<U, F>(self, map: F) -> BroadcastStream<T, F>
    where
        F: FnMut(Result<T, BroadcastLagged>) -> Option<U> + Unpin,
    {
        BroadcastStream {
            inner: tokio_stream::wrappers::BroadcastStream::new(self.inner),
            map,
        }
    }
}

#[cfg(feature = "event-stream")]
impl<T, U, F> futures_core::Stream for BroadcastStream<T, F>
where
    T: Clone + Send + 'static,
    F: FnMut(Result<T, BroadcastLagged>) -> Option<U> + Unpin,
{
    type Item = U;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<U>> {
        let this = self.get_mut();
        loop {
            let result = match std::task::ready!(std::pin::Pin::new(&mut this.inner).poll_next(cx))
            {
                Some(result) => result,
                None => return std::task::Poll::Ready(None),
            };
            let result = result.map_err(|error| match error {
                tokio_stream::wrappers::errors::BroadcastStreamRecvError::Lagged(skipped) => {
                    BroadcastLagged { skipped }
                }
            });
            if let Some(value) = (this.map)(result) {
                return std::task::Poll::Ready(Some(value));
            }
        }
    }
}
