//! Complete Python task dictionaries beside the typed store models. Each public read
//! owns one actor call and one deferred transaction, including visibility and notes.

use super::{
    decorate_conn, order_key,
    pane_view::{decorated_wire, row_wire, slice_length},
    row_to_task, validate_kind, validate_status, visible_scopes, Decorated, RecentNote, Task,
    Tasks,
};
use crate::{db::map_sqlite, error::DunkError, flock::Flock};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};

/// A complete `_full` dictionary and its typed task from the same read snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskWire {
    pub typed: Task,
    pub wire: Value,
}

/// A complete `_decorate` dictionary and its typed model (without children/parent).
#[derive(Debug, Clone, PartialEq)]
pub struct DecoratedWire {
    pub typed: Decorated,
    pub wire: Value,
}

/// A visible full task, including the last three reason-bearing notes.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskWireNotes {
    pub typed: Task,
    /// Includes `notes`; consumers can return this dictionary without re-querying.
    pub wire: Value,
    pub notes: Vec<RecentNote>,
}

impl Tasks {
    /// Read `_full` and enforce the existing Flock privacy rule in the same snapshot.
    pub async fn get_full_wire(
        &self,
        id: &str,
        pane_id: Option<&str>,
    ) -> Result<TaskWire, DunkError> {
        let id = id.to_owned();
        let pane = pane_id.map(str::to_owned);
        self.writer
            .call(move |conn| {
                let tx = conn.transaction().map_err(map_sqlite)?;
                full_conn(&tx, &id, pane.as_deref(), crate::clock::now_s())
                // Dropping our read transaction rolls it back; no foreign transaction commit.
            })
            .await
    }

    /// `Tasks.list`, preserving the signed Python slice limit (None = 200, zero = []).
    #[allow(clippy::too_many_arguments)]
    pub async fn list_visible_wire(
        &self,
        scope: Option<&str>,
        status: Option<&str>,
        assignee: Option<&str>,
        include_done: bool,
        limit: Option<i64>,
        pane_id: Option<&str>,
        kind: Option<&str>,
        needs_owner: bool,
    ) -> Result<Vec<DecoratedWire>, DunkError> {
        // Keep Python's scope/status/kind validation order, even for an empty result.
        let scopes = visible_scopes(scope, pane_id)?;
        let status = status
            .filter(|s| !s.is_empty())
            .map(validate_status)
            .transpose()?;
        let kind = kind
            .filter(|s| !s.is_empty())
            .map(validate_kind)
            .transpose()?;
        // A truthy whitespace assignee becomes "" and still filters after decoration.
        let assignee = assignee
            .filter(|s| !s.is_empty())
            .map(|s| s.trim().to_owned());
        let pane = pane_id.map(str::to_owned);
        let limit = limit.unwrap_or(200);
        self.writer
            .call(move |conn| {
                let tx = conn.transaction().map_err(map_sqlite)?;
                let now = crate::clock::now_s();
                let marks = vec!["?"; scopes.len()].join(",");
                let mut sql = format!("SELECT id FROM tasks WHERE scope IN ({marks})");
                let mut binds = scopes;
                if let Some(status) = status {
                    sql.push_str(" AND status = ?");
                    binds.push(status);
                } else if !include_done {
                    sql.push_str(" AND status NOT IN ('done', 'cancelled')");
                }
                if let Some(kind) = kind {
                    sql.push_str(" AND kind = ?");
                    binds.push(kind);
                }
                if needs_owner {
                    sql.push_str(" AND status = 'blocked' AND block_reason LIKE 'needs-owner:%'");
                }
                let mut stmt = tx.prepare(&sql).map_err(map_sqlite)?;
                let ids = stmt
                    .query_map(rusqlite::params_from_iter(binds.iter()), |r| {
                        r.get::<_, String>(0)
                    })
                    .map_err(map_sqlite)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(map_sqlite)?;
                let mut rows = Vec::with_capacity(ids.len());
                for id in ids {
                    let typed = decorate_conn(&tx, &id, pane.as_deref(), now)?;
                    // Construct wire before the assignee filter, like Python's _decorate pass.
                    let wire = decorated_wire(&tx, &typed, now)?;
                    if assignee
                        .as_ref()
                        .is_none_or(|a| typed.assignees.contains(a))
                    {
                        rows.push(DecoratedWire { typed, wire });
                    }
                }
                rows.sort_by_key(|r| {
                    order_key(
                        &r.typed.task.priority,
                        r.typed.task.created_at,
                        &r.typed.task.id,
                    )
                });
                rows.truncate(slice_length(rows.len(), limit));
                Ok(rows)
            })
            .await
    }

