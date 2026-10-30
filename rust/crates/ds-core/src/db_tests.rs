//! `db` tests (T05): the two rules the module exists for, plus the Python's
//! connection rules that a port silently loses.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rusqlite::Connection;

use super::*;

fn table(path: &Path) -> rusqlite::Result<()> {
    let connection = Connection::open(path)?;
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS counter (id INTEGER PRIMARY KEY CHECK (id = 1), n INTEGER NOT NULL);
         INSERT OR IGNORE INTO counter (id, n) VALUES (1, 0);",
    )?;
    Ok(())
}

#[test]
fn two_writers_serialize_their_transactions() {
    // Two actors on ONE file, each on its OWN runtime and OS thread - which is what
    // two daemons sharing a database look like. Sharing one test runtime made the
    // assertion about tokio's scheduler instead of about SQLite's locks.
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("two-writers.db");
    table(&path).expect("schema");
    let increments = 25i64;

    let worker = |path: std::path::PathBuf| {
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("runtime");
            runtime.block_on(async move {
                let writer = Writer::spawn(&path).expect("writer");
                for _ in 0..increments {
                    writer
                        .call(|connection| {
                            connection
                                .execute_batch("BEGIN IMMEDIATE")
                                .map_err(map_sqlite)?;
                            let current: i64 = connection
                                .query_row("SELECT n FROM counter WHERE id = 1", [], |row| {
                                    row.get(0)
                                })
                                .map_err(map_sqlite)?;
                            connection
                                .execute("UPDATE counter SET n = ? WHERE id = 1", [current + 1])
                                .map_err(map_sqlite)?;
                            connection.execute_batch("COMMIT").map_err(map_sqlite)?;
                            Ok(())
                        })
                        .await
                        .expect("increment");
                }
                writer.shutdown().await;
            });
        })
    };
    let first = worker(path.clone());
    let second = worker(path.clone());
    first.join().expect("first writer thread");
    second.join().expect("second writer thread");

    let reader = open_reader(&path).expect("reader");
    let total: i64 = reader
        .query_row("SELECT n FROM counter WHERE id = 1", [], |row| row.get(0))
        .expect("count");
    assert_eq!(
        increments * 2,
        total,
        "a transaction interleaved: {} of {} increments landed",
        total,
        increments * 2
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reader_never_blocks_on_an_open_write_transaction() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("reader-vs-writer.db");
    table(&path).expect("schema");
    let pool = ReadPool::open(&path, 2).expect("pool");

    // An uncommitted write transaction held by the WRITER's own connection.
    let writer = open_writer(&path).expect("writer connection");
    writer
        .execute_batch("BEGIN IMMEDIATE; UPDATE counter SET n = n + 1 WHERE id = 1;")
        .expect("hold an open write transaction");

    // WAL is the point: the reader sees the last committed snapshot and does not
    // wait for the writer to commit.
    let started = Instant::now();
    let value = tokio::task::spawn_blocking(move || {
        let read = pool.with(|connection| {
            connection
                .query_row("SELECT n FROM counter WHERE id = 1", [], |row| {
                    row.get::<_, i64>(0)
                })
                .map_err(map_sqlite)
        });
        (read, started.elapsed())
    })
    .await
    .expect("join");
    let (read, elapsed) = value;
    assert_eq!(0, read.expect("read"), "the read saw an uncommitted write");
    assert!(
        elapsed < Duration::from_secs(2),
        "the reader waited {elapsed:?} on an open write transaction"
    );
    writer.execute_batch("COMMIT").expect("commit");
}

#[test]
fn a_locked_database_maps_to_busy() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("busy.db");
    table(&path).expect("schema");

    // A short busy timeout, because the assertion is about the MAPPING: waiting out
    // the production five seconds inside a blocked writer proves nothing and hides a
    // hang. The blocker below holds a write lock for the duration.
    let writer = open_writer_with_timeout(&path, 50).expect("writer connection");
    let blocker = Connection::open(&path).expect("blocker");
    blocker
        .execute_batch("BEGIN EXCLUSIVE")
        .expect("exclusive lock");
    let refused = writer
        .execute("UPDATE counter SET n = n + 1 WHERE id = 1", [])
        .map_err(map_sqlite);
    assert!(
        matches!(refused, Err(DunkError::Busy)),
        "expected DunkError::Busy, got {refused:?}"
    );
    blocker.execute_batch("ROLLBACK").expect("unlock");
}

#[test]
fn a_hand_built_uri_is_refused_like_the_python() {
    let error = open_writer(Path::new("file:whatever.db?mode=ro")).expect_err("uri refused");
    assert!(
        error.to_string().contains("requires a filesystem path"),
        "unexpected message: {error}"
    );
}

#[test]
fn a_reader_cannot_write() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("readonly.db");
    table(&path).expect("schema");
    let pool = ReadPool::open(&path, 1).expect("pool");
    let refused = pool.with(|connection| {
        connection
            .execute("UPDATE counter SET n = 1 WHERE id = 1", [])
            .map_err(map_sqlite)
    });
    assert!(
        refused.is_err(),
        "a pooled read connection accepted a write"
    );
}

#[test]
fn a_reader_does_not_create_a_missing_database() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("absent.db");
    assert!(
        open_reader(&path).is_err(),
        "opening a missing db for read must fail"
    );
    assert!(!path.exists(), "the read-only open created the file");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_reader_in_the_pool_is_usable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("pool.db");
    table(&path).expect("schema");
    let pool = Arc::new(ReadPool::open(&path, DEFAULT_READ_POOL).expect("pool"));
    assert_eq!(DEFAULT_READ_POOL, pool.size());
    let seen = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();
    for _ in 0..DEFAULT_READ_POOL {
        let pool = Arc::clone(&pool);
        let seen = Arc::clone(&seen);
        handles.push(tokio::task::spawn_blocking(move || {
            let value: i64 = pool
                .with(|connection| {
                    connection
                        .query_row("SELECT n FROM counter WHERE id = 1", [], |row| row.get(0))
                        .map_err(map_sqlite)
                })
                .expect("read");
            seen.fetch_add(value as u64, Ordering::SeqCst);
        }));
    }
    for handle in handles {
        handle.await.expect("join");
    }
    assert_eq!(0, seen.load(Ordering::SeqCst));
}
