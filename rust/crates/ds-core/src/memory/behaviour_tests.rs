//! The three read-side behaviours of `tests/test_dunk_memory.py` that no other Rust test covers:
//! `test_recall_never_ingests_or_enqueues`, `test_read_during_write_does_not_block` and
//! `test_parallel_recall_small`. They run against a real database through the real `ReadPool`
//! (a `query_only` connection), so "recall cannot write" is enforced by SQLite itself, not just
//! observed.

use std::io::Write;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use serde_json::json;

use super::ingest::apply_full_schema;
use super::jsonl::ingest_jsonl;
use super::recall::{recall, Filters};
use crate::db::{open_writer, ReadPool};

const NOW: f64 = 1_793_400_000.0;

fn loaded(sessions: usize, messages: usize) -> (tempfile::TempDir, std::path::PathBuf, ReadPool) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("memory.db");
    let mut writer = open_writer(&path).expect("writer");
    apply_full_schema(&writer).expect("schema");
    let source = dir.path().join("w.jsonl");
    let mut file = std::fs::File::create(&source).expect("create");
    for session in 0..sessions {
        for index in 0..messages {
            writeln!(
                file,
                "{}",
                json!({"role": if index % 2 == 0 {"user"} else {"assistant"},
                       "text": format!("t note {index} about flux capacitor t-{session}-{index}"),
                       "session_id": format!("t-session-{session}"),
                       "cwd": "/home/hunter/workspace/demo",
                       "timestamp": 1780000000 + session * 100 + index})
            )
            .expect("write");
        }
    }
    ingest_jsonl(
        &mut writer,
        &source,
        "generic",
        Some("demo"),
        &|| false,
        Duration::ZERO,
    )
    .expect("ingest");
    drop(writer);
    let pool = ReadPool::open(&path, 4).expect("pool");
    (dir, path, pool)
}

fn counts(path: &std::path::Path) -> Vec<i64> {
    let db = Connection::open(path).expect("open");
    [
        "sources",
        "ingest_queue",
        "sessions",
        "chunks",
        "memory_events",
    ]
    .iter()
    .map(|table| {
        db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .expect("count")
    })
    .collect()
}

#[test]
fn recall_never_ingests_or_enqueues() {
    let (_dir, path, pool) = loaded(3, 8);
    let before = counts(&path);
    let hits = recall(&pool, "flux capacitor", 10, &Filters::default(), NOW, 240).expect("recall");
    assert!(!hits.is_empty());
    assert_eq!(
        before,
        counts(&path),
        "a query must not add a source, queue row, session or chunk"
    );
    // and the pool cannot write at all: SQLite refuses it (query_only), not just our discipline
    let refused = pool.with(|c| {
        Ok(c.execute(
            "INSERT INTO ingest_queue(source_id, state, created_at) VALUES (1,'queued',0)",
            [],
        )
        .is_err())
    });
    assert!(
        refused.expect("pool"),
        "a read connection must refuse a write"
    );
    assert_eq!(before, counts(&path));
}

#[test]
fn a_read_during_a_write_does_not_block() {
    let (_dir, path, pool) = loaded(3, 8);
    let writer = open_writer(&path).expect("second writer");
    writer
        .execute_batch("BEGIN IMMEDIATE")
        .expect("hold the write lock");
    let started = Instant::now();
    let hits = recall(&pool, "flux capacitor", 5, &Filters::default(), NOW, 240).expect("recall");
    let elapsed = started.elapsed();
    writer.execute_batch("ROLLBACK").expect("release");
    assert!(!hits.is_empty());
    assert!(
        elapsed < Duration::from_millis(500),
        "a read queued behind a writer ({elapsed:?}); the stall is back"
    );
}

#[test]
fn parallel_recall_has_no_errors_and_a_bounded_tail() {
    let (_dir, _path, pool) = loaded(8, 20);
    let deadline = Instant::now() + Duration::from_secs(3);
    let results: Vec<(Vec<Duration>, usize)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    let (mut latencies, mut errors) = (Vec::new(), 0usize);
                    while Instant::now() < deadline {
                        let started = Instant::now();
                        if recall(&pool, "flux capacitor", 5, &Filters::default(), NOW, 240)
                            .is_err()
                        {
                            errors += 1;
                        }
                        latencies.push(started.elapsed());
                    }
                    (latencies, errors)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("worker"))
            .collect()
    });
    let errors: usize = results.iter().map(|(_, e)| e).sum();
    let mut all: Vec<Duration> = results.into_iter().flat_map(|(l, _)| l).collect();
    all.sort();
    assert_eq!(0, errors);
    assert!(!all.is_empty());
    let p95 = all[((all.len() - 1) as f64 * 0.95) as usize];
    let p50 = all[(all.len() - 1) / 2];
    // A tail bound has to be a bound on the TAIL, not on the host. Measured under host load
    // 55-72 (full ds-core runs, t1461) the eight workers were starved evenly: p50 280-440 ms
    // and p95 1.0-1.25 s, a ratio of 2.5-3.6, and the fixed 1 s failed 5 of 30 runs. A real
    // stall (a reader queued behind a writer) is a long tail over a short median, so the
    // bound is the old 1 s or ten medians, whichever is larger: it self-calibrates to how
    // fast this host runs this run. The writer-stall regression itself is pinned, in
    // absolute terms, by the 500 ms test above.
    let bound = Duration::from_secs(1).max(p50 * 10);
    assert!(
        p95 < bound,
        "p95 {p95:?} over {bound:?} (p50 {p50:?}, n {})",
        all.len()
    );
}
