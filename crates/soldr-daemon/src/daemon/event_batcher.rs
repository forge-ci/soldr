//! L4 (issue soldr#980): coalesce per-compile state-DB transactions into a
//! background flusher.
//!
//! ## Problem
//!
//! Pre-L4, every `Request::RecordCompile` IPC call landed in
//! [`crate::daemon::db::append_event`], which opens the shared
//! `state.sqlite3`, runs a write txn to bump `next_event_id` (fsync #1) and
//! a second write txn to insert the event row (fsync #2). The cold
//! profile recorded 171 compile misses ≈ ~342 fsyncs ≈ ~3.4 s of
//! pure-sync wait on Windows/WSL2 at ~10 ms/fsync. None of those events
//! is critical-path for the build itself; they are diagnostics consumed
//! by `soldr daemon status` and `soldr daemon ls-builds` after the build
//! finishes.
//!
//! ## Design
//!
//! [`EventBatcher::start`] spawns one background tokio task that owns the
//! SQLite writer. The hot path ([`EventBatcher::record`]) just pushes a
//! [`crate::daemon::db::Event`] onto a bounded mpsc channel — a few
//! microseconds at most. The drain task batches up to
//! [`MAX_BATCH_ROWS`] rows OR up to [`MAX_BATCH_LATENCY`] of wall time
//! before opening a single SQLite write txn that allocates a contiguous
//! range of event IDs and inserts every staged row in one fsync.
//!
//! On `BuildSessionEnd` and on daemon shutdown the server sends an
//! explicit [`BatcherCmd::Flush`] with a oneshot acknowledgement so the
//! caller can confirm every staged row landed before the session is
//! treated as finalized.
//!
//! soldr#1536: the batcher additionally maintains an in-memory
//! [`SessionAggregate`] per build session, updated synchronously in
//! [`EventBatcher::record`]. `BuildSessionEnd` consumes it via
//! [`EventBatcher::take_session_aggregate`] so finalization is
//! proportional to the current session instead of scanning (and
//! prost-decoding) the entire retained `daemon_events` table; the
//! historical `aggregate_session` scan remains only as the fallback for
//! sessions this daemon process did not observe from their start.
//!
//! ## Race window
//!
//! A reader querying the state DB between two flushes (e.g. a `soldr daemon
//! status` racing with an in-progress build) sees a snapshot that does
//! NOT include rows still buffered in memory. This is acceptable for the
//! per-compile diagnostic path — at worst the operator sees one or two
//! fewer events than the daemon will eventually persist; the next
//! interval tick / batch-fullness trip will flush the rest. Shutdown and
//! `BuildSessionEnd` both force a flush before they complete.
//!
//! ## Unwritable state DB (soldr#3688)
//!
//! A failed write keeps its rows for retry, but at most
//! [`MAX_RETAINED_ROWS`]; beyond that the oldest rows are dropped and
//! counted ([`EventBatcher::dropped_events`], logged at `warn`).
//! Opportunistic flushes (batch-full / heartbeat) back off exponentially
//! from [`INITIAL_RETRY_BACKOFF`] to [`MAX_RETRY_BACKOFF`] after a failure;
//! explicit `Flush` / `Shutdown` always try.

use crate::cache_lib::state_store::open_state_db;
use crate::daemon::db::{Event, EventKind};
use crate::daemon::wire::{self, prost_tagged_bytes};
use crate::platform::sync::Mutex;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

/// Mirrors [`crate::daemon::db`]'s meta key. Kept private here so the
/// batcher can allocate a contiguous ID range in one write instead of
/// re-allocating IDs one at a time through `daemon::db` (which would
/// defeat the entire point of L4).
const META_NEXT_EVENT_ID: &str = "next_event_id";

/// Flush whenever the staged batch reaches this many rows. Picked to
/// keep one write txn small enough to commit in a single fsync without
/// stalling the daemon's IPC accept loop.
const MAX_BATCH_ROWS: usize = 64;

/// Flush at least this often, even when fewer than [`MAX_BATCH_ROWS`]
/// rows have accumulated. Keeps `soldr daemon status` snapshots fresh
/// during slow builds.
const MAX_BATCH_LATENCY: Duration = Duration::from_millis(100);