    /// `Flock.task_read`: row, privacy check, full decoration and notes share a snapshot.
    /// Failure to fetch/interpret notes alone returns an empty list, as Python does.
    pub async fn read_visible_wire_with_notes(
        &self,
        id: &str,
        pane_id: Option<&str>,
    ) -> Result<TaskWireNotes, DunkError> {
        let id = id.to_owned();
        let pane = pane_id.map(str::to_owned);
        self.writer
            .call(move |conn| {
                let tx = conn.transaction().map_err(map_sqlite)?;
                let mut full = full_conn(&tx, &id, pane.as_deref(), crate::clock::now_s())?;
                let notes = notes_conn(&tx, &full.typed.id).unwrap_or_default();
                full.wire["notes"] = json!(notes
                    .iter()
                    .map(|n| json!({
                        "event": n.event, "at": n.at, "actor": n.actor, "reason": n.reason,
                    }))
                    .collect::<Vec<_>>());
                Ok(TaskWireNotes {
                    typed: full.typed,
                    wire: full.wire,
                    notes,
                })
            })
            .await
    }
}

fn full_conn(
    conn: &Connection,
    id: &str,
    pane: Option<&str>,
    now: f64,
) -> Result<TaskWire, DunkError> {
    let wanted = id;
    let id = id.trim();
    let typed = conn.query_row(
        "SELECT id, scope, title, body, status, priority, kind, assignees, depends_on, block_reason, created_at, updated_at, completed_by, completed_at FROM tasks WHERE id = ?",
        [id], row_to_task,
    ).optional().map_err(map_sqlite)?
        .ok_or_else(|| DunkError::Refused(format!("no task with id {wanted:?}")))?;
    // Reuse the established rule; callers cannot authorize one row and return another.
    Flock::require_task_visible(&typed, pane)?;
    let decorated = decorate_conn(conn, id, pane, now)?;
    let mut wire = decorated_wire(conn, &decorated, now)?;
    let mut stmt = conn
        .prepare("SELECT id FROM tasks WHERE parent_id = ? ORDER BY created_at")
        .map_err(map_sqlite)?;
    let children = stmt
        .query_map([id], |r| r.get::<_, String>(0))
        .map_err(map_sqlite)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sqlite)?;
    wire["children"] = Value::Array(
        children
            .into_iter()
            .map(|id| {
                let child = decorate_conn(conn, &id, pane, now)?;
                decorated_wire(conn, &child, now)
            })
            .collect::<Result<_, _>>()?,
    );
    if let Some(parent) = wire["parent_id"].as_str().filter(|s| !s.is_empty()) {
        wire["parent"] = conn
            .query_row(
                "SELECT id, title, status FROM tasks WHERE id = ?",
                [parent],
                row_wire,
            )
            .optional()
            .map_err(map_sqlite)?
            .map(Value::Object)
            .unwrap_or(Value::Null);
    }
    Ok(TaskWire { typed, wire })
}

fn notes_conn(conn: &Connection, id: &str) -> Result<Vec<RecentNote>, DunkError> {
    let mut stmt = conn.prepare(
        "SELECT event, at, actor, meta FROM task_events WHERE task_id = ? AND meta IS NOT NULL ORDER BY id DESC LIMIT 12",
    ).map_err(map_sqlite)?;
    let rows = stmt
        .query_map([id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
            ))
        })
        .map_err(map_sqlite)?;
    let mut notes = Vec::new();
    for row in rows {
        let (event, at, actor, meta) = row.map_err(map_sqlite)?;
        let Ok(meta) = serde_json::from_str::<Value>(&meta) else {
            continue;
        };
        if truthy(&meta) && !meta.is_object() {
            // Python (meta or {}).get raises on truthy non-mappings: the outer read
            // catches that failure and discards the entire notes result.
            return Err(DunkError::Refused("note meta is not a mapping".into()));
        }
        let Some(reason) = meta.get("reason").filter(|v| truthy(v)) else {
            continue;
        };
        let reason = match reason {
            Value::String(text) => text.clone(),
            other => python_repr(other),
        }
        .trim()
        .to_owned();
        if !reason.is_empty() {
            notes.push(RecentNote {
                event,
                at,
                actor,
                reason,
            });
            if notes.len() == 3 {
                break;
            }
        }
    }
    notes.reverse();
    Ok(notes)
}

// Python str(reason) also accepts scalar/container reasons written by non-CLI callers.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::Number(v) => v.as_f64() != Some(0.0),
        Value::String(v) => !v.is_empty(),
        Value::Array(v) => !v.is_empty(),
        Value::Object(v) => !v.is_empty(),
    }
}

fn python_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::String(s) => crate::context::store::py_repr(s),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(python_repr)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(k, v)| format!("{}: {}", crate::context::store::py_repr(k), python_repr(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => other.to_string(),
    }
}

#[cfg(test)]
pub(crate) mod tests;
