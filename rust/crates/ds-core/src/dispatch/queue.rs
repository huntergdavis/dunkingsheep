//! The standby queue (`dunk_dispatch.Queue`): ordered assignments any ready lane can take, in a
//! small sqlite file (`dispatch.db`).

use std::fmt;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection};
use serde_json::{json, Map, Value};

use crate::db;
use crate::error::DunkError;

/// `DispatchError`: a named refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchError(pub String);

impl fmt::Display for DispatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DispatchError {}

impl From<DunkError> for DispatchError {
    fn from(error: DunkError) -> Self {
        DispatchError(error.to_string())
    }
}

/// `SCHEMA`.
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS standby (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id TEXT,
    brief TEXT NOT NULL,
    lane TEXT,
    created_at REAL NOT NULL,
    taken_at REAL,
    taken_by TEXT,
    sent_message TEXT
);
";

fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_none_or(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn python_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(number) if number.is_i64() || number.is_u64() => "int",
        Value::Number(_) => "float",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

fn sqlite_value(value: &Value, parameter: usize) -> Result<rusqlite::types::Value, DispatchError> {
    use rusqlite::types::Value as SqlValue;
    Ok(match value {
        Value::Null => SqlValue::Null,
        Value::Bool(value) => SqlValue::Integer(if *value { 1 } else { 0 }),
        Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                SqlValue::Integer(value)
            } else if let Some(value) = value.as_u64() {
                let value = i64::try_from(value).map_err(|_| {
                    DispatchError(format!(
                        "Error binding parameter {parameter}: Python int too large to convert to SQLite INTEGER"
                    ))
                })?;
                SqlValue::Integer(value)
            } else if let Some(value) = value.as_f64() {
                SqlValue::Real(value)
            } else {
                return Err(DispatchError(format!(
                    "Error binding parameter {parameter}: Python int too large to convert to SQLite INTEGER"
                )));
            }
        }
        Value::String(value) => SqlValue::Text(value.clone()),
        value => {
            return Err(DispatchError(format!(
                "Error binding parameter {parameter}: type '{}' is not supported",
                python_type_name(value)
            )))
        }
    })
}

fn python_repr(value: &Value) -> String {
    crate::api::python_value_repr(value)
}

/// The standby list.
pub struct Queue {
    db: Mutex<Connection>,
}

fn row_value(row: &rusqlite::Row<'_>, names: &[String]) -> rusqlite::Result<Value> {
    let mut object = Map::new();
    for (index, name) in names.iter().enumerate() {
        let value: rusqlite::types::Value = row.get(index)?;
        object.insert(
            name.clone(),
            match value {
                rusqlite::types::Value::Null | rusqlite::types::Value::Blob(_) => Value::Null,
                rusqlite::types::Value::Integer(i) => json!(i),
                rusqlite::types::Value::Real(f) => json!(f),
                rusqlite::types::Value::Text(t) => json!(t),
            },
        );
    }
    Ok(Value::Object(object))
}

