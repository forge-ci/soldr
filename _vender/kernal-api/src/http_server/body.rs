use super::{Limits, SseEvent};
use bytes::Bytes;
use futures_core::Stream;
use hyper::body::{Body, Frame, SizeHint};
use std::{
    future::Future,
    io::{self, Read, Seek},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

#[derive(Clone, Debug)]
pub(super) struct ReadBudget(Arc<tokio::sync::Semaphore>);

impl ReadBudget {
    pub(super) fn new(capacity: usize) -> Self {
        Self(Arc::new(tokio::sync::Semaphore::new(capacity)))
    }

    pub(super) fn spawn<F, T>(
        &self,
        operation: F,
    ) -> io::Result<tokio::task::JoinHandle<io::Result<T>>>
    where
        F: FnOnce() -> io::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let permit = self.0.clone().try_acquire_owned().map_err(|_| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "HTTP file read capacity exhausted",
            )
        })?;
        Ok(tokio::task::spawn_blocking(move || {
            // Connection cancellation can detach this worker, but cannot admit
            // a replacement native read until its OS call actually finishes.
            let _permit = permit;
            operation()
        }))
    }
}

enum Source {
    Bytes(Bytes),
    File(FileSource),
    Events(EventSource),
}

struct FileSource {
    file: Arc<std::fs::File>,
    remaining: u64,
    prefix: Bytes,
    pending: Option<tokio::task::JoinHandle<io::Result<Bytes>>>,
    budget: ReadBudget,
}

struct EventSource {
    stream: EventStream,
    keepalive: Duration,
    timer: Pin<Box<tokio::time::Sleep>>,
    pending: Bytes,
    max_event: usize,
}

enum EventStream {
    Data(Pin<Box<dyn Stream<Item = io::Result<String>> + Send>>),
    Rich(Pin<Box<dyn Stream<Item = io::Result<SseEvent>> + Send>>),
}

impl EventStream {
    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<io::Result<SseEvent>>> {
        match self {
            Self::Data(stream) => stream.as_mut().poll_next(cx).map(|item| {
                item.map(|result| {
                    result.map(|data| SseEvent {
                        id: None,
                        event: None,
                        data,
                    })
                })
            }),
            Self::Rich(stream) => stream.as_mut().poll_next(cx),
        }
    }
}

fn encode_event(event: SseEvent, max_event: usize) -> io::Result<Bytes> {
    if event.data.len()
        + event.id.as_ref().map_or(0, String::len)
        + event.event.as_ref().map_or(0, String::len)
        > max_event
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "SSE event exceeds configured limit",
        ));
    }
    if event
        .id
        .as_ref()
        .is_some_and(|id| id.contains(['\r', '\n', '\0']))
        || event
            .event
            .as_ref()
            .is_some_and(|kind| kind.contains(['\r', '\n', '\0']))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid SSE field",
        ));
    }
    let normalized = event.data.replace("\r\n", "\n").replace('\r', "\n");
    let mut encoded = String::new();
    if let Some(id) = event.id {
        encoded.push_str("id: ");
        encoded.push_str(&id);
        encoded.push('\n');
    }
    if let Some(kind) = event.event {
        encoded.push_str("event: ");
        encoded.push_str(&kind);
        encoded.push('\n');
    }
    for line in normalized.split('\n') {
        encoded.push_str("data: ");
        encoded.push_str(line);
        encoded.push('\n');
    }
    encoded.push('\n');
    Ok(encoded.into())
}

pub(super) struct ServerBody {
    source: Source,
    chunk: usize,
    done: bool,
}

impl std::fmt::Debug for ServerBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerBody")
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

impl ServerBody {
    pub(super) fn bytes(bytes: Bytes) -> Self {
        Self {
            source: Source::Bytes(bytes),
            chunk: 65536,
            done: false,
        }
    }

