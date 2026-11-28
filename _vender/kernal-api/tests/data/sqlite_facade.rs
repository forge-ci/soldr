#![cfg(feature = "sqlite")]

use kernal_api::sqlite::{Connection, Error, QueryLimits, Row, Value, MAX_BUSY_TIMEOUT};
use std::{mem::size_of, time::Duration};
use tempfile::tempdir;

fn database() -> (tempfile::TempDir, std::path::PathBuf, Connection) {
    let directory = tempdir().unwrap();
    let path = directory.path().join("state.sqlite");
    let connection = Connection::open(&path).unwrap();
    (directory, path, connection)
}

#[test]
fn transaction_rolls_back_on_drop_and_error() {
    let (_directory, _path, mut connection) = database();
    connection
        .execute("CREATE TABLE values_table (value INTEGER UNIQUE)", &[])
        .unwrap();
    {
        let mut transaction = connection.begin_immediate().unwrap();
        transaction
            .execute("INSERT INTO values_table VALUES (?)", &[Value::Integer(7)])
            .unwrap();
        assert!(transaction
            .execute("INSERT INTO values_table VALUES (?)", &[Value::Integer(7)])
            .is_err());
        assert!(transaction
            .execute("INSERT INTO values_table VALUES (?)", &[Value::Integer(8)])
            .is_err());
    }
    assert_eq!(
        connection
            .query(
                "SELECT value FROM values_table",
                &[],
                QueryLimits::default()
            )
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn transaction_commit_and_checkpoint_are_explicit() {
    let (_directory, _path, mut connection) = database();
    connection
        .execute("CREATE TABLE values_table (value INTEGER)", &[])
        .unwrap();
    let mut transaction = connection.begin_immediate().unwrap();
    transaction
        .execute("INSERT INTO values_table VALUES (7)", &[])
        .unwrap();
    transaction.commit().unwrap();
    assert_eq!(
        connection
            .query(
                "SELECT value FROM values_table",
                &[],
                QueryLimits::default()
            )
            .unwrap()
            .len(),
        1
    );
    assert!(!connection.checkpoint().unwrap().busy);
}

#[test]
fn reader_remains_independent_while_writer_is_open() {
    let (_directory, path, mut writer) = database();
    writer
        .execute("CREATE TABLE values_table (value INTEGER)", &[])
        .unwrap();
    writer
        .execute("INSERT INTO values_table VALUES (1)", &[])
        .unwrap();
    let mut transaction = writer.begin_immediate().unwrap();
    transaction
        .execute("INSERT INTO values_table VALUES (2)", &[])
        .unwrap();
    let reader = Connection::open_read_only(&path).unwrap();
    assert_eq!(
        reader
            .query(
                "SELECT value FROM values_table",
                &[],
                QueryLimits::default()
            )
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn immediate_writer_exclusion_is_busy() {
    let (_directory, path, mut first) = database();
    first
        .execute("CREATE TABLE values_table (value INTEGER)", &[])
        .unwrap();
    let _transaction = first.begin_immediate().unwrap();
    let mut second = Connection::open(&path).unwrap();
    assert!(matches!(second.begin_immediate(), Err(Error::Busy)));
}

#[test]
fn readonly_never_creates_or_mutates() {
    let directory = tempdir().unwrap();
    let missing = directory.path().join("missing.sqlite");
    assert!(Connection::open_read_only(&missing).is_err());
    assert!(!missing.exists());
    let (_directory, path, connection) = database();
    connection
        .execute("CREATE TABLE values_table (value INTEGER)", &[])
        .unwrap();
    let readonly = Connection::open_read_only(&path).unwrap();
    assert!(readonly
        .execute("INSERT INTO values_table VALUES (1)", &[])
        .is_err());
}

#[test]
fn backup_captures_wal_state_and_never_overwrites() {
    let (directory, path, connection) = database();
    connection
        .execute("CREATE TABLE values_table (value INTEGER)", &[])
        .unwrap();
    connection
        .execute("INSERT INTO values_table VALUES (7)", &[])
        .unwrap();
    let backup = directory.path().join("backup.sqlite");
    connection.backup_to(&backup).unwrap();
    assert_eq!(
        Connection::open_read_only(&backup)
            .unwrap()
            .query(
                "SELECT value FROM values_table",
                &[],
                QueryLimits::default()
            )
            .unwrap()
            .len(),
        1
    );
    assert!(matches!(
        connection.backup_to(&backup),
        Err(Error::AlreadyExists(_))
    ));
    assert!(path.exists());
}

#[test]
fn backup_allows_more_than_ten_successful_page_batches() {
    let (directory, _path, connection) = database();
    connection
        .execute("CREATE TABLE values_table (value BLOB)", &[])
        .unwrap();
    // More than 1,000 SQLite pages: this proves progress is not counted as
    // contention retries.
    connection
        .execute("INSERT INTO values_table VALUES (zeroblob(5000000))", &[])
        .unwrap();
    let backup = directory.path().join("large-backup.sqlite");
    connection.backup_to(&backup).unwrap();
    assert_eq!(
        Connection::open_read_only(&backup)
            .unwrap()
            .query(
                "SELECT length(value) FROM values_table",
                &[],
                QueryLimits::default()
            )
            .unwrap()[0]
            .get(0),
        Some(&Value::Integer(5_000_000))
    );
}

#[test]
fn query_limits_fail_instead_of_truncating() {
    let (_directory, _path, connection) = database();
    connection
        .execute("CREATE TABLE values_table (value TEXT)", &[])
        .unwrap();
    connection
        .execute(
            "INSERT INTO values_table VALUES (?)",
            &[Value::Text("four".into())],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO values_table VALUES (?)",
            &[Value::Text("five".into())],
        )
        .unwrap();
    assert!(matches!(
        connection.query(
            "SELECT value FROM values_table",
            &[],
            QueryLimits {
                max_rows: 1,
                max_bytes: 128
            }
        ),
        Err(Error::ResultLimitExceeded { .. })
    ));
    assert!(matches!(
        connection.query(
            "SELECT value FROM values_table",
            &[],
            QueryLimits {
                max_rows: 1,
                max_bytes: 3
            }
        ),
        Err(Error::ResultLimitExceeded { .. })
    ));
}

#[test]
fn foreign_keys_and_integrity_are_enforced() {
    let (_directory, _path, connection) = database();
    connection
        .execute("CREATE TABLE parent (id INTEGER PRIMARY KEY)", &[])
        .unwrap();
    connection
        .execute(
            "CREATE TABLE child (parent_id INTEGER REFERENCES parent(id) ON DELETE CASCADE)",
            &[],
        )
        .unwrap();
    connection
        .execute("INSERT INTO parent VALUES (1)", &[])
        .unwrap();
    connection
        .execute("INSERT INTO child VALUES (1)", &[])
        .unwrap();
    connection
        .execute("DELETE FROM parent WHERE id = 1", &[])
        .unwrap();
    assert_eq!(
        connection
            .query("SELECT parent_id FROM child", &[], QueryLimits::default())
            .unwrap()
            .len(),
        0
    );
    connection.integrity_check().unwrap();
}

#[test]
fn values_are_facade_owned() {
    let (_directory, _path, connection) = database();
    let values = [
        Value::Null,
        Value::Integer(-7),
        Value::Real(1.25),
        Value::Text("text".into()),
        Value::Blob(vec![1, 2]),
    ];
    let rows = connection
        .query("SELECT ?, ?, ?, ?, ?", &values, QueryLimits::default())
        .unwrap();
    assert_eq!(rows[0].values(), &values);
    assert!(rows[0].get(5).is_none());
}

#[test]
fn bundled_sqlite_contains_the_wal_reset_fix() {
    let (_directory, _path, connection) = database();
    let rows = connection
        .query("SELECT sqlite_version()", &[], QueryLimits::default())
        .unwrap();
    let version = match rows[0].get(0) {
        Some(Value::Text(version)) => version,
        other => panic!("sqlite_version() returned {other:?}"),
    };
    let mut parts = version.split('.').map(|part| part.parse::<u32>().unwrap());
    let actual = (
        parts.next().unwrap(),
        parts.next().unwrap(),
        parts.next().unwrap(),
    );
    assert!(
        actual >= (3, 51, 3),
        "SQLite {version} predates the WAL-reset fix"
    );
}

#[test]
fn query_limits_charge_empty_multicolumn_rows_exactly() {
    let (_directory, _path, connection) = database();
    let exact = size_of::<Row>() + (2 * size_of::<Value>()) + 16;
    assert!(connection
        .query(
            "SELECT NULL, NULL",
            &[],
            QueryLimits {
                max_rows: 1,
                max_bytes: exact
            }
        )
        .is_ok());
    assert!(matches!(
        connection.query(
            "SELECT NULL, NULL",
            &[],
            QueryLimits {
                max_rows: 1,
                max_bytes: exact - 1
            }
        ),
        Err(Error::ResultLimitExceeded { .. })
    ));
}

#[test]
fn busy_timeout_is_configurable_but_bounded() {
    let (directory, _path, _connection) = database();
    let second = directory.path().join("second.sqlite");
    Connection::open_with_busy_timeout(&second, MAX_BUSY_TIMEOUT).unwrap();
    assert!(matches!(
        Connection::open_with_busy_timeout(&second, MAX_BUSY_TIMEOUT + Duration::from_millis(1)),
        Err(Error::BusyTimeoutTooLarge)
    ));
}
