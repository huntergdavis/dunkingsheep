//! `memory::queries` (T51, last read-side slice): `latest_session`, `context`, `stats`,
//! `status` and the query-latency percentiles, ported from `dunk_memory.MemoryStore`.
//!
//! `recall` itself is `recall.rs` (judged by the golden corpus); `context` is a digest built
//! on it. Output is `serde_json::Value` so the daemon can return it on the wire as the Python
//! does; the tests compare against values produced by running the Python.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};

use super::ingest::SCHEMA_VERSION;
use super::pure::round_places;
use super::recall::{recall, Filters, Hit};
use crate::db::{map_sqlite, ReadPool};
use crate::error::DunkError;

/// `latest_session`: the most recently updated indexed session for a harness and cwd, or
/// None. Read-only and exact; an empty harness or cwd is None without touching the db.
pub fn latest_session(
    pool: &ReadPool,
    harness: &str,
    cwd: &str,
) -> Result<Option<Value>, DunkError> {
    if harness.is_empty() || cwd.is_empty() {
        return Ok(None);
    }
    pool.with(|connection| {
        connection
            .query_row(
                "SELECT id, harness, cwd, updated_at FROM sessions \
                 WHERE harness = ? AND cwd = ? ORDER BY updated_at DESC LIMIT 1",
                [harness, cwd],
                |row| {
                    Ok(json!({
                        "id": row.get::<_, String>(0)?,
                        "harness": row.get::<_, String>(1)?,
                        "cwd": row.get::<_, Option<String>>(2)?,
                        "updated_at": row.get::<_, Option<f64>>(3)?,
                    }))
                },
            )
            .optional()
            .map_err(map_sqlite)
    })
}

struct Bucket {
    hit: Hit,
    snippets: Vec<Value>,
}

