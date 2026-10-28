//! Machine-local fenced lease used only by front-door broker resurrection.
//!
//! The database is disposable coordination state, never route state. Every
//! operation opens and closes its own SQLite connection, which guarantees a
//! broker child cannot inherit a database handle across `spawn()`.

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LEASE_DURATION: Duration = Duration::from_secs(5);
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_millis(10);
const ACQUIRE_BUSY_CEILING: Duration = Duration::from_secs(5);
const JITTER_MIN_MS: u64 = 5;
const JITTER_MAX_MS: u64 = 50;

/// Fencing identity returned to the one front door authorized to resurrect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BrokerLease {
    path: PathBuf,
    pub(crate) generation: u64,
    pub(crate) nonce: [u8; 16],
    owner_pid: u32,
    owner_start_token: u64,
    boot_id: String,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum BrokerLeaseError {
    #[error(
        "broker resurrection lease database {path:?} remained busy for {waited:?}; last-known holder: {last_known_holder}"
    )]
    Busy {
        path: PathBuf,
        waited: Duration,
        last_known_holder: String,
    },
    #[error("broker resurrection lease was fenced by a newer owner")]
    Fenced,
    #[error("broker resurrection lease database {path:?} failed: {source}")]
    Database {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
    #[error("broker resurrection lease recovery at {path:?} failed: {source}")]
    Recovery {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("broker resurrection nonce generation failed: {0}")]
    Random(getrandom::Error),
}

impl BrokerLease {
    /// Compete for the one-row lease. A dead/reused owner is reclaimed
    /// immediately; expiry authorizes takeover even when the former owner is
    /// still live or SIGSTOP'd.
    pub(crate) fn acquire(path: &Path) -> Result<Self, BrokerLeaseError> {
        Self::acquire_with_ceiling(path, ACQUIRE_BUSY_CEILING)
    }

