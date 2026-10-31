//! `memory::queries` tests. The expected `context`, `latest_session` and `stats` values were
//! produced by running the Python `MemoryStore` (time frozen at NOW) on the SAME jsonl, ingested
//! by the Python and, here, by `ingest_jsonl` - so this also checks the two ingests agree,
//! including the Python quirk that a flushed chunk takes the session id current AT FLUSH
//! (`t-0-7` is stored under `t-session-1`).

use std::io::Write;
use std::time::Duration;

use serde_json::json;

use super::*;
use crate::memory::ingest::apply_full_schema;
use crate::memory::jsonl::ingest_jsonl;

const NOW: f64 = 1793400000.0;

fn loaded() -> (tempfile::TempDir, std::path::PathBuf, ReadPool) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("memory.db");
    let mut writer = crate::db::open_writer(&db_path).expect("writer");
    apply_full_schema(&writer).expect("schema");
    let source = dir.path().join("ctx.jsonl");
    let mut file = std::fs::File::create(&source).expect("create");
    for session in 0..3 {
        for index in 0..8 {
            let extra = if index == 3 { " extra\nlines here" } else { "" };
            writeln!(
                file,
                "{}",
                json!({"role": if index % 2 == 0 {"user"} else {"assistant"},
                       "text": format!("t note {index} about flux capacitor t-{session}-{index}{extra}"),
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
    crate::memory::ingest::upsert_source_row(
        &writer,
        source.to_str().unwrap(),
        "generic",
        Some("demo"),
    )
    .expect("source");
    let pool = ReadPool::open(&db_path, 1).expect("pool");
    (dir, db_path, pool)
}

#[test]
fn context_matches_the_python_digest() {
    let (_dir, _path, pool) = loaded();
    let digest = context(&pool, "flux capacitor", 2, 2, NOW).expect("context");
    let expected = json!({
        "query": "flux capacitor",
        "generated_at": NOW,
        "sessions": [
            {"session_id": "t-session-0", "harness": "generic", "project": "demo",
             "cwd": "/home/hunter/workspace/demo", "title": null, "updated_at": 1780000007.0,
             "score": 0.0016,
             "snippets": [
                {"chunk_id": 1, "role": "user", "snippet": "t note 0 about flux capacitor t-0-0", "score": 0.0016},
                {"chunk_id": 2, "role": "assistant", "snippet": "t note 1 about flux capacitor t-0-1", "score": 0.0016}]},
            {"session_id": "t-session-1", "harness": "generic", "project": "demo",
             "cwd": "/home/hunter/workspace/demo", "title": null, "updated_at": 1780000107.0,
             "score": 0.0016,
             "snippets": [
                {"chunk_id": 8, "role": "assistant", "snippet": "t note 7 about flux capacitor t-0-7", "score": 0.0016},
                {"chunk_id": 9, "role": "user", "snippet": "t note 0 about flux capacitor t-1-0", "score": 0.0016}]}],
        "counts": {"sessions": 2, "snippets": 4},
        "text": "[generic] demo\n  - t note 0 about flux capacitor t-0-0\n  - t note 1 about flux capacitor t-0-1\n[generic] demo\n  - t note 7 about flux capacitor t-0-7\n  - t note 0 about flux capacitor t-1-0",
    });
    assert_eq!(expected, digest);
}

#[test]
fn latest_session_is_exact_or_none() {
    let (_dir, _path, pool) = loaded();
    assert_eq!(
        Some(json!({"id": "t-session-2", "harness": "generic",
                    "cwd": "/home/hunter/workspace/demo", "updated_at": 1780000207.0})),
        latest_session(&pool, "generic", "/home/hunter/workspace/demo").expect("latest")
    );
    assert_eq!(
        None,
        latest_session(&pool, "generic", "/nope").expect("latest")
    );
    assert_eq!(None, latest_session(&pool, "", "x").expect("latest"));
    assert_eq!(None, latest_session(&pool, "generic", "").expect("latest"));
}

#[test]
fn stats_match_the_python() {
    let (_dir, path, pool) = loaded();
    let queries = QueryStats::default();
    let stats = stats(&pool, &path, &queries).expect("stats");
    assert_eq!(1, stats["schema_version"]);
    assert_eq!(
        (1, 3, 24, 0),
        (
            stats["sources"].as_i64().unwrap(),
            stats["sessions"].as_i64().unwrap(),
            stats["chunks"].as_i64().unwrap(),
            stats["partial_sources"].as_i64().unwrap()
        )
    );
    assert_eq!(
        json!([{"harness": "generic", "chunks": 24, "sessions": 3}]),
        stats["harness"]
    );
    assert_eq!(
        json!({"queued": 0, "leased": 0, "failed": 0, "skipped": 0, "done": 0}),
        stats["queue"]
    );
    assert_eq!(0, stats["queue_depth"]);
    assert!(stats["db_bytes"]["db"].as_u64().unwrap() > 0);
    assert!(stats["last_ingest"].as_f64().unwrap() > 1.7e9);
    assert_eq!(
        json!({"count": 0, "p50": null, "p95": null, "p99": null, "max": null, "errors": 0}),
        stats["queries"]
    );
}

#[test]
fn status_is_ok_only_without_failed_ingests() {
    let (_dir, path, pool) = loaded();
    let queries = QueryStats::default();
    assert_eq!(
        json!(true),
        status(&pool, &path, &queries).expect("status")["ok"]
    );
    let writer = crate::db::open_writer(&path).expect("writer");
    writer
        .execute(
            "INSERT INTO ingest_queue(source_id, state, created_at) VALUES (1, 'failed', 0)",
            [],
        )
        .expect("failed row");
    let after = status(&pool, &path, &queries).expect("status");
    assert_eq!(json!(false), after["ok"]);
    assert_eq!(
        1, after["queue_depth"],
        "failed rows count toward the depth"
    );
}

#[test]
fn percentiles_round_like_the_python() {
    let queries = QueryStats::default();
    for (n, latency) in [10.0, 20.0, 30.0, 40.0, 1.005].iter().enumerate() {
        queries.record(*latency, n == 4, n == 1);
    }
    // sorted: 1.005 10 20 30 40; len-1 = 4; p50 -> index round(2.0)=2; p95 -> round(3.8)=4
    let p = queries.percentiles();
    assert_eq!(
        json!({"count": 5, "p50": 20.0, "p95": 40.0, "p99": 40.0, "max": 40.0,
                      "errors": 1, "cache_hits": 1}),
        p
    );
}
