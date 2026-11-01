//! `memory::ingestor` tests, ported from `tests/test_dunk_memory.py`
//! (`test_ingestor_queue_and_status`, `test_stopped_ingest_is_skipped_not_failed` through the
//! loop, `test_recover_leases_requeues_an_expired_ingest`, pause/resume) plus the stop/restart
//! contract. Real files, a real database, a real worker thread; waits poll with a deadline.

use std::io::Write;
use std::time::{Duration, Instant};

use rusqlite::Connection;

use super::*;
use crate::memory::ingest::apply_full_schema;
use std::path::PathBuf;

const POLL: Duration = Duration::from_millis(20);

fn store() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("memory.db");
    apply_full_schema(&open_writer(&path).expect("writer")).expect("schema");
    (dir, path)
}

fn sample(dir: &tempfile::TempDir, name: &str, prefix: &str, sessions: usize) -> String {
    let path = dir.path().join(name);
    let mut file = std::fs::File::create(&path).expect("create");
    for session in 0..sessions {
        for index in 0..8 {
            writeln!(
                file,
                "{}",
                json!({"role": if index % 2 == 0 {"user"} else {"assistant"},
                       "text": format!("{prefix} note {index} about flux capacitor {prefix}-{session}-{index}"),
                       "session_id": format!("{prefix}-session-{session}"),
                       "cwd": "/home/hunter/workspace/demo",
                       "timestamp": 1780000000 + session * 100 + index})
            )
            .expect("write");
        }
    }
    path.to_string_lossy().into_owned()
}

fn count(path: &Path, sql: &str) -> i64 {
    Connection::open(path)
        .expect("open")
        .query_row(sql, [], |row| row.get(0))
        .expect("count")
}

