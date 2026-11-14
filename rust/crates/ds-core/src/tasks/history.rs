//! Opt-in task history. Legacy task reads and raw event names retain their wire shape.
use super::{row_to_task, wire::full_conn, Task, Tasks};
use crate::{db::map_sqlite, error::DunkError, flock::Flock};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Map, Value};

pub const NOTE_KINDS: [&str; 5] = ["evidence", "decision", "handoff", "progress", "correction"];
const ALL_KINDS: [&str; 6] = [
    "evidence",
    "decision",
    "handoff",
    "progress",
    "correction",
    "note",
];
pub const HISTORY_PAGE_CEILING: i64 = 200;

pub fn validate_note_kind(value: &Value) -> Result<&str, DunkError> {
    value
        .as_str()
        .filter(|kind| NOTE_KINDS.contains(kind))
        .ok_or_else(|| {
            DunkError::Refused(format!(
                "note kind must be one of {}",
                NOTE_KINDS.join(", ")
            ))
        })
}

#[derive(Debug, Clone)]
pub enum HistoryView {
    Summary,
    Notes {
        limit: i64,
        kind: Option<String>,
        before: Option<i64>,
    },
}

impl HistoryView {
    /// None means the unchanged default read; explicit modes never silently fall back.
    pub fn from_args(args: &Map<String, Value>) -> Result<Option<Self>, DunkError> {
        let summary = match args.get("summary") {
            None | Some(Value::Bool(false)) => false,
            Some(Value::Bool(true)) => true,
            _ => return Err(DunkError::Refused("summary must be a boolean".into())),
        };
        if summary {
            if args.contains_key("notes")
                || args.contains_key("note_kind")
                || args.contains_key("before_note")
            {
                return Err(DunkError::Refused(
                    "summary cannot be combined with notes, kind or before_note".into(),
                ));
            }
            return Ok(Some(Self::Summary));
        }
        let Some(notes) = args.get("notes") else {
            if args.contains_key("note_kind") || args.contains_key("before_note") {
                return Err(DunkError::Refused(
                    "kind and before_note require notes".into(),
                ));
            }
            return Ok(None);
        };
        let limit = match notes.as_str() {
            Some("all") => HISTORY_PAGE_CEILING,
            Some(text) => text
                .parse::<i64>()
                .ok()
                .filter(|n| (1..=HISTORY_PAGE_CEILING).contains(n))
                .ok_or_else(|| {
                    DunkError::Refused(
                        "notes must be 1..200 (CEILING), or all for a 200-note page".into(),
                    )
                })?,
            None => {
                return Err(DunkError::Refused(
                    "notes must be a string: N or all".into(),
                ))
            }
        };
        let kind = args
            .get("note_kind")
            .map(|value| {
                value
                    .as_str()
                    .filter(|kind| ALL_KINDS.contains(kind))
                    .map(str::to_owned)
                    .ok_or_else(|| DunkError::Refused("invalid history note kind".into()))
            })
            .transpose()?;
        let before = args
            .get("before_note")
            .map(|value| {
                value
                    .as_str()
                    .and_then(|s| {
                        s.parse::<i64>()
                            .ok()
                            .filter(|n| *n > 0 && n.to_string() == s)
                    })
                    .ok_or_else(|| {
                        DunkError::Refused(
                            "before_note must be a canonical positive note id string".into(),
                        )
                    })
            })
            .transpose()?;
        Ok(Some(Self::Notes {
            limit,
            kind,
            before,
        }))
    }
}

// The kind is metadata, never inferred from a legacy free-form event name or reason.
// Old rows, including arbitrary --event names, belong to the legacy `note` kind.
const KIND_SQL: &str = "CASE WHEN json_valid(meta) THEN CASE WHEN json_extract(meta, '$.kind') IN ('evidence','decision','handoff','progress','correction','note') THEN json_extract(meta, '$.kind') ELSE 'note' END ELSE 'note' END";

