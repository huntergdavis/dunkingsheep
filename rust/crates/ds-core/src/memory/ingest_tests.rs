//! `memory::ingest` tests, ported from `tests/test_dunk_memory.py` (queue/lease/partial
//! cases) plus the pure helpers. The harness parsers are not in this slice.

use std::io::Write;

use serde_json::json;

use super::*;

fn store() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().expect("tempdir");
    let connection = crate::db::open_writer(&dir.path().join("memory.db")).expect("writer");
    apply_full_schema(&connection).expect("schema");
    (dir, connection)
}

fn sample(dir: &tempfile::TempDir, name: &str) -> String {
    let path = dir.path().join(name);
    let mut file = std::fs::File::create(&path).expect("create");
    writeln!(file, "{{\"type\":\"user\"}}").expect("write");
    path.to_string_lossy().into_owned()
}

fn count(connection: &Connection, table: &str, state: &str) -> i64 {
    connection
        .query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE state=?"),
            [state],
            |row| row.get(0),
        )
        .expect("count")
}

#[test]
fn enqueue_queues_a_pending_source() {
    let (dir, mut db) = store();
    let path = sample(&dir, "q.jsonl");
    enqueue(&mut db, &path, Some("generic"), Some("demo")).expect("enqueue");
    assert_eq!(1, count(&db, "ingest_queue", "queued"));
    assert_eq!(1, count(&db, "sources", "pending"));
    assert!(enqueue(&mut db, "/no/such/file.jsonl", None, None).is_err());
}

#[test]
fn stopped_ingest_is_skipped_not_failed() {
    let (dir, mut db) = store();
    let path = sample(&dir, "stop.jsonl");
    enqueue(&mut db, &path, Some("generic"), Some("demo")).expect("enqueue");
    let lease = claim_next(&mut db, 120.0).expect("claim").expect("lease");
    finish(
        &mut db,
        &lease,
        Outcome::Skipped("stopped by operator"),
        &json!({}),
    )
    .expect("finish");
    assert_eq!(1, count(&db, "ingest_queue", "skipped"));
    assert_eq!(0, count(&db, "ingest_queue", "failed"));
    assert_eq!(1, count(&db, "sources", "partial"));
    enqueue(&mut db, &path, Some("generic"), Some("demo")).expect("resume");
    assert_eq!(1, count(&db, "ingest_queue", "queued"));
}

#[test]
fn recover_leases_requeues_an_expired_ingest() {
    let (dir, mut db) = store();
    let path = sample(&dir, "lease.jsonl");
    enqueue(&mut db, &path, Some("generic"), Some("demo")).expect("enqueue");
    claim_next(&mut db, 0.01).expect("claim").expect("lease");
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert_eq!(1, recover_leases(&mut db).expect("recover"));
    assert_eq!(1, count(&db, "ingest_queue", "queued"));
}

#[test]
fn recover_requeues_a_partial_source_from_a_stop() {
    let (dir, mut db) = store();
    let path = sample(&dir, "partial.jsonl");
    enqueue(&mut db, &path, Some("generic"), Some("demo")).expect("enqueue");
    let lease = claim_next(&mut db, 120.0).expect("claim").expect("lease");
    finish(
        &mut db,
        &lease,
        Outcome::Skipped("stopped by operator"),
        &json!({}),
    )
    .expect("finish");
    assert_eq!(1, requeue_partial(&mut db).expect("requeue"));
    assert_eq!(1, count(&db, "ingest_queue", "queued"));
}

#[test]
fn failure_backs_off_and_success_records_an_event() {
    let (dir, mut db) = store();
    let path = sample(&dir, "f.jsonl");
    enqueue(&mut db, &path, None, None).expect("enqueue");
    let lease = claim_next(&mut db, 120.0).expect("claim").expect("lease");
    finish(&mut db, &lease, Outcome::Failed("boom"), &json!({})).expect("finish");
    assert_eq!(1, count(&db, "ingest_queue", "failed"));
    // next_at is ~10s out (5 * 2^1), so nothing is claimable yet
    assert!(claim_next(&mut db, 120.0).expect("claim").is_none());
    db.execute("UPDATE ingest_queue SET next_at=0", [])
        .expect("age");
    let lease = claim_next(&mut db, 120.0).expect("claim").expect("again");
    assert_eq!(1, lease.attempts);
    finish(&mut db, &lease, Outcome::Ok, &json!({"chunks": 3})).expect("ok");
    let hits: i64 = db
        .query_row(
            "SELECT hits FROM memory_events WHERE event='ingest'",
            [],
            |r| r.get(0),
        )
        .expect("event");
    assert_eq!(3, hits);
}

#[test]
fn chunks_are_redacted_deduped_and_searchable() {
    let (_dir, mut db) = store();
    upsert_session(
        &db,
        "s1",
        "generic",
        Some("demo"),
        None,
        None,
        None,
        None,
        None,
    )
    .expect("session");
    let chunks = vec!["flux capacitor token sk-abcdefghijklmnopqrstuvwxyz123456".to_string()];
    assert_eq!(
        1,
        insert_chunks(&mut db, "s1", &chunks, Some("user"), None, None, None).expect("insert")
    );
    assert_eq!(
        0,
        insert_chunks(&mut db, "s1", &chunks, Some("user"), None, None, None).expect("dupe")
    );
    let text: String = db
        .query_row("SELECT text FROM chunks", [], |r| r.get(0))
        .expect("text");
    assert!(!text.contains("sk-abcdefghijkl"), "{text}");
    let hits: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM chunks_fts WHERE chunks_fts MATCH 'capacitor'",
            [],
            |r| r.get(0),
        )
        .expect("fts");
    assert_eq!(1, hits);
}

#[test]
fn helpers_match_the_python() {
    assert_eq!(
        "codex",
        harness_from_path("/home/x/.codex/sessions/a.jsonl")
    );
    assert_eq!(
        "claude",
        harness_from_path("/home/x/.claude/projects/a.jsonl")
    );
    assert_eq!("opencode", harness_from_path("/x/opencode.db"));
    assert_eq!("generic", harness_from_path("/tmp/a.jsonl"));
    assert_eq!(Some("proj".to_string()), project_from_cwd(Some("/a/proj/")));
    assert_eq!(None, project_from_cwd(None));
    let content = json!([{"type":"thinking","text":"hmm"},{"text":"a"},"b",{"content":"c"}]);
    assert_eq!("a\nb\nc", text_from_content(&content));
}
