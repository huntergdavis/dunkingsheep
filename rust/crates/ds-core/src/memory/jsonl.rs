//! `memory::jsonl` (T51, ingest slice 2): the JSONL session parsers, ported from
//! `dunk_memory.MemoryStore._ingest_jsonl` plus `_iso_time` / `_event_time`.
//!
//! One function, four layouts: `codex` rollouts, `claude` projects, `pi` sessions and the
//! generic `{role, text, session_id, cwd, timestamp}` row. The layout is chosen by the
//! `harness` argument exactly as in the Python. Not here: the codex thread-history and
//! opencode SQLite sources, `ingest_path`'s dispatch, the ingestor loop.
//!
//! Known difference, stated: a timestamp string with NO offset is read by Python's
//! `datetime.fromisoformat(...).timestamp()` in the machine's LOCAL zone; this does the
//! same through `chrono::Local`, and anything `fromisoformat` accepts beyond RFC 3339 and
//! `YYYY-MM-DD[T ]HH:MM:SS[.f]` is treated as unparseable (the Python's `None`).

use std::collections::BTreeSet;
use std::io::BufRead;
use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Local, NaiveDateTime, TimeZone};
use rusqlite::Connection;
use serde_json::{Map, Value};

use super::ingest::{insert_chunks, project_from_cwd, text_from_content, upsert_session};
use super::pure::chunk_text;
use super::pure::{DEFAULT_CHUNK_CHARS, DEFAULT_OVERLAP_CHARS};
use crate::clock::now_s;
use crate::error::DunkError;

/// What `_ingest_jsonl` returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestStats {
    pub sessions: usize,
    pub chunks: usize,
    pub harness: String,
    pub stopped: bool,
}

/// `_iso_time`: seconds since the epoch from a number (milliseconds when above 1e12) or an
/// ISO-8601 string; falsy values and anything unparseable are `None`.
pub fn iso_time(value: &Value) -> Option<f64> {
    match value {
        Value::Null | Value::Bool(false) => None,
        Value::Number(number) => {
            let seconds = number.as_f64()?;
            if seconds == 0.0 {
                return None;
            }
            Some(if seconds > 1e12 {
                seconds / 1000.0
            } else {
                seconds
            })
        }
        Value::String(text) if text.is_empty() => None,
        Value::String(text) => parse_iso(text),
        other => parse_iso(&other.to_string()),
    }
}

fn parse_iso(text: &str) -> Option<f64> {
    let text = text.replace('Z', "+00:00");
    if let Ok(stamp) = DateTime::parse_from_rfc3339(&text) {
        return Some(stamp.timestamp_millis() as f64 / 1000.0);
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d"] {
        let naive = NaiveDateTime::parse_from_str(&text, format)
            .ok()
            .or_else(|| {
                chrono::NaiveDate::parse_from_str(&text, format)
                    .ok()
                    .and_then(|date| date.and_hms_opt(0, 0, 0))
            });
        if let Some(naive) = naive {
            let local = Local.from_local_datetime(&naive).earliest()?;
            return Some(local.timestamp_millis() as f64 / 1000.0);
        }
    }
    None
}

/// `_event_time`: the first of timestamp/time/created_at that parses to something truthy,
/// else now.
pub fn event_time(event: &Map<String, Value>) -> f64 {
    for key in ["timestamp", "time", "created_at"] {
        if let Some(value) = event.get(key) {
            if let Some(parsed) = iso_time(value) {
                if parsed != 0.0 {
                    return parsed;
                }
            }
        }
    }
    now_s()
}

/// Python truthiness of a JSON value, as a string when it is a usable id/label.
fn truthy(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) if number.as_f64() != Some(0.0) => Some(number.to_string()),
        Value::Bool(true) => Some("True".to_string()),
        _ => None,
    }
}

/// `event.get(key) or {}` as an object.
fn object<'a>(
    map: &'a Map<String, Value>,
    key: &str,
    empty: &'a Map<String, Value>,
) -> &'a Map<String, Value> {
    map.get(key).and_then(Value::as_object).unwrap_or(empty)
}