    fn acquire_with_ceiling(path: &Path, busy_ceiling: Duration) -> Result<Self, BrokerLeaseError> {
        let started = Instant::now();
        let deadline = started + busy_ceiling;
        let owner_pid = std::process::id();
        let owner_start_token = current_process_start_token();
        let boot_id = running_process::broker::host_identity::current().boot_id;
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce).map_err(BrokerLeaseError::Random)?;
        let mut recovered_corruption = false;
        let mut last_known_holder = None;
        // The ceiling bounds *retrying*, not whether we try at all. Everything
        // above -- the start token, host identity, the nonce -- runs inside the
        // deadline, so a short ceiling on a slow host could be spent before a
        // single acquisition was attempted. The caller then got `Busy` saying
        // the database "never became readable" about a database nobody had
        // opened: a diagnosis of the wrong thing entirely.
        //
        // That is how windows-gnu failed this contract's own test with the
        // `Recovery` it should have reported replaced by that `Busy`.
        let mut attempted = false;

        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() && attempted {
                return Err(BrokerLeaseError::Busy {
                    path: path.to_path_buf(),
                    waited: busy_ceiling,
                    last_known_holder: last_known_holder
                        .unwrap_or_else(|| "unavailable (database never became readable)".into()),
                });
            }
            attempted = true;
            // A zero `remaining` here means the ceiling is already spent, so
            // this attempt does not block -- it is one non-waiting probe, which
            // is what lets the real error (corruption, fencing) surface instead
            // of a timeout that describes nothing.
            match acquire_once(
                path,
                owner_pid,
                owner_start_token,
                &boot_id,
                nonce,
                remaining.min(SQLITE_BUSY_TIMEOUT),
            ) {
                Ok(Some(generation)) => {
                    return Ok(Self {
                        path: path.to_path_buf(),
                        generation,
                        nonce,
                        owner_pid,
                        owner_start_token,
                        boot_id,
                    });
                }
                Ok(None) => return Err(BrokerLeaseError::Fenced),
                Err(source)
                    if crate::cache_lib::state_store::is_busy(&source)
                        && Instant::now() < deadline =>
                {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if !remaining.is_zero() {
                        last_known_holder = read_holder_summary_with_timeout(
                            path,
                            remaining.min(SQLITE_BUSY_TIMEOUT),
                        )
                        .or(last_known_holder);
                    }
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if !remaining.is_zero() {
                        std::thread::sleep(full_jitter().min(remaining));
                    }
                }
                Err(source) if crate::cache_lib::state_store::is_busy(&source) => {
                    return Err(BrokerLeaseError::Busy {
                        path: path.to_path_buf(),
                        waited: busy_ceiling,
                        last_known_holder: last_known_holder.unwrap_or_else(|| {
                            "unavailable (database never became readable)".into()
                        }),
                    });
                }
                Err(_source) if !recovered_corruption => {
                    recover_corrupt_database(path, deadline).map_err(|source| {
                        BrokerLeaseError::Recovery {
                            path: path.to_path_buf(),
                            source,
                        }
                    })?;
                    recovered_corruption = true;
                }
                Err(source) => {
                    return Err(BrokerLeaseError::Database {
                        path: path.to_path_buf(),
                        source,
                    });
                }
            }
        }
    }

    pub(crate) fn contention_delay() -> Duration {
        full_jitter()
    }

    /// Extend this exact fenced generation. The connection is closed before
    /// returning, so callers can safely spawn immediately afterward.
    pub(crate) fn renew(&self) -> Result<(), BrokerLeaseError> {
        let connection = open_database(&self.path).map_err(|source| self.database_error(source))?;
        let now = unix_now_ms();
        let changed = connection
            .execute(
                "UPDATE broker_lease SET renewed_ms=?1, expires_ms=?2 \
                 WHERE singleton=1 AND generation=?3 AND nonce=?4 \
                 AND owner_pid=?5 AND owner_start_token=?6 AND boot_id=?7",
                params![
                    now,
                    now.saturating_add(LEASE_DURATION.as_millis() as u64),
                    self.generation,
                    &self.nonce[..],
                    self.owner_pid,
                    self.owner_start_token,
                    self.boot_id,
                ],
            )
            .map_err(|source| self.database_error(source))?;
        if changed == 1 {
            Ok(())
        } else {
            Err(BrokerLeaseError::Fenced)
        }
    }

    /// Re-read the row immediately before a mutation or process spawn.
    pub(crate) fn check_fence(&self) -> Result<(), BrokerLeaseError> {
        let connection = open_database(&self.path).map_err(|source| self.database_error(source))?;
        let matches = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM broker_lease WHERE singleton=1 \
                 AND generation=?1 AND nonce=?2 AND owner_pid=?3 \
                 AND owner_start_token=?4 AND boot_id=?5 AND expires_ms>?6)",
                params![
                    self.generation,
                    &self.nonce[..],
                    self.owner_pid,
                    self.owner_start_token,
                    self.boot_id,
                    unix_now_ms(),
                ],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|source| self.database_error(source))?;
        if matches {
            Ok(())
        } else {
            Err(BrokerLeaseError::Fenced)
        }
    }

    /// Relinquish this exact generation. A newer generation is untouched.
    pub(crate) fn release(&self) {
        let Ok(connection) = open_database(&self.path) else {
            return;
        };
        let _ = connection.execute(
            "UPDATE broker_lease SET expires_ms=0 WHERE singleton=1 \
             AND generation=?1 AND nonce=?2",
            params![self.generation, &self.nonce[..]],
        );
    }

    fn database_error(&self, source: rusqlite::Error) -> BrokerLeaseError {
        BrokerLeaseError::Database {
            path: self.path.clone(),
            source,
        }
    }
}

fn acquire_once(
    path: &Path,
    owner_pid: u32,
    owner_start_token: u64,
    boot_id: &str,
    nonce: [u8; 16],
    busy_timeout: Duration,
) -> rusqlite::Result<Option<u64>> {
    let mut connection = open_database_with_timeout(path, busy_timeout)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let now = unix_now_ms();
    let row = transaction
        .query_row(
            "SELECT generation, owner_pid, owner_start_token, boot_id, expires_ms \
             FROM broker_lease WHERE singleton=1",
            [],
            |row| {
                Ok((
                    row.get::<_, u64>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, u64>(4)?,
                ))
            },
        )
        .optional()?;

    let generation = match row {
        None => 1,
        Some((generation, row_owner_pid, row_owner_start_token, row_boot_id, expires_ms))
            if row_boot_id != boot_id
                || !process_matches(row_owner_pid, row_owner_start_token)
                || expires_ms <= now =>
        {
            generation.saturating_add(1)
        }
        Some(_) => return Ok(None),
    };
    transaction.execute(
        "INSERT INTO broker_lease(singleton, generation, nonce, owner_pid, \
         owner_start_token, boot_id, acquired_ms, renewed_ms, expires_ms, \
         attempt, retry_after_ms) VALUES(1, ?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, 1, 0) \
         ON CONFLICT(singleton) DO UPDATE SET generation=excluded.generation, \
         nonce=excluded.nonce, owner_pid=excluded.owner_pid, \
         owner_start_token=excluded.owner_start_token, boot_id=excluded.boot_id, \
         acquired_ms=excluded.acquired_ms, renewed_ms=excluded.renewed_ms, \
         expires_ms=excluded.expires_ms, attempt=broker_lease.attempt+1, retry_after_ms=0",
        params![
            generation,
            &nonce[..],
            owner_pid,
            owner_start_token,
            boot_id,
            now,
            now.saturating_add(LEASE_DURATION.as_millis() as u64),
        ],
    )?;
    transaction.commit()?;
    Ok(Some(generation))
}