    pub(super) fn file(mut file: std::fs::File, prefix: Vec<u8>) -> io::Result<Self> {
        let metadata = file.metadata()?;
        if !metadata.is_file() || prefix.len() > 1024 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file response requires a regular file and bounded prefix",
            ));
        }
        let remaining = metadata.len().saturating_sub(file.stream_position()?);
        Ok(Self {
            source: Source::File(FileSource {
                file: Arc::new(file),
                remaining,
                prefix: prefix.into(),
                pending: None,
                budget: ReadBudget::new(1),
            }),
            chunk: 65536,
            done: false,
        })
    }

    pub(super) fn events<S>(events: S, keepalive: Duration) -> io::Result<Self>
    where
        S: Stream<Item = io::Result<String>> + Send + 'static,
    {
        Self::event_source(EventStream::Data(Box::pin(events)), keepalive)
    }

    pub(super) fn sse_events<S>(events: S, keepalive: Duration) -> io::Result<Self>
    where
        S: Stream<Item = io::Result<SseEvent>> + Send + 'static,
    {
        Self::event_source(EventStream::Rich(Box::pin(events)), keepalive)
    }

    fn event_source(stream: EventStream, keepalive: Duration) -> io::Result<Self> {
        if keepalive.is_zero() || keepalive > Duration::from_secs(365 * 24 * 3600) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid SSE keepalive",
            ));
        }
        crate::async_engine::RuntimeHandle::current().map_err(io::Error::other)?;
        Ok(Self {
            source: Source::Events(EventSource {
                stream,
                keepalive,
                timer: Box::pin(tokio::time::sleep(keepalive)),
                pending: Bytes::new(),
                max_event: 65536,
            }),
            chunk: 65536,
            done: false,
        })
    }

    pub(super) fn configure(&mut self, limits: Limits) -> io::Result<()> {
        self.chunk = limits.max_stream_chunk_bytes;
        let accepted = match &mut self.source {
            Source::Bytes(bytes) => bytes.len() <= limits.max_response_body_bytes,
            Source::File(file) => {
                file.prefix.len() <= limits.max_response_body_bytes
                    && file
                        .remaining
                        .checked_add(file.prefix.len() as u64)
                        .is_some_and(|n| n <= limits.max_file_bytes)
            }
            Source::Events(events) => {
                events.max_event = limits.max_event_bytes;
                true
            }
        };
        if accepted {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP response exceeds configured limit",
            ))
        }
    }

    pub(super) fn set_read_budget(&mut self, budget: ReadBudget) {
        if let Source::File(file) = &mut self.source {
            file.budget = budget;
        }
    }

    fn poll_data(&mut self, cx: &mut Context<'_>) -> Poll<Option<io::Result<Bytes>>> {
        match &mut self.source {
            Source::Bytes(bytes) => {
                if bytes.is_empty() {
                    Poll::Ready(None)
                } else {
                    Poll::Ready(Some(Ok(bytes.split_to(bytes.len().min(self.chunk)))))
                }
            }
            Source::File(file) => {
                if !file.prefix.is_empty() {
                    return Poll::Ready(Some(Ok(file
                        .prefix
                        .split_to(file.prefix.len().min(self.chunk)))));
                }
                if file.remaining == 0 {
                    return Poll::Ready(None);
                }
                if file.pending.is_none() {
                    let size = file.remaining.min(self.chunk as u64) as usize;
                    let handle = file.file.clone();
                    match file.budget.spawn(move || {
                        let mut buffer = vec![0; size];
                        let count = loop {
                            match (&*handle).read(&mut buffer) {
                                Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                                    continue
                                }
                                result => break result?,
                            }
                        };
                        buffer.truncate(count);
                        Ok(Bytes::from(buffer))
                    }) {
                        Ok(read) => file.pending = Some(read),
                        Err(error) => return Poll::Ready(Some(Err(error))),
                    }
                }
                let result = match &mut file.pending {
                    Some(read) => std::task::ready!(Pin::new(read).poll(cx)),
                    None => return Poll::Ready(Some(Err(io::Error::other("missing file read")))),
                };
                file.pending = None;
                match result.map_err(io::Error::other).and_then(|result| result) {
                    Err(error) => Poll::Ready(Some(Err(error))),
                    Ok(bytes) if bytes.is_empty() => Poll::Ready(Some(Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "response file was truncated",
                    )))),
                    Ok(bytes) => {
                        file.remaining -= bytes.len() as u64;
                        Poll::Ready(Some(Ok(bytes)))
                    }
                }
            }
            Source::Events(events) => {
                if events.pending.is_empty() {
                    match events.stream.poll_next(cx) {
                        Poll::Ready(None) => return Poll::Ready(None),
                        Poll::Ready(Some(Err(error))) => return Poll::Ready(Some(Err(error))),
                        Poll::Ready(Some(Ok(event))) => {
                            events.pending = match encode_event(event, events.max_event) {
                                Ok(encoded) => encoded,
                                Err(error) => return Poll::Ready(Some(Err(error))),
                            };
                        }
                        Poll::Pending => {
                            std::task::ready!(events.timer.as_mut().poll(cx));
                            events.pending = Bytes::from_static(b":\n\n");
                        }
                    }
                    events
                        .timer
                        .as_mut()
                        .reset(tokio::time::Instant::now() + events.keepalive);
                }
                Poll::Ready(Some(Ok(events
                    .pending
                    .split_to(events.pending.len().min(self.chunk)))))
            }
        }
    }
}

