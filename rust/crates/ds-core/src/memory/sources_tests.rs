//! `memory::sources` tests. Expectations were produced by running the Python
//! `MemoryStore.ingest_path` on databases built from the SAME statements (2026-10-30).

use serde_json::json;

use super::*;
use crate::memory::ingest::apply_full_schema;

fn store() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().expect("tempdir");
    let connection = crate::db::open_writer(&dir.path().join("memory.db")).expect("writer");
    apply_full_schema(&connection).expect("schema");
    (dir, connection)
}

fn never() -> bool {
    false
}

fn codex_fixture(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let path = dir.path().join("cx_thread_history.sqlite");
    let db = Connection::open(&path).expect("open");
    db.execute_batch(
        "CREATE TABLE thread_items(thread_id, rollout_ordinal, created_at_ms, item_json)",
    )
    .expect("table");
    let rows = [
        (
            "t1",
            0,
            1780000000000_i64,
            json!({"type":"userMessage","content":"codex hist question"}).to_string(),
        ),
        (
            "t1",
            1,
            1780000001000,
            json!({"type":"agentMessage","content":[{"text":"hist answer"}]}).to_string(),
        ),
        (
            "t1",
            2,
            1780000002000,
            json!({"type":"toolCall"}).to_string(),
        ),
        ("t1", 3, 1780000003000, "not json".to_string()),
        (
            "t2",
            0,
            0,
            json!({"type":"agentMessage","content":"second thread"}).to_string(),
        ),
    ];
    for (thread, ordinal, ms, item) in rows {
        db.execute(
            "INSERT INTO thread_items VALUES (?,?,?,?)",
            rusqlite::params![thread, ordinal, ms, item],
        )
        .expect("row");
    }
    path
}

fn opencode_fixture(dir: &tempfile::TempDir) -> std::path::PathBuf {
    let path = dir.path().join("opencode.db");
    let db = Connection::open(&path).expect("open");
    db.execute_batch(
        "CREATE TABLE session(id,title,directory,time_created); \
         CREATE TABLE message(id,session_id,time_created,data); \
         CREATE TABLE part(id,message_id,time_created,data); \
         INSERT INTO session VALUES('s1','Oc title','/work/ocproj',1780000000000);",
    )
    .expect("tables");
    for (id, t, data) in [
        ("m1", 1780000000100_i64, json!({"role":"user"})),
        ("m2", 1780000000200, json!({"role":"assistant"})),
    ] {
        db.execute(
            "INSERT INTO message VALUES (?, 's1', ?, ?)",
            rusqlite::params![id, t, data.to_string()],
        )
        .expect("message");
    }
    for (id, message, t, data) in [
        (
            "p1",
            "m1",
            1780000000150_i64,
            json!({"type":"text","text":"oc question"}),
        ),
        (
            "p2",
            "m2",
            1780000000250,
            json!({"type":"text","text":"oc answer"}),
        ),
        ("p3", "m2", 1780000000260, json!({"type":"tool"})),
        ("p4", "m2", 1780000000270, json!({"text":"no type part"})),
    ] {
        db.execute(
            "INSERT INTO part VALUES (?, ?, ?, ?)",
            rusqlite::params![id, message, t, data.to_string()],
        )
        .expect("part");
    }
    path
}

fn chunk_rows(db: &Connection) -> Vec<(String, i64, String, String, Option<f64>)> {
    let mut statement = db
        .prepare("SELECT session_id, ordinal, role, text, created_at FROM chunks ORDER BY id")
        .expect("prepare");
    statement
        .query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })
        .expect("query")
        .map(|r| r.expect("row"))
        .collect()
}

