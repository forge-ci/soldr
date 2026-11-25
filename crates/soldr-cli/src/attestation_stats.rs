//! Privacy-preserving statistics for remote build attestations.

use std::io::Write as _;
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

pub(crate) mod outbox;
mod server;
pub(crate) use server::{run, AttestationCommand};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Timing {
    pub version: u8,
    pub started_unix_ms: u64,
    pub lock_ms: u64,
    pub preparation_ms: u64,
    pub command_ms: u64,
    pub postprocessing_ms: u64,
    pub worker_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub publication_ms: Option<u64>,
}

pub(crate) struct WorkerTimer {
    started: Instant,
    phase: Instant,
    timing: Timing,
}

impl WorkerTimer {
    pub(crate) fn start() -> Self {
        let started = Instant::now();
        Self {
            started,
            phase: started,
            timing: Timing {
                version: 1,
                started_unix_ms: unix_ms(),
                lock_ms: 0,
                preparation_ms: 0,
                command_ms: 0,
                postprocessing_ms: 0,
                worker_ms: 0,
                publication_ms: None,
            },
        }
    }

    fn next_phase(&mut self) -> u64 {
        let now = Instant::now();
        let elapsed = now.duration_since(self.phase).as_millis() as u64;
        self.phase = now;
        elapsed
    }

    pub(crate) fn locked(&mut self) {
        self.timing.lock_ms = self.next_phase();
    }

    pub(crate) fn command_started(&mut self) {
        self.timing.preparation_ms = self.next_phase();
    }

    pub(crate) fn command_finished(&mut self) {
        self.timing.command_ms = self.next_phase();
    }

    pub(crate) fn finish(mut self) -> Timing {
        self.timing.postprocessing_ms = self.next_phase();
        self.timing.worker_ms = self.phase.duration_since(self.started).as_millis() as u64;
        self.timing
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Completion {
    pub version: u8,
    pub worker: String,
    pub session: String,
    pub action: String,
    pub target: Option<String>,
    pub exit_code: i32,
    pub timing: Timing,
}

#[derive(Debug, Serialize)]
struct Stats {
    version: u8,
    start_unix_ms: u64,
    end_unix_ms: u64,
    completed: usize,
    succeeded: usize,
    failed: usize,
    worker_ms: Distribution,
    command_ms: Distribution,
    publication_ms: Distribution,
}

#[derive(Debug, Serialize)]
struct Distribution {
    p50: Option<u64>,
    p95: Option<u64>,
}

const DAY_MS: u64 = 86_400_000;
const RETENTION_MS: u64 = 30 * DAY_MS;
const ACK_HEADER: &str = "x-soldr-attestation-ingest";

pub(crate) fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub(crate) fn client_receipt(
    session: &str,
    attestation_ms: u64,
    total_ms: u64,
    downloaded: bool,
) -> serde_json::Result<Vec<u8>> {
    serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "session": session,
        "attestation_ms": attestation_ms,
        "artifact_download_ms": downloaded.then_some(total_ms - attestation_ms),
        "total_ms": total_ms,
    }))
}

pub(crate) fn write_explicit_client_receipt(
    attestation: &Path,
    session: &str,
    started: Instant,
    attestation_ms: u64,
    downloaded: bool,
) {
    if session.len() != 32 || !session.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return;
    }
    let receipt = client_receipt(
        session,
        attestation_ms,
        started.elapsed().as_millis() as u64,
        downloaded,
    );
    let result = receipt.map_err(std::io::Error::other).and_then(|receipt| {
        std::fs::File::create_new(attestation.with_extension(format!("{session}.client-timing")))?
            .write_all(&receipt)
    });
    if let Err(error) = result {
        eprintln!("soldr: could not record Forge CI client timing: {error}");
    }
}

fn open(path: &Path) -> rusqlite::Result<Connection> {
    let database = Connection::open(path)?;
    database.busy_timeout(std::time::Duration::from_secs(2))?;
    database.execute_batch(
        "CREATE TABLE IF NOT EXISTS completions (
             worker TEXT NOT NULL, session TEXT NOT NULL,
             started_ms INTEGER NOT NULL, exit_code INTEGER NOT NULL,
             action TEXT NOT NULL, target TEXT,
             worker_ms INTEGER NOT NULL, command_ms INTEGER NOT NULL,
             publication_ms INTEGER NOT NULL,
             PRIMARY KEY(worker, session)
         );
         CREATE INDEX IF NOT EXISTS completions_started ON completions(started_ms);",
    )?;
    Ok(database)
}