fn note_rows(
    conn: &Connection,
    id: &str,
    kind: Option<&str>,
    limit: i64,
    before: Option<i64>,
) -> Result<Vec<Value>, DunkError> {
    let sql = format!("SELECT id, event, at, actor, meta, {KIND_SQL} FROM task_events
        WHERE task_id = ?1 AND (event = 'note' OR CASE WHEN json_valid(meta) THEN
            json_type(meta, '$.reason') IS NOT NULL OR json_extract(meta, '$.kind') IN ('evidence','decision','handoff','progress','correction','note')
            ELSE 0 END) AND (?2 IS NULL OR {KIND_SQL} = ?2)
        AND (?4 IS NULL OR id < ?4) ORDER BY id DESC LIMIT ?3");
    let mut stmt = conn.prepare(&sql).map_err(map_sqlite)?;
    let rows = stmt
        .query_map(params![id, kind, limit, before], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, f64>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .map_err(map_sqlite)?;
    rows.map(|row| {
        let (note_id, event, at, actor, meta, kind) = row.map_err(map_sqlite)?;
        let meta = meta.and_then(|m| serde_json::from_str::<Value>(&m).ok()).unwrap_or(Value::Null);
        let reason = match meta.get("reason") {
            Some(Value::String(text)) => text.clone(),
            Some(other) => super::wire::python_repr(other),
            None => String::new(),
        };
        Ok(json!({"note_id":note_id,"event":event,"kind":kind,"at":at,"actor":actor,"reason":reason}))
    }).collect()
}

/// Budget the JSON-encoded string, including escapes, not only UTF-8 characters.
fn bounded(text: &str, bytes: usize) -> String {
    if serde_json::to_string(text)
        .expect("string serialization")
        .len()
        <= bytes
    {
        return text.to_owned();
    }
    let mut prefix = String::new();
    for ch in text.chars() {
        let candidate = format!("{prefix}{ch} [more]");
        if serde_json::to_string(&candidate)
            .expect("string serialization")
            .len()
            > bytes
        {
            break;
        }
        prefix.push(ch);
    }
    format!("{prefix} [more]")
}

fn summary(conn: &Connection, task: &Task) -> Result<Value, DunkError> {
    let assignees: Vec<Value> = serde_json::from_str(&task.assignees)
        .map_err(|_| DunkError::Refused("invalid task assignees".into()))?;
    let mut notes = Vec::new();
    for kind in ALL_KINDS {
        if let Some(row) = note_rows(conn, &task.id, Some(kind), 1, None)?.pop() {
            notes.push(json!({"kind":kind,"at":row["at"],
                "actor":row["actor"].as_str().map(|s| bounded(s,16)),
                "reason":bounded(row["reason"].as_str().unwrap_or(""),96)}));
        }
    }
    Ok(json!({"id":bounded(&task.id,32),"summary":true,
        "title":bounded(&task.title,96),"status":bounded(&task.status,24),
        "assignees":assignees.iter().take(4).map(|s| bounded(&s.as_str().map(str::to_owned).unwrap_or_else(|| super::wire::python_repr(s)),24)).collect::<Vec<_>>(),
        "assignees_more":assignees.len().saturating_sub(4),
        "block_reason":task.block_reason.as_deref().map(|s| bounded(s,96)),
        "body":bounded(&task.body,256),"notes":notes}))
}

impl Tasks {
    /// Visibility, task row and history share a read-only snapshot. Summary avoids
    /// expanding children, leases or dependencies and fetches at most six notes.
    pub async fn read_history_wire(
        &self,
        id: &str,
        pane: Option<&str>,
        view: HistoryView,
    ) -> Result<Value, DunkError> {
        let id = id.to_owned();
        let pane = pane.map(str::to_owned);
        self.writer.call(move |conn| {
            let tx = conn.transaction().map_err(map_sqlite)?;
            match view {
                HistoryView::Summary => {
                    let task = tx.query_row("SELECT id, scope, title, body, status, priority, kind, assignees, depends_on, block_reason, created_at, updated_at, completed_by, completed_at FROM tasks WHERE id = ?", [id.trim()], row_to_task)
                        .optional().map_err(map_sqlite)?.ok_or_else(|| DunkError::Refused(format!("no task with id {id:?}")))?;
                    Flock::require_task_visible(&task, pane.as_deref())?;
                    summary(&tx, &task)
                }
                HistoryView::Notes { limit, kind, before } => {
                    let mut full = full_conn(&tx, &id, pane.as_deref(), crate::clock::now_s())?;
                    if !(1..=HISTORY_PAGE_CEILING).contains(&limit) {
                        return Err(DunkError::Refused("history page limit exceeds CEILING 200".into()));
                    }
                    if let Some(cursor) = before {
                        if cursor < 1 { return Err(DunkError::Refused("invalid note cursor".into())); }
                        let upper = cursor.checked_add(1);
                        let boundary = note_rows(&tx, &full.typed.id, kind.as_deref(), 1, upper)?;
                        if boundary.first().and_then(|row| row["note_id"].as_i64()) != Some(cursor) {
                            return Err(DunkError::Refused("before_note is outside this task or note kind".into()));
                        }
                    }
                    let mut notes = note_rows(&tx, &full.typed.id, kind.as_deref(), limit + 1, before)?;
                    let more = notes.len() as i64 > limit;
                    if more { notes.pop(); }
                    notes.reverse();
                    full.wire["next_before_note"] = if more { json!(notes.first().and_then(|row| row["note_id"].as_i64()).map(|id| id.to_string())) } else { Value::Null };
                    full.wire["notes"] = json!(notes);
                    full.wire["notes_more"] = json!(more);
                    full.wire["notes_kind"] = json!(kind);
                    Ok(full.wire)
                }
            }
        }).await
    }
}
