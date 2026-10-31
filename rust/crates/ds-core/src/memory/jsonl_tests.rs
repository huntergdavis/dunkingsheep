//! `memory::jsonl` tests. The claude/codex/pi expectations were produced by running the
//! Python `MemoryStore.ingest_path` on the SAME fixture lines (2026-10-30) and pinning what
//! it stored: session rows (project/cwd/title/started/updated) and chunk rows
//! (role/text/created_at). The generic case is `write_jsonl` from tests/test_dunk_memory.py.

use std::io::Write;

use serde_json::json;

use super::*;
use crate::memory::ingest::apply_full_schema;

fn store() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().expect("tempdir");
    let connection = crate::db::open_writer(&dir.path().join("memory.db")).expect("writer");
    apply_full_schema(&connection).expect("schema");
    (dir, connection)
}

fn write(
    dir: &tempfile::TempDir,
    name: &str,
    lines: &[serde_json::Value],
    tail: &str,
) -> std::path::PathBuf {
    let path = dir.path().join(name);
    let mut file = std::fs::File::create(&path).expect("create");
    for line in lines {
        writeln!(file, "{line}").expect("write");
    }
    write!(file, "{tail}").expect("tail");
    path
}

fn never() -> bool {
    false
}

type SessionRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    f64,
    f64,
);

fn sessions(db: &Connection) -> Vec<SessionRow> {
    let mut statement = db
        .prepare("SELECT id, harness, project, cwd, title, started_at, updated_at FROM sessions ORDER BY id")
        .expect("prepare");
    statement
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        })
        .expect("query")
        .map(|r| r.expect("row"))
        .collect()
}

fn chunks(db: &Connection) -> Vec<(String, i64, String, String, f64)> {
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

const JUNK: &str = "not json\n\n[1]\n";

#[test]
fn claude_matches_the_python() {
    let (dir, mut db) = store();
    let path = write(
        &dir,
        "sess-file.jsonl",
        &[
            json!({"type":"ai-title","title":"Fix the flux"}),
            json!({"type":"user","message":{"role":"user","content":"hello flux capacitor"},"sessionId":"c-1","cwd":"/work/demo/","timestamp":"2026-10-30T10:00:00Z","slug":"s"}),
            json!({"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","text":"hm"},{"type":"text","text":"reply one"}]},"sessionId":"c-1","cwd":"/work/demo/","timestamp":"2026-10-30T10:00:05.500Z"}),
            json!({"type":"system"}),
        ],
        JUNK,
    );
    let stats =
        ingest_jsonl(&mut db, &path, "claude", None, &never, Duration::ZERO).expect("ingest");
    assert_eq!(
        IngestStats {
            sessions: 1,
            chunks: 2,
            harness: "claude".into(),
            stopped: false
        },
        stats
    );
    assert_eq!(
        vec![(
            "c-1".to_string(),
            "claude".to_string(),
            Some("demo".to_string()),
            Some("/work/demo/".to_string()),
            Some("Fix the flux".to_string()),
            1793354400.0,
            1793354405.5
        )],
        sessions(&db)
    );
    assert_eq!(
        vec![
            (
                "c-1".to_string(),
                0,
                "user".to_string(),
                "hello flux capacitor".to_string(),
                1793354400.0
            ),
            (
                "c-1".to_string(),
                0,
                "assistant".to_string(),
                "reply one".to_string(),
                1793354400.0
            ),
        ],
        chunks(&db)
    );
}

#[test]
fn codex_matches_the_python() {
    let (dir, mut db) = store();
    let path = write(
        &dir,
        "sess-file.jsonl",
        &[
            json!({"type":"session_meta","payload":{"id":"x-1","cwd":"/work/cx","timestamp":"2026-10-30T11:00:00Z","originator":"codex_cli"},"timestamp":"2026-10-30T11:00:00Z"}),
            json!({"type":"turn_context","payload":{"cwd":"/work/cx2"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"codex question flux"}]},"timestamp":"2026-10-30T11:00:01Z"}),
            json!({"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"codex answer"}]},"timestamp":1780000000}),
            json!({"type":"response_item","payload":{"type":"function_call"}}),
        ],
        JUNK,
    );
    let stats =
        ingest_jsonl(&mut db, &path, "codex", None, &never, Duration::ZERO).expect("ingest");
    assert_eq!((1, 2), (stats.sessions, stats.chunks));
    assert_eq!(
        vec![(
            "x-1".to_string(),
            "codex".to_string(),
            Some("cx2".to_string()),
            Some("/work/cx2".to_string()),
            Some("codex_cli".to_string()),
            1793358000.0,
            1793358001.0
        )],
        sessions(&db)
    );
    assert_eq!(
        vec![
            (
                "x-1".to_string(),
                0,
                "user".to_string(),
                "codex question flux".to_string(),
                1793358001.0
            ),
            (
                "x-1".to_string(),
                0,
                "assistant".to_string(),
                "codex answer".to_string(),
                1793358001.0
            ),
        ],
        chunks(&db)
    );
}