fn role_of(value: Option<&Value>) -> Option<&'static str> {
    match value.and_then(Value::as_str) {
        Some("user") => Some("user"),
        Some("assistant") => Some("assistant"),
        _ => None,
    }
}

struct Pending {
    chunks: Vec<String>,
    role: Option<&'static str>,
    at: Option<f64>,
}

/// `_ingest_jsonl`. `should_stop` is polled before every line; on a stop the pending batch
/// is flushed and the stats carry `stopped: true`. `throttle` sleeps after each flush.
pub fn ingest_jsonl(
    connection: &mut Connection,
    path: &Path,
    harness: &str,
    project: Option<&str>,
    should_stop: &dyn Fn() -> bool,
    throttle: Duration,
) -> Result<IngestStats, DunkError> {
    let mut session_id = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut project: Option<String> = project.map(String::from);
    let (mut cwd, mut title, mut started_at): (Option<String>, Option<String>, Option<f64>) =
        (None, None, None);
    let mut sessions: BTreeSet<String> = BTreeSet::new();
    let mut chunk_count = 0usize;
    let mut pending = Pending {
        chunks: Vec::new(),
        role: None,
        at: None,
    };
    let mut last_at: Option<f64> = None;
    let empty = Map::new();

    let flush = |connection: &mut Connection,
                 pending: &mut Pending,
                 session_id: &str,
                 chunk_count: &mut usize|
     -> Result<(), DunkError> {
        if !pending.chunks.is_empty() {
            *chunk_count += insert_chunks(
                connection,
                session_id,
                &pending.chunks,
                pending.role,
                None,
                pending.at,
                None,
            )?;
            pending.chunks.clear();
            if !throttle.is_zero() {
                std::thread::sleep(throttle);
            }
        }
        Ok(())
    };

    let file = std::fs::File::open(path)
        .map_err(|error| DunkError::Refused(format!("{}: {error}", path.display())))?;
    let mut reader = std::io::BufReader::new(file);
    let mut raw = Vec::new();
    loop {
        raw.clear();
        if reader.read_until(b'\n', &mut raw)? == 0 {
            break;
        }
        if should_stop() {
            flush(connection, &mut pending, &session_id, &mut chunk_count)?;
            return Ok(IngestStats {
                sessions: sessions.len().max(1),
                chunks: chunk_count,
                harness: harness.to_string(),
                stopped: true,
            });
        }
        let line = String::from_utf8_lossy(&raw);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(Value::Object(event)) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let mut now = event_time(&event);
        let role: &'static str;
        let text: String;
        match harness {
            "codex" => {
                let kind = event.get("type").and_then(Value::as_str);
                let payload = object(&event, "payload", &empty);
                match kind {
                    Some("session_meta") => {
                        session_id = truthy(payload.get("session_id"))
                            .or_else(|| truthy(payload.get("id")))
                            .unwrap_or(session_id);
                        cwd = truthy(payload.get("cwd")).or(cwd);
                        started_at =
                            started_at.or_else(|| payload.get("timestamp").and_then(iso_time));
                        title = title.or_else(|| truthy(payload.get("originator")));
                        upsert_session(
                            connection,
                            &session_id,
                            harness,
                            project
                                .clone()
                                .or_else(|| project_from_cwd(cwd.as_deref()))
                                .as_deref(),
                            cwd.as_deref(),
                            title.as_deref(),
                            started_at,
                            Some(now),
                            None,
                        )?;
                        continue;
                    }
                    Some("turn_context") => {
                        cwd = truthy(payload.get("cwd")).or(cwd);
                        continue;
                    }
                    Some("response_item") => {}
                    _ => continue,
                }
                if payload.get("type").and_then(Value::as_str) != Some("message") {
                    continue;
                }
                let Some(found) = role_of(payload.get("role")) else {
                    continue;
                };
                role = found;
                text = text_from_content(payload.get("content").unwrap_or(&Value::Null));
            }
            "claude" => {
                let kind = event.get("type").and_then(Value::as_str);
                if !matches!(kind, Some("user" | "assistant")) {
                    if kind == Some("ai-title") {
                        title = truthy(event.get("title")).or(title);
                    }
                    continue;
                }
                let message = object(&event, "message", &empty);
                let Some(found) =
                    role_of(message.get("role")).or_else(|| role_of(event.get("type")))
                else {
                    continue;
                };
                role = found;
                text = text_from_content(message.get("content").unwrap_or(&Value::Null));
                session_id = truthy(event.get("sessionId")).unwrap_or(session_id);
                cwd = truthy(event.get("cwd")).or(cwd);
                started_at = started_at.or_else(|| event.get("timestamp").and_then(iso_time));
                now = event.get("timestamp").and_then(iso_time).unwrap_or(now);
                title = title.or_else(|| truthy(event.get("slug")));
            }
            "pi" => {
                match event.get("type").and_then(Value::as_str) {
                    Some("session") => {
                        session_id = truthy(event.get("id")).unwrap_or(session_id);
                        cwd = truthy(event.get("cwd")).or(cwd);
                        started_at =
                            started_at.or_else(|| event.get("timestamp").and_then(iso_time));
                        upsert_session(
                            connection,
                            &session_id,
                            harness,
                            project
                                .clone()
                                .or_else(|| project_from_cwd(cwd.as_deref()))
                                .as_deref(),
                            cwd.as_deref(),
                            title.as_deref(),
                            started_at,
                            Some(now),
                            None,
                        )?;
                        continue;
                    }
                    Some("message") => {}
                    _ => continue,
                }
                let message = object(&event, "message", &empty);
                let Some(found) = role_of(message.get("role")) else {
                    continue;
                };
                role = found;
                text = text_from_content(message.get("content").unwrap_or(&Value::Null));
            }
            _ => {
                let message = object(&event, "message", &empty);
                let Some(found) = role_of(event.get("role").filter(|v| !v.is_null()))
                    .or_else(|| role_of(message.get("role")))
                else {
                    continue;
                };
                role = found;
                text = match truthy(event.get("text")) {
                    Some(own) => own,
                    None => text_from_content(message.get("content").unwrap_or(&Value::Null)),
                };
                session_id = truthy(event.get("session_id")).unwrap_or(session_id);
                cwd = truthy(event.get("cwd")).or(cwd);
                now = event.get("timestamp").and_then(iso_time).unwrap_or(now);
            }
        }

        if text.trim().is_empty() {
            continue;
        }
        if project.is_none() {
            if let Some(found) = project_from_cwd(cwd.as_deref()) {
                project = Some(found);
            }
        }
        upsert_session(
            connection,
            &session_id,
            harness,
            project.as_deref(),
            cwd.as_deref(),
            title.as_deref(),
            started_at,
            Some(now),
            None,
        )?;
        // Flush when the role changes so chunks keep role semantics.
        if !pending.chunks.is_empty() && Some(role) != pending.role {
            flush(connection, &mut pending, &session_id, &mut chunk_count)?;
        }
        pending.role = Some(role);
        pending.at = pending.at.or(Some(now));
        last_at = Some(now);
        pending.chunks.extend(chunk_text(
            &text,
            DEFAULT_CHUNK_CHARS,
            DEFAULT_OVERLAP_CHARS,
        ));
        sessions.insert(session_id.clone());
        if pending.chunks.len() >= 500 {
            flush(connection, &mut pending, &session_id, &mut chunk_count)?;
        }
    }
    flush(connection, &mut pending, &session_id, &mut chunk_count)?;
    upsert_session(
        connection,
        &session_id,
        harness,
        project.as_deref(),
        cwd.as_deref(),
        title.as_deref(),
        started_at,
        Some(last_at.unwrap_or_else(now_s)),
        None,
    )?;
    Ok(IngestStats {
        sessions: sessions.len().max(1),
        chunks: chunk_count,
        harness: harness.to_string(),
        stopped: false,
    })
}

#[cfg(test)]
#[path = "jsonl_tests.rs"]
mod tests;
