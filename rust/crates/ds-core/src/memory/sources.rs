//! `memory::sources` (T51, ingest slice 3): the two SQLite sources and `ingest_path`,
//! ported from `_ingest_codex_history`, `_ingest_opencode` and `ingest_path`.
//!
//! The source databases are opened READ-ONLY on their own connection; every write goes
//! through the memory writer connection passed in. Quirks of the Python that are kept on
//! purpose (the fixtures in `sources_tests.rs` pin them from a real Python run):
//!
//! * the codex-history flush always stores role `assistant`, even for user rows;
//! * a thread whose first row has no `created_at_ms` gets `updated_at = now`;
//! * `opencode`'s `row["time_created"]` is the FIRST such column, the session's, not the part's;
//! * an opencode part with no `type` is text.
//!
//! Difference, stated: a non-object `item_json` makes the Python raise `AttributeError`
//! (failing the whole source); here the row is skipped.

use std::path::Path;
use std::time::Duration;

use rusqlite::{types::ValueRef, Connection, OpenFlags};
use serde_json::Value;

use super::ingest::{
    absolute, harness_from_path, insert_chunks, project_from_cwd, text_from_content,
    upsert_session, upsert_source_row,
};
use super::jsonl::{ingest_jsonl, IngestStats};
use super::pure::{chunk_text, DEFAULT_CHUNK_CHARS, DEFAULT_OVERLAP_CHARS};
use crate::db::map_sqlite;
use crate::error::DunkError;

fn open_source(path: &Path) -> Result<Connection, DunkError> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(map_sqlite)
}

/// A column as the Python's `str(row[...])` would print it.
fn text_of(value: ValueRef<'_>) -> String {
    match value {
        ValueRef::Null => "None".to_string(),
        ValueRef::Integer(number) => number.to_string(),
        ValueRef::Real(number) => number.to_string(),
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => {
            String::from_utf8_lossy(bytes).into_owned()
        }
    }
}

fn opt_text(value: ValueRef<'_>) -> Option<String> {
    match value {
        ValueRef::Null => None,
        other => Some(text_of(other)),
    }
}

/// A numeric column that Python treats as falsy when NULL or 0.
fn truthy_number(value: ValueRef<'_>) -> Option<f64> {
    let number = match value {
        ValueRef::Integer(number) => number as f64,
        ValueRef::Real(number) => number,
        _ => return None,
    };
    (number != 0.0).then_some(number)
}

fn parse_json(value: ValueRef<'_>) -> Option<Value> {
    let raw = match value {
        ValueRef::Null => "{}".to_string(),
        other => text_of(other),
    };
    let raw = if raw.is_empty() {
        "{}".to_string()
    } else {
        raw
    };
    serde_json::from_str(&raw).ok()
}

/// `_ingest_codex_history`.
pub fn ingest_codex_history(
    memory: &mut Connection,
    path: &Path,
    project: Option<&str>,
    should_stop: &dyn Fn() -> bool,
) -> Result<IngestStats, DunkError> {
    let source = open_source(path)?;
    let mut statement = source
        .prepare(
            "SELECT thread_id, created_at_ms, item_json FROM thread_items \
             ORDER BY thread_id, rollout_ordinal",
        )
        .map_err(map_sqlite)?;
    let mut rows = statement.query([]).map_err(map_sqlite)?;
    let mut chunk_count = 0usize;
    let mut sessions = std::collections::BTreeSet::new();
    let mut current: Option<String> = None;
    let mut pending: Vec<String> = Vec::new();
    let mut pending_at: Option<f64> = None;
    let mut stopped = false;
    while let Some(row) = rows.next().map_err(map_sqlite)? {
        if should_stop() {
            stopped = true;
            break;
        }
        let thread_id = text_of(row.get_ref(0).map_err(map_sqlite)?);
        let created_ms = truthy_number(row.get_ref(1).map_err(map_sqlite)?);
        if current.as_deref() != Some(thread_id.as_str()) {
            if let Some(previous) = &current {
                chunk_count += insert_chunks(
                    memory,
                    previous,
                    &pending,
                    Some("assistant"),
                    None,
                    pending_at,
                    None,
                )?;
            }
            current = Some(thread_id.clone());
            pending.clear();
            pending_at = created_ms.map(|ms| ms / 1000.0);
            upsert_session(
                memory, &thread_id, "codex", project, None, None, pending_at, pending_at, None,
            )?;
            sessions.insert(thread_id.clone());
        }
        let Some(item) = parse_json(row.get_ref(2).map_err(map_sqlite)?) else {
            continue;
        };
        let kind = item.get("type").and_then(Value::as_str).unwrap_or("");
        if kind != "userMessage" && kind != "agentMessage" {
            continue;
        }
        let role = if kind == "userMessage" {
            "user"
        } else {
            "assistant"
        };
        let text = text_from_content(item.get("content").unwrap_or(&Value::Null));
        if text.trim().is_empty() {
            continue;
        }
        pending.extend(chunk_text(
            &text,
            DEFAULT_CHUNK_CHARS,
            DEFAULT_OVERLAP_CHARS,
        ));
        if pending.len() >= 500 {
            chunk_count += insert_chunks(
                memory,
                &thread_id,
                &pending,
                Some(role),
                None,
                created_ms.map(|ms| ms / 1000.0),
                None,
            )?;
            pending.clear();
        }
    }
    if let (Some(current), false) = (&current, pending.is_empty()) {
        chunk_count += insert_chunks(
            memory,
            current,
            &pending,
            Some("assistant"),
            None,
            pending_at,
            None,
        )?;
    }
    Ok(IngestStats {
        sessions: sessions.len(),
        chunks: chunk_count,
        harness: "codex".to_string(),
        stopped,
    })
}