/// Bounded backpressure on the producer side. 4096 in-flight events is
/// far more than any realistic build session generates between flushes;
/// the channel acts mostly as a queue, not a buffer.
const CHANNEL_CAPACITY: usize = 4096;

/// Upper bound on concurrently tracked per-session aggregates
/// (soldr#1536). Realistic hosts run a handful of parallel builds; the
/// cap only exists so sessions that never receive a `BuildSessionEnd`
/// (wrapper killed mid-build) cannot grow the map without bound. On
/// overflow the entry with the oldest last-observed timestamp is
/// evicted; its finalization then falls back to the state-DB scan.
const MAX_TRACKED_SESSIONS: usize = 128;

/// Upper bound on rows retained in memory while the state DB refuses
/// writes (soldr#3688). Past this the oldest rows are dropped and counted
/// in [`EventBatcher::dropped_events`]; without it the daemon grew for as
/// long as the store stayed unwritable.
const MAX_RETAINED_ROWS: usize = 16 * MAX_BATCH_ROWS;

/// First retry delay after a failed opportunistic flush; doubles on each
/// further failure up to [`MAX_RETRY_BACKOFF`]. Explicit `Flush` /
/// `Shutdown` commands always attempt a write regardless of backoff.
const INITIAL_RETRY_BACKOFF: Duration = Duration::from_millis(250);
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(30);

/// Counters shared between the drain task and every handle (soldr#3688).
#[derive(Debug, Default)]
struct BatcherStats {
    /// Rows currently staged in the drain task's buffer.
    retained: AtomicUsize,
    /// Rows discarded because the retained buffer hit its cap (or a flush
    /// task panicked) since the batcher started.
    dropped: AtomicU64,
}

/// Daemon-owned per-session rollup (soldr#1536). Mirrors the semantics
/// of [`crate::daemon::db::aggregate_session`] — crate count is the
/// number of `CompileEnd` events (falling back to `CompileStart` when
/// no compile completed) and the slowest crate is tracked across every
/// event that carries a `duration_us` — but is maintained incrementally
/// as events flow through [`EventBatcher::record`], so build-session
/// finalization no longer has to decode the entire retained event
/// table.
#[derive(Debug, Clone, Default)]
pub struct SessionAggregate {
    start_count: u32,
    end_count: u32,
    slowest_us: Option<u64>,
    slowest_name: Option<String>,
    /// True only when the aggregate observed the session from its
    /// `SessionStart` event. A session first seen via a compile event
    /// was started before this daemon process existed (daemon restart /
    /// late auto-start mid-build), so earlier events may already sit in
    /// the state DB — the aggregate is incomplete and finalization must fall
    /// back to the historical scan.
    complete: bool,
    /// Timestamp of the most recent observed event; used only for
    /// oldest-first eviction when [`MAX_TRACKED_SESSIONS`] overflows.
    last_seen_ms: i64,
}

impl SessionAggregate {
    fn observe(&mut self, event: &Event) {
        self.last_seen_ms = self.last_seen_ms.max(event.ts_ms);
        match event.kind {
            EventKind::CompileStart => self.start_count = self.start_count.saturating_add(1),
            EventKind::CompileEnd => self.end_count = self.end_count.saturating_add(1),
            EventKind::SessionStart | EventKind::SessionEnd => {}
        }
        if let Some(d) = event.duration_us {
            if d > self.slowest_us.unwrap_or(0) {
                self.slowest_us = Some(d);
                self.slowest_name = event.crate_name.clone();
            }
        }
    }

    /// Collapse into the `(crate_count, slowest_us, slowest_name)` tuple
    /// [`crate::daemon::db::aggregate_session`] returns.
    pub fn finalize(self) -> (u32, Option<u64>, Option<String>) {
        let count = if self.end_count > 0 {
            self.end_count
        } else {
            self.start_count
        };
        (count, self.slowest_us, self.slowest_name)
    }
}

type SessionAggregates = Arc<Mutex<HashMap<u64, SessionAggregate>>>;

