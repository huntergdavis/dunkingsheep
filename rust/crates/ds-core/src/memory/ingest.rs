//! `memory::ingest` (T51, second half): the sources/queue state machine and the
//! session/chunk writes, ported from `dunk_memory.MemoryStore`.
//!
//! Every function takes the writer's `&mut Connection` (run it through
//! `db::SyncWriter::call`), so each Python `_begin_write ... commit/rollback` block is
//! one transaction here. NOT in this slice: the four harness JSONL/sqlite parsers
//! (`_ingest_jsonl`, `_ingest_codex_history`, `_ingest_opencode`) and `ingest_path`.

use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::pure::redact;
use crate::clock::now_s;
use crate::db::map_sqlite;
use crate::error::DunkError;

/// The rest of `dunk_memory.SCHEMA` (recall.rs holds sessions/chunks/FTS), VERBATIM
/// with whitespace collapsed.
pub const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS schema_meta ( key TEXT PRIMARY KEY, value TEXT NOT NULL )",
    "CREATE TABLE IF NOT EXISTS sources ( id INTEGER PRIMARY KEY, harness TEXT NOT NULL, path TEXT NOT NULL UNIQUE, project TEXT, mtime REAL NOT NULL, size INTEGER NOT NULL, sha256 TEXT, state TEXT NOT NULL DEFAULT 'pending', attempts INTEGER NOT NULL DEFAULT 0, next_at REAL, lease_until REAL, last_ingest REAL, error TEXT, metadata TEXT ); CREATE INDEX IF NOT EXISTS sources_by_state ON sources(state, next_at)",
    "CREATE TABLE IF NOT EXISTS ingest_queue ( id INTEGER PRIMARY KEY, source_id INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE, state TEXT NOT NULL DEFAULT 'queued', attempts INTEGER NOT NULL DEFAULT 0, next_at REAL, lease_until REAL, error TEXT, created_at REAL NOT NULL, finished_at REAL ); CREATE INDEX IF NOT EXISTS ingest_queue_ready ON ingest_queue(state, next_at)",
    "CREATE TABLE IF NOT EXISTS memory_events ( id INTEGER PRIMARY KEY, at REAL NOT NULL, event TEXT NOT NULL, actor TEXT, latency_ms INTEGER, hits INTEGER, source TEXT, metadata TEXT ); CREATE INDEX IF NOT EXISTS memory_events_by_time ON memory_events(at, event)",
];

pub const SCHEMA_VERSION: i64 = 1;

/// Create every table and record the schema version (`MemoryStore.__init__`).
pub fn apply_full_schema(connection: &Connection) -> Result<(), DunkError> {
    super::recall::apply_schema(connection)?;
    for statement in SCHEMA {
        connection.execute_batch(statement).map_err(map_sqlite)?;
    }
    connection
        .execute(
            "INSERT INTO schema_meta(key, value) VALUES('version', ?) \
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [SCHEMA_VERSION.to_string()],
        )
        .map_err(map_sqlite)?;
    Ok(())
}

/// `_harness_from_path`.
pub fn harness_from_path(path: &str) -> &'static str {
    let lowered = path.to_lowercase();
    if lowered.contains("/.codex/") || lowered.contains("thread_history") {
        "codex"
    } else if lowered.contains("/.claude/") {
        "claude"
    } else if lowered.contains("/.pi/") {
        "pi"
    } else if lowered.contains("opencode") {
        "opencode"
    } else {
        "generic"
    }
}

/// `_project_from_cwd`: the last path component, or the whole string if there is none.
pub fn project_from_cwd(cwd: Option<&str>) -> Option<String> {
    let cwd = cwd.filter(|value| !value.is_empty())?;
    let trimmed = cwd.trim_end_matches('/');
    Some(
        Path::new(trimmed)
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or(trimmed)
            .to_string(),
    )
}

/// `_text_from_content`: text out of a Claude/Codex/pi content value; thinking blocks
/// are skipped, and a block yields its first string among text/input_text/output_text/content.
pub fn text_from_content(content: &Value) -> String {
    match content {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| match block {
                Value::String(text) => Some(text.clone()),
                Value::Object(map) => {
                    if matches!(
                        map.get("type").and_then(Value::as_str),
                        Some("thinking" | "redacted_thinking")
                    ) {
                        return None;
                    }
                    ["text", "input_text", "output_text", "content"]
                        .iter()
                        .find_map(|key| map.get(*key).and_then(Value::as_str).map(String::from))
                }
                _ => None,
            })
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) => text_from_content(&Value::Array(vec![content.clone()])),
        other => other.to_string(),
    }
}