fn open_database(path: &Path) -> rusqlite::Result<Connection> {
    open_database_with_timeout(path, SQLITE_BUSY_TIMEOUT)
}

fn open_database_with_timeout(path: &Path, busy_timeout: Duration) -> rusqlite::Result<Connection> {
    let connection = Connection::open(path)?;
    // Owner-only lease file. Windows ignores mode bits, so the facade
    // applies them only where they carry meaning.
    crate::platform::fs::permissions::restore_mode(path, Some(0o600)).map_err(|error| {
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_PERM),
            Some(error.to_string()),
        )
    })?;
    connection.busy_timeout(busy_timeout)?;
    connection.pragma_update(None, "journal_mode", "DELETE")?;
    connection.pragma_update(None, "synchronous", "OFF")?;
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS broker_lease (
            singleton INTEGER PRIMARY KEY CHECK(singleton=1),
            generation INTEGER NOT NULL,
            nonce BLOB NOT NULL,
            owner_pid INTEGER NOT NULL,
            owner_start_token INTEGER NOT NULL,
            boot_id TEXT NOT NULL,
            acquired_ms INTEGER NOT NULL,
            renewed_ms INTEGER NOT NULL,
            expires_ms INTEGER NOT NULL,
            attempt INTEGER NOT NULL,
            retry_after_ms INTEGER NOT NULL
        );",
    )?;
    // CREATE TABLE IF NOT EXISTS deliberately leaves an incompatible table
    // untouched. Preparing this read turns schema drift into a recoverable
    // open failure before a contender mutates the disposable database.
    connection
        .query_row(
            "SELECT singleton, generation, nonce, owner_pid, owner_start_token, \
             boot_id, acquired_ms, renewed_ms, expires_ms, attempt, retry_after_ms \
             FROM broker_lease LIMIT 1",
            [],
            |_| Ok(()),
        )
        .optional()?;
    Ok(connection)
}

fn read_holder_summary(path: &Path) -> Option<String> {
    read_holder_summary_with_timeout(path, SQLITE_BUSY_TIMEOUT)
}

fn read_holder_summary_with_timeout(path: &Path, busy_timeout: Duration) -> Option<String> {
    let connection = open_database_with_timeout(path, busy_timeout).ok()?;
    connection
        .query_row(
            "SELECT generation, owner_pid, owner_start_token, boot_id, expires_ms \
             FROM broker_lease WHERE singleton=1",
            [],
            |row| {
                Ok(format!(
                    "generation={} pid={} start_token={} boot_id={} expires_ms={}",
                    row.get::<_, u64>(0)?,
                    row.get::<_, u32>(1)?,
                    row.get::<_, u64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, u64>(4)?,
                ))
            },
        )
        .optional()
        .ok()
        .flatten()
}

/// Delete disposable coordination state under a separate OS file lock.
/// A contender rechecks the database after acquiring the lock because another
/// process may already have completed recovery.
///
/// `deadline` is the *acquisition's* absolute deadline (soldr#2478): the
/// public busy ceiling describes total wall-clock acquisition time, so
/// recovery must not open a second budget of its own. Lock polling and
/// jitter are clamped to the remaining budget, and once it is exhausted the
/// attributed timeout returns immediately — no diagnostic or validation
/// reads after expiry.
fn recover_corrupt_database(path: &Path, deadline: Instant) -> std::io::Result<()> {
    use fs2::FileExt as _;

    let recovery_timeout = || {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "corruption recovery lock was still held at the acquisition deadline",
        )
    };
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("lease database has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let lock_path = parent.join("broker-lease-recovery.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(recovery_timeout());
        }
        match lock.try_lock_exclusive() {
            Ok(()) => break,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(full_jitter().min(remaining));
            }
            Err(error) => return Err(error),
        }
    }
    if Instant::now() >= deadline {
        return Err(recovery_timeout());
    }
    if open_database(path).is_ok() {
        return Ok(());
    }
    for candidate in [
        path.to_path_buf(),
        PathBuf::from(format!("{}-journal", path.display())),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ] {
        if candidate.exists() {
            std::fs::remove_file(candidate)?;
        }
    }
    Ok(())
}

