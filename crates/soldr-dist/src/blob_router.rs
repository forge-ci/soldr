//! Routes the blob streams of many concurrent joblets sharing one QUIC session.
//!
//! A client keeps one long-lived session per worker and runs every joblet on
//! its own bidirectional stream. Blobs (inputs one way, outputs the other) are
//! unidirectional streams on the same connection, so whoever calls
//! `accept_uni` would otherwise receive the next blob of *any* joblet. Each
//! joblet therefore owns a session-scoped `job_token`; every blob stream starts
//! with a `Blob` header naming it, and one task per connection hands each
//! stream to that joblet's inbox.
//!
//! Streams that arrive before their joblet registers (speculative uploads race
//! the worker reading the `Offer`) are parked: bounded in count, aged out, and
//! held under QUIC flow control because their bodies are not read. Streams for
//! a joblet that already finished, including one the worker declined before it
//! received its inputs, are stopped instead of parked forever.
use crate::{cas::Cas, proto::Blob, transport, Result};
use iroh::endpoint::{Connection, RecvStream};
use soldr_platform::sync::Mutex;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

/// Streams waiting for an unregistered joblet across the whole session.
const PARKED_LIMIT: usize = 1024;
/// A parked stream older than this belongs to a joblet that will never register.
const PARKED_TTL: Duration = Duration::from_secs(60);
/// Recently finished tokens whose late streams are refused, not parked.
const FINISHED_MEMORY: usize = 4096;
/// QUIC application code sent when a stream cannot be routed.
const UNROUTABLE: u32 = 2;

type Routed = (Blob, RecvStream);

#[derive(Default)]
struct Routes {
    inboxes: HashMap<u64, mpsc::UnboundedSender<Routed>>,
    parked: HashMap<u64, Vec<(Instant, Routed)>>,
    parked_count: usize,
    finished: HashSet<u64>,
    finished_order: VecDeque<u64>,
    closed: bool,
}

impl Routes {
    fn finish(&mut self, token: u64) {
        if self.finished.insert(token) {
            self.finished_order.push_back(token);
            while self.finished_order.len() > FINISHED_MEMORY {
                if let Some(old) = self.finished_order.pop_front() {
                    self.finished.remove(&old);
                }
            }
        }
    }

    fn expire_parked(&mut self, now: Instant) {
        let mut expired = 0;
        self.parked.retain(|token, streams| {
            let before = streams.len();
            streams.retain(|(at, _)| now.duration_since(*at) < PARKED_TTL);
            if streams.len() != before {
                eprintln!(
                    "soldr dist: dropped {} blob stream(s) for job token {token}: \
                     the joblet never registered within {}s",
                    before - streams.len(),
                    PARKED_TTL.as_secs()
                );
            }
            expired += before - streams.len();
            !streams.is_empty()
        });
        self.parked_count -= expired;
    }
}

/// Per-connection router. Cloning shares the same routing table.
#[derive(Clone, Default)]
pub struct BlobRouter {
    routes: Arc<Mutex<Routes>>,
}

impl BlobRouter {
    /// Starts routing `conn`'s unidirectional streams; the task ends with the connection.
    pub fn spawn(conn: Connection) -> Self {
        let router = Self::default();
        let task = router.clone();
        tokio::spawn(async move { task.run(conn).await });
        router
    }

    /// Claims `token`'s blob streams, including any that arrived early.
    pub fn register(&self, token: u64) -> Result<BlobInbox> {
        let mut routes = self.routes.lock();
        if routes.closed {
            return Err("dist session closed".into());
        }
        if routes.inboxes.contains_key(&token) || routes.finished.contains(&token) {
            return Err(format!("duplicate dist job token {token}").into());
        }
        let (send, receive) = mpsc::unbounded_channel();
        if let Some(early) = routes.parked.remove(&token) {
            routes.parked_count -= early.len();
            for (_, routed) in early {
                // The receiver is alive: it is returned below.
                let _ = send.send(routed);
            }
        }
        routes.inboxes.insert(token, send);
        Ok(BlobInbox {
            token,
            receive,
            router: self.clone(),
        })
    }

