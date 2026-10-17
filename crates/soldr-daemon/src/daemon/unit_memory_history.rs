//! Per-unit measured peak memory, remembered across builds (soldr#3152 step 4).
//!
//! The owner's decision for memory-aware admission: remember the last measured
//! peak of each compile unit and admit on it, instead of predicting peak memory
//! from command-line features. The first 1,161 joined CI units showed those
//! features barely predict it (R\u{b2} 0.09 on `--extern` bytes), while a unit's
//! measured peak is stable from run to run.
//!
//! Units are keyed by `memory_estimate::unit_key` (`<crate name>/<cargo -C
//! metadata>`), the identity that survives zccache rewriting the argument
//! vector. The measurement comes from zccache's per-compile `ChildMemory`:
//! the compiler's own high-water mark and the sampled peak of its whole live
//! process tree, which is the one that includes a linking `rustc`'s `ld`.
//!
//! Rows live in the shared `state.sqlite3` table `unit_memory_history` as
//! `[0x01][prost WireUnitMemory]`, like every other persisted row.

use crate::platform::sync::Condvar;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension};
use tokio::sync::{mpsc, oneshot, watch};

use crate::cache_lib::target_registry::RegistryError;
use crate::core::wire::{prost_tagged_bytes, proto::WireUnitMemory, REDB_TAG_PROST};
use crate::platform::sync::Mutex;
use prost::Message;

/// One unit's last measured peak memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnitMemory {
    pub peak_rss_bytes: u64,
    pub tree_peak_rss_bytes: u64,
    pub updated_ms: i64,
    /// How many measurements have replaced this row.
    pub samples: u64,
}

fn invalid_data(message: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> RegistryError {
    RegistryError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message,
    ))
}

fn decode(bytes: &[u8]) -> Result<UnitMemory, RegistryError> {
    let Some((&REDB_TAG_PROST, body)) = bytes.split_first() else {
        return Err(invalid_data(
            "unit_memory_history row is not a tagged prost row",
        ));
    };
    let wire = WireUnitMemory::decode(body).map_err(invalid_data)?;
    Ok(UnitMemory {
        peak_rss_bytes: wire.peak_rss_bytes,
        tree_peak_rss_bytes: wire.tree_peak_rss_bytes,
        updated_ms: wire.updated_ms,
        samples: wire.samples,
    })
}

/// The unit's last measured peak, or `None` when it has never been measured.
pub fn lookup_in(db: &Connection, unit_key: &str) -> Result<Option<UnitMemory>, RegistryError> {
    let row: Option<Vec<u8>> = db
        .query_row(
            "SELECT value FROM unit_memory_history WHERE key = ?1",
            params![unit_key],
            |row| row.get(0),
        )
        .optional()?;
    row.as_deref().map(decode).transpose()
}

/// Record a new measurement for `unit_key`. The latest measurement replaces
/// the stored peaks, so a unit that got cheaper is not held to an old figure;
/// `samples` counts how many measurements the row has seen.
pub fn record_in(
    db: &Connection,
    unit_key: &str,
    peak_rss_bytes: u64,
    tree_peak_rss_bytes: u64,
    now_ms: i64,
) -> Result<(), RegistryError> {
    // SQLite errors propagate. An undecodable existing row counts as "no prior
    // samples" so a fresh valid row overwrites it (soldr#3646).
    let existing: Option<Vec<u8>> = db
        .query_row(
            "SELECT value FROM unit_memory_history WHERE key = ?1",
            params![unit_key],
            |row| row.get(0),
        )
        .optional()?;
    let samples = existing
        .as_deref()
        .map(decode)
        .and_then(Result::ok)
        .map_or(0, |unit| unit.samples)
        .saturating_add(1);
    let bytes = prost_tagged_bytes(&WireUnitMemory {
        peak_rss_bytes,
        tree_peak_rss_bytes,
        updated_ms: now_ms,
        samples,
    });
    db.execute(
        "INSERT INTO unit_memory_history(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![unit_key, bytes],
    )?;
    Ok(())
}

/// Every recorded unit, for warming an in-memory map at daemon start.
pub fn load_all_in(db: &Connection) -> Result<Vec<(String, UnitMemory)>, RegistryError> {
    let mut statement = db.prepare("SELECT key, value FROM unit_memory_history")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    let mut units = Vec::new();
    for row in rows {
        let (key, bytes) = row?;
        match decode(&bytes) {
            Ok(unit) => units.push((key, unit)),
            Err(error) => eprintln!(
                "soldr-daemon: unit memory history: skipping undecodable row {key:?}: {error}"
            ),
        }
    }
    Ok(units)
}

/// Bounded so a stalled disk cannot grow memory without limit; a full channel
/// drops the write, never the compile. The in-memory map still has the value.
const CHANNEL_CAPACITY: usize = 1024;

/// Most rows written in one SQLite transaction batch.
const MAX_BATCH_ROWS: usize = 256;