/// The synchronous persistence operation owned by the drain task. Keeping it
/// injectable lets the retention contract be tested without manufacturing a
/// filesystem or SQLite failure whose lock lifetime belongs to the OS.
type BatchWriter = Arc<dyn Fn(&Path, &[Event]) -> std::io::Result<()> + Send + Sync>;

/// Commands accepted by the drain task.
enum BatcherCmd {
    /// Stage a new event for the next batch.
    Insert(Event),
    /// Flush whatever is buffered NOW; reply via the oneshot after the
    /// commit succeeds (or after the failed write is logged).
    Flush(oneshot::Sender<Result<(), String>>),
    /// Drain and exit. Reply via the oneshot to acknowledge.
    Shutdown(oneshot::Sender<Result<(), String>>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventBatcherError(pub String);

impl std::fmt::Display for EventBatcherError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EventBatcherError {}

/// Cheap-to-clone handle. Each request handler in
/// [`crate::daemon::server`] holds one; the inner `mpsc::Sender` is
/// itself a clone-friendly handle so the wrapper just forwards.
#[derive(Clone)]
pub struct EventBatcher {
    tx: mpsc::Sender<BatcherCmd>,
    /// soldr#1536: incrementally-maintained per-session rollups, updated
    /// synchronously in [`record`](Self::record) before the event is
    /// staged for persistence. `Request::BuildSessionEnd` consumes an
    /// entry via [`take_session_aggregate`](Self::take_session_aggregate)
    /// instead of scanning the whole `daemon_events` table.
    aggregates: SessionAggregates,
    stats: Arc<BatcherStats>,
}

impl EventBatcher {
    /// Start the background drain task. The returned handle can be
    /// freely cloned across request handlers and outlives every
    /// in-flight IPC call; the task itself exits cleanly when
    /// [`shutdown`] is called or when the last [`EventBatcher`] is
    /// dropped (the receiver returns `None`, which the loop treats as a
    /// final flush + exit).
    pub fn start(db_path: PathBuf) -> Self {
        Self::start_with_batch_writer(db_path, Arc::new(write_batch))
    }

    /// Start the drain task with its persistence operation. Production always
    /// uses [`write_batch`]; tests use a deterministic one-shot failure to
    /// prove that the same staged batch is retained for a later retry.
    fn start_with_batch_writer(db_path: PathBuf, writer: BatchWriter) -> Self {
        let (tx, rx) = mpsc::channel::<BatcherCmd>(CHANNEL_CAPACITY);
        let stats = Arc::new(BatcherStats::default());
        tokio::spawn(drain_loop(db_path, rx, writer, Arc::clone(&stats)));
        Self {
            tx,
            aggregates: Arc::new(Mutex::new(HashMap::new())),
            stats,
        }
    }

    /// Total rows dropped since start because the state DB stayed
    /// unwritable long enough to overflow [`MAX_RETAINED_ROWS`].
    pub fn dropped_events(&self) -> u64 {
        self.stats.dropped.load(Ordering::Relaxed)
    }

    /// Rows currently staged in memory awaiting a successful write.
    pub fn retained_events(&self) -> usize {
        self.stats.retained.load(Ordering::Relaxed)
    }

    /// Hot path. Non-blocking on the common case (channel has free
    /// capacity); the await is microseconds. On a full channel the send
    /// awaits until the drain task makes room, which is the desired
    /// backpressure shape — we never want to silently drop diagnostic
    /// events.
    pub async fn record(&self, event: Event) -> Result<(), EventBatcherError> {
        self.observe_for_aggregate(&event);
        if self.tx.send(BatcherCmd::Insert(event)).await.is_err() {
            return Err(EventBatcherError("event batcher drain task stopped".into()));
        }
        Ok(())
    }

    /// Update the in-memory per-session rollup (soldr#1536). Runs on the
    /// caller's task before the event is staged so the aggregate is
    /// never behind the persistence channel.
    fn observe_for_aggregate(&self, event: &Event) {
        let Some(session_id) = event.session_id else {
            return;
        };
        let mut map = self.aggregates.lock();
        match event.kind {
            EventKind::SessionStart => {
                // Session observed from birth: the aggregate is
                // authoritative for the whole session.
                let entry = map.entry(session_id).or_default();
                entry.complete = true;
                entry.observe(event);
            }
            EventKind::CompileStart | EventKind::CompileEnd => {
                // First sight mid-session (daemon auto-started or
                // restarted mid-build): track it, but leave `complete`
                // false so finalization falls back to the state-DB scan.
                map.entry(session_id).or_default().observe(event);
            }
            // The terminator is recorded by the finalizer AFTER the
            // aggregate was consumed — never resurrect an entry for it.
            EventKind::SessionEnd => return,
        }
        if map.len() > MAX_TRACKED_SESSIONS {
            if let Some(oldest) = map
                .iter()
                .min_by_key(|(_, agg)| agg.last_seen_ms)
                .map(|(id, _)| *id)
            {
                map.remove(&oldest);
            }
        }
    }

    /// Consume the per-session rollup for `session_id` (soldr#1536).
    /// Returns `None` when the daemon did not observe the session from
    /// its `SessionStart` (restart / late auto-start mid-build) or never
    /// saw it at all — callers must then fall back to
    /// [`crate::daemon::db::aggregate_session`]. The entry is removed
    /// either way; the session is over.
    pub fn take_session_aggregate(&self, session_id: u64) -> Option<SessionAggregate> {
        let mut map = self.aggregates.lock();
        map.remove(&session_id).filter(|agg| agg.complete)
    }

    /// Put a consumed aggregate back when finalization could not be persisted.
    /// This keeps a later retry lossless.
    pub fn restore_session_aggregate(&self, session_id: u64, aggregate: SessionAggregate) {
        let mut map = self.aggregates.lock();
        map.insert(session_id, aggregate);
    }

    /// Force a flush and wait for the drain task to acknowledge. Called
    /// from `Request::BuildSessionEnd` before the session-level
    /// aggregation runs, so the aggregator sees every event recorded
    /// during the session.
    pub async fn flush(&self) -> Result<(), EventBatcherError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self.tx.send(BatcherCmd::Flush(ack_tx)).await.is_err() {
            return Err(EventBatcherError("event batcher drain task stopped".into()));
        }
        ack_rx
            .await
            .map_err(|_| EventBatcherError("event batcher dropped flush acknowledgement".into()))?
            .map_err(EventBatcherError)
    }

    /// Final flush + drain task exit. Called from the daemon shutdown
    /// path before the state DB is released.
    pub async fn shutdown(&self) -> Result<(), EventBatcherError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self.tx.send(BatcherCmd::Shutdown(ack_tx)).await.is_err() {
            return Err(EventBatcherError("event batcher drain task stopped".into()));
        }
        ack_rx
            .await
            .map_err(|_| {
                EventBatcherError("event batcher dropped shutdown acknowledgement".into())
            })?
            .map_err(EventBatcherError)
    }
}