/// What `claim_next` hands the worker (`dict(row)`).
#[derive(Debug, Clone, PartialEq)]
pub struct Lease {
    pub queue_id: i64,
    pub source_id: i64,
    pub attempts: i64,
    pub path: String,
    pub harness: String,
    pub project: Option<String>,
}

/// `enqueue`: upsert the source as pending and queue one ingest. Returns
/// `(source_id, queue_id)`; a missing path is the Python's `ValueError`.
pub fn enqueue(
    connection: &mut Connection,
    path: &str,
    harness: Option<&str>,
    project: Option<&str>,
) -> Result<(i64, i64), DunkError> {
    let path = absolute(path);
    let metadata = std::fs::metadata(&path)
        .map_err(|_| DunkError::Refused(format!("source does not exist: {path}")))?;
    let harness = harness
        .unwrap_or_else(|| harness_from_path(&path))
        .to_string();
    let project = project.map(str::trim).filter(|value| !value.is_empty());
    let (mtime, size) = (mtime_of(&metadata), metadata.len() as i64);
    let now = now_s();
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    tx.execute(
        "INSERT INTO sources(harness, path, project, mtime, size, state, next_at) \
         VALUES(?,?,?,?,?,'pending',?) \
         ON CONFLICT(path) DO UPDATE SET harness=excluded.harness, \
         project=excluded.project, mtime=excluded.mtime, size=excluded.size, \
         state='pending', next_at=excluded.next_at, error=NULL",
        params![harness, path, project, mtime, size, now],
    )
    .map_err(map_sqlite)?;
    let source_id: i64 = tx
        .query_row("SELECT id FROM sources WHERE path=?", [&path], |row| {
            row.get(0)
        })
        .map_err(map_sqlite)?;
    tx.execute(
        "INSERT INTO ingest_queue(source_id, state, next_at, created_at) VALUES(?, 'queued', ?, ?)",
        params![source_id, now, now],
    )
    .map_err(map_sqlite)?;
    let queue_id = tx.last_insert_rowid();
    tx.commit().map_err(map_sqlite)?;
    Ok((source_id, queue_id))
}

/// `requeue_partial`: sources left partial by a deliberate stop go back to the queue.
pub fn requeue_partial(connection: &mut Connection) -> Result<usize, DunkError> {
    let now = now_s();
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    let ids: Vec<i64> = {
        let mut statement = tx
            .prepare(
                "SELECT q.id FROM ingest_queue q JOIN sources s ON s.id=q.source_id \
                 WHERE q.state='skipped' AND s.state='partial' \
                 AND (q.error LIKE 'stopped%' OR s.error LIKE 'stopped%')",
            )
            .map_err(map_sqlite)?;
        let rows = statement
            .query_map([], |row| row.get(0))
            .map_err(map_sqlite)?
            .collect::<Result<_, _>>()
            .map_err(map_sqlite)?;
        rows
    };
    for id in &ids {
        tx.execute(
            "UPDATE ingest_queue SET state='queued', next_at=?, error=NULL, lease_until=NULL WHERE id=?",
            params![now, id],
        )
        .map_err(map_sqlite)?;
    }
    if !ids.is_empty() {
        tx.execute(
            "UPDATE sources SET state='pending', lease_until=NULL WHERE state='partial'",
            [],
        )
        .map_err(map_sqlite)?;
    }
    tx.commit().map_err(map_sqlite)?;
    Ok(ids.len())
}

/// `recover_leases`: requeue leases whose worker died; any 'ingesting' source is pending again.
pub fn recover_leases(connection: &mut Connection) -> Result<usize, DunkError> {
    let now = now_s();
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    let requeued = tx
        .execute(
            "UPDATE ingest_queue SET state='queued', lease_until=NULL WHERE state='leased' \
             AND (lease_until IS NULL OR lease_until < ?)",
            [now],
        )
        .map_err(map_sqlite)?;
    tx.execute(
        "UPDATE sources SET state='pending', lease_until=NULL WHERE state='ingesting'",
        [],
    )
    .map_err(map_sqlite)?;
    tx.commit().map_err(map_sqlite)?;
    Ok(requeued)
}