/// Smallest peak treated as a real measurement of a unit (soldr#3152).
///
/// zccache's watchdog takes its first memory sample at spawn, before rustc has
/// grown. A compile that finishes within about one 250 ms tick reports only
/// that reading. In a local two-pass build of `soldr-core`, the 90 first-pass
/// rows below 8 MiB ran a median 212 ms (max 338 ms) and read as little as
/// 36-40 KiB for units whose next compile peaked at 1-144 MiB, while every
/// healthy row read above it. Below this floor a reading is ignored, so it can
/// neither create a history nor overwrite a real one; a unit that genuinely
/// peaks lower simply has no history and admission falls back to the classifier.
pub const MIN_TRUSTED_PEAK_BYTES: u64 = 8 * 1024 * 1024;

enum Command {
    Record {
        unit_key: String,
        peak_rss_bytes: u64,
        tree_peak_rss_bytes: u64,
        now_ms: i64,
    },
    Flush(oneshot::Sender<Result<(), String>>),
}

/// The daemon's live view of [`unit_memory_history`](self).
///
/// Admission reads [`UnitMemoryHistory::lookup`] on the compile path, so it
/// never touches SQLite: lookups answer from an in-memory map that
/// [`UnitMemoryHistory::record`] updates immediately. One background task owns
/// the database. It warms the map from the table at start, then writes
/// recorded measurements in batches through `spawn_blocking`, the same shape
/// as the event batcher (soldr#980).
#[derive(Clone, Debug)]
pub struct UnitMemoryHistory {
    units: Arc<Mutex<HashMap<String, UnitMemory>>>,
    tx: mpsc::Sender<Command>,
    loaded: watch::Receiver<bool>,
    /// Set by the blocking load itself, so a synchronous waiter never depends
    /// on the async owner task being scheduled (soldr#3686).
    load_done: Arc<(Mutex<bool>, Condvar)>,
    dropped: Arc<AtomicU64>,
}

/// How long admission waits for the warm load before answering from what is
/// already in memory (soldr#3686). Bounded so a stalled disk delays a compile
/// by at most this much, once, right after a daemon start.
pub const ADMISSION_LOAD_WAIT: Duration = Duration::from_secs(2);

/// Fold the warm-loaded rows into the live map (soldr#3686). A unit recorded
/// while the load ran keeps its newer timestamp, adds the stored sample count
/// (that record is a further sample of the same unit, and the persisted row
/// will count it the same way) and keeps the larger of each peak, so a
/// measurement taken during the load cannot erase a trusted history.
pub(crate) fn merge_loaded(
    map: &mut HashMap<String, UnitMemory>,
    stored: Vec<(String, UnitMemory)>,
) {
    for (key, unit) in stored {
        map.entry(key)
            .and_modify(|live| {
                live.samples = live.samples.saturating_add(unit.samples);
                live.peak_rss_bytes = live.peak_rss_bytes.max(unit.peak_rss_bytes);
                live.tree_peak_rss_bytes = live.tree_peak_rss_bytes.max(unit.tree_peak_rss_bytes);
                live.updated_ms = live.updated_ms.max(unit.updated_ms);
            })
            .or_insert(unit);
    }
}