    async fn run(self, conn: Connection) {
        while let Ok(mut stream) = conn.accept_uni().await {
            // Read each header on its own task: one slow sender must not stall routing.
            let router = self.clone();
            tokio::spawn(async move {
                match transport::recv::<Blob>(&mut stream).await {
                    Ok(blob) => router.route(blob, stream),
                    Err(error) => {
                        eprintln!("soldr dist: unreadable blob stream header: {error}");
                        let _ = stream.stop(UNROUTABLE.into());
                    }
                }
            });
        }
        let mut routes = self.routes.lock();
        routes.closed = true;
        // Dropping the senders ends every inbox with "session closed".
        routes.inboxes.clear();
        routes.parked.clear();
        routes.parked_count = 0;
    }

    fn route(&self, blob: Blob, mut stream: RecvStream) {
        let token = blob.job_token;
        let mut routes = self.routes.lock();
        if routes.closed || routes.finished.contains(&token) {
            let _ = stream.stop(UNROUTABLE.into());
            return;
        }
        if let Some(inbox) = routes.inboxes.get(&token) {
            if let Err(mpsc::error::SendError((_, mut stream))) = inbox.send((blob, stream)) {
                let _ = stream.stop(UNROUTABLE.into());
            }
            return;
        }
        let now = Instant::now();
        routes.expire_parked(now);
        if routes.parked_count >= PARKED_LIMIT {
            eprintln!(
                "\x1b[31msoldr dist: refused a blob stream for job token {token}: \
                 {PARKED_LIMIT} streams already wait for unregistered joblets\x1b[0m"
            );
            let _ = stream.stop(UNROUTABLE.into());
            return;
        }
        routes.parked_count += 1;
        routes
            .parked
            .entry(token)
            .or_default()
            .push((now, (blob, stream)));
    }

    fn release(&self, token: u64) {
        let mut routes = self.routes.lock();
        routes.inboxes.remove(&token);
        if let Some(streams) = routes.parked.remove(&token) {
            routes.parked_count -= streams.len();
            for (_, (_, mut stream)) in streams {
                let _ = stream.stop(UNROUTABLE.into());
            }
        }
        routes.finish(token);
    }

    #[cfg(test)]
    fn parked(&self) -> usize {
        self.routes.lock().parked_count
    }
}

/// One joblet's view of its session's blob streams. Dropping it finishes the token.
pub struct BlobInbox {
    token: u64,
    receive: mpsc::UnboundedReceiver<Routed>,
    router: BlobRouter,
}

impl BlobInbox {
    pub fn token(&self) -> u64 {
        self.token
    }

    /// Receives the next blob into `cas`, accepting only digests in `expected`.
    /// Returns the digest and its size.
    pub async fn receive(&mut self, cas: &Cas, expected: &[(String, u64)]) -> Result<(String, u64)> {
        let (blob, mut stream) = self
            .receive
            .recv()
            .await
            .ok_or("dist session closed while a joblet awaited a blob")?;
        if blob.size > transport::BLOB_LIMIT
            || !expected
                .iter()
                .any(|(d, s)| d == &blob.digest && *s == blob.size)
        {
            let _ = stream.stop(UNROUTABLE.into());
            return Err("unsolicited blob or incorrect size".into());
        }
        cas.receive_stream(&blob.digest, blob.size, &mut stream)
            .await?;
        Ok((blob.digest, blob.size))
    }
}

impl Drop for BlobInbox {
    fn drop(&mut self) {
        self.router.release(self.token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_cannot_be_reused_after_their_joblet_finishes() {
        let router = BlobRouter::default();
        drop(router.register(7).unwrap());
        assert!(router.register(7).is_err());
        let live = router.register(8).unwrap();
        assert!(router.register(8).is_err());
        assert_eq!(live.token(), 8);
    }

    #[test]
    fn finished_memory_is_bounded() {
        let mut routes = Routes::default();
        for token in 0..(FINISHED_MEMORY as u64 + 10) {
            routes.finish(token);
        }
        assert_eq!(routes.finished.len(), FINISHED_MEMORY);
        assert!(!routes.finished.contains(&0));
        assert!(routes.finished.contains(&(FINISHED_MEMORY as u64 + 9)));
    }

    #[test]
    fn a_closed_session_refuses_new_joblets() {
        let router = BlobRouter::default();
        router.routes.lock().closed = true;
        assert!(router.register(1).is_err());
        assert_eq!(router.parked(), 0);
    }
}
