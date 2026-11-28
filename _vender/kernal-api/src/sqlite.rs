//! Bounded synchronous SQLite mechanics for application-owned SQL and schema.
//!
//! A [`Connection`] is intentionally neither an async API nor a pool. Execute
//! it on the kernel's blocking worker and retain pooling, schema, migrations,
//! and SQL policy in the application.

use rusqlite::{
    backup::{Backup, StepResult},
    types::ValueRef,
    Connection as RawConnection, Error as RawError, ErrorCode, OpenFlags, ToSql,
};
use std::{
    fs, io,
    mem::size_of,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

const BUSY_TIMEOUT: Duration = Duration::from_millis(100);
/// Upper bound for a caller-selected SQLite contention wait.
pub const MAX_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const BACKUP_RETRY_DELAY: Duration = Duration::from_millis(10);
const BACKUP_MAX_RETRIES: usize = 10;
const ROW_OVERHEAD: usize = size_of::<Row>();
const CELL_OVERHEAD: usize = size_of::<Value>();

/// A SQLite scalar owned by this facade.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

struct Binding<'a>(&'a Value);
impl ToSql for Binding<'_> {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        match self.0 {
            Value::Null => Ok(rusqlite::types::ToSqlOutput::Owned(
                rusqlite::types::Value::Null,
            )),
            Value::Integer(value) => Ok(rusqlite::types::ToSqlOutput::Owned(
                rusqlite::types::Value::Integer(*value),
            )),
            Value::Real(value) => Ok(rusqlite::types::ToSqlOutput::Owned(
                rusqlite::types::Value::Real(*value),
            )),
            Value::Text(value) => Ok(rusqlite::types::ToSqlOutput::Borrowed(ValueRef::Text(
                value.as_bytes(),
            ))),
            Value::Blob(value) => Ok(rusqlite::types::ToSqlOutput::Borrowed(ValueRef::Blob(
                value,
            ))),
        }
    }
}

/// One query row, represented only with facade-owned values.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    values: Vec<Value>,
}

impl Row {
    pub fn values(&self) -> &[Value] {
        &self.values
    }
    pub fn get(&self, index: usize) -> Option<&Value> {
        self.values.get(index)
    }
}

/// Explicit logical-owned-memory ceiling for a materialized query result.
///
/// `max_bytes` includes facade row/value structs, scalar payloads, and
/// text/blob bytes before copying. It excludes `Vec` spare capacity, allocator
/// bookkeeping, and SQLite transient memory, so it is not an RSS limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueryLimits {
    pub max_rows: usize,
    pub max_bytes: usize,
}

impl Default for QueryLimits {
    fn default() -> Self {
        Self {
            max_rows: 1_000,
            max_bytes: 1_048_576,
        }
    }
}

/// A successful WAL checkpoint observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    pub busy: bool,
    pub log_frames: u32,
    pub checkpointed_frames: u32,
}

/// Facade failure classification; no backend error type crosses this boundary.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("SQLite is busy")]
    Busy,
    #[error("refusing to overwrite {}", .0.display())]
    AlreadyExists(PathBuf),
    #[error("SQLite transaction is no longer active")]
    TransactionInactive,
    #[error("SQLite busy timeout exceeds the five-second facade maximum")]
    BusyTimeoutTooLarge,
    #[error("SQLite query limits must be nonzero")]
    InvalidLimits,
    #[error("SQLite result exceeds {max_rows} rows or {max_bytes} bytes")]
    ResultLimitExceeded { max_rows: usize, max_bytes: usize },
    #[error("SQLite integrity check failed: {0}")]
    IntegrityCheckFailed(String),
    #[error("{0}")]
    Io(io::Error),
    #[error("SQLite error: {0}")]
    Sqlite(String),
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

fn map_error(error: RawError) -> Error {
    if let RawError::SqliteFailure(code, _) = &error {
        if matches!(
            code.code,
            ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked
        ) {
            return Error::Busy;
        }
    }
    Error::Sqlite(error.to_string())
}

fn bind(values: &[Value]) -> Vec<Binding<'_>> {
    values.iter().map(Binding).collect()
}

fn value_from(reference: ValueRef<'_>) -> Result<Value, Error> {
    Ok(match reference {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(value) => Value::Integer(value),
        ValueRef::Real(value) => Value::Real(value),
        ValueRef::Text(value) => Value::Text(
            String::from_utf8(value.to_vec())
                .map_err(|error| Error::Sqlite(format!("invalid UTF-8 SQLite TEXT: {error}")))?,
        ),
        ValueRef::Blob(value) => Value::Blob(value.to_vec()),
    })
}

fn value_cost(reference: ValueRef<'_>) -> usize {
    match reference {
        ValueRef::Text(value) | ValueRef::Blob(value) => value.len(),
        _ => 8,
    }
}

