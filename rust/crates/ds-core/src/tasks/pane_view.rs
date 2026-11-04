//! The read-only task block appended to inbox output.

use super::{decorate_conn, order_key, visible_scopes, Decorated, Tasks};
use crate::{db::map_sqlite, error::DunkError};
use rusqlite::{params, types::Value as SqlValue, Connection, Row};
use serde_json::{json, Map, Value};

impl Tasks {
    /// Python `Tasks.for_pane`: visible global/local work and a non-mutating next pick.
    /// `None` uses 20; zero and negative limits retain Python list-slice semantics.
    pub async fn for_pane(&self, pane_id: &str, limit: Option<i64>) -> Result<Value, DunkError> {
        let scopes = visible_scopes(Some(pane_id), Some(pane_id))?;
        let pane = pane_id.to_owned();
        let limit = limit.unwrap_or(20);
        let now = crate::clock::now_s();
        self.writer.call(move |conn| {
            let marks = vec!["?"; scopes.len()].join(",");
            let mut query = conn.prepare(&format!(
                "SELECT id FROM tasks WHERE scope IN ({marks}) AND status NOT IN ('done','cancelled')"
            )).map_err(map_sqlite)?;
            let ids = query.query_map(rusqlite::params_from_iter(scopes.iter()), |r| r.get::<_, String>(0))
                .map_err(map_sqlite)?.collect::<Result<Vec<_>, _>>().map_err(map_sqlite)?;
            let mut rows = Vec::with_capacity(ids.len());
            for id in ids {
                let decorated = decorate_conn(conn, &id, Some(&pane), now)?;
                let wire = decorated_wire(conn, &decorated, now)?;
                rows.push((decorated, wire));
            }
            rows.sort_by_key(|(d, _)| order_key(&d.task.priority, d.task.created_at, &d.task.id));
            let global: Vec<_> = rows.iter().filter(|(d, _)| d.is_global).collect();
            let global_rows: Vec<_> = global.iter().take(slice_length(global.len(), limit))
                .map(|(_, wire)| wire.clone()).collect();
            // Python applies the list limit before excluding global rows from the pane list.
            let pane_rows: Vec<_> = rows.iter().take(slice_length(rows.len(), limit))
                .filter(|(d, _)| !d.is_global).map(|(_, wire)| wire.clone()).collect();
            // `next` calls list with its own default 200, independent of the outer limit.
            let global_next = global.iter().take(200).find(|(d, _)| d.eligible).copied();
            let next_pane = (!pane.is_empty() && pane != "global").then_some(pane.as_str());
            let pane_next = next_pane.and_then(|_| rows.iter().take(200)
                .find(|(d, _)| !d.is_global && d.eligible));
            let pick = match (global_next, pane_next) {
                (Some(global), Some(local)) => {
                    let key = |(d, _): &(Decorated, Value)| order_key(&d.task.priority, d.task.created_at, &d.task.id);
                    Some(if key(local) < key(global) { local } else { global })
                }
                (global, local) => global.or(local),
            };
            Ok(json!({
                "pane_id": pane,
                "global": global_rows,
                "pane": pane_rows,
                "next": {
                    "pane_id": next_pane,
                    "global": global_next.map(|(_, wire)| wire),
                    "pane": pane_next.map(|(_, wire)| wire),
                    "pick": pick.map(|(_, wire)| wire),
                }
            }))
        }).await
    }
}

pub(super) fn slice_length(length: usize, limit: i64) -> usize {
    if limit >= 0 {
        length.min(usize::try_from(limit).unwrap_or(usize::MAX))
    } else {
        length.saturating_sub(usize::try_from(limit.unsigned_abs()).unwrap_or(usize::MAX))
    }
}

/// Preserve the complete stored row, including metadata absent from the smaller `Task` type.
pub(super) fn row_wire(row: &Row<'_>) -> rusqlite::Result<Map<String, Value>> {
    let mut result = Map::new();
    for (index, name) in row.as_ref().column_names().iter().enumerate() {
        let value = match row.get::<_, SqlValue>(index)? {
            SqlValue::Null => Value::Null,
            SqlValue::Integer(value) => json!(value),
            SqlValue::Real(value) => crate::github::python_float_json_value(value)
                .expect("SQLite REAL cannot contain NaN"),
            SqlValue::Text(value) => Value::String(value),
            SqlValue::Blob(_) => {
                return Err(rusqlite::Error::InvalidColumnType(
                    index,
                    (*name).to_owned(),
                    rusqlite::types::Type::Blob,
                ))
            }
        };
        result.insert((*name).to_owned(), value);
    }
    Ok(result)
}

pub(super) fn decorated_wire(
    conn: &Connection,
    d: &Decorated,
    now: f64,
) -> Result<Value, DunkError> {
    let mut wire = conn
        .query_row("SELECT * FROM tasks WHERE id = ?", [&d.task.id], row_wire)
        .map_err(map_sqlite)?;
    let mut query = conn
        .prepare("SELECT * FROM task_leases WHERE task_id = ? AND expires_at > ?")
        .map_err(map_sqlite)?;
    let leases = query
        .query_map(params![d.task.id, now], row_wire)
        .map_err(map_sqlite)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(map_sqlite)?;
    for (key, value) in [
        ("assignees", json!(d.assignees)),
        ("depends_on", json!(d.depends_on)),
        ("allocation_waiting", json!(d.allocation_waiting)),
        ("multi_assignee", json!(d.multi_assignee)),
        ("is_global", json!(d.is_global)),
        ("leases", json!(leases)),
        ("claimed_by", json!(d.claimed_by)),
        (
            "lease_expires_at",
            d.lease_expires_at
                .map(|value| {
                    crate::github::python_float_json_value(value)
                        .expect("task lease expiry cannot be NaN")
                })
                .unwrap_or(Value::Null),
        ),
        ("blocked_by", json!(d.deps.blocked_by)),
        ("unlanded_dependencies", json!(d.deps.unlanded_dependencies)),
        ("dead_dependencies", json!(d.deps.dead_dependencies)),
        ("missing_dependencies", json!(d.deps.missing_dependencies)),
        ("unblock_needed", json!(d.deps.unblock_needed)),
        ("dependency_note", json!(d.deps.dependency_note)),
        ("claimable", json!(d.claimable)),
        ("eligible", json!(d.eligible)),
    ] {
        wire.insert(key.into(), value);
    }
    Ok(Value::Object(wire))
}

#[cfg(test)]
mod tests;