impl Body for ServerBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        let this = self.get_mut();
        if this.done {
            return Poll::Ready(None);
        }
        match std::task::ready!(this.poll_data(cx)) {
            Some(Ok(bytes)) => Poll::Ready(Some(Ok(Frame::data(bytes)))),
            Some(Err(error)) => {
                this.done = true;
                Poll::Ready(Some(Err(error)))
            }
            None => {
                this.done = true;
                Poll::Ready(None)
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.done
            || match &self.source {
                Source::Bytes(bytes) => bytes.is_empty(),
                Source::File(file) => file.prefix.is_empty() && file.remaining == 0,
                Source::Events(_) => false,
            }
    }

    fn size_hint(&self) -> SizeHint {
        if self.done {
            return SizeHint::with_exact(0);
        }
        match &self.source {
            Source::Bytes(bytes) => SizeHint::with_exact(bytes.len() as u64),
            Source::File(file) => {
                SizeHint::with_exact(file.remaining.saturating_add(file.prefix.len() as u64))
            }
            Source::Events(_) => SizeHint::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_metadata_cannot_inject_another_field() {
        for value in ["x\nevent: hacked", "x\rid: hacked", "x\0y"] {
            let event = SseEvent {
                id: Some(value.into()),
                event: None,
                data: "safe".into(),
            };
            assert!(encode_event(event, 65536).is_err());
            let event = SseEvent {
                id: None,
                event: Some(value.into()),
                data: "safe".into(),
            };
            assert!(encode_event(event, 65536).is_err());
        }
    }

    #[tokio::test]
    async fn cancelled_read_keeps_shared_admission_until_native_work_finishes() {
        let budget = ReadBudget::new(1);
        let (release, waiting) = std::sync::mpsc::channel::<()>();
        let (started, began) = tokio::sync::oneshot::channel();
        let read = budget
            .spawn(move || {
                let _ = started.send(());
                let _ = waiting.recv();
                Ok(Bytes::new())
            })
            .unwrap();
        began.await.unwrap();
        drop(read);
        let file = tempfile::NamedTempFile::new().unwrap();
        file.as_file().set_len(1).unwrap();
        let mut body =
            ServerBody::file(std::fs::File::open(file.path()).unwrap(), Vec::new()).unwrap();
        body.set_read_budget(budget.clone());
        assert_eq!(
            body.frame().await.unwrap().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(body.frame().await.is_none());
        for _ in 0..10 {
            assert_eq!(
                budget.spawn(|| Ok(Bytes::new())).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
        }
        drop(release);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                match budget.spawn(|| Ok(Bytes::new())) {
                    Ok(read) => {
                        read.await.unwrap().unwrap();
                        break;
                    }
                    Err(error) => assert_eq!(error.kind(), io::ErrorKind::WouldBlock),
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    use http_body_util::BodyExt;
    use std::io::Write;

    #[tokio::test]
    async fn file_frames_bound_reads_and_prefix_does_not_read_ahead() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&vec![b'x'; 8192]).unwrap();
        let source = std::fs::File::open(file.path()).unwrap();
        let mut observer = source.try_clone().unwrap();
        let mut body = ServerBody::file(source, b"prefix".to_vec()).unwrap();
        body.configure(Limits {
            max_stream_chunk_bytes: 1024,
            ..Limits::default()
        })
        .unwrap();
        assert_eq!(observer.stream_position().unwrap(), 0);
        assert_eq!(
            body.frame().await.unwrap().unwrap().into_data().unwrap(),
            "prefix"
        );
        assert_eq!(observer.stream_position().unwrap(), 0);
        assert_eq!(
            body.frame()
                .await
                .unwrap()
                .unwrap()
                .into_data()
                .unwrap()
                .len(),
            1024
        );
        assert_eq!(observer.stream_position().unwrap(), 1024);
        while let Some(frame) = body.frame().await {
            assert!(frame.unwrap().into_data().unwrap().len() <= 1024);
        }
    }

    #[tokio::test]
    async fn file_growth_is_ignored_and_truncation_is_sticky_failure() {
        let file = tempfile::NamedTempFile::new().unwrap();
        file.as_file().set_len(4).unwrap();
        let mut body =
            ServerBody::file(std::fs::File::open(file.path()).unwrap(), Vec::new()).unwrap();
        file.as_file().set_len(8).unwrap();
        assert_eq!(
            body.frame()
                .await
                .unwrap()
                .unwrap()
                .into_data()
                .unwrap()
                .len(),
            4
        );
        assert!(body.frame().await.is_none());
        let mut body =
            ServerBody::file(std::fs::File::open(file.path()).unwrap(), Vec::new()).unwrap();
        file.as_file().set_len(0).unwrap();
        assert_eq!(
            body.frame().await.unwrap().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert!(body.frame().await.is_none());
        assert!(body.is_end_stream());
    }

    struct Events {
        items: std::collections::VecDeque<String>,
        polls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Stream for Events {
        type Item = io::Result<String>;
        fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            self.polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Poll::Ready(self.items.pop_front().map(Ok))
        }
    }

    #[tokio::test]
    async fn sse_chunks_do_not_prefetch_and_oversized_events_fail_stickily() {
        let polls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let events = Events {
            items: ["\r\nx".repeat(1000), "x".repeat(4001)].into(),
            polls: polls.clone(),
        };
        let mut body = ServerBody::events(events, Duration::from_secs(1)).unwrap();
        body.configure(Limits {
            max_stream_chunk_bytes: 1024,
            max_event_bytes: 4000,
            ..Limits::default()
        })
        .unwrap();
        assert_eq!(polls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let frame = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(frame.len(), 1024);
        let frame = body.frame().await.unwrap().unwrap().into_data().unwrap();
        assert_eq!(frame.len(), 1024);
        assert_eq!(polls.load(std::sync::atomic::Ordering::SeqCst), 1);
        loop {
            match body.frame().await.unwrap() {
                Ok(frame) => assert!(frame.into_data().unwrap().len() <= 1024),
                Err(error) => {
                    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
                    break;
                }
            }
        }
        assert!(body.frame().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn sse_keepalive_is_a_comment_and_carriage_returns_cannot_inject_fields() {
        struct Pending;
        impl Stream for Pending {
            type Item = io::Result<String>;
            fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
                Poll::Pending
            }
        }
        let mut body = ServerBody::events(Pending, Duration::from_secs(10)).unwrap();
        let start = tokio::time::Instant::now();
        assert_eq!(
            body.frame().await.unwrap().unwrap().into_data().unwrap(),
            ":\n\n"
        );
        assert_eq!(start.elapsed(), Duration::from_secs(10));
        let events = Events {
            items: ["a\rb\r\nc".to_string()].into(),
            polls: Default::default(),
        };
        let mut body = ServerBody::events(events, Duration::from_secs(10)).unwrap();
        assert_eq!(
            body.frame().await.unwrap().unwrap().into_data().unwrap(),
            "data: a\ndata: b\ndata: c\n\n"
        );
    }
}