/// Retry pacing for opportunistic flushes after a failure (soldr#3688).
struct RetryBackoff {
    delay: Duration,
    next_attempt: Option<Instant>,
}

impl RetryBackoff {
    fn new() -> Self {
        Self {
            delay: INITIAL_RETRY_BACKOFF,
            next_attempt: None,
        }
    }

    fn ready(&self) -> bool {
        self.next_attempt.is_none_or(|at| Instant::now() >= at)
    }

    fn record(&mut self, result: &Result<(), String>) {
        if result.is_ok() {
            *self = Self::new();
        } else {
            self.next_attempt = Some(Instant::now() + self.delay);
            self.delay = (self.delay * 2).min(MAX_RETRY_BACKOFF);
        }
    }
}

/// Drop the oldest staged rows beyond [`MAX_RETAINED_ROWS`], counting them.
fn enforce_retention_cap(buf: &mut Vec<Event>, stats: &BatcherStats) {
    if buf.len() <= MAX_RETAINED_ROWS {
        return;
    }
    let excess = buf.len() - MAX_RETAINED_ROWS;
    buf.drain(..excess);
    let total = stats.dropped.fetch_add(excess as u64, Ordering::Relaxed) + excess as u64;
    // Log once per batch-sized chunk of drops, not per row.
    if total.is_multiple_of(MAX_BATCH_ROWS as u64) || excess > 1 {
        tracing::warn!(
            "event_batcher: state DB unwritable; dropped {total} event rows so far \
             (retaining newest {MAX_RETAINED_ROWS})"
        );
    }
}

