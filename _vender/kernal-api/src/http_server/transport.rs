use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// A write-progress budget, armed only while an attempted write/flush/shutdown
/// is pending. Idle application streams do not spend this budget.
pub(super) struct ProgressIo<S> {
    socket: S,
    timeout: Duration,
    deadline: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl<S> ProgressIo<S> {
    pub(super) fn new(socket: S, timeout: Duration) -> Self {
        Self {
            socket,
            timeout,
            deadline: None,
        }
    }

    fn expired(&mut self, cx: &mut Context<'_>) -> bool {
        self.deadline
            .as_mut()
            .is_some_and(|timer| timer.as_mut().poll(cx).is_ready())
    }

    fn wait<T>(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<T>> {
        self.deadline
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(self.timeout)));
        if self.expired(cx) {
            Poll::Ready(Err(stalled()))
        } else {
            Poll::Pending
        }
    }
}

fn stalled() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "HTTP response write made no progress",
    )
}

impl<S: AsyncRead + Unpin> AsyncRead for ProgressIo<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().socket).poll_read(cx, buffer)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for ProgressIo<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if this.expired(cx) {
            return Poll::Ready(Err(stalled()));
        }
        match Pin::new(&mut this.socket).poll_write(cx, bytes) {
            Poll::Pending => this.wait(cx),
            Poll::Ready(result) => {
                if matches!(result, Ok(n) if n > 0) {
                    this.deadline = None;
                }
                Poll::Ready(result)
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.expired(cx) {
            return Poll::Ready(Err(stalled()));
        }
        match Pin::new(&mut this.socket).poll_flush(cx) {
            Poll::Pending => this.wait(cx),
            Poll::Ready(result) => {
                if result.is_ok() {
                    this.deadline = None;
                }
                Poll::Ready(result)
            }
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.expired(cx) {
            return Poll::Ready(Err(stalled()));
        }
        match Pin::new(&mut this.socket).poll_shutdown(cx) {
            Poll::Pending => this.wait(cx),
            Poll::Ready(result) => {
                if result.is_ok() {
                    this.deadline = None;
                }
                Poll::Ready(result)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ProgressIo;
    use std::{io, time::Duration};
    use tokio::io::AsyncWriteExt;

    #[tokio::test(start_paused = true)]
    async fn write_timeout_starts_on_attempt_not_idle_connection_creation() {
        let (socket, _reader) = tokio::io::duplex(1);
        let mut writer = ProgressIo::new(socket, Duration::from_secs(10));
        tokio::time::sleep(Duration::from_secs(100)).await;
        writer.write_all(b"a").await.unwrap();
        let start = tokio::time::Instant::now();
        let error = writer.write_all(b"b").await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(start.elapsed(), Duration::from_secs(10));
    }
}