impl Queue {
    /// `Queue(path)`; `":memory:"` is an in-memory queue, like the Python.
    pub fn open(path: impl AsRef<Path>) -> Result<Queue, DispatchError> {
        let path = path.as_ref();
        let connection = if path == Path::new(":memory:") {
            Connection::open_in_memory().map_err(|e| DispatchError(e.to_string()))?
        } else {
            if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                std::fs::create_dir_all(parent).map_err(|e| DispatchError(e.to_string()))?;
            }
            db::open_writer(path)?
        };
        connection
            .execute_batch(SCHEMA)
            .map_err(|e| DispatchError(e.to_string()))?;
        Ok(Queue {
            db: Mutex::new(connection),
        })
    }

    fn connection(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn fetch(
        connection: &Connection,
        sql: &str,
        args: &[&dyn rusqlite::ToSql],
    ) -> Result<Vec<Value>, DispatchError> {
        let mut statement = connection
            .prepare(sql)
            .map_err(|e| DispatchError(e.to_string()))?;
        let names: Vec<String> = statement
            .column_names()
            .iter()
            .map(|n| (*n).to_string())
            .collect();
        let rows = statement
            .query_map(args, |row| row_value(row, &names))
            .map_err(|e| DispatchError(e.to_string()))?;
        rows.collect::<Result<_, _>>()
            .map_err(|e| DispatchError(e.to_string()))
    }

    /// `add(brief, task_id, lane)`: the new row.
    pub fn add(
        &self,
        brief: &str,
        task_id: Option<&str>,
        lane: Option<&str>,
    ) -> Result<Value, DispatchError> {
        let brief = brief.trim();
        if brief.is_empty() {
            return Err(DispatchError(
                "a standby item needs a one-line brief".into(),
            ));
        }
        let id = {
            let connection = self.connection();
            connection
                .execute(
                    "INSERT INTO standby (task_id, brief, lane, created_at) VALUES (?,?,?,?)",
                    params![task_id, brief, lane, crate::clock::now_s()],
                )
                .map_err(|e| DispatchError(e.to_string()))?;
            connection.last_insert_rowid()
        };
        Ok(self.get(id)?.unwrap_or(Value::Null))
    }

    /// Wire-compatible raw add used by `h_dispatch`; preserve Python's validation and SQLite
    /// binding order instead of coercing values in the request adapter.
    pub fn add_values(
        &self,
        brief: &Value,
        task_id: &Value,
        lane: &Value,
    ) -> Result<Value, DispatchError> {
        let brief = match brief {
            Value::Null | Value::Bool(false) => "",
            Value::Number(number) if number.as_f64() == Some(0.0) => "",
            Value::String(text) => text.as_str(),
            value if json_truthy(value) => {
                return Err(DispatchError(format!(
                    "'{}' object has no attribute 'strip'",
                    python_type_name(value)
                )));
            }
            _ => "",
        }
        .trim();
        if brief.is_empty() {
            return Err(DispatchError(
                "a standby item needs a one-line brief".into(),
            ));
        }
        // Python reaches SQLite binding only after `.strip()` and the non-empty check.
        let task_id = sqlite_value(task_id, 1)?;
        let lane = sqlite_value(lane, 2)?;
        let id = {
            let connection = self.connection();
            connection
                .execute(
                    "INSERT INTO standby (task_id, brief, lane, created_at) VALUES (?,?,?,?)",
                    params![task_id, brief, lane, crate::clock::now_s()],
                )
                .map_err(|e| DispatchError(e.to_string()))?;
            connection.last_insert_rowid()
        };
        Ok(self.get(id)?.unwrap_or(Value::Null))
    }

    /// `get(item_id)`.
    pub fn get(&self, item_id: i64) -> Result<Option<Value>, DispatchError> {
        let connection = self.connection();
        let rows = Self::fetch(
            &connection,
            "SELECT * FROM standby WHERE id = ?",
            &[&item_id],
        )?;
        Ok(rows.into_iter().next())
    }

    /// `items(pending_only)`, in id order.
    pub fn items(&self, pending_only: bool) -> Result<Vec<Value>, DispatchError> {
        let where_clause = if pending_only {
            " WHERE taken_at IS NULL"
        } else {
            ""
        };
        let connection = self.connection();
        Self::fetch(
            &connection,
            &format!("SELECT * FROM standby{where_clause} ORDER BY id"),
            &[],
        )
    }

    /// `remove(item_id)`: `{"removed": id}`.
    pub fn remove(&self, item_id: i64) -> Result<Value, DispatchError> {
        let changed = {
            let connection = self.connection();
            connection
                .execute("DELETE FROM standby WHERE id = ?", [item_id])
                .map_err(|e| DispatchError(e.to_string()))?
        };
        if changed == 0 {
            return Err(DispatchError(format!("no standby item {item_id}")));
        }
        Ok(json!({"removed": item_id}))
    }

    /// Raw `Queue.remove(id)` boundary for request handlers.
    pub fn remove_value(&self, item_id: &Value) -> Result<Value, DispatchError> {
        let requested = item_id.clone();
        let item_id = sqlite_value(item_id, 1)?;
        let changed = {
            let connection = self.connection();
            connection
                .execute("DELETE FROM standby WHERE id = ?", [item_id])
                .map_err(|e| DispatchError(e.to_string()))?
        };
        if changed == 0 {
            return Err(DispatchError(format!(
                "no standby item {}",
                python_repr(&requested)
            )));
        }
        Ok(json!({"removed": requested}))
    }

    /// `take(item_id, lane, sent_message, now)`: mark the item taken; refuses a missing or
    /// already-taken item.
    pub fn take(
        &self,
        item_id: i64,
        lane: &str,
        sent_message: Option<&str>,
        now: Option<f64>,
    ) -> Result<Value, DispatchError> {
        {
            let connection = self.connection();
            let rows = Self::fetch(
                &connection,
                "SELECT taken_at FROM standby WHERE id = ?",
                &[&item_id],
            )?;
            let Some(row) = rows.first() else {
                return Err(DispatchError(format!("no standby item {item_id}")));
            };
            if !row["taken_at"].is_null() {
                return Err(DispatchError(format!(
                    "standby item {item_id} was already taken"
                )));
            }
            let taken_at = now
                .filter(|n| *n != 0.0)
                .unwrap_or_else(crate::clock::now_s);
            connection
                .execute(
                    "UPDATE standby SET taken_at = ?, taken_by = ?, sent_message = ? WHERE id = ?",
                    params![taken_at, lane, sent_message, item_id],
                )
                .map_err(|e| DispatchError(e.to_string()))?;
        }
        Ok(self.get(item_id)?.unwrap_or(Value::Null))
    }

    /// `next_for(lane)`: the first pending item assigned to this lane, else the first
    /// unassigned.
    pub fn next_for(&self, lane: &str) -> Result<Option<Value>, DispatchError> {
        let pending = self.items(true)?;
        let lane_of = |item: &Value| {
            item["lane"]
                .as_str()
                .filter(|l| !l.is_empty())
                .map(str::to_string)
        };
        if let Some(item) = pending
            .iter()
            .find(|item| lane_of(item).as_deref() == Some(lane))
        {
            return Ok(Some(item.clone()));
        }
        Ok(pending.into_iter().find(|item| lane_of(item).is_none()))
    }
}
