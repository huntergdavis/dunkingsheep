//! T978: the synchronous handle over the async writer, proven by a real write.
//!
//! The acceptance: a write through the handle is visible to a FRESH ReadPool - i.e. a
//! store can keep the Python's blocking method shape while the writer actor stays async.

use ds_core::db::{map_sqlite, ReadPool, SyncWriter};
use ds_core::error::DunkError;

#[test]
fn a_write_through_the_sync_handle_is_visible_to_a_fresh_read_pool() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("events.db");
    let writer = SyncWriter::spawn(&path).expect("writer");
    writer
        .call(|connection| {
            connection
                .execute(
                    concat!(
                        "CREATE TABLE events (id INTEGER PRIMARY KEY AUTOINCREMENT, ",
                        "kind TEXT NOT NULL)"
                    ),
                    [],
                )
                .map_err(map_sqlite)?;
            connection
                .execute("INSERT INTO events (kind) VALUES (?1)", ("message.sent",))
                .map_err(map_sqlite)?;
            Ok(())
        })
        .expect("write");
    // A pool opened AFTER the write must see it: WAL, nothing to invalidate.
    let pool = ReadPool::open(&path, 2).expect("pool");
    let rows: Vec<String> = pool
        .with(|connection| {
            let mut statement = connection
                .prepare("SELECT kind FROM events")
                .map_err(map_sqlite)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(map_sqlite)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(map_sqlite)?;
            Ok(rows)
        })
        .expect("read");
    assert_eq!(vec!["message.sent".to_string()], rows);
    writer.shutdown();
}

#[test]
fn a_failing_write_is_reported_not_swallowed() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("empty.db");
    let writer = SyncWriter::spawn(&path).expect("writer");
    let error = writer
        .call(|connection| {
            connection
                .execute("INSERT INTO no_such_table VALUES (1)", [])
                .map_err(map_sqlite)?;
            Ok(())
        })
        .expect_err("a missing table is a refusal");
    assert!(matches!(error, DunkError::Refused(_)), "{error:?}");
    assert!(error.to_string().contains("no such table"), "{error}");
    // The actor survives a failed job and still serves the next one.
    let count: i64 = writer
        .call(|connection| {
            connection
                .query_row("SELECT COUNT(*) FROM sqlite_master", [], |row| row.get(0))
                .map_err(map_sqlite)
        })
        .expect("the writer is still alive");
    assert!(count >= 0);
    writer.shutdown();
}

#[test]
fn writes_serialise_through_the_one_actor() {
    let directory = tempfile::tempdir().expect("tempdir");
    let path = directory.path().join("serial.db");
    let writer = SyncWriter::spawn(&path).expect("writer");
    writer
        .call(|connection| {
            connection
                .execute("CREATE TABLE n (v INTEGER NOT NULL)", [])
                .map_err(map_sqlite)?;
            Ok(())
        })
        .expect("schema");
    for value in 0..25 {
        writer
            .call(move |connection| {
                connection
                    .execute("INSERT INTO n (v) VALUES (?1)", (value,))
                    .map_err(map_sqlite)?;
                Ok(())
            })
            .expect("insert");
    }
    let pool = ReadPool::open(&path, 1).expect("pool");
    let total: i64 = pool
        .with(|connection| {
            connection
                .query_row("SELECT COUNT(*) FROM n", [], |row| row.get(0))
                .map_err(map_sqlite)
        })
        .expect("count");
    assert_eq!(25, total);
    writer.shutdown();
}