/// Drain-loop entry point. Owns the in-memory staging buffer and the
/// 100 ms heartbeat that flushes a partial batch.
async fn drain_loop(
    db_path: PathBuf,
    mut rx: mpsc::Receiver<BatcherCmd>,
    writer: BatchWriter,
    stats: Arc<BatcherStats>,
) {
    let mut buf: Vec<Event> = Vec::with_capacity(MAX_BATCH_ROWS);
    let mut backoff = RetryBackoff::new();
    let mut interval = tokio::time::interval(MAX_BATCH_LATENCY);
    // Skip the first tick — `interval` fires immediately on the first
    // poll otherwise, which would flush an empty buffer for no reason.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    interval.tick().await;
    loop {
        tokio::select! {
            cmd = rx.recv() => match cmd {
                Some(BatcherCmd::Insert(event)) => {
                    buf.push(event);
                    enforce_retention_cap(&mut buf, &stats);
                    if buf.len() >= MAX_BATCH_ROWS && backoff.ready() {
                        let result = flush_batch(&db_path, &mut buf, &writer, &stats).await;
                        backoff.record(&result);
                    }
                }
                Some(BatcherCmd::Flush(ack)) => {
                    let result = flush_batch(&db_path, &mut buf, &writer, &stats).await;
                    backoff.record(&result);
                    stats.retained.store(buf.len(), Ordering::Relaxed);
                    let _ = ack.send(result);
                }
                Some(BatcherCmd::Shutdown(ack)) => {
                    let result = flush_batch(&db_path, &mut buf, &writer, &stats).await;
                    let done = result.is_ok();
                    backoff.record(&result);
                    let _ = ack.send(result);
                    if done { return; }
                }
                None => {
                    // Last sender dropped. Drain whatever is left and
                    // exit — no one is around to receive an ack.
                    let _ = flush_batch(&db_path, &mut buf, &writer, &stats).await;
                    return;
                }
            },
            _ = interval.tick() => {
                if !buf.is_empty() && backoff.ready() {
                    let result = flush_batch(&db_path, &mut buf, &writer, &stats).await;
                    backoff.record(&result);
                }
            }
        }
        stats.retained.store(buf.len(), Ordering::Relaxed);
    }
}

/// One SQLite write txn that allocates a contiguous range of event IDs
/// and inserts every staged row. Empty buffer is a no-op. On a write
/// error the rows are put back for a later retry and the error is logged
/// at `error` and returned; the caller paces retries and
/// [`enforce_retention_cap`] bounds what is kept (soldr#3688).
async fn flush_batch(
    db_path: &Path,
    buf: &mut Vec<Event>,
    writer: &BatchWriter,
    stats: &BatcherStats,
) -> Result<(), String> {
    if buf.is_empty() {
        return Ok(());
    }
    let count = buf.len();
    tracing::debug!("event_batcher: flushing {count} pending event rows");

    // The write is synchronous and genuinely slow: `open_state_db` takes a
    // process-wide mutex and can wait on SQLite's busy timeout, then the txn
    // ends in a `commit()` fsync. Running that inline on the runtime blocked
    // a Tokio worker for its whole duration, delaying compile IPC,
    // cancellation and shutdown on that thread (#1669).
    //
    // The batch is moved into the blocking task and handed back either way,
    // so the retain-on-failure contract below is preserved exactly. `buf`
    // cannot grow while we await: `drain_loop` holds `&mut buf` across this
    // call, so it is not processing new commands — inbound events queue in
    // the bounded channel, which is the same backpressure the synchronous
    // version applied.
    let batch = std::mem::take(buf);
    let path = db_path.to_path_buf();
    let writer = Arc::clone(writer);
    let joined = tokio::task::spawn_blocking(move || {
        let result = writer(&path, &batch);
        (result, batch)
    })
    .await;

    match joined {
        Ok((Ok(()), _consumed)) => Ok(()),
        Ok((Err(err), batch)) => {
            // Put the rows back so the next flush retries them.
            *buf = batch;
            tracing::error!(
                "event_batcher: flush failed; retaining {count} pending event rows: {err}"
            );
            Err(err.to_string())
        }
        Err(join) => {
            // A panic in the blocking task loses that batch — these are
            // best-effort diagnostic rows, and a panic in `write_batch`
            // would be a bug worth surfacing rather than silently retrying.
            stats.dropped.fetch_add(count as u64, Ordering::Relaxed);
            tracing::error!("event_batcher: flush task failed; dropped {count} rows: {join}");
            Err(format!("event flush task failed: {join}"))
        }
    }
}

