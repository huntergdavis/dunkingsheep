//! `state.rs` (T54, slice 6a): the agent-state store. Ports `dunk_state.py`'s `StateStore`,
//! `SCHEMA`, `_migrate`, `project_of`, `_flag` and the flag-name constants.
//!
//! Continuous agent-state capture: what it would take to put a lane back. A row per lane per
//! capture, with a null and a reason wherever a field is not readable. Nothing here guesses:
//! `model` and `thinking` come from the launching argv when it carries them, and are null with a
//! reason otherwise - a wrong model in a restore is worse than no model, because it looks like a
//! decision.
//!
//! NOT in this slice (6b/6c): `capture`, `session_of` and the /proc walkers (`_proc_snapshot`,
//! `_read_bound`, `_still_the_same_process`, `_descendants`, `_env_of`, `_active_session_file`).
//!
//! The `argv` column is written as compact JSON (`["a","b"]`); the Python writes `json.dumps`
//! with spaces. Both read back identically.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection};
use serde_json::{json, Map, Value};

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
                    .map(|v| serde_json::to_string(v))
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
                        text("source"),
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
        let mut clauses = Vec::new();
        let mut values: Vec<String> = Vec::new();
        if let Some(pane) = pane_id.filter(|p| !p.is_empty()) {
            clauses.push("pane_id = ?");
            values.push(pane.to_string());
        }
        if let Some(project) = project.filter(|p| !p.is_empty()) {
            clauses.push("project = ?");
            values.push(project.to_string());
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
        .filter(|part| !part.is_empty())
        .last()
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
mod tests;