/// `_ingest_opencode`.
pub fn ingest_opencode(
    memory: &mut Connection,
    path: &Path,
    project: Option<&str>,
    should_stop: &dyn Fn() -> bool,
) -> Result<IngestStats, DunkError> {
    let source = open_source(path)?;
    let mut statement = source
        .prepare(
            "SELECT s.id, s.title, s.directory, s.time_created, \
             m.data AS message_data, p.data AS part_data, p.time_created \
             FROM session s JOIN message m ON m.session_id = s.id \
             JOIN part p ON p.message_id = m.id \
             ORDER BY s.id, m.time_created, p.time_created",
        )
        .map_err(map_sqlite)?;
    let mut rows = statement.query([]).map_err(map_sqlite)?;
    let mut chunk_count = 0usize;
    let mut sessions = std::collections::BTreeSet::new();
    let mut current: Option<String> = None;
    let mut pending: Vec<String> = Vec::new();
    let mut pending_at: Option<f64> = None;
    let mut stopped = false;
    while let Some(row) = rows.next().map_err(map_sqlite)? {
        if should_stop() {
            stopped = true;
            break;
        }
        let session_id = text_of(row.get_ref(0).map_err(map_sqlite)?);
        if current.as_deref() != Some(session_id.as_str()) {
            if let Some(previous) = &current {
                if !pending.is_empty() {
                    chunk_count += insert_chunks(
                        memory,
                        previous,
                        &pending,
                        Some("assistant"),
                        None,
                        pending_at,
                        None,
                    )?;
                }
            }
            current = Some(session_id.clone());
            pending.clear();
            // index 3 is s.time_created: the Python's row["time_created"] is the first match
            let created = truthy_number(row.get_ref(3).map_err(map_sqlite)?);
            pending_at = created.map(|ms| ms / 1000.0);
            let directory = opt_text(row.get_ref(2).map_err(map_sqlite)?);
            let title = opt_text(row.get_ref(1).map_err(map_sqlite)?);
            let project_name = project
                .map(String::from)
                .or_else(|| project_from_cwd(directory.as_deref()));
            upsert_session(
                memory,
                &session_id,
                "opencode",
                project_name.as_deref(),
                directory.as_deref(),
                title.as_deref(),
                pending_at,
                pending_at,
                None,
            )?;
            sessions.insert(session_id.clone());
        }
        let (Some(message), Some(part)) = (
            parse_json(row.get_ref(4).map_err(map_sqlite)?),
            parse_json(row.get_ref(5).map_err(map_sqlite)?),
        ) else {
            continue;
        };
        match part.get("type") {
            None | Some(Value::Null) => {}
            Some(Value::String(kind)) if kind == "text" => {}
            _ => continue,
        }
        let text = part.get("text").and_then(Value::as_str).unwrap_or("");
        if text.trim().is_empty() {
            continue;
        }
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .filter(|role| !role.is_empty())
            .unwrap_or("assistant")
            .to_string();
        pending.extend(chunk_text(text, DEFAULT_CHUNK_CHARS, DEFAULT_OVERLAP_CHARS));
        if pending.len() >= 500 {
            chunk_count += insert_chunks(
                memory,
                &session_id,
                &pending,
                Some(&role),
                None,
                pending_at,
                None,
            )?;
            pending.clear();
        }
    }
    if let (Some(current), false) = (&current, pending.is_empty()) {
        chunk_count += insert_chunks(
            memory,
            current,
            &pending,
            Some("assistant"),
            None,
            pending_at,
            None,
        )?;
    }
    Ok(IngestStats {
        sessions: sessions.len(),
        chunks: chunk_count,
        harness: "opencode".to_string(),
        stopped,
    })
}

/// `ingest_path`: parse one source by its extension, then record it as ok.
pub fn ingest_path(
    memory: &mut Connection,
    path: &str,
    harness: Option<&str>,
    project: Option<&str>,
    should_stop: &dyn Fn() -> bool,
    throttle: Duration,
) -> Result<IngestStats, DunkError> {
    let path = absolute(path);
    let harness = harness
        .unwrap_or_else(|| harness_from_path(&path))
        .to_string();
    let lowered = path.to_lowercase();
    let file = Path::new(&path);
    let stats = if lowered.ends_with(".jsonl") || lowered.ends_with(".json") {
        ingest_jsonl(memory, file, &harness, project, should_stop, throttle)?
    } else if lowered.ends_with(".sqlite")
        || lowered.ends_with(".sqlite3")
        || lowered.ends_with(".db")
    {
        if lowered.contains("thread_history") || harness.contains("codex") {
            ingest_codex_history(memory, file, project, should_stop)?
        } else {
            ingest_opencode(memory, file, project, should_stop)?
        }
    } else {
        return Err(DunkError::Refused(format!(
            "unsupported memory source: {path}"
        )));
    };
    upsert_source_row(memory, &path, &harness, project)?;
    Ok(stats)
}

#[cfg(test)]
#[path = "sources_tests.rs"]
mod tests;