/// Open the state store, allocate `count` consecutive event IDs in a
/// single `daemon_meta` write, and insert every row in the same
/// transaction. One commit covers the entire batch.
///
/// `pub(crate)` so tests (soldr#1536 finalization-scaling guards) can
/// seed large event histories in a single transaction.
pub(crate) fn write_batch(db_path: &Path, buf: &[Event]) -> std::io::Result<()> {
    use rusqlite::{params, OptionalExtension};

    // soldr#3290: carry the SQLite extended code, so a `database is locked`
    // here says whether it was an instant snapshot conflict (517) or a
    // genuine timed-out wait (5). soldr#3288 could not tell them apart.
    let sql_io = |e: rusqlite::Error| {
        std::io::Error::other(format!(
            "sqlite: {}",
            crate::cache_lib::state_store::describe_sqlite_error(&e)
        ))
    };
    let handle = open_state_db(db_path)?;
    let tx = handle.unchecked_transaction().map_err(sql_io)?;
    // Allocate `count` consecutive IDs in one daemon_meta write.
    let current: i64 = tx
        .query_row(
            "SELECT value FROM daemon_meta WHERE key = ?1",
            params![META_NEXT_EVENT_ID],
            |row| row.get(0),
        )
        .optional()
        .map_err(sql_io)?
        .unwrap_or(1);
    let start_id = current as u64;
    let next = start_id.saturating_add(buf.len() as u64);
    tx.execute(
        "INSERT INTO daemon_meta(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![META_NEXT_EVENT_ID, next as i64],
    )
    .map_err(sql_io)?;
    for (i, event) in buf.iter().enumerate() {
        let id = start_id.saturating_add(i as u64);
        let bytes = prost_tagged_bytes(&wire::event_to_wire(event));
        tx.execute(
            "INSERT INTO daemon_events(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![id as i64, bytes],
        )
        .map_err(sql_io)?;
    }
    tx.commit().map_err(sql_io)?;
    Ok(())
}

#[cfg(test)]
#[allow(unused_must_use)]
mod tests {
    use super::*;
    use crate::daemon::db::{aggregate_session, list_builds, EventKind};
    use tempfile::TempDir;
    use tokio::runtime::Builder;

    fn sample_event(session: u64, krate: &str, dur_us: Option<u64>) -> Event {
        Event {
            ts_ms: 1_700_000_000_000,
            session_id: Some(session),
            kind: if dur_us.is_some() {
                EventKind::CompileEnd
            } else {
                EventKind::CompileStart
            },
            crate_name: Some(krate.into()),
            duration_us: dur_us,
            target_dir: Some("/t".into()),
            exit_code: None,
        }
    }

