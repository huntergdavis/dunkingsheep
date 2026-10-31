//! `memory::recall` (T51): the FTS5 store and the ranked query, ported from
//! `dunk_memory.Memory.recall`.
//!
//! The golden corpus (`rust/fixtures/memory/corpus.json`) is the judge: it pins the
//! ORDER, the scores to four decimal places, the snippets and the counts for every
//! query, so this module is written against it rather than against a reading of the
//! Python. Two facts measured before writing it:
//!
//! * the bundled SQLite (3.46.0) and the host's Python sqlite (3.46.1) produce the
//!   SAME `bm25` for the corpus (-3.0415091215132444 for '"lease" AND "requeue"'),
//!   so the pinned scores are reachable from this build;
//! * the score is not bm25 itself: `score = -bm25 * 100 + 0.25 * exp(-age_days/30)`,
//!   rounded to four places, with a per-session cap of two.

use std::collections::HashMap;

use rusqlite::Connection;
use serde::Serialize;

use super::pure::{fts_query, fts_tokens, make_snippet};
use crate::db::{map_sqlite, ReadPool};
use crate::error::DunkError;

/// The part of `dunk_memory.SCHEMA` that the read path needs, VERBATIM (whitespace
/// collapsed): the two tables, their indexes, the FTS5 table with the Python's
/// tokenizer and prefix settings, and the three content-table triggers that keep the
/// index in step.
pub const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS sessions ( id TEXT PRIMARY KEY, harness TEXT NOT NULL, project TEXT, cwd TEXT, title TEXT, started_at REAL, updated_at REAL, metadata TEXT ); CREATE INDEX IF NOT EXISTS sessions_by_project ON sessions(project, updated_at); CREATE INDEX IF NOT EXISTS sessions_by_harness ON sessions(harness, updated_at)",
    "CREATE TABLE IF NOT EXISTS chunks ( id INTEGER PRIMARY KEY, session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE, ordinal INTEGER NOT NULL, role TEXT, kind TEXT, text TEXT NOT NULL, token_count INTEGER, sha256 TEXT NOT NULL, created_at REAL, metadata TEXT ); CREATE INDEX IF NOT EXISTS chunks_by_session ON chunks(session_id, ordinal); CREATE UNIQUE INDEX IF NOT EXISTS chunks_dedupe ON chunks(sha256, session_id, ordinal)",
    "CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5( text, content='chunks', content_rowid='id', tokenize='porter unicode61', prefix='2 3' )",
    "CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN INSERT INTO chunks_fts(rowid, text) VALUES (new.id, new.text); END; CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES ('delete', old.id, old.text); END; CREATE TRIGGER IF NOT EXISTS chunks_au AFTER UPDATE ON chunks BEGIN INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES ('delete', old.id, old.text); INSERT INTO chunks_fts(rowid, text) VALUES (new.id, new.text); END",
];

/// How many chunks one session may contribute before the rest are dropped
/// (`per_session` in the Python: `>= 2` skips).
pub const PER_SESSION_LIMIT: usize = 2;

/// How many matches to ask SQLite for before the ranking trims to `limit`
/// (`max(limit * 3, 20)`).
pub fn fetch_limit(limit: usize) -> usize {
    (limit * 3).max(20)
}

/// The Python's score: bm25 (negative, lower is better) turned into a positive
/// number, plus a freshness term that decays over thirty days.
pub fn rank_score(bm25: f64, updated_at: f64, now: f64) -> f64 {
    let age_days = ((now - updated_at) / 86_400.0).max(0.0);
    let score = -bm25 * 100.0 + 0.25 * (-age_days / 30.0).exp();
    (score * 10_000.0).round() / 10_000.0
}

/// One recall row, with the fields the fixture pins.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Hit {
    pub chunk_id: i64,
    pub session_id: String,
    pub harness: String,
    pub project: Option<String>,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub role: Option<String>,
    pub kind: Option<String>,
    pub ordinal: i64,
    pub created_at: Option<f64>,
    pub updated_at: Option<f64>,
    pub score: f64,
    pub snippet: String,
    pub text: String,
}

/// The filters `recall` accepts, all optional.
#[derive(Debug, Clone, Default)]
pub struct Filters {
    pub harness: Option<String>,
    pub project: Option<String>,
    pub since: Option<f64>,
}

/// Apply the memory schema to a connection (the daemon migrates once; tests do it on
/// a scratch database).
pub fn apply_schema(connection: &Connection) -> Result<(), DunkError> {
    for statement in SCHEMA {
        connection.execute_batch(statement).map_err(map_sqlite)?;
    }
    Ok(())
}