fn query(
    raw: &RawConnection,
    sql: &str,
    values: &[Value],
    limits: QueryLimits,
) -> Result<Vec<Row>, Error> {
    if limits.max_rows == 0 || limits.max_bytes == 0 {
        return Err(Error::InvalidLimits);
    }
    let params = bind(values);
    let mut statement = raw.prepare(sql).map_err(map_error)?;
    let columns = statement.column_count();
    let mut cursor = statement
        .query(rusqlite::params_from_iter(params.iter()))
        .map_err(map_error)?;
    let mut used = 0usize;
    let mut output = Vec::new();
    while let Some(raw_row) = cursor.next().map_err(map_error)? {
        if output.len() == limits.max_rows {
            return Err(Error::ResultLimitExceeded {
                max_rows: limits.max_rows,
                max_bytes: limits.max_bytes,
            });
        }
        let mut row_cost =
            ROW_OVERHEAD
                .checked_add(columns.checked_mul(CELL_OVERHEAD).ok_or(
                    Error::ResultLimitExceeded {
                        max_rows: limits.max_rows,
                        max_bytes: limits.max_bytes,
                    },
                )?)
                .ok_or(Error::ResultLimitExceeded {
                    max_rows: limits.max_rows,
                    max_bytes: limits.max_bytes,
                })?;
        // Inspect SQLite's borrowed value before allocating Text or Blob.
        for index in 0..columns {
            row_cost = row_cost
                .checked_add(value_cost(raw_row.get_ref(index).map_err(map_error)?))
                .ok_or(Error::ResultLimitExceeded {
                    max_rows: limits.max_rows,
                    max_bytes: limits.max_bytes,
                })?;
        }
        used = used
            .checked_add(row_cost)
            .ok_or(Error::ResultLimitExceeded {
                max_rows: limits.max_rows,
                max_bytes: limits.max_bytes,
            })?;
        if used > limits.max_bytes {
            return Err(Error::ResultLimitExceeded {
                max_rows: limits.max_rows,
                max_bytes: limits.max_bytes,
            });
        }
        let mut row = Vec::with_capacity(columns);
        for index in 0..columns {
            row.push(value_from(raw_row.get_ref(index).map_err(map_error)?)?);
        }
        output.push(Row { values: row });
    }
    Ok(output)
}

/// One private SQLite connection with WAL, foreign keys, and a bounded busy wait.
pub struct Connection {
    raw: RawConnection,
}