fn prune(database: &Connection, now: u64) -> rusqlite::Result<()> {
    database.execute(
        "DELETE FROM completions WHERE started_ms < ?1",
        [now.saturating_sub(RETENTION_MS)],
    )?;
    Ok(())
}

fn insert(database: &Connection, event: &Completion, now: u64) -> rusqlite::Result<bool> {
    let transaction = database.unchecked_transaction()?;
    prune(&transaction, now)?;
    let inserted = transaction.execute(
        "INSERT INTO completions VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(worker, session) DO NOTHING",
        params![
            event.worker,
            event.session,
            event.timing.started_unix_ms,
            event.exit_code,
            event.action,
            event.target,
            event.timing.worker_ms,
            event.timing.command_ms,
            event.timing.publication_ms.unwrap_or_default(),
        ],
    )?;
    transaction.commit()?;
    Ok(inserted == 1)
}

fn distribution(mut samples: Vec<u64>) -> Distribution {
    samples.sort_unstable();
    let percentile = |percent: usize| {
        let rank = (samples.len() * percent).div_ceil(100);
        rank.checked_sub(1).map(|index| samples[index])
    };
    Distribution {
        p50: percentile(50),
        p95: percentile(95),
    }
}

fn query(database: &Connection, start: u64, end: u64) -> rusqlite::Result<Stats> {
    let mut statement = database.prepare(
        "SELECT exit_code, worker_ms, command_ms, publication_ms FROM completions
         WHERE started_ms >= ?1 AND started_ms < ?2",
    )?;
    let mut rows = statement.query([start, end])?;
    let mut succeeded = 0;
    let mut worker_ms = Vec::new();
    let mut command_ms = Vec::new();
    let mut publication_ms = Vec::new();
    while let Some(row) = rows.next()? {
        succeeded += usize::from(row.get::<_, i32>(0)? == 0);
        worker_ms.push(row.get(1)?);
        command_ms.push(row.get(2)?);
        publication_ms.push(row.get(3)?);
    }
    Ok(Stats {
        version: 1,
        start_unix_ms: start,
        end_unix_ms: end,
        completed: worker_ms.len(),
        succeeded,
        failed: worker_ms.len() - succeeded,
        worker_ms: distribution(worker_ms),
        command_ms: distribution(command_ms),
        publication_ms: distribution(publication_ms),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_survive_restart_and_count_each_completed_session_once() {
        let scratch = tempfile::tempdir().unwrap();
        let path = scratch.path().join("stats.sqlite");
        let database = open(&path).unwrap();
        let now = unix_ms();
        let start = now - 20;
        let mut event = Completion {
            version: 1,
            worker: "e640".into(),
            session: "1".repeat(32),
            action: "build".into(),
            target: Some("x86_64-unknown-linux-gnu".into()),
            exit_code: 0,
            timing: Timing {
                version: 1,
                started_unix_ms: start,
                lock_ms: 0,
                preparation_ms: 1,
                command_ms: 8,
                postprocessing_ms: 1,
                worker_ms: 10,
                publication_ms: Some(2),
            },
        };
        assert!(insert(&database, &event, now).unwrap());
        assert!(!insert(&database, &event, now).unwrap());
        event.session = "2".repeat(32);
        event.exit_code = 1;
        event.timing.started_unix_ms += 1;
        event.timing.command_ms = 18;
        event.timing.worker_ms = 20;
        assert!(insert(&database, &event, now).unwrap());
        drop(database);

        let database = open(&path).unwrap();
        let stats = query(&database, start, now).unwrap();
        assert_eq!((stats.completed, stats.succeeded, stats.failed), (2, 1, 1));
        assert_eq!(
            (stats.worker_ms.p50, stats.worker_ms.p95),
            (Some(10), Some(20))
        );
        assert_eq!(stats.command_ms.p95, Some(18));
        assert_eq!(query(&database, start, start + 1).unwrap().completed, 1);
        assert_eq!(query(&database, now, now + 1).unwrap().worker_ms.p95, None);
        let expired = now - RETENTION_MS - 1;
        event.session = "3".repeat(32);
        event.timing.started_unix_ms = expired;
        assert!(insert(&database, &event, expired).unwrap());
        prune(&database, now).unwrap();
        assert_eq!(query(&database, expired, expired + 1).unwrap().completed, 0);
    }
}