/// `claim_next`: lease the oldest ready queue row, or None.
pub fn claim_next(connection: &mut Connection, lease_s: f64) -> Result<Option<Lease>, DunkError> {
    let now = now_s();
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    let lease = tx
        .query_row(
            "SELECT q.id, q.source_id, q.attempts, s.path, s.harness, s.project \
             FROM ingest_queue q JOIN sources s ON s.id = q.source_id \
             WHERE q.state IN ('queued', 'failed') AND (q.next_at IS NULL OR q.next_at <= ?) \
             ORDER BY q.id LIMIT 1",
            [now],
            |row| {
                Ok(Lease {
                    queue_id: row.get(0)?,
                    source_id: row.get(1)?,
                    attempts: row.get(2)?,
                    path: row.get(3)?,
                    harness: row.get(4)?,
                    project: row.get(5)?,
                })
            },
        )
        .optional()
        .map_err(map_sqlite)?;
    let Some(lease) = lease else {
        tx.commit().map_err(map_sqlite)?;
        return Ok(None);
    };
    tx.execute(
        "UPDATE ingest_queue SET state='leased', lease_until=?, attempts=attempts+1 WHERE id=?",
        params![now + lease_s, lease.queue_id],
    )
    .map_err(map_sqlite)?;
    tx.execute(
        "UPDATE sources SET state='ingesting', lease_until=? WHERE id=?",
        params![now + lease_s, lease.source_id],
    )
    .map_err(map_sqlite)?;
    tx.commit().map_err(map_sqlite)?;
    Ok(Some(lease))
}