impl Connection {
    /// Opens or creates a read-write database and applies kernel safety policy.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        Self::open_with_busy_timeout(path, BUSY_TIMEOUT)
    }

    /// Opens or creates a database with a caller-selected bounded busy wait.
    pub fn open_with_busy_timeout(
        path: impl AsRef<Path>,
        busy_timeout: Duration,
    ) -> Result<Self, Error> {
        if busy_timeout > MAX_BUSY_TIMEOUT {
            return Err(Error::BusyTimeoutTooLarge);
        }
        let raw = RawConnection::open(path).map_err(map_error)?;
        raw.busy_timeout(busy_timeout).map_err(map_error)?;
        raw.pragma_update(None, "foreign_keys", "ON")
            .map_err(map_error)?;
        raw.pragma_update(None, "journal_mode", "WAL")
            .map_err(map_error)?;
        Ok(Self { raw })
    }

    /// Opens an existing main database without creating schema or migrations.
    ///
    /// SQLite itself may create or update adjacent WAL bookkeeping files when
    /// the directory is writable; callers requiring strict filesystem
    /// immutability must enforce it at the filesystem boundary.
    pub fn open_read_only(path: impl AsRef<Path>) -> Result<Self, Error> {
        Self::open_read_only_with_busy_timeout(path, BUSY_TIMEOUT)
    }

    /// Opens an existing database read-only with a caller-selected bounded wait.
    pub fn open_read_only_with_busy_timeout(
        path: impl AsRef<Path>,
        busy_timeout: Duration,
    ) -> Result<Self, Error> {
        if busy_timeout > MAX_BUSY_TIMEOUT {
            return Err(Error::BusyTimeoutTooLarge);
        }
        let raw = RawConnection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(map_error)?;
        raw.busy_timeout(busy_timeout).map_err(map_error)?;
        Ok(Self { raw })
    }

    pub fn execute(&self, sql: &str, values: &[Value]) -> Result<usize, Error> {
        let params = bind(values);
        self.raw
            .execute(sql, rusqlite::params_from_iter(params.iter()))
            .map_err(map_error)
    }
    pub fn query(
        &self,
        sql: &str,
        values: &[Value],
        limits: QueryLimits,
    ) -> Result<Vec<Row>, Error> {
        query(&self.raw, sql, values, limits)
    }
    pub fn begin_immediate(&mut self) -> Result<Transaction<'_>, Error> {
        self.raw
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(map_error)?;
        Ok(Transaction {
            raw: &self.raw,
            active: true,
        })
    }
    /// Forces a bounded WAL checkpoint and reports whether another connection blocked it.
    pub fn checkpoint(&self) -> Result<Checkpoint, Error> {
        let rows = self.query(
            "PRAGMA wal_checkpoint(TRUNCATE)",
            &[],
            QueryLimits {
                max_rows: 1,
                max_bytes: 256,
            },
        )?;
        let values = rows
            .first()
            .and_then(|row| row.values().get(0..3))
            .ok_or_else(|| Error::Sqlite("invalid wal_checkpoint result".into()))?;
        let integer = |value: &Value| match value {
            Value::Integer(value) if *value >= 0 => Ok(*value as u32),
            _ => Err(Error::Sqlite("invalid wal_checkpoint result".into())),
        };
        Ok(Checkpoint {
            busy: integer(&values[0])? != 0,
            log_frames: integer(&values[1])?,
            checkpointed_frames: integer(&values[2])?,
        })
    }
    pub fn integrity_check(&self) -> Result<(), Error> {
        let rows = self.query(
            "PRAGMA integrity_check",
            &[],
            QueryLimits {
                max_rows: 2,
                max_bytes: 4096,
            },
        )?;
        match rows.first().and_then(|row| row.get(0)) {
            Some(Value::Text(value)) if value == "ok" => Ok(()),
            Some(Value::Text(value)) => Err(Error::IntegrityCheckFailed(value.clone())),
            _ => Err(Error::Sqlite("invalid integrity_check result".into())),
        }
    }
    /// Creates a new, SQLite-consistent backup. Existing destinations are never touched.
    pub fn backup_to(&self, destination: impl AsRef<Path>) -> Result<(), Error> {
        let destination = destination.as_ref();
        if fs::symlink_metadata(destination).is_ok() {
            return Err(Error::AlreadyExists(destination.to_path_buf()));
        }
        let parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let file_name = destination
            .file_name()
            .ok_or_else(|| Error::Sqlite("backup destination has no file name".into()))?;
        let staging = tempfile::Builder::new()
            .prefix(&format!(
                ".{}.kernal-api-backup-",
                file_name.to_string_lossy()
            ))
            .tempdir_in(parent)
            .map_err(Error::Io)?;
        let staged_database = staging.path().join("database.sqlite");
        let outcome = (|| {
            {
                let mut target = RawConnection::open(&staged_database).map_err(map_error)?;
                let backup = Backup::new(&self.raw, &mut target).map_err(map_error)?;
                let deadline = Instant::now() + Duration::from_secs(1);
                let mut busy_retries = 0;
                loop {
                    if Instant::now() >= deadline {
                        return Err(Error::Busy);
                    }
                    match backup.step(100).map_err(map_error)? {
                        StepResult::Done => break,
                        StepResult::More => busy_retries = 0,
                        StepResult::Busy | StepResult::Locked => {
                            busy_retries += 1;
                            if busy_retries == BACKUP_MAX_RETRIES {
                                return Err(Error::Busy);
                            }
                            thread::sleep(BACKUP_RETRY_DELAY);
                        }
                        _ => return Err(Error::Sqlite("unsupported SQLite backup state".into())),
                    }
                }
            }
            // The target is closed before publishing, and sync makes the new
            // standalone snapshot durable before its no-replace publication.
            fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&staged_database)?
                .sync_all()?;
            Ok(())
        })();
        if outcome.is_ok() {
            if let Err(error) = fs::hard_link(&staged_database, destination) {
                return if error.kind() == io::ErrorKind::AlreadyExists {
                    Err(Error::AlreadyExists(destination.to_path_buf()))
                } else {
                    Err(error.into())
                };
            }
        }
        outcome
    }
}

/// An immediate transaction which rolls back when dropped or when an operation fails.
pub struct Transaction<'connection> {
    raw: &'connection RawConnection,
    active: bool,
}

impl Transaction<'_> {
    fn abort(&mut self) {
        if self.active {
            let _ = self.raw.execute_batch("ROLLBACK");
            self.active = false;
        }
    }
    fn result<T>(&mut self, result: Result<T, Error>) -> Result<T, Error> {
        if result.is_err() {
            self.abort();
        }
        result
    }
    fn require_active(&self) -> Result<(), Error> {
        if self.active {
            Ok(())
        } else {
            Err(Error::TransactionInactive)
        }
    }
    pub fn execute(&mut self, sql: &str, values: &[Value]) -> Result<usize, Error> {
        self.require_active()?;
        self.result({
            let params = bind(values);
            self.raw
                .execute(sql, rusqlite::params_from_iter(params.iter()))
                .map_err(map_error)
        })
    }
    pub fn query(
        &mut self,
        sql: &str,
        values: &[Value],
        limits: QueryLimits,
    ) -> Result<Vec<Row>, Error> {
        self.require_active()?;
        self.result(query(self.raw, sql, values, limits))
    }
    pub fn commit(mut self) -> Result<(), Error> {
        self.raw.execute_batch("COMMIT").map_err(map_error)?;
        self.active = false;
        Ok(())
    }
    pub fn rollback(mut self) -> Result<(), Error> {
        self.raw.execute_batch("ROLLBACK").map_err(map_error)?;
        self.active = false;
        Ok(())
    }
}
impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        self.abort();
    }
}