/// `recall(query, limit, filters)`: ranked hits, AND first then OR, per-session
/// capped, exactly as the Python orders and trims them.
pub fn recall(
    pool: &ReadPool,
    query: &str,
    limit: usize,
    filters: &Filters,
    now: f64,
    max_chars: usize,
) -> Result<Vec<Hit>, DunkError> {
    let tokens = fts_tokens(query);
    let Some(and_fts) = fts_query(query, "AND") else {
        return Ok(Vec::new());
    };
    let or_fts = fts_query(query, "OR");
    let mut rows = fetch(pool, &and_fts, limit, filters)?;
    if rows.is_empty() {
        if let Some(or_fts) = or_fts {
            if or_fts != and_fts {
                rows = fetch(pool, &or_fts, limit, filters)?;
            }
        }
    }
    let mut ranked: Vec<Hit> = rows
        .into_iter()
        .map(|raw| {
            let updated = raw.updated_at.or(raw.created_at).unwrap_or(now);
            let text = raw.text.clone();
            Hit {
                score: rank_score(raw.bm25, updated, now),
                snippet: make_snippet(&text, &tokens, 240)
                    .chars()
                    .take(max_chars)
                    .collect(),
                text: text.chars().take(max_chars).collect(),
                chunk_id: raw.chunk_id,
                session_id: raw.session_id,
                harness: raw.harness,
                project: raw.project,
                cwd: raw.cwd,
                title: raw.title,
                role: raw.role,
                kind: raw.kind,
                ordinal: raw.ordinal,
                created_at: raw.created_at,
                updated_at: raw.updated_at,
            }
        })
        .collect();
    // Descending score, then the per-session cap: the Python sorts by score and skips
    // a third chunk from the same session.
    ranked.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut per_session: HashMap<String, usize> = HashMap::new();
    let mut final_rows = Vec::new();
    for hit in ranked {
        let count = per_session.entry(hit.session_id.clone()).or_insert(0);
        if *count >= PER_SESSION_LIMIT {
            continue;
        }
        *count += 1;
        final_rows.push(hit);
        if final_rows.len() >= limit {
            break;
        }
    }
    Ok(final_rows)
}

/// A raw row before ranking.
struct Raw {
    chunk_id: i64,
    session_id: String,
    harness: String,
    project: Option<String>,
    cwd: Option<String>,
    title: Option<String>,
    role: Option<String>,
    kind: Option<String>,
    ordinal: i64,
    created_at: Option<f64>,
    updated_at: Option<f64>,
    text: String,
    bm25: f64,
}

fn fetch(
    pool: &ReadPool,
    match_expression: &str,
    limit: usize,
    filters: &Filters,
) -> Result<Vec<Raw>, DunkError> {
    pool.with(|connection| {
        let mut sql = String::from(
            "SELECT c.id AS chunk_id, c.session_id, c.role, c.kind, c.ordinal, c.text, \
             c.created_at, s.harness, s.project, s.cwd, s.title, s.updated_at, \
             bm25(chunks_fts) AS score \
             FROM chunks_fts JOIN chunks c ON c.id = chunks_fts.rowid \
             JOIN sessions s ON s.id = c.session_id \
             WHERE chunks_fts MATCH ?",
        );
        let mut params: Vec<rusqlite::types::Value> =
            vec![rusqlite::types::Value::Text(match_expression.to_string())];
        if let Some(harness) = &filters.harness {
            sql.push_str(" AND s.harness = ?");
            params.push(rusqlite::types::Value::Text(harness.clone()));
        }
        if let Some(project) = &filters.project {
            sql.push_str(" AND s.project = ?");
            params.push(rusqlite::types::Value::Text(project.clone()));
        }
        if let Some(since) = filters.since {
            sql.push_str(" AND COALESCE(s.updated_at, c.created_at) >= ?");
            params.push(rusqlite::types::Value::Real(since));
        }
        sql.push_str(" ORDER BY bm25(chunks_fts) LIMIT ?");
        params.push(rusqlite::types::Value::Integer(fetch_limit(limit) as i64));
        let mut statement = connection.prepare(&sql).map_err(map_sqlite)?;
        let rows = statement
            .query_map(rusqlite::params_from_iter(params.iter()), |row| {
                Ok(Raw {
                    chunk_id: row.get("chunk_id")?,
                    session_id: row.get("session_id")?,
                    harness: row.get("harness")?,
                    project: row.get("project")?,
                    cwd: row.get("cwd")?,
                    title: row.get("title")?,
                    role: row.get("role")?,
                    kind: row.get("kind")?,
                    ordinal: row.get("ordinal")?,
                    created_at: row.get("created_at")?,
                    updated_at: row.get("updated_at")?,
                    text: row.get("text")?,
                    bm25: row.get("score")?,
                })
            })
            .map_err(map_sqlite)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(map_sqlite)?);
        }
        Ok(out)
    })
}

#[cfg(test)]
#[path = "recall_tests.rs"]
mod recall_tests;