/// How `finish` closes a lease.
pub enum Outcome<'a> {
    Ok,
    /// A deliberate stop: skipped/partial, resumable by a later enqueue.
    Skipped(&'a str),
    Failed(&'a str),
}

/// `finish`: record the result. Failure backs off `min(300, 5 * 2^min(attempts+1, 6))` s
/// (the Python's `attempts` here is the lease's attempts BEFORE this claim, plus one).
pub fn finish(
    connection: &mut Connection,
    lease: &Lease,
    outcome: Outcome<'_>,
    stats: &Value,
) -> Result<(), DunkError> {
    let now = now_s();
    let clip = |text: &str| text.chars().take(2000).collect::<String>();
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    match outcome {
        Outcome::Skipped(reason) => {
            tx.execute(
                "UPDATE ingest_queue SET state='skipped', finished_at=?, error=?, lease_until=NULL WHERE id=?",
                params![now, clip(reason), lease.queue_id],
            )
            .map_err(map_sqlite)?;
            tx.execute(
                "UPDATE sources SET state='partial', error=?, lease_until=NULL WHERE id=?",
                params![clip(reason), lease.source_id],
            )
            .map_err(map_sqlite)?;
        }
        Outcome::Ok => {
            tx.execute(
                "UPDATE ingest_queue SET state='done', finished_at=?, error=NULL, lease_until=NULL WHERE id=?",
                params![now, lease.queue_id],
            )
            .map_err(map_sqlite)?;
            tx.execute(
                "UPDATE sources SET state='ok', last_ingest=?, lease_until=NULL, error=NULL WHERE id=?",
                params![now, lease.source_id],
            )
            .map_err(map_sqlite)?;
            let chunks = stats.get("chunks").and_then(Value::as_i64).unwrap_or(0);
            tx.execute(
                "INSERT INTO memory_events(at, event, source, hits, metadata) VALUES(?, 'ingest', ?, ?, ?)",
                params![now, lease.path, chunks, stats.to_string()],
            )
            .map_err(map_sqlite)?;
        }
        Outcome::Failed(error) => {
            let attempts = lease.attempts + 1;
            let delay = 300.min(5 * (1i64 << attempts.min(6)));
            tx.execute(
                "UPDATE ingest_queue SET state='failed', error=?, next_at=?, lease_until=NULL WHERE id=?",
                params![clip(error), now + delay as f64, lease.queue_id],
            )
            .map_err(map_sqlite)?;
            tx.execute(
                "UPDATE sources SET state='error', error=?, lease_until=NULL WHERE id=?",
                params![clip(error), lease.source_id],
            )
            .map_err(map_sqlite)?;
        }
    }
    tx.commit().map_err(map_sqlite)
}

/// `_upsert_session`.
#[allow(clippy::too_many_arguments)]
pub fn upsert_session(
    connection: &Connection,
    session_id: &str,
    harness: &str,
    project: Option<&str>,
    cwd: Option<&str>,
    title: Option<&str>,
    started_at: Option<f64>,
    updated_at: Option<f64>,
    metadata: Option<&Value>,
) -> Result<(), DunkError> {
    let metadata = metadata.filter(|v| !v.is_null()).map(Value::to_string);
    connection
        .execute(
            "INSERT INTO sessions(id, harness, project, cwd, title, started_at, updated_at, metadata) \
             VALUES(?,?,?,?,?,?,?,?) \
             ON CONFLICT(id) DO UPDATE SET project=COALESCE(excluded.project, sessions.project), \
             cwd=COALESCE(excluded.cwd, sessions.cwd), \
             title=COALESCE(excluded.title, sessions.title), \
             started_at=COALESCE(sessions.started_at, excluded.started_at), \
             updated_at=MAX(COALESCE(sessions.updated_at, 0), COALESCE(excluded.updated_at, 0)), \
             metadata=COALESCE(excluded.metadata, sessions.metadata)",
            params![
                session_id,
                harness,
                project,
                cwd,
                title,
                started_at,
                updated_at.unwrap_or_else(now_s),
                metadata
            ],
        )
        .map_err(map_sqlite)?;
    Ok(())
}

/// `_insert_chunks`: redact, hash and insert in one transaction; duplicates by
/// `(sha256, session_id, ordinal)` are ignored. Returns the rows actually inserted.
pub fn insert_chunks(
    connection: &mut Connection,
    session_id: &str,
    chunks: &[String],
    role: Option<&str>,
    kind: Option<&str>,
    created_at: Option<f64>,
    metadata: Option<&Value>,
) -> Result<usize, DunkError> {
    if chunks.is_empty() {
        return Ok(0);
    }
    let metadata = metadata.filter(|v| !v.is_null()).map(Value::to_string);
    let tx = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    let mut inserted = 0;
    for (ordinal, text) in chunks.iter().enumerate() {
        let clean = redact(text);
        let digest = format!("{:x}", Sha256::digest(clean.as_bytes()));
        let tokens = (clean.chars().count() / 4).max(1) as i64;
        inserted += tx
            .execute(
                "INSERT OR IGNORE INTO chunks(session_id, ordinal, role, kind, text, token_count, sha256, created_at, metadata) \
                 VALUES(?,?,?,?,?,?,?,?,?)",
                params![session_id, ordinal as i64, role, kind, clean, tokens, digest, created_at, metadata],
            )
            .map_err(map_sqlite)?;
    }
    tx.commit().map_err(map_sqlite)?;
    // best effort, like the Python's PASSIVE checkpoint
    let _ = connection.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()));
    Ok(inserted)
}

/// `_upsert_source_row`: record a finished source as ok.
pub fn upsert_source_row(
    connection: &Connection,
    path: &str,
    harness: &str,
    project: Option<&str>,
) -> Result<(), DunkError> {
    let metadata =
        std::fs::metadata(path).map_err(|error| DunkError::Refused(format!("{path}: {error}")))?;
    connection
        .execute(
            "INSERT INTO sources(harness, path, project, mtime, size, state, last_ingest) \
             VALUES(?,?,?,?,?,'ok',?) \
             ON CONFLICT(path) DO UPDATE SET harness=excluded.harness, \
             project=COALESCE(excluded.project, sources.project), mtime=excluded.mtime, \
             size=excluded.size, state='ok', last_ingest=excluded.last_ingest, error=NULL",
            params![
                harness,
                path,
                project,
                mtime_of(&metadata),
                metadata.len() as i64,
                now_s()
            ],
        )
        .map_err(map_sqlite)?;
    Ok(())
}

fn absolute(path: &str) -> String {
    let expanded = match path.strip_prefix("~/") {
        Some(rest) => std::env::var("HOME")
            .map(|home| format!("{home}/{rest}"))
            .unwrap_or_else(|_| path.to_string()),
        None => path.to_string(),
    };
    std::path::absolute(&expanded)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or(expanded)
}

fn mtime_of(metadata: &std::fs::Metadata) -> f64 {
    metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0.0, |d| d.as_secs_f64())
}

#[cfg(test)]
#[path = "ingest_tests.rs"]
mod tests;