/// Wait for `done`, failing only when `progress` stops changing for `STALL` (or after an
/// absolute `CAP`). A fixed wall-clock bound on work whose speed depends on the host (every
/// commit is an fsync) measured the host, not the code: under load 55-72 the resumed ingest of
/// the stop/resume test was still advancing (163 -> 168 chunks between two looks) when a fixed
/// 15 s ran out, and the test failed in 21 of 30 full-suite runs (t1461).
fn wait_for_progress(
    what: &str,
    mut done: impl FnMut() -> bool,
    mut progress: impl FnMut() -> i64,
) {
    const STALL: Duration = Duration::from_secs(20);
    const CAP: Duration = Duration::from_secs(300);
    let started = Instant::now();
    let (mut last, mut last_change) = (progress(), Instant::now());
    while started.elapsed() < CAP {
        if done() {
            return;
        }
        let now = progress();
        if now != last {
            (last, last_change) = (now, Instant::now());
        } else if last_change.elapsed() > STALL {
            panic!("timed out waiting for {what}: no progress for {STALL:?} (stuck at {last})");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {what}: still not done after {CAP:?} (progress {last})");
}

fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn the_ingestor_drains_the_queue_and_the_work_is_searchable() {
    let (dir, db) = store();
    let ingestor = Ingestor::new(&db, POLL, Some(Duration::ZERO));
    ingestor
        .enqueue(
            &sample(&dir, "q.jsonl", "q", 3),
            Some("generic"),
            Some("demo"),
        )
        .expect("enqueue");
    assert_eq!(
        1,
        count(
            &db,
            "SELECT COUNT(*) FROM ingest_queue WHERE state='queued'"
        )
    );
    ingestor.start().expect("start");
    wait_until("the queue to drain", || {
        count(&db, "SELECT COUNT(*) FROM ingest_queue WHERE state='done'") == 1
    });
    ingestor.stop();
    assert_eq!(
        0,
        count(
            &db,
            "SELECT COUNT(*) FROM ingest_queue WHERE state IN ('queued','leased','failed')"
        )
    );
    assert_eq!(24, count(&db, "SELECT COUNT(*) FROM chunks"));
    assert_eq!(
        1,
        count(
            &db,
            "SELECT COUNT(*) FROM chunks_fts WHERE chunks_fts MATCH 'capacitor' LIMIT 1"
        )
        .min(1)
    );
    let status = ingestor.status();
    assert!(!status.running);
    assert_eq!(None, status.last_error);
    assert_eq!(
        Some(24),
        status
            .last_result
            .as_ref()
            .and_then(|r| r["chunks"].as_i64())
    );
}

#[test]
fn pause_holds_the_queue_and_resume_drains_it() {
    let (dir, db) = store();
    let ingestor = Ingestor::new(&db, POLL, Some(Duration::ZERO));
    ingestor.pause();
    assert!(ingestor.paused());
    ingestor
        .enqueue(&sample(&dir, "p.jsonl", "p", 1), Some("generic"), None)
        .expect("enqueue");
    ingestor.start().expect("start");
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(
        1,
        count(
            &db,
            "SELECT COUNT(*) FROM ingest_queue WHERE state='queued'"
        ),
        "paused: nothing claimed"
    );
    assert!(ingestor.status().paused);
    ingestor.resume();
    assert!(!ingestor.paused());
    wait_until("resume to drain", || {
        count(&db, "SELECT COUNT(*) FROM ingest_queue WHERE state='done'") == 1
    });
    ingestor.stop();
}

#[test]
fn a_stop_mid_ingest_is_skipped_not_failed_and_the_next_start_resumes_it() {
    let (dir, db) = store();
    // t1440: DETERMINISTIC. The throttle is long enough that the worker is parked after the
    // first flushed session until we stop it, and the stop wakes the pause at once; no sleep
    // decides where the stop lands. A few sessions are plenty: the old 40 sessions (320 chunks,
    // ingested twice) took ~10 s in a debug build and failed under any load.
    let source = sample(&dir, "s.jsonl", "s", 6);
    let ingestor = Ingestor::new(&db, POLL, Some(Duration::from_secs(60)));
    ingestor
        .enqueue(&source, Some("generic"), Some("demo"))
        .expect("enqueue");
    ingestor.start().expect("start");
    wait_until("the first flushed session", || {
        count(&db, "SELECT COUNT(*) FROM chunks") > 0
    });
    let stopping = Instant::now();
    ingestor.stop();
    assert!(
        stopping.elapsed() < Duration::from_secs(10),
        "stop must not wait out the 60 s throttle (took {:?})",
        stopping.elapsed()
    );
    assert_eq!(
        1,
        count(
            &db,
            "SELECT COUNT(*) FROM ingest_queue WHERE state='skipped'"
        )
    );
    assert_eq!(
        0,
        count(
            &db,
            "SELECT COUNT(*) FROM ingest_queue WHERE state='failed'"
        )
    );
    assert_eq!(
        1,
        count(&db, "SELECT COUNT(*) FROM sources WHERE state='partial'")
    );
    let partial = count(&db, "SELECT COUNT(*) FROM chunks");
    assert!(
        partial > 0 && partial < 48,
        "the stop landed mid-file: {partial} of 48 chunks"
    );
    // a restart requeues the partial source and finishes it
    let again = Ingestor::new(&db, POLL, Some(Duration::ZERO));
    again.start().expect("start");
    wait_for_progress(
        "the resumed source",
        || count(&db, "SELECT COUNT(*) FROM ingest_queue WHERE state='done'") == 1,
        || count(&db, "SELECT COUNT(*) FROM chunks"),
    );
    again.stop();
    assert_eq!(48, count(&db, "SELECT COUNT(*) FROM chunks"));
}

#[test]
fn start_recovers_an_expired_lease() {
    let (dir, db) = store();
    let source = sample(&dir, "l.jsonl", "l", 1);
    {
        let mut connection = open_writer(&db).expect("writer");
        enqueue(&mut connection, &source, Some("generic"), None).expect("enqueue");
        claim_next(&mut connection, 0.01)
            .expect("claim")
            .expect("lease");
    }
    std::thread::sleep(Duration::from_millis(60));
    let ingestor = Ingestor::new(&db, POLL, Some(Duration::ZERO));
    ingestor.start().expect("start");
    wait_until("the recovered lease to finish", || {
        count(&db, "SELECT COUNT(*) FROM ingest_queue WHERE state='done'") == 1
    });
    ingestor.stop();
}

#[test]
fn a_failed_source_is_recorded_and_the_worker_stays_alive() {
    let (dir, db) = store();
    let bad = dir.path().join("notes.txt");
    std::fs::write(&bad, "x").expect("write");
    let ingestor = Ingestor::new(&db, POLL, Some(Duration::ZERO));
    ingestor
        .enqueue(bad.to_str().unwrap(), Some("generic"), None)
        .expect("enqueue");
    ingestor.start().expect("start");
    wait_until("the failure", || {
        count(
            &db,
            "SELECT COUNT(*) FROM ingest_queue WHERE state='failed'",
        ) == 1
    });
    assert!(
        ingestor.status().running,
        "a failure must not kill the worker"
    );
    assert!(ingestor
        .status()
        .last_error
        .expect("error")
        .starts_with("unsupported memory source"));
    // and a good source queued afterwards still gets done
    ingestor
        .enqueue(&sample(&dir, "ok.jsonl", "ok", 1), Some("generic"), None)
        .expect("enqueue");
    wait_until("the good source", || {
        count(&db, "SELECT COUNT(*) FROM ingest_queue WHERE state='done'") == 1
    });
    ingestor.stop();
}

#[test]
fn a_second_start_is_a_no_op_and_stop_is_idempotent() {
    let (_dir, db) = store();
    let ingestor = Ingestor::new(&db, POLL, Some(Duration::ZERO));
    ingestor.start().expect("start");
    ingestor.start().expect("second start");
    assert!(ingestor.status().running);
    ingestor.stop();
    ingestor.stop();
    assert!(!ingestor.status().running);
}

#[test]
fn the_throttle_comes_from_the_argument_or_the_default() {
    let (_dir, db) = store();
    assert_eq!(
        0.05,
        Ingestor::new(&db, POLL, Some(Duration::from_millis(50)))
            .status()
            .throttle_s
    );
    assert_eq!(
        0.0,
        Ingestor::new(&db, POLL, Some(Duration::ZERO))
            .status()
            .throttle_s
    );
}
