//! `state.rs` (T54, slice 6a): the agent-state store. Ports `dunk_state.py`'s `StateStore`,
//! `SCHEMA`, `_migrate`, `project_of`, `_flag` and the flag-name constants.
//!
//! Continuous agent-state capture: what it would take to put a lane back. A row per lane per
//! capture, with a null and a reason wherever a field is not readable. Nothing here guesses:
//! `model` and `thinking` come from the launching argv when it carries them, and are null with a
//! reason otherwise - a wrong model in a restore is worse than no model, because it looks like a
//! decision.
//!
//! Slice 6b adds the /proc side behind a trait ([`proc`]) and `session_of` ([`session`]).
//! Slice 6c adds `capture` ([`capture`]), over a `CaptureEnv` for the flock-side reads.
//!
//! The `argv` column is written as compact JSON (`["a","b"]`); the Python writes `json.dumps`
//! with spaces. Both read back identically.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection};
use serde_json::{json, Map, Value};

pub mod capture;
pub mod proc;
pub mod session;

use crate::db;
use crate::error::DunkError;

/// `SCHEMA`.
pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS agent_state (
    at REAL NOT NULL,
    pane_id TEXT NOT NULL,
    project TEXT,
    harness TEXT,
    cwd TEXT,
    argv TEXT,
    session_id TEXT,
    session_reason TEXT,
    model TEXT,
    model_reason TEXT,
    thinking TEXT,
    thinking_reason TEXT,
    state TEXT,
    state_source TEXT,
    state_reason TEXT,
    task_id TEXT,
    source TEXT
);
CREATE INDEX IF NOT EXISTS agent_state_pane ON agent_state(pane_id, at DESC);
CREATE INDEX IF NOT EXISTS agent_state_time ON agent_state(at DESC);
";

/// `_MODEL_FLAGS`.
pub const MODEL_FLAGS: [&str; 2] = ["--model", "-m"];
/// `_THINKING_FLAGS`.
pub const THINKING_FLAGS: [&str; 3] = ["--thinking", "--reasoning", "--effort"];
/// `_SESSION_FLAGS`.
pub const SESSION_FLAGS: [&str; 2] = ["--session", "--resume"];
/// `_AGENT_EXES`.
pub const AGENT_EXES: [&str; 4] = ["claude", "pi", "codex", "opencode"];

/// The columns `add` writes, in order.
const COLUMNS: [&str; 17] = [
    "at",
    "pane_id",
    "project",
    "harness",
    "cwd",
    "argv",
    "session_id",
    "session_reason",
    "model",
    "model_reason",
    "thinking",
    "thinking_reason",
    "state",
    "state_source",
    "state_reason",
    "task_id",
    "source",
];