#[test]
fn pi_matches_the_python() {
    let (dir, mut db) = store();
    let path = write(
        &dir,
        "sess-file.jsonl",
        &[
            json!({"type":"session","id":"p-1","cwd":"/work/pi","timestamp":"2026-10-30T12:00:00Z"}),
            json!({"type":"message","message":{"role":"user","content":"pi hello"},"timestamp":"2026-10-30T12:00:01Z"}),
            json!({"type":"message","message":{"role":"toolResult","content":"skip"}}),
            json!({"type":"message","message":{"role":"assistant","content":"pi reply"},"timestamp":1780000000000_i64}),
        ],
        JUNK,
    );
    let stats = ingest_jsonl(&mut db, &path, "pi", None, &never, Duration::ZERO).expect("ingest");
    assert_eq!((1, 2), (stats.sessions, stats.chunks));
    assert_eq!(
        vec![(
            "p-1".to_string(),
            "pi".to_string(),
            Some("pi".to_string()),
            Some("/work/pi".to_string()),
            None,
            1793361600.0,
            1793361601.0
        )],
        sessions(&db)
    );
    assert_eq!(
        vec![
            (
                "p-1".to_string(),
                0,
                "user".to_string(),
                "pi hello".to_string(),
                1793361601.0
            ),
            (
                "p-1".to_string(),
                0,
                "assistant".to_string(),
                "pi reply".to_string(),
                1793361601.0
            ),
        ],
        chunks(&db)
    );
}

/// `test_ingest_recall_context_and_stats`: three sessions of eight messages, 24 chunks.
#[test]
fn the_generic_layout_ingests_three_sessions_and_twenty_four_chunks() {
    let (dir, mut db) = store();
    let mut rows = Vec::new();
    for session in 0..3 {
        for index in 0..8 {
            rows.push(json!({
                "role": if index % 2 == 0 { "user" } else { "assistant" },
                "text": format!("t note {index} about flux capacitor t-{session}-{index}"),
                "session_id": format!("t-session-{session}"),
                "cwd": "/home/hunter/workspace/demo",
                "timestamp": 1780000000 + session * 100 + index,
            }));
        }
    }
    let path = write(&dir, "s.jsonl", &rows, "");
    let stats = ingest_jsonl(
        &mut db,
        &path,
        "generic",
        Some("demo"),
        &never,
        Duration::ZERO,
    )
    .expect("ingest");
    assert_eq!((3, 24), (stats.sessions, stats.chunks));
    let found: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM chunks_fts WHERE chunks_fts MATCH '\"flux\" AND \"capacitor\"'",
            [],
            |r| r.get(0),
        )
        .expect("fts");
    assert!(found > 0);
    let (harness, project): (String, String) = db
        .query_row("SELECT harness, project FROM sessions LIMIT 1", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .expect("session");
    assert_eq!(
        ("generic".to_string(), "demo".to_string()),
        (harness, project)
    );
}

#[test]
fn a_stop_flushes_what_is_pending_and_says_so() {
    let (dir, mut db) = store();
    let rows: Vec<_> = (0..4)
        .map(|n| json!({"role":"user","text":format!("line {n}"),"session_id":"s","timestamp":1780000000 + n}))
        .collect();
    let path = write(&dir, "stop.jsonl", &rows, "");
    let polls = std::cell::Cell::new(0);
    let stop = || {
        polls.set(polls.get() + 1);
        polls.get() > 2
    };
    let stats =
        ingest_jsonl(&mut db, &path, "generic", None, &stop, Duration::ZERO).expect("ingest");
    assert!(stats.stopped);
    assert_eq!(
        2, stats.chunks,
        "the two lines before the stop were flushed"
    );
}

#[test]
fn iso_time_follows_the_python() {
    assert_eq!(None, iso_time(&json!(null)));
    assert_eq!(None, iso_time(&json!("")));
    assert_eq!(None, iso_time(&json!(0)));
    assert_eq!(Some(1780000000.0), iso_time(&json!(1780000000)));
    assert_eq!(
        Some(1780000000.0),
        iso_time(&json!(1780000000000_i64)),
        "milliseconds above 1e12"
    );
    assert_eq!(Some(1793354400.0), iso_time(&json!("2026-10-30T10:00:00Z")));
    assert_eq!(
        Some(1793354405.5),
        iso_time(&json!("2026-10-30T10:00:05.500+00:00"))
    );
    assert_eq!(None, iso_time(&json!("yesterday")));
}