impl UnitMemoryHistory {
    /// Start the owner task. Must be called inside a tokio runtime.
    pub fn start(db_path: PathBuf) -> Self {
        let units = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (loaded_tx, loaded) = watch::channel(false);
        let load_done = Arc::new((Mutex::new(false), Condvar::new()));
        tokio::spawn(drain(
            db_path,
            Arc::clone(&units),
            rx,
            loaded_tx,
            Arc::clone(&load_done),
        ));
        Self {
            units,
            tx,
            loaded,
            load_done,
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The unit's last measured peak, if this daemon has one.
    pub fn lookup(&self, unit_key: &str) -> Option<UnitMemory> {
        self.units.lock().get(unit_key).copied()
    }

    /// [`Self::lookup`] after waiting at most `timeout` for the warm load, so
    /// admission right after a restart sees the trusted history (soldr#3686).
    pub fn lookup_after_load(&self, unit_key: &str, timeout: Duration) -> Option<UnitMemory> {
        let (lock, cvar) = &*self.load_done;
        let done = lock.lock();
        let _ = cvar.wait_timeout_while(done, timeout, |done| !*done);
        self.lookup(unit_key)
    }

    /// Record a compile's measured memory. A measurement whose larger peak is
    /// below [`MIN_TRUSTED_PEAK_BYTES`] is ignored: that covers both a cache hit
    /// (no compiler ran, so nothing was measured) and a spawn-instant reading.
    pub fn record(&self, unit_key: &str, peak_rss_bytes: u64, tree_peak_rss_bytes: u64) {
        if peak_rss_bytes.max(tree_peak_rss_bytes) < MIN_TRUSTED_PEAK_BYTES {
            return;
        }
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        {
            let mut units = self.units.lock();
            let samples = units
                .get(unit_key)
                .map_or(0, |unit| unit.samples)
                .saturating_add(1);
            units.insert(
                unit_key.to_string(),
                UnitMemory {
                    peak_rss_bytes,
                    tree_peak_rss_bytes,
                    updated_ms: now_ms,
                    samples,
                },
            );
        }
        let sent = self.tx.try_send(Command::Record {
            unit_key: unit_key.to_string(),
            peak_rss_bytes,
            tree_peak_rss_bytes,
            now_ms,
        });
        if sent.is_err() && self.dropped.fetch_add(1, Ordering::Relaxed) == 0 {
            eprintln!(
                "soldr-daemon: unit memory history: write queue full or closed; \
                 dropping persisted measurements (first dropped: {unit_key})"
            );
        }
    }

    /// How many measurements were not queued for persistence.
    pub fn dropped_writes(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Resolve once the warm load from the table has finished.
    pub async fn ready(&self) -> Result<(), String> {
        let mut loaded = self.loaded.clone();
        loaded
            .wait_for(|done| *done)
            .await
            .map(|_| ())
            .map_err(|_| "unit memory history task stopped before loading".to_string())
    }

    /// Resolve once every measurement recorded so far is on disk.
    pub async fn flush(&self) -> Result<(), String> {
        let (reply, done) = oneshot::channel();
        self.tx
            .send(Command::Flush(reply))
            .await
            .map_err(|_| "unit memory history task stopped".to_string())?;
        done.await
            .map_err(|_| "unit memory history task dropped the flush".to_string())?
    }
}

async fn drain(
    db_path: PathBuf,
    units: Arc<Mutex<HashMap<String, UnitMemory>>>,
    mut rx: mpsc::Receiver<Command>,
    loaded: watch::Sender<bool>,
    load_done: Arc<(Mutex<bool>, Condvar)>,
) {
    let load_path = db_path.clone();
    let signal = Arc::clone(&load_done);
    let load = tokio::task::spawn_blocking(move || {
        let stored = load_stored(&load_path);
        {
            let mut map = units.lock();
            merge_loaded(&mut map, stored);
        }
        signal_done(&signal);
    })
    .await;
    if let Err(error) = load {
        eprintln!("soldr-daemon: unit memory history: warm load task failed: {error}");
        signal_done(&load_done);
    }
    let _ = loaded.send(true);

    let mut pending = Vec::new();
    while let Some(command) = rx.recv().await {
        let mut flushes = Vec::new();
        let mut next = Some(command);
        while let Some(command) = next.take() {
            match command {
                Command::Record {
                    unit_key,
                    peak_rss_bytes,
                    tree_peak_rss_bytes,
                    now_ms,
                } => pending.push((unit_key, peak_rss_bytes, tree_peak_rss_bytes, now_ms)),
                Command::Flush(reply) => flushes.push(reply),
            }
            if pending.len() < MAX_BATCH_ROWS {
                next = rx.try_recv().ok();
            }
        }
        let result = write_batch(db_path.clone(), std::mem::take(&mut pending)).await;
        if let Err(error) = &result {
            eprintln!("soldr-daemon: unit memory history: write failed: {error}");
        }
        for reply in flushes {
            let _ = reply.send(result.clone());
        }
    }
}

fn signal_done(signal: &(Mutex<bool>, Condvar)) {
    *signal.0.lock() = true;
    signal.1.notify_all();
}

fn load_stored(load_path: &std::path::Path) -> Vec<(String, UnitMemory)> {
    let db = match crate::cache_lib::state_store::open_state_db(load_path) {
        Ok(db) => db,
        Err(error) => {
            eprintln!("soldr-daemon: unit memory history: open failed: {error}");
            return Vec::new();
        }
    };
    match load_all_in(&db) {
        Ok(units) => units,
        Err(error) => {
            eprintln!("soldr-daemon: unit memory history: warm load failed: {error}");
            Vec::new()
        }
    }
}

async fn write_batch(db_path: PathBuf, batch: Vec<(String, u64, u64, i64)>) -> Result<(), String> {
    if batch.is_empty() {
        return Ok(());
    }
    tokio::task::spawn_blocking(move || {
        let db = crate::cache_lib::state_store::open_state_db(&db_path)
            .map_err(|error| error.to_string())?;
        // soldr#3288/#3290: this used to be up to MAX_BATCH_ROWS (256)
        // separate autocommit writes, each one a commit that could
        // invalidate another connection's WAL snapshot mid-read — the
        // likely antagonist behind #3288's `event_batcher` failure, since
        // this fires on every compile response with several concurrent
        // compile streams feeding it. One transaction means one commit for
        // the whole batch, and it also fixes a latent partial-write: a
        // `record_in` error used to return early with earlier rows in the
        // batch already committed.
        let describe =
            |error: rusqlite::Error| crate::cache_lib::state_store::describe_sqlite_error(&error);
        let tx = db.unchecked_transaction().map_err(describe)?;
        for (unit_key, peak, tree_peak, now_ms) in batch {
            record_in(&tx, &unit_key, peak, tree_peak, now_ms)
                .map_err(|error| error.to_string())?;
        }
        tx.commit().map_err(describe)?;
        Ok(())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[cfg(test)]
#[path = "unit_memory_history_tests.rs"]
mod tests;