/// `context`: recall `max(20, sessions * 4)` hits, group by session in first-seen order, keep
/// `snippets_per_session` each, order by score (stable, highest first), and render the text.
pub fn context(
    pool: &ReadPool,
    query: &str,
    sessions: i64,
    snippets_per_session: usize,
    now: f64,
) -> Result<Value, DunkError> {
    let rows = recall(
        pool,
        query,
        sessions.saturating_mul(4).max(20) as usize,
        &Filters::default(),
        now,
        240,
    )?;
    let mut buckets: Vec<Bucket> = Vec::new();
    for row in rows {
        let index = match buckets
            .iter()
            .position(|b| b.hit.session_id == row.session_id)
        {
            Some(index) => index,
            None => {
                buckets.push(Bucket {
                    hit: row.clone(),
                    snippets: Vec::new(),
                });
                buckets.len() - 1
            }
        };
        if buckets[index].snippets.len() < snippets_per_session {
            buckets[index].snippets.push(json!({
                "chunk_id": row.chunk_id, "role": row.role,
                "snippet": row.snippet, "score": row.score,
            }));
        }
    }
    // Python: sorted(..., key=score, reverse=True) - stable, so ties keep first-seen order.
    buckets.sort_by(|a, b| {
        b.hit
            .score
            .partial_cmp(&a.hit.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let output_limit = if sessions < 0 {
        buckets
            .len()
            .saturating_sub(sessions.saturating_abs() as usize)
    } else {
        sessions as usize
    };
    buckets.truncate(output_limit);
    let mut lines: Vec<String> = Vec::new();
    for bucket in &buckets {
        let title: String = bucket
            .hit
            .title
            .clone()
            .unwrap_or_default()
            .replace('\n', " ")
            .trim()
            .chars()
            .take(120)
            .collect();
        let project = bucket
            .hit
            .project
            .clone()
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| "-".into());
        lines.push(format!(
            "[{}] {}{}",
            bucket.hit.harness,
            project,
            if title.is_empty() {
                String::new()
            } else {
                format!(" | {title}")
            }
        ));
        for snippet in &bucket.snippets {
            let body = snippet["snippet"]
                .as_str()
                .unwrap_or("")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if !body.is_empty() {
                lines.push(format!(
                    "  - {}",
                    body.chars().take(280).collect::<String>()
                ));
            }
        }
    }
    let rendered: Vec<Value> = buckets
        .iter()
        .map(|b| {
            json!({
                "session_id": b.hit.session_id, "harness": b.hit.harness, "project": b.hit.project,
                "cwd": b.hit.cwd, "title": b.hit.title, "updated_at": b.hit.updated_at,
                "score": b.hit.score, "snippets": b.snippets,
            })
        })
        .collect();
    let snippet_total: usize = buckets.iter().map(|b| b.snippets.len()).sum();
    Ok(json!({
        "query": query,
        "generated_at": now,
        "sessions": rendered,
        "counts": {"sessions": buckets.len(), "snippets": snippet_total},
        "text": lines.join("\n"),
    }))
}

/// In-memory query metrics (`_record_query` / `_percentiles`): per-query INSERTs on the writer
/// would put every read behind the ingest writer (the 2026-10-18 bug), so they live here.
#[derive(Default)]
pub struct QueryStats {
    inner: Mutex<QueryInner>,
}

#[derive(Default)]
struct QueryInner {
    latencies: VecDeque<f64>,
    count: u64,
    errors: u64,
    cache_hits: u64,
}

const LATENCY_WINDOW: usize = 4000;

impl QueryStats {
    pub fn record(&self, latency_ms: f64, error: bool, cached: bool) {
        let mut inner = self.inner.lock().expect("query stats lock");
        inner.count += 1;
        if cached {
            inner.cache_hits += 1;
        }
        if error {
            inner.errors += 1;
        }
        if inner.latencies.len() == LATENCY_WINDOW {
            inner.latencies.pop_front();
        }
        inner.latencies.push_back(latency_ms);
    }

    pub fn percentiles(&self) -> Value {
        let inner = self.inner.lock().expect("query stats lock");
        let mut values: Vec<f64> = inner.latencies.iter().copied().collect();
        values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        if values.is_empty() {
            return json!({"count": 0, "p50": null, "p95": null, "p99": null, "max": null,
                          "errors": inner.errors});
        }
        let pick = |fraction: f64| {
            // Python: min(len-1, int(round(fraction * (len-1)))), round-half-even.
            let position = fraction * (values.len() - 1) as f64;
            let index = (position.round_ties_even() as usize).min(values.len() - 1);
            round_places(values[index], 2)
        };
        json!({"count": inner.count, "p50": pick(0.5), "p95": pick(0.95), "p99": pick(0.99),
               "max": round_places(values[values.len() - 1], 2),
               "errors": inner.errors, "cache_hits": inner.cache_hits})
    }
}

fn count(connection: &Connection, table: &str, filter: &str) -> Result<i64, DunkError> {
    connection
        .query_row(
            &format!("SELECT COUNT(*) FROM {table} {filter}"),
            [],
            |row| row.get(0),
        )
        .map_err(map_sqlite)
}

/// `stats`: counts, per-harness totals, the ingest queue, file sizes and query percentiles.
pub fn stats(pool: &ReadPool, db_path: &Path, queries: &QueryStats) -> Result<Value, DunkError> {
    let (sources, sessions, chunks, partial, by_harness, queue, last) = pool.with(|c| {
        let mut statement = c
            .prepare(
                "SELECT harness, COUNT(*) AS chunks, COUNT(DISTINCT session_id) AS sessions \
                 FROM chunks c JOIN sessions s ON s.id = c.session_id \
                 GROUP BY harness ORDER BY chunks DESC",
            )
            .map_err(map_sqlite)?;
        let by_harness: Vec<Value> = statement
            .query_map([], |row| {
                Ok(
                    json!({"harness": row.get::<_, String>(0)?, "chunks": row.get::<_, i64>(1)?,
                          "sessions": row.get::<_, i64>(2)?}),
                )
            })
            .map_err(map_sqlite)?
            .collect::<Result<_, _>>()
            .map_err(map_sqlite)?;
        let state = |name: &str| count(c, "ingest_queue", &format!("WHERE state='{name}'"));
        let queue = json!({"queued": state("queued")?, "leased": state("leased")?,
                           "failed": state("failed")?, "skipped": state("skipped")?,
                           "done": state("done")?});
        let last: Option<f64> = c
            .query_row("SELECT MAX(last_ingest) FROM sources", [], |row| row.get(0))
            .map_err(map_sqlite)?;
        Ok((
            count(c, "sources", "")?,
            count(c, "sessions", "")?,
            count(c, "chunks", "")?,
            count(c, "sources", "WHERE state='partial'")?,
            by_harness,
            queue,
            last,
        ))
    })?;
    let size = |suffix: &str| {
        let mut name = db_path.as_os_str().to_owned();
        name.push(suffix);
        std::fs::metadata(&name).map(|m| m.len()).unwrap_or(0)
    };
    let depth = queue["queued"].as_i64().unwrap_or(0)
        + queue["leased"].as_i64().unwrap_or(0)
        + queue["failed"].as_i64().unwrap_or(0);
    Ok(json!({
        "path": db_path.to_string_lossy(),
        "schema_version": SCHEMA_VERSION,
        "sources": sources, "sessions": sessions, "chunks": chunks,
        "harness": by_harness, "queue": queue, "queue_depth": depth,
        "partial_sources": partial,
        "db_bytes": {"db": size(""), "-wal": size("-wal"), "-shm": size("-shm")},
        "last_ingest": last,
        "queries": queries.percentiles(),
    }))
}

/// `status`: stats plus `ok` (no failed ingest).
pub fn status(pool: &ReadPool, db_path: &Path, queries: &QueryStats) -> Result<Value, DunkError> {
    let mut value = stats(pool, db_path, queries)?;
    let ok = value["queue"]["failed"].as_i64() == Some(0);
    value["ok"] = json!(ok);
    Ok(value)
}

#[cfg(test)]
#[path = "queries_tests.rs"]
mod tests;