fn full_jitter() -> Duration {
    let mut random = [0_u8; 8];
    if getrandom::fill(&mut random).is_err() {
        return Duration::from_millis(JITTER_MIN_MS);
    }
    let span = JITTER_MAX_MS - JITTER_MIN_MS + 1;
    Duration::from_millis(JITTER_MIN_MS + u64::from_le_bytes(random) % span)
}

fn unix_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn current_process_start_token() -> u64 {
    process_start_token(std::process::id()).unwrap_or_default()
}

fn process_start_token(pid: u32) -> Option<u64> {
    use sysinfo::{Pid, ProcessRefreshKind, System};

    // Lease acquisition is a fail-fast startup path and stampedes can put
    // dozens of contenders here at once. Refresh only the named process;
    // `System::new_all()` scans every process and device on the machine and
    // can consume enough time under load for a healthy five-second lease to
    // expire while contenders are merely trying to validate it.
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    system
        .refresh_process_specifics(pid, ProcessRefreshKind::new())
        .then(|| system.process(pid).map(|process| process.start_time()))
        .flatten()
}

fn process_matches(pid: u32, expected_start_token: u64) -> bool {
    if expected_start_token == 0 {
        return running_process::broker::backend_lifecycle::verify_pid::process_is_alive(pid);
    }
    process_start_token(pid) == Some(expected_start_token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn lease_is_single_winner_and_release_allows_next_generation() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("lease.sqlite3");
        let first = BrokerLease::acquire(&path).unwrap();
        assert!(matches!(
            BrokerLease::acquire(&path),
            Err(BrokerLeaseError::Fenced)
        ));
        first.release();
        let second = BrokerLease::acquire(&path).unwrap();
        assert!(second.generation > first.generation);
    }

    #[test]
    fn expired_row_is_taken_over_even_when_pid_is_live() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("lease.sqlite3");
        let first = BrokerLease::acquire(&path).unwrap();
        let connection = open_database(&path).unwrap();
        connection
            .execute("UPDATE broker_lease SET expires_ms=0", [])
            .unwrap();
        drop(connection);
        let second = BrokerLease::acquire(&path).unwrap();
        assert!(second.generation > first.generation);
        assert!(matches!(first.check_fence(), Err(BrokerLeaseError::Fenced)));
        assert!(matches!(first.renew(), Err(BrokerLeaseError::Fenced)));
        first.release();
        second
            .check_fence()
            .expect("a former holder cannot release its successor");
    }

    #[test]
    fn dead_owner_is_taken_over_before_expiry() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("lease.sqlite3");
        let first = BrokerLease::acquire(&path).unwrap();
        let connection = open_database(&path).unwrap();
        connection
            .execute(
                "UPDATE broker_lease SET owner_pid=?1, expires_ms=?2",
                params![u32::MAX, unix_now_ms() + 60_000],
            )
            .unwrap();
        drop(connection);

        let second = BrokerLease::acquire(&path).expect("dead owner is immediately reclaimable");
        assert!(second.generation > first.generation);
    }

    #[test]
    fn reused_pid_with_wrong_start_token_is_taken_over_before_expiry() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("lease.sqlite3");
        let first = BrokerLease::acquire(&path).unwrap();
        let connection = open_database(&path).unwrap();
        connection
            .execute(
                "UPDATE broker_lease SET owner_start_token=?1, expires_ms=?2",
                params![
                    current_process_start_token().saturating_add(1),
                    unix_now_ms() + 60_000
                ],
            )
            .unwrap();
        drop(connection);

        let second = BrokerLease::acquire(&path).expect("reused PID is immediately reclaimable");
        assert!(second.generation > first.generation);
    }

    #[test]
    fn concurrent_stampede_has_exactly_one_winner() {
        let temp = tempfile::tempdir().unwrap();
        let path = Arc::new(temp.path().join("lease.sqlite3"));
        let barrier = Arc::new(Barrier::new(64));
        let threads: Vec<_> = (0..64)
            .map(|_| {
                let path = Arc::clone(&path);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    BrokerLease::acquire_with_ceiling(&path, Duration::from_secs(2)).ok()
                })
            })
            .collect();
        let winners: Vec<_> = threads
            .into_iter()
            .filter_map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(winners.len(), 1);
    }

    #[test]
    fn continuous_sqlite_busy_is_bounded_and_names_the_database() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("lease.sqlite3");
        let first = BrokerLease::acquire(&path).unwrap();
        first.release();
        let mut blocker = open_database(&path).unwrap();
        let _transaction = blocker
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();

        let started = Instant::now();
        let error = BrokerLease::acquire_with_ceiling(&path, Duration::from_millis(40))
            .expect_err("a continuously locked database must fail loudly");
        assert!(started.elapsed() < Duration::from_secs(1));
        let BrokerLeaseError::Busy {
            path: reported,
            waited,
            ..
        } = error
        else {
            panic!("unexpected busy error: {error}");
        };
        assert_eq!(reported, path);
        assert_eq!(waited, Duration::from_millis(40));
    }

    #[test]
    fn held_recovery_lock_respects_the_acquisition_deadline() {
        use fs2::FileExt as _;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("lease.sqlite3");
        std::fs::write(&path, b"this is not sqlite").unwrap();
        // Another contender is mid-recovery: the OS lock is held and never
        // released while this acquisition runs.
        let lock_path = temp.path().join("broker-lease-recovery.lock");
        let held = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
            .unwrap();
        held.lock_exclusive().unwrap();

        let started = Instant::now();
        let error = BrokerLease::acquire_with_ceiling(&path, Duration::from_millis(200))
            .expect_err("corrupt state behind a held recovery lock must fail, not hang");
        // soldr#2478: recovery used to start its own five-second deadline,
        // so a 200 ms ceiling still waited ~5 s here. The caller's ceiling
        // is the whole wall-clock contract; 2 s leaves a 10x scheduler
        // margin over the ceiling while sitting far below the old overrun.
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "acquisition overran its ceiling: {:?}",
            started.elapsed()
        );
        assert!(
            matches!(error, BrokerLeaseError::Recovery { .. }),
            "unexpected error: {error}"
        );
    }

    /// soldr#2888: an already-spent ceiling must still buy one attempt.
    ///
    /// `acquire_with_ceiling` starts its deadline before reading the process
    /// start token, host identity and nonce. On a slow host that setup can
    /// outlast a short ceiling, and the loop's first act was to return `Busy`
    /// -- reporting that the database "never became readable" when nothing had
    /// tried to read it.
    ///
    /// windows-gnu hit exactly that: `held_recovery_lock_respects_the_
    /// acquisition_deadline` got that `Busy` in place of its `Recovery`. That
    /// reproduction is timing-dependent, which is no way to hold a contract, so
    /// this one states it directly -- a zero ceiling is the same condition with
    /// the race removed, and it must still surface the real error.
    #[test]
    fn an_expired_ceiling_still_buys_one_attempt() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("lease.sqlite3");
        std::fs::write(&path, b"this is not sqlite").unwrap();

        let error = BrokerLease::acquire_with_ceiling(&path, Duration::ZERO)
            .expect_err("a corrupt database cannot be acquired");
        assert!(
            !matches!(error, BrokerLeaseError::Busy { .. }),
            concat!(
                "the ceiling preempted the first attempt, so the corruption ",
                "was never seen and the error describes a wait that never ",
                "happened: {}"
            ),
            error
        );
    }

    #[test]
    fn corrupt_disposable_database_is_deleted_and_recreated() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("lease.sqlite3");
        std::fs::write(&path, b"this is not sqlite").unwrap();

        let lease = BrokerLease::acquire(&path).expect("recover corrupt coordination state");
        assert_eq!(lease.generation, 1);
        lease
            .check_fence()
            .expect("replacement database is healthy");

        assert_eq!(std::fs::read(&path).unwrap()[..16], *b"SQLite format 3\0");
    }

    #[test]
    fn incompatible_schema_is_deleted_and_recreated() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("lease.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("CREATE TABLE broker_lease(singleton INTEGER PRIMARY KEY);")
            .unwrap();
        drop(connection);

        let lease = BrokerLease::acquire(&path).expect("recover incompatible schema");
        lease.check_fence().expect("replacement schema is healthy");
        assert_eq!(std::fs::read(&path).unwrap()[..16], *b"SQLite format 3\0");
    }
}
