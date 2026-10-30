//! The task store (T11), ported from `dunk_tasks.py` - ASYNC, because the writer is.
//!
//! `db::Writer::call` is `pub async fn` over a tokio actor (T05), so every method here is
//! `async` and every test is `#[tokio::test]`. That is not a style choice: a synchronous
//! method cannot reach the writer, and a lane that starts T11 without knowing this writes
//! sync code that cannot compile against it.
//!
//! The SCHEMA below is extracted from `dunk_tasks.py` rather than retyped - a retyped
//! schema is a schema that drifts, and this task's acceptance is that the port opens a
//! COPY of the live `tasks.db` and lists identical rows.
//!
//! `migrate` is the half that is easy to forget: a live database's `CREATE TABLE` is the
//! PRE-MIGRATION text (measured: the live `tasks` table has 25 effective columns where its
//! `sqlite_master` DDL lists 18), so the port must add what is missing or it reads fewer
//! fields than the daemon wrote.
//!
//! PORTED: schema, migration, add, get, list, the row model.
//! NOT PORTED (next steps in TASKS.md order): the status transitions, `_decorate`/
//! `_order_key` beyond what get/list need, the events writers, and the live-copy acceptance.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};

use crate::db;
use crate::error::DunkError;

/// The Python store's schema, verbatim for the three tables this module owns.
pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS tasks (
    id            TEXT PRIMARY KEY,
    scope         TEXT NOT NULL,
    title         TEXT NOT NULL,
    body          TEXT NOT NULL DEFAULT '',
    status        TEXT NOT NULL DEFAULT 'open',
    priority      TEXT NOT NULL DEFAULT 'low',
    parent_id     TEXT,
    multi_assignee INTEGER NOT NULL DEFAULT 0,
    attempts      INTEGER NOT NULL DEFAULT 0,
    assignees     TEXT NOT NULL DEFAULT '[]',
    depends_on    TEXT NOT NULL DEFAULT '[]',
    created_at    REAL NOT NULL,
    updated_at    REAL NOT NULL,
    created_by    TEXT,
    completed_by  TEXT,
    completed_at  REAL,
    cancelled_at  REAL,
    block_reason  TEXT,
    kind          TEXT NOT NULL DEFAULT 'work',
    author        TEXT,
    repo          TEXT,
    pr            INTEGER,
    head          TEXT,
    verdict       TEXT,
    accounting_pr INTEGER,
    -- t906: 1 when this task's work is on main. A task completed on a STACKED
    -- branch is done but not landed, and its dependents must not open on it.
    landed        INTEGER NOT NULL DEFAULT 1
);
-- One live review per PR head (tasks_one_review_per_head) is created in
-- _migrate, after the kind/repo/pr/head columns exist on older databases: a
-- duplicate is refused by the store, not by a check in the timer (the CoS
-- filed twelve identical reviews for PR #117).
CREATE TABLE IF NOT EXISTS task_leases (
    task_id     TEXT NOT NULL,
    pane_id     TEXT NOT NULL,
    claimed_at  REAL NOT NULL,
    heartbeat_at REAL NOT NULL,
    expires_at  REAL NOT NULL,
    caller_pid   INTEGER,
    caller_start TEXT,
    caller_name  TEXT,
    -- 1 when _claim_locked added this pane to tasks.assignees, 0 when the pane
    -- was already an assignee (the owner's own assignment). release() removes
    -- only what the claim added, so giving a claim back returns the task to the
    -- queue instead of stranding it behind an assignee nobody chose (t595).
    assignee_added INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (task_id, pane_id)
);
CREATE TABLE IF NOT EXISTS task_events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id     TEXT NOT NULL,
    event       TEXT NOT NULL,
    at          REAL NOT NULL,
    actor       TEXT,
    scope       TEXT,
    priority    TEXT,
    status_from TEXT,
    status_to   TEXT,
    duration_ms INTEGER,
    meta        TEXT
);
"#;

/// `dunk_tasks.DEFAULT_STATUS`.
pub const DEFAULT_STATUS: &str = "open";
/// `dunk_tasks.DEFAULT_PRIORITY`.
pub const DEFAULT_PRIORITY: &str = "low";
/// `dunk_tasks.DEFAULT_KIND`.
pub const DEFAULT_KIND: &str = "work";

/// One task row, as the store keeps it.
#[derive(Debug, Clone, PartialEq)]
pub struct Task {
    /// `t<n>`.
    pub id: String,
    /// `global` or a pane id.
    pub scope: String,
    /// One line.
    pub title: String,
    /// The body.
    pub body: String,
    /// open | in_progress | blocked | done | cancelled.
    pub status: String,
    /// urgent | high | medium | low.
    pub priority: String,
    /// work | review | verify | report.
    pub kind: String,
    /// JSON array of pane ids.
    pub assignees: String,
    /// JSON array of task ids.
    pub depends_on: String,
    /// Why it is blocked.
    pub block_reason: Option<String>,
    /// Unix seconds.
    pub created_at: f64,
    /// Unix seconds.
    pub updated_at: f64,
}

/// The task store: one writer actor, as `db.rs` provides.
pub struct Tasks {
    writer: db::Writer,
    path: PathBuf,
}

impl Tasks {
    /// Open (or create) the store and bring its schema up to date.
    pub async fn open(path: impl AsRef<Path>) -> Result<Tasks, DunkError> {
        let path = path.as_ref().to_path_buf();
        let writer = db::Writer::spawn(&path)?;
        writer
            .call(|conn| {
                conn.execute_batch(SCHEMA).map_err(db::map_sqlite)?;
                migrate(conn)
            })
            .await?;
        Ok(Tasks { writer, path })
    }

    /// The file this store owns.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Add a task with the Python's defaults.
    pub async fn add(&self, scope: &str, title: &str, body: &str) -> Result<Task, DunkError> {
        let title = title.trim().to_string();
        if title.is_empty() {
            return Err(DunkError::Refused("a task needs a title".into()));
        }
        let scope = scope.to_string();
        let body = body.to_string();
        let now = crate::clock::now_s();
        let id = self.writer.call(move |conn| {
            let next: i64 = conn.query_row(
                "SELECT COALESCE(MAX(CAST(SUBSTR(id, 2) AS INTEGER)), 0) + 1 FROM tasks",
                [], |row| row.get(0)).map_err(db::map_sqlite)?;
            let id = format!("t{next}");
            conn.execute(
                "INSERT INTO tasks (id, scope, title, body, status, priority, kind, assignees, depends_on, created_at, updated_at) VALUES (?,?,?,?,?,?,?,?,?,?,?)",
                params![id, scope, title, body, DEFAULT_STATUS, DEFAULT_PRIORITY,
                        DEFAULT_KIND, "[]", "[]", now, now],
            ).map_err(db::map_sqlite)?;
            Ok(id)
        }).await?;
        self.get(&id).await
    }

    /// One task by id.
    pub async fn get(&self, id: &str) -> Result<Task, DunkError> {
        let wanted = id.to_string();
        let id = wanted.clone();
        let found = self.writer.call(move |conn| {
            conn.query_row(
                "SELECT id, scope, title, body, status, priority, kind, assignees, depends_on, block_reason, created_at, updated_at FROM tasks WHERE id = ?",
                params![id], row_to_task).optional().map_err(db::map_sqlite)
        }).await?;
        found.ok_or_else(|| DunkError::Refused(format!("no task with id {wanted:?}")))
    }

    /// Tasks in one scope, oldest first (the Python's `list` order).
    pub async fn list(&self, scope: &str) -> Result<Vec<Task>, DunkError> {
        let scope = scope.to_string();
        self.writer.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, scope, title, body, status, priority, kind, assignees, depends_on, block_reason, created_at, updated_at FROM tasks WHERE scope = ? ORDER BY created_at, id").map_err(db::map_sqlite)?;
            let rows = stmt.query_map(params![scope], row_to_task).map_err(db::map_sqlite)?;
            let collected: Result<Vec<_>, _> = rows.collect();
            Ok(collected.map_err(db::map_sqlite)?)
        }).await
    }
}

