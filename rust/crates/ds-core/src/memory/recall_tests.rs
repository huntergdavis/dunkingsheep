//! `memory::recall` against the GOLDEN CORPUS (T51's judge).
//!
//! The fixture pins order, scores to four places, snippets and counts. The chunk
//! texts are already redacted, so a port that forgets a SECRET_PATTERNS entry shows
//! up here as a diff - which is the point of the corpus.

use std::path::Path;

use super::*;
use crate::memory::pure::redact;

const CORPUS: &str = include_str!("../../../../fixtures/memory/corpus.json");

/// Load the fixture's sessions and chunks into a scratch database through the real
/// schema and the real FTS triggers - no hand-built index, so the tokenizer and the
/// triggers are exercised exactly as the daemon would use them.
fn loaded() -> (tempfile::TempDir, std::path::PathBuf) {
    let corpus: serde_json::Value = serde_json::from_str(CORPUS).expect("corpus json");
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("memory.db");
    let connection = crate::db::open_writer(&path).expect("writer");
    apply_schema(&connection).expect("schema");
    for session in corpus["sessions"].as_array().expect("sessions") {
        connection
            .execute(
                "INSERT INTO sessions (id, harness, project, cwd, title, started_at, updated_at) \
                 VALUES (?,?,?,?,?,?,?)",
                rusqlite::params![
                    session["id"].as_str(),
                    session["harness"].as_str(),
                    session["project"].as_str(),
                    session["cwd"].as_str(),
                    session["title"].as_str(),
                    session["started_at"].as_f64().unwrap_or(0.0),
                    session["updated_at"].as_f64(),
                ],
            )
            .expect("session");
    }
    for (index, chunk) in corpus["chunks"]
        .as_array()
        .expect("chunks")
        .iter()
        .enumerate()
    {
        // Redact on the way in, as ingest does: the fixture is already clean, so this
        // asserts the redaction is a no-op on clean text and never mangles it.
        let text = redact(chunk["text"].as_str().unwrap_or_default());
        connection
            .execute(
                "INSERT INTO chunks (id, session_id, ordinal, role, kind, text, sha256, created_at) \
                 VALUES (?,?,?,?,?,?,?,?)",
                rusqlite::params![
                    index as i64 + 1,
                    chunk["session_id"].as_str(),
                    chunk["ordinal"].as_i64(),
                    chunk["role"].as_str(),
                    chunk["kind"].as_str(),
                    text,
                    format!("sha-{}-{}", chunk["session_id"].as_str().unwrap_or(""), chunk["ordinal"].as_i64().unwrap_or(0)),
                    chunk["created_at"].as_f64().unwrap_or(0.0),
                ],
            )
            .expect("chunk");
    }
    (dir, path)
}

#[test]
fn the_schema_matches_the_pythons_text() {
    // The schema is copied, not derived: assert the pieces the read path depends on
    // are present with the Python's own settings.
    let joined = SCHEMA.join("\n");
    assert!(joined.contains("tokenize='porter unicode61'"), "{joined}");
    assert!(joined.contains("prefix='2 3'"), "{joined}");
    assert!(joined.contains("content='chunks'"), "{joined}");
    assert!(joined.contains("content_rowid='id'"), "{joined}");
    for trigger in ["chunks_ai", "chunks_ad", "chunks_au"] {
        assert!(joined.contains(trigger), "missing trigger {trigger}");
    }
}

#[test]
fn the_score_formula_is_the_pythons() {
    // -bm25 * 100 + 0.25 * exp(-age/30), to four places.
    // -bm25*100 = 304.1509, plus the freshness term 0.25 at zero age.
    assert_eq!(
        304.4009,
        rank_score(-3.0415091215132444, 1_700_000_000.0, 1_700_000_000.0)
    );
    // A day of age: 0.25 * exp(-1/30) = 0.2418, so 304.3927.
    let aged = rank_score(
        -3.0415091215132444,
        1_700_000_000.0,
        1_700_000_000.0 + 86_400.0,
    );
    assert_eq!(304.3927, aged);
}

#[test]
fn the_fixture_replays_exactly() {
    let corpus: serde_json::Value = serde_json::from_str(CORPUS).expect("corpus json");
    let (_dir, path) = loaded();
    let pool = crate::db::ReadPool::open(&path, 2).expect("pool");
    let recall_cases = corpus["expected"]["recall"]
        .as_array()
        .expect("recall cases");
    assert!(!recall_cases.is_empty());
    for case in recall_cases {
        let query = case["query"].as_str().expect("query");
        let kwargs = &case["kwargs"];
        let limit = kwargs["limit"].as_u64().unwrap_or(10) as usize;
        let filters = Filters {
            harness: kwargs["harness"].as_str().map(|value| value.to_string()),
            project: kwargs["project"].as_str().map(|value| value.to_string()),
            since: kwargs["since"].as_f64(),
        };
        // The fixture does not carry its `now`: the Python used the wall clock when
        // the corpus was generated, which was far enough after the session timestamps
        // that the freshness term rounds away to zero at four places. Replaying it
        // therefore needs a `now` at least ~300 days after `updated_at` (measured:
        // 0.25*exp(-300/30) = 0.0000114, which rounds out at 4 dp). This is a real
        // fragility of the fixture - a session generated "today" would pin a score
        // that no later replay could reproduce - and it is recorded rather than
        // hidden by rounding the comparison.
        let now = case["now"]
            .as_f64()
            .unwrap_or(1_700_000_100.0 + 400.0 * 86_400.0);
        let hits = recall(&pool, query, limit, &filters, now, 1200).expect("recall");
        let expected = case["recall"].as_array().expect("expected rows");
        assert_eq!(
            expected.len(),
            hits.len(),
            "count differs for {query:?}: {hits:?}"
        );
        for (want, got) in expected.iter().zip(hits.iter()) {
            assert_eq!(
                want["chunk_id"].as_i64(),
                Some(got.chunk_id),
                "chunk_id for {query:?}"
            );
            assert_eq!(
                want["session_id"].as_str(),
                Some(got.session_id.as_str()),
                "session for {query:?}"
            );
            assert_eq!(
                want["role"].as_str(),
                got.role.as_deref(),
                "role for {query:?}"
            );
            assert_eq!(
                want["kind"].as_str(),
                got.kind.as_deref(),
                "kind for {query:?}"
            );
            assert_eq!(
                want["ordinal"].as_i64(),
                Some(got.ordinal),
                "ordinal for {query:?}"
            );
            assert_eq!(
                want["score"].as_f64(),
                Some(got.score),
                "SCORE for {query:?}"
            );
            assert_eq!(
                want["snippet"].as_str(),
                Some(got.snippet.as_str()),
                "snippet for {query:?}"
            );
        }
    }
}