/// `StateStore`.
pub struct StateStore {
    pub path: std::path::PathBuf,
    db: Mutex<Connection>,
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_none_or(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

impl StateStore {
    /// `StateStore(path)`: creates the directory, the schema, and adds columns an older database
    /// is missing without losing its rows.
    pub fn open(path: impl AsRef<Path>) -> Result<StateStore, DunkError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let connection = db::open_writer(&path)?;
        connection.execute_batch(SCHEMA).map_err(db::map_sqlite)?;
        migrate(&connection)?;
        Ok(StateStore {
            path,
            db: Mutex::new(connection),
        })
    }

    fn connection(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.db
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Run a closure on the connection (tests, and the later capture slices).
    pub fn with_connection<T>(&self, work: impl FnOnce(&Connection) -> T) -> T {
        work(&self.connection())
    }

    /// `add(rows, at)`: one row per capture; returns how many were written. A row without a
    /// `pane_id` is the Python's `KeyError`.
    pub fn add(&self, rows: &[Value], at: f64) -> Result<usize, DunkError> {
        let mut connection = self.connection();
        let transaction = connection.transaction().map_err(db::map_sqlite)?;
        {
            let mut statement = transaction
                .prepare(&format!(
                    "INSERT INTO agent_state ({}) VALUES ({})",
                    COLUMNS.join(", "),
                    vec!["?"; COLUMNS.len()].join(",")
                ))
                .map_err(db::map_sqlite)?;
            for row in rows {
                let text = |key: &str| row.get(key).and_then(Value::as_str).map(str::to_string);
                let pane_id =
                    text("pane_id").ok_or_else(|| DunkError::Refused("'pane_id'".to_string()))?;
                let argv = row
                    .get("argv")
                    .filter(|v| truthy(v))
                    .map(serde_json::to_string)
                    .transpose()?;
                statement
                    .execute(params![
                        at,
                        pane_id,
                        text("project"),
                        text("harness"),
                        text("cwd"),
                        argv,
                        text("session_id"),
                        text("session_reason"),
                        text("model"),
                        text("model_reason"),
                        text("thinking"),
                        text("thinking_reason"),
                        text("state"),
                        text("state_source"),
                        text("state_reason"),
                        text("task_id"),
                        sqlite_value(row.get("source").unwrap_or(&Value::Null), 17)?,
                    ])
                    .map_err(db::map_sqlite)?;
            }
        }
        transaction.commit().map_err(db::map_sqlite)?;
        Ok(rows.len())
    }

    /// `latest(pane_id, project, limit)`: newest first; `argv` is decoded (null when it does not
    /// parse).
    pub fn latest(
        &self,
        pane_id: Option<&str>,
        project: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Value>, DunkError> {
        self.latest_raw(
            pane_id.map(|v| json!(v)).as_ref(),
            project.map(|v| json!(v)).as_ref(),
            Some(limit),
        )
    }

    /// The newest sample of every pane sampled at or after `since`, keyed by pane id (t4473):
    /// `{at, model, model_reason, thinking, argv}`. One query; nothing is written.
    pub fn latest_model_samples(
        &self,
        since: f64,
    ) -> Result<std::collections::HashMap<String, Value>, DunkError> {
        let connection = self.connection();
        let mut statement = connection
            .prepare(
                "SELECT pane_id, MAX(at), model, model_reason, thinking, argv FROM agent_state \
                 WHERE at >= ? GROUP BY pane_id",
            )
            .map_err(db::map_sqlite)?;
        let rows = statement
            .query_map([since], |row| {
                let pane: String = row.get(0)?;
                let argv: Option<String> = row.get(5)?;
                let argv = argv
                    .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                    .unwrap_or(Value::Null);
                Ok((
                    pane,
                    json!({
                        "at": row.get::<_, f64>(1)?,
                        "model": row.get::<_, Option<String>>(2)?,
                        "model_reason": row.get::<_, Option<String>>(3)?,
                        "thinking": row.get::<_, Option<String>>(4)?,
                        "argv": argv,
                    }),
                ))
            })
            .map_err(db::map_sqlite)?;
        rows.collect::<Result<_, _>>().map_err(db::map_sqlite)
    }

    /// Raw Python SQLite bindings. `None` limit represents an integer outside SQLite range;
    /// defer that error until after earlier filter parameters have been bound.
    pub fn latest_raw(
        &self,
        pane_id: Option<&Value>,
        project: Option<&Value>,
        limit: Option<i64>,
    ) -> Result<Vec<Value>, DunkError> {
        let mut clauses = Vec::new();
        let mut values = Vec::new();
        for (column, value) in [("pane_id", pane_id), ("project", project)] {
            if let Some(value) = value.filter(|v| truthy(v)) {
                clauses.push(format!("{column} = ?"));
                values.push(value);
            }
        }
        let clause = if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        };
        let connection = self.connection();
        let mut statement = connection
            .prepare(&format!(
                "SELECT * FROM agent_state{clause} ORDER BY at DESC LIMIT ?"
            ))
            .map_err(db::map_sqlite)?;
        let names: Vec<String> = statement
            .column_names()
            .iter()
            .map(|n| (*n).to_string())
            .collect();
        let values = values
            .iter()
            .enumerate()
            .map(|(index, value)| sqlite_value(value, index + 1))
            .collect::<Result<Vec<_>, _>>()?;
        let limit = limit.ok_or_else(|| {
            DunkError::Refused("Python int too large to convert to SQLite INTEGER".into())
        })?;
        let mut bound: Vec<&dyn rusqlite::ToSql> =
            values.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
        bound.push(&limit);
        let mapped = statement
            .query_map(bound.as_slice(), |row| {
                let mut object = Map::new();
                for (index, name) in names.iter().enumerate() {
                    let value: rusqlite::types::Value = row.get(index)?;
                    object.insert(
                        name.clone(),
                        match value {
                            rusqlite::types::Value::Null => Value::Null,
                            rusqlite::types::Value::Integer(i) => json!(i),
                            rusqlite::types::Value::Real(f) => json!(f),
                            rusqlite::types::Value::Text(t) => json!(t),
                            rusqlite::types::Value::Blob(_) => Value::Null,
                        },
                    );
                }
                Ok(Value::Object(object))
            })
            .map_err(db::map_sqlite)?;
        let mut rows = Vec::new();
        for row in mapped {
            let mut row = row.map_err(db::map_sqlite)?;
            if let Some(Value::String(text)) = row.get("argv").filter(|v| truthy(v)) {
                row["argv"] = serde_json::from_str(text).unwrap_or(Value::Null);
            }
            rows.push(row);
        }
        Ok(rows)
    }
}

/// `_migrate`: add columns an older database is missing, without losing its rows.
///
/// session_reason exists because a null session_id used to be explained with the MODEL reason
/// string ("the launching argv carries no model flag"), which is false for session_id and left a
/// restore with nothing to refuse on. A field's reason must name its own field.
fn migrate(connection: &Connection) -> Result<(), DunkError> {
    let mut statement = connection
        .prepare("PRAGMA table_info(agent_state)")
        .map_err(db::map_sqlite)?;
    let columns: Vec<String> = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(db::map_sqlite)?
        .collect::<Result<_, _>>()
        .map_err(db::map_sqlite)?;
    if !columns.iter().any(|c| c == "session_reason") {
        connection
            .execute("ALTER TABLE agent_state ADD COLUMN session_reason TEXT", [])
            .map_err(db::map_sqlite)?;
    }
    Ok(())
}

/// `project_of`: the project label: the workspace directory name, or `None`.
pub fn project_of(cwd: Option<&str>) -> Option<String> {
    let cwd = cwd.filter(|c| !c.is_empty())?;
    // The Python's `home` branch returns `parts[-1]` on both sides of its length test.
    cwd.trim()
        .trim_matches('/')
        .split('/')
        .rfind(|part| !part.is_empty())
        .map(str::to_string)
}

/// `_flag(argv, names)`: the value after a flag, or the value of `--flag=value`.
pub fn flag(argv: &[String], names: &[&str]) -> Option<String> {
    for (index, token) in argv.iter().enumerate() {
        if names.contains(&token.as_str()) && index + 1 < argv.len() {
            return Some(argv[index + 1].clone());
        }
        for name in names {
            if let Some(value) = token.strip_prefix(&format!("{name}=")) {
                return Some(value.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod capture_tests;
#[cfg(test)]
mod session_tests;
#[cfg(test)]
mod tests;

/// Python sqlite3 accepts JSON scalars without coercing containers to text.
fn sqlite_value(value: &Value, index: usize) -> Result<rusqlite::types::Value, DunkError> {
    use rusqlite::types::Value as Sql;
    Ok(match value {
        Value::Null => Sql::Null,
        Value::Bool(v) => Sql::Integer(i64::from(*v)),
        Value::String(v) => Sql::Text(v.clone()),
        Value::Number(v) => {
            if let Some(n) = v.as_i64() {
                Sql::Integer(n)
            } else if v.as_u64().is_some() || !v.to_string().contains(['.', 'e', 'E']) {
                return Err(DunkError::Refused(
                    "Python int too large to convert to SQLite INTEGER".into(),
                ));
            } else {
                // arbitrary_precision Number::as_f64 excludes nonfinite results. JSON float
                // tokens can still overflow in Python; preserve their sign when binding.
                Sql::Real(v.as_f64().unwrap_or_else(|| {
                    if v.to_string().starts_with('-') {
                        f64::NEG_INFINITY
                    } else {
                        f64::INFINITY
                    }
                }))
            }
        }
        Value::Array(_) | Value::Object(_) => {
            return Err(DunkError::Refused(format!(
                "Error binding parameter {index}: type '{}' is not supported",
                if value.is_array() { "list" } else { "dict" }
            )))
        }
    })
}
