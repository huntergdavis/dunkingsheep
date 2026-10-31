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

/// The property the t1006 fix gives: opening a second writer beside an active write
/// transaction succeeds.
///
/// HONEST SCOPE: this is NOT a deterministic reproduction of the flake. Measured before
/// the fix, `two_writers_serialize_their_transactions` failed 1 run in 20 with
/// `writer: Busy` inside `Writer::spawn`; this test holds a transaction and opens a
/// second writer, which exercises the same code path but passes on the OLD code too
/// (checked), because the window is probabilistic. The evidence that the fix works is
/// the rate: 1 failure in 20 runs before, 0 in 30 after. What this test does pin is that
/// the busy path does not regress into a hard failure whenever a holder is present.
#[test]
fn a_writer_opened_while_another_holds_the_database_waits_instead_of_failing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join("open-race.db");
    table(&path).expect("schema");
    // Hold a write transaction open on one connection...
    let holder = open_writer(&path).expect("holder");
    holder.execute_batch("BEGIN IMMEDIATE").expect("begin");
    holder
        .execute("UPDATE counter SET n = n + 1 WHERE id = 1", [])
        .expect("write inside the transaction");
    // ...and open a second writer WHILE it is open. Before t1006 this returned Busy
    // about one time in twenty; it must now wait for the holder and succeed.
    let opened = open_writer(&path);
    holder.execute_batch("COMMIT").expect("commit");
    assert!(
        opened.is_ok(),
        "opening a writer beside an active transaction failed: {opened:?}"
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

/// t1241: the pool used to hold its lock across the read and use only the first
/// connection, so two readers could never be inside `with` at once. Each reader announces
/// itself and waits (bounded) for the other to arrive INSIDE the closure: on the old code the
/// second never arrives and the test FAILS after a second instead of hanging.
#[test]
fn two_reads_run_at_the_same_time() {
    use std::sync::atomic::AtomicUsize;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("pool.db");
    {
        let writer = open_writer(&path).expect("writer");
        writer
            .execute_batch("CREATE TABLE t (x INTEGER)")
            .expect("schema");
    }
    let pool = ReadPool::open(&path, 2).expect("pool");
    let inside = AtomicUsize::new(0);
    let overlapped: Vec<bool> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    pool.with(|_| {
                        inside.fetch_add(1, Ordering::SeqCst);
                        let until = Instant::now() + Duration::from_secs(1);
                        while inside.load(Ordering::SeqCst) < 2 && Instant::now() < until {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Ok(inside.load(Ordering::SeqCst) == 2)
                    })
                    .expect("read")
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("reader"))
            .collect()
    });
    assert_eq!(
        vec![true, true],
        overlapped,
        "two reads never overlapped: the pool is serialised"
    );
    assert_eq!(2, pool.size());
}