#[test]
fn codex_thread_history_matches_the_python_including_its_quirks() {
    let (dir, mut db) = store();
    let path = codex_fixture(&dir);
    let stats = ingest_path(
        &mut db,
        path.to_str().unwrap(),
        None,
        None,
        &never,
        Duration::ZERO,
    )
    .expect("ingest");
    assert_eq!(
        IngestStats {
            sessions: 2,
            chunks: 3,
            harness: "codex".into(),
            stopped: false
        },
        stats
    );
    // the flush stores role 'assistant' even for the user row
    let s = |text: &str, ordinal: i64, at: Option<f64>, id: &str| {
        (
            id.to_string(),
            ordinal,
            "assistant".to_string(),
            text.to_string(),
            at,
        )
    };
    assert_eq!(
        vec![
            s("codex hist question", 0, Some(1780000000.0), "t1"),
            s("hist answer", 1, Some(1780000000.0), "t1"),
            s("second thread", 0, None, "t2"),
        ],
        chunk_rows(&db)
    );
    let t1: (Option<f64>, Option<f64>) = db
        .query_row(
            "SELECT started_at, updated_at FROM sessions WHERE id='t1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("t1");
    assert_eq!((Some(1780000000.0), Some(1780000000.0)), t1);
    let t2: (Option<f64>, Option<f64>) = db
        .query_row(
            "SELECT started_at, updated_at FROM sessions WHERE id='t2'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("t2");
    assert_eq!(None, t2.0);
    assert!(t2.1.expect("updated_at is now, not NULL") > 1_700_000_000.0);
    let harness: String = db
        .query_row("SELECT harness FROM sources", [], |r| r.get(0))
        .expect("source");
    assert_eq!("codex", harness);
    let state: String = db
        .query_row("SELECT state FROM sources", [], |r| r.get(0))
        .expect("source");
    assert_eq!("ok", state);
}

#[test]
fn opencode_matches_the_python() {
    let (dir, mut db) = store();
    let path = opencode_fixture(&dir);
    let stats = ingest_path(
        &mut db,
        path.to_str().unwrap(),
        None,
        None,
        &never,
        Duration::ZERO,
    )
    .expect("ingest");
    assert_eq!(
        IngestStats {
            sessions: 1,
            chunks: 3,
            harness: "opencode".into(),
            stopped: false
        },
        stats
    );
    let a = |text: &str, ordinal: i64| {
        (
            "s1".to_string(),
            ordinal,
            "assistant".to_string(),
            text.to_string(),
            Some(1780000000.0),
        )
    };
    assert_eq!(
        vec![a("oc question", 0), a("oc answer", 1), a("no type part", 2)],
        chunk_rows(&db)
    );
    let session: (Option<String>, Option<String>, Option<String>, f64, f64) = db
        .query_row(
            "SELECT project, cwd, title, started_at, updated_at FROM sessions WHERE id='s1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .expect("session");
    assert_eq!(
        (
            Some("ocproj".to_string()),
            Some("/work/ocproj".to_string()),
            Some("Oc title".to_string()),
            1780000000.0,
            1780000000.0
        ),
        session
    );
}

#[test]
fn an_unsupported_extension_is_refused_by_name() {
    let (dir, mut db) = store();
    let path = dir.path().join("notes.txt");
    std::fs::write(&path, "x").expect("write");
    let error = ingest_path(
        &mut db,
        path.to_str().unwrap(),
        None,
        None,
        &never,
        Duration::ZERO,
    )
    .unwrap_err();
    assert!(
        error.to_string().starts_with("unsupported memory source: "),
        "{error}"
    );
}

#[test]
fn a_jsonl_path_dispatches_and_records_the_source() {
    let (dir, mut db) = store();
    let path = dir.path().join("g.jsonl");
    std::fs::write(
        &path,
        "{\"role\":\"user\",\"text\":\"hello\",\"session_id\":\"g\"}\n",
    )
    .expect("write");
    let stats = ingest_path(
        &mut db,
        path.to_str().unwrap(),
        Some("generic"),
        Some("demo"),
        &never,
        Duration::ZERO,
    )
    .expect("ingest");
    assert_eq!((1, 1), (stats.sessions, stats.chunks));
    let (harness, project): (String, Option<String>) = db
        .query_row("SELECT harness, project FROM sources", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .expect("source");
    assert_eq!(
        ("generic".to_string(), Some("demo".to_string())),
        (harness, project)
    );
}