fn row_to_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        id: row.get(0)?,
        scope: row.get(1)?,
        title: row.get(2)?,
        body: row.get(3)?,
        status: row.get(4)?,
        priority: row.get(5)?,
        kind: row.get(6)?,
        assignees: row.get(7)?,
        depends_on: row.get(8)?,
        block_reason: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

/// Add the columns a live database may be missing (`dunk_tasks._migrate`).
fn migrate(conn: &Connection) -> Result<(), DunkError> {
    // Every column the live table has, in its own order: an old file gains whatever
    // it lacks. Derived from PRAGMA table_info on the live store, not retyped.
    let wanted = [
        ("id", "TEXT"),
        ("scope", "TEXT NOT NULL"),
        ("title", "TEXT NOT NULL"),
        ("body", "TEXT NOT NULL DEFAULT ''"),
        ("status", "TEXT NOT NULL DEFAULT 'open'"),
        ("priority", "TEXT NOT NULL DEFAULT 'medium'"),
        ("parent_id", "TEXT"),
        ("multi_assignee", "INTEGER NOT NULL DEFAULT 0"),
        ("attempts", "INTEGER NOT NULL DEFAULT 0"),
        ("assignees", "TEXT NOT NULL DEFAULT '[]'"),
        ("depends_on", "TEXT NOT NULL DEFAULT '[]'"),
        ("created_at", "REAL NOT NULL"),
        ("updated_at", "REAL NOT NULL"),
        ("created_by", "TEXT"),
        ("completed_by", "TEXT"),
        ("completed_at", "REAL"),
        ("cancelled_at", "REAL"),
        ("block_reason", "TEXT"),
        ("kind", "TEXT NOT NULL DEFAULT 'work'"),
        ("author", "TEXT"),
        ("repo", "TEXT"),
        ("pr", "INTEGER"),
        ("head", "TEXT"),
        ("verdict", "TEXT"),
        ("accounting_pr", "INTEGER"),
    ];
    let have: Vec<String> = conn
        .prepare("PRAGMA table_info(tasks)")
        .map_err(db::map_sqlite)?
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(db::map_sqlite)?
        .collect::<Result<_, _>>()
        .map_err(db::map_sqlite)?;
    for (name, decl) in wanted {
        if !have.iter().any(|column| column == name) {
            conn.execute(&format!("ALTER TABLE tasks ADD COLUMN {name} {decl}"), [])
                .map_err(db::map_sqlite)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store(tag: &str) -> Tasks {
        let dir = std::env::temp_dir().join(format!("ds-store-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tasks.db");
        let _ = std::fs::remove_file(&path);
        Tasks::open(&path).await.unwrap()
    }

    #[tokio::test]
    async fn add_then_get_round_trips() {
        let store = store("roundtrip").await;
        let task = store
            .add("global", "port the store", "body text")
            .await
            .unwrap();
        assert_eq!("t1", task.id);
        assert_eq!(DEFAULT_STATUS, task.status);
        assert_eq!(DEFAULT_PRIORITY, task.priority);
        assert_eq!(DEFAULT_KIND, task.kind);
        assert_eq!(task, store.get("t1").await.unwrap());
    }

    #[tokio::test]
    async fn a_task_needs_a_title() {
        let store = store("title").await;
        let error = store.add("global", "   ", "").await.unwrap_err();
        assert!(error.to_string().contains("needs a title"));
    }

    #[tokio::test]
    async fn list_is_per_scope_and_ordered() {
        let store = store("list").await;
        store.add("global", "first", "").await.unwrap();
        store.add("w2:p1", "private", "").await.unwrap();
        store.add("global", "second", "").await.unwrap();
        let global = store.list("global").await.unwrap();
        assert_eq!(
            vec!["first", "second"],
            global.iter().map(|t| t.title.as_str()).collect::<Vec<_>>()
        );
        assert_eq!(1, store.list("w2:p1").await.unwrap().len());
    }

    #[tokio::test]
    async fn opening_an_old_database_adds_the_columns_it_is_missing() {
        let dir = std::env::temp_dir().join(format!("ds-store-old-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.db");
        let _ = std::fs::remove_file(&path);
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE tasks (id TEXT PRIMARY KEY, scope TEXT NOT NULL, title TEXT NOT NULL, body TEXT NOT NULL DEFAULT '', status TEXT NOT NULL DEFAULT 'open');").unwrap();
        }
        let store = Tasks::open(&path).await.unwrap();
        store.add("global", "after migration", "").await.unwrap();
        let task = store.get("t1").await.unwrap();
        assert_eq!(
            "work", task.kind,
            "the kind column was added by the migration"
        );
        assert_eq!("low", task.priority);
    }
}