    // #1669 moved the write into `spawn_blocking`, so the batch is now moved
    // out of the caller's buffer and handed back. The failure half of that
    // contract is already covered by `failed_flush_retains_rows_for_retry`
    // above (verified: it fails if the restore is removed); this pins the
    // success half.
    #[test]
    fn successful_flush_clears_the_buffer() {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let dir = TempDir::new().expect("tempdir");
            let path = dir.path().join("state.sqlite3");
            crate::daemon::db::ensure_initialized(&path).expect("init");

            let mut buf = vec![
                sample_event(9, "one", None),
                sample_event(9, "two", Some(1_000)),
            ];
            let writer: BatchWriter = Arc::new(write_batch);
            flush_batch(&path, &mut buf, &writer, &BatcherStats::default())
                .await
                .expect("flush");

            // Persistence itself is covered by
            // `batcher_persists_rows_after_flush`; what matters here is that
            // the buffer handed to `spawn_blocking` is not handed back on the
            // success path.
            assert!(buf.is_empty(), "a successful flush must drain the buffer");
        });
    }

    #[test]
    fn batcher_persists_rows_after_flush() {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let dir = TempDir::new().expect("tempdir");
            let path = dir.path().join("state.sqlite3");
            crate::daemon::db::ensure_initialized(&path).expect("init");
            let batcher = EventBatcher::start(path.clone());
            for i in 0..5 {
                batcher
                    .record(sample_event(99, &format!("c{i}"), Some(1000 + i)))
                    .await;
            }
            batcher.flush().await;
            let (count, slowest, name) = aggregate_session(&path, 99).expect("agg");
            assert_eq!(count, 5);
            assert_eq!(slowest, Some(1004));
            assert_eq!(name.as_deref(), Some("c4"));
            batcher.shutdown().await;
            // Build list table is untouched by the batcher.
            assert!(list_builds(&path, 10, None).expect("list").is_empty());
        });
    }

    fn session_start_event(session: u64) -> Event {
        Event {
            ts_ms: 1_700_000_000_000,
            session_id: Some(session),
            kind: EventKind::SessionStart,
            crate_name: None,
            duration_us: None,
            target_dir: None,
            exit_code: None,
        }
    }

    #[test]
    fn session_aggregate_matches_db_aggregate_and_is_consumed_once() {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let dir = TempDir::new().expect("tempdir");
            let path = dir.path().join("state.sqlite3");
            crate::daemon::db::ensure_initialized(&path).expect("init");
            let batcher = EventBatcher::start(path.clone());
            batcher.record(session_start_event(21)).await;
            for i in 0..4 {
                batcher
                    .record(sample_event(21, &format!("s{i}"), None))
                    .await;
                batcher
                    .record(sample_event(21, &format!("s{i}"), Some(500 + i)))
                    .await;
            }
            batcher.flush().await;

            let agg = batcher
                .take_session_aggregate(21)
                .expect("aggregate tracked from SessionStart");
            let in_memory = agg.finalize();
            let from_db = aggregate_session(&path, 21).expect("db agg");
            assert_eq!(
                in_memory, from_db,
                "incremental rollup must mirror the scan"
            );
            assert_eq!(in_memory.0, 4);
            assert_eq!(in_memory.1, Some(503));
            assert_eq!(in_memory.2.as_deref(), Some("s3"));

            // Consumed: a second take falls back to the scan path.
            assert!(batcher.take_session_aggregate(21).is_none());
            batcher.shutdown().await;
        });
    }

    #[test]
    fn session_aggregate_without_session_start_is_untrusted() {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let dir = TempDir::new().expect("tempdir");
            let path = dir.path().join("state.sqlite3");
            crate::daemon::db::ensure_initialized(&path).expect("init");
            let batcher = EventBatcher::start(path.clone());
            // Daemon (re)started mid-build: compiles arrive without a
            // SessionStart. Earlier events may already be in the state DB, so
            // the incremental rollup must NOT claim authority.
            batcher.record(sample_event(33, "late", Some(9))).await;
            batcher.flush().await;
            assert!(
                batcher.take_session_aggregate(33).is_none(),
                "mid-session aggregates must force the state-DB-scan fallback"
            );
            batcher.shutdown().await;
        });
    }

    #[test]
    fn session_end_event_does_not_resurrect_aggregate() {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let dir = TempDir::new().expect("tempdir");
            let path = dir.path().join("state.sqlite3");
            crate::daemon::db::ensure_initialized(&path).expect("init");
            let batcher = EventBatcher::start(path.clone());
            batcher.record(session_start_event(55)).await;
            assert!(batcher.take_session_aggregate(55).is_some());
            // The finalizer records the SessionEnd terminator after the
            // take; it must not re-create a (now-stale) entry.
            batcher
                .record(Event {
                    ts_ms: 1_700_000_000_500,
                    session_id: Some(55),
                    kind: EventKind::SessionEnd,
                    crate_name: None,
                    duration_us: None,
                    target_dir: None,
                    exit_code: Some(0),
                })
                .await;
            assert!(batcher.take_session_aggregate(55).is_none());
            batcher.shutdown().await;
        });
    }

    #[test]
    fn batcher_batches_at_capacity_boundary() {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let dir = TempDir::new().expect("tempdir");
            let path = dir.path().join("state.sqlite3");
            crate::daemon::db::ensure_initialized(&path).expect("init");
            let batcher = EventBatcher::start(path.clone());
            // Push more than MAX_BATCH_ROWS to force an auto-flush before
            // we explicitly call flush().
            for i in 0..(MAX_BATCH_ROWS + 7) {
                batcher
                    .record(sample_event(7, &format!("c{i}"), Some(i as u64)))
                    .await;
            }
            batcher.flush().await;
            let (count, _, _) = aggregate_session(&path, 7).expect("agg");
            assert_eq!(count as usize, MAX_BATCH_ROWS + 7);
            batcher.shutdown().await;
        });
    }

    #[test]
    fn failed_flush_retains_rows_for_retry() {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let dir = TempDir::new().expect("tempdir");
            let path = dir.path().join("state.sqlite3");
            crate::daemon::db::ensure_initialized(&path).expect("init");

            // soldr#2714: the subject is retention after a failed write, not
            // SQLite's OS-owned file-lock lifetime. Fail the first writer call
            // before it opens the database, then let the same batch retry
            // through the production writer. This has no sleep, retry budget,
            // delete/recreate, corruption, or reopen race on Windows.
            let fail_once = Arc::new(std::sync::atomic::AtomicBool::new(true));
            let writer: BatchWriter = {
                let fail_once = Arc::clone(&fail_once);
                Arc::new(move |path, batch| {
                    if fail_once.swap(false, std::sync::atomic::Ordering::SeqCst) {
                        return Err(std::io::Error::other("injected first-write failure"));
                    }
                    write_batch(path, batch)
                })
            };
            let batcher = EventBatcher::start_with_batch_writer(path.clone(), writer);
            batcher
                .record(sample_event(101, "retained", Some(7)))
                .await
                .expect("queue event");
            assert!(
                batcher.flush().await.is_err(),
                "the injected first-write failure must fail the flush"
            );
            batcher.flush().await.expect("retry flush");
            let (count, _, _) = aggregate_session(&path, 101).expect("aggregate");
            assert_eq!(count, 1, "failed batch must be retried, not dropped");
            assert!(
                !fail_once.load(std::sync::atomic::Ordering::SeqCst),
                "the deterministic first-write seam must have been consumed"
            );
            batcher.shutdown().await.expect("shutdown");
        });
    }

    // soldr#3688: a permanently unwritable state DB must neither grow the
    // buffer without bound nor retry the write on every insert.
    #[test]
    fn unwritable_state_db_keeps_buffer_bounded_and_backs_off() {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let dir = TempDir::new().expect("tempdir");
            let path = dir.path().join("state.sqlite3");
            let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let writer: BatchWriter = {
                let calls = Arc::clone(&calls);
                Arc::new(move |_path, _batch| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Err(std::io::Error::other("state DB unwritable"))
                })
            };
            let batcher = EventBatcher::start_with_batch_writer(path, writer);
            const PUSHED: usize = 10_000;
            for i in 0..PUSHED {
                batcher
                    .record(sample_event(5, "c", Some(i as u64)))
                    .await
                    .expect("queue event");
            }
            assert!(batcher.flush().await.is_err(), "writes keep failing");
            let retained = batcher.retained_events();
            assert!(
                retained <= MAX_RETAINED_ROWS,
                "retained {retained} rows, cap is {MAX_RETAINED_ROWS}"
            );
            assert_eq!(
                batcher.dropped_events() as usize + retained,
                PUSHED,
                "every row is either retained or counted as dropped"
            );
            let attempts = calls.load(Ordering::SeqCst);
            assert!(
                attempts < 20,
                "{attempts} write attempts for {PUSHED} inserts: retries must back off"
            );
        });
    }
}
