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
use serde_json::json;

pub mod metrics;

use crate::db;
use crate::error::DunkError;

mod claim;
pub use claim::{claimable, refusal, Candidate, Lease, Refusal};

/// `DEFAULT_LEASE_S`: how long a claim stays valid without a heartbeat. The Python reads
/// `DUNKINGSHEEP_LEASE_S` once at import; this reads it per call.
pub fn default_lease_s() -> f64 {
    std::env::var("DUNKINGSHEEP_LEASE_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1800.0)
}

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
    observers: std::sync::Mutex<Vec<Observer>>,
}

/// An `on_event` callback: (task id, event, completed-by, kind). Cheap, and it must not
/// write the task store; a panicking callback is contained, never raised (`_fire`).
pub type Observer = Box<dyn Fn(&str, &str, Option<&str>, Option<&str>) + Send + Sync>;

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
        Ok(Tasks {
            writer,
            path,
            observers: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// Register an `on_event` observer (`Tasks.on_event`).
    pub fn on_event(&self, observer: Observer) {
        if let Ok(mut observers) = self.observers.lock() {
            observers.push(observer);
        }
    }

    /// `_fire`: run the observers AFTER the commit; one that panics is logged and skipped,
    /// so an observer can never break the store.
    fn fire(&self, id: &str, event: &str, by: Option<&str>, kind: Option<&str>) {
        let Ok(observers) = self.observers.lock() else {
            return;
        };
        for observer in observers.iter() {
            let call = std::panic::AssertUnwindSafe(|| observer(id, event, by, kind));
            if std::panic::catch_unwind(call).is_err() {
                eprintln!("task on_event callback failed task={id} event={event}");
            }
        }
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
            // t1018 finding 5: ONE statement allocates and returns the number, which is
            // exactly the Python's F1 fix ("the old SELECT-then-upsert let two processes
            // compute the same number"). The SELECT-MAX form the slice used is the pattern
            // that comment names.
            let next: i64 = conn
                .query_row(
                    "INSERT INTO counters (name, value) VALUES ('task', 1) \
                     ON CONFLICT(name) DO UPDATE SET value = value + 1 RETURNING value",
                    [], |row| row.get(0),
                )
                .map_err(db::map_sqlite)?;
            let id = format!("t{next}");
            conn.execute(
                "INSERT INTO tasks (id, scope, title, body, status, priority, kind, assignees, depends_on, created_at, updated_at) VALUES (?,?,?,?,?,?,?,?,?,?,?)",
                params![id, scope, title, body, DEFAULT_STATUS, DEFAULT_PRIORITY,
                        DEFAULT_KIND, "[]", "[]", now, now],
            ).map_err(db::map_sqlite)?;
            Ok(id)
        }).await?;
        self.fire(&id, "created", None, None);
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
    /// Set a status, and END THE CLAIM when the status is terminal.
    ///
    /// The Python store learned this the hard way twice today (t872, PR166): `block` left
    /// the lease behind, the reaper found a lease on a task that was not in_progress,
    /// reopened it, and the push loop re-claimed the duplicate - three rounds. `complete`
    /// left it too. So every terminal transition deletes the leases, in the same
    /// transaction as the status write.
    pub async fn set_status(
        &self,
        id: &str,
        status: &str,
        reason: Option<&str>,
        actor: Option<&str>,
    ) -> Result<Task, DunkError> {
        let wanted = id.to_string();
        let id = wanted.clone();
        let status = status.to_string();
        let reason = reason.map(|r| r.to_string());
        let actor = actor.map(|a| a.to_string());
        let now = crate::clock::now_s();
        self.writer
            .call(move |conn| {
                // t1018 finding 1 (a t854 regression): a terminal status is FINAL, and
                // the guard must be the WHERE clause, not a read-then-write check - the
                // Python's update() refuses with this sentence and backs it with
                // `WHERE id = ? AND status = ?` so a concurrent cancel still wins.
                let current: String = conn
                    .query_row("SELECT status FROM tasks WHERE id = ?", params![id],
                               |row| row.get(0))
                    .optional()
                    .map_err(db::map_sqlite)?
                    .ok_or_else(|| DunkError::Refused(format!("no task with id {id:?}")))?;
                if is_terminal(current.as_str()) && current != status {
                    return Err(DunkError::Refused(format!(
                        "task {id} is {current}; a finished task is not reopened by an edit (t854)")));
                }
                // FINDING 2: the transition writes the columns that give it meaning, and
                // exactly one event row - task_metrics is computed from those rows, so a
                // silent transition is invisible to the metrics.
                let (completed_by, completed_at, cancelled_at, block_reason) = match
                    status.as_str()
                {
                    "done" => (actor.clone(), Some(now), None, None),
                    "cancelled" => (None, None, Some(now), reason.clone()),
                    "blocked" => (None, None, None, reason.clone()),
                    _ => (None, None, None, None),
                };
                let changed = conn
                    .execute(
                        "UPDATE tasks SET status = ?, updated_at = ?, completed_by = ?, \
                         completed_at = ?, cancelled_at = ?, block_reason = ? \
                         WHERE id = ? AND status = ?",
                        params![status, now, completed_by, completed_at, cancelled_at,
                                block_reason, id, current],
                    )
                    .map_err(db::map_sqlite)?;
                if changed == 0 {
                    // Someone moved it between our read and our write: re-read and refuse
                    // rather than resurrect it (the t854 race, one level down).
                    let now_status: String = conn
                        .query_row("SELECT status FROM tasks WHERE id = ?", params![id],
                                   |row| row.get(0))
                        .map_err(db::map_sqlite)?;
                    if is_terminal(now_status.as_str()) {
                        return Err(DunkError::Refused(format!(
                            "task {id} is {now_status}; a finished task is not reopened by an                              edit (t854)")));
                    }
                    return Err(DunkError::Retryable(format!(
                        "task {id} changed to {now_status} while this edit was applied; read it again (t854)")));
                }
                let event = match status.as_str() {
                    "done" => "completed",
                    "cancelled" => "cancelled",
                    "blocked" => "blocked",
                    other if current == "blocked" && other == "open" => "unblocked",
                    _ => "status",
                };
                let duration_ms = if status == "done" {
                    let created: f64 = conn
                        .query_row("SELECT created_at FROM tasks WHERE id = ?", params![id],
                                   |row| row.get(0))
                        .map_err(db::map_sqlite)?;
                    Some(((now - created) * 1000.0) as i64)
                } else {
                    None
                };
                conn.execute(
                    "INSERT INTO task_events (task_id, event, at, actor, status_from, \
                     status_to, duration_ms, meta) VALUES (?,?,?,?,?,?,?,?)",
                    params![id, event, now, actor, current, status, duration_ms,
                            reason.clone().map(|r| json!({"reason": r}).to_string())],
                )
                .map_err(db::map_sqlite)?;
                if ends_claim(status.as_str()) {
                    conn.execute("DELETE FROM task_leases WHERE task_id = ?", params![id])
                        .map_err(db::map_sqlite)?;
                }
                // t1218 A: the event path. Completing a task unblocks what it blocks, in
                // the same writer call as the completion, so no lane runs `task unblock`.
                if status == "done" && current != "done" {
                    auto_unblock(conn, now, Some(&id))?;
                }
                Ok(())
            })
            .await?;
        self.get(&wanted).await
    }

    /// `block`: a task that waits on somebody, with the reason recorded.
    pub async fn block(&self, id: &str, reason: &str) -> Result<Task, DunkError> {
        self.set_status(id, "blocked", Some(reason), None).await
    }

    /// `task block <id> --by <tN>` (t1218 B): blocked with reason `blocked-by:<tN>` and
    /// the dependency recorded, so completing `tN` unblocks it (event path A). A free-text
    /// `block` stays manual.
    pub async fn block_by(&self, id: &str, by: &str) -> Result<Task, DunkError> {
        let (id, by) = (id.trim().to_string(), by.trim().to_string());
        if id == by {
            return Err(DunkError::Refused("a task cannot depend on itself".into()));
        }
        let wanted = id.clone();
        let now = crate::clock::now_s();
        self.writer
            .call(move |conn| {
                let exists = |t: &str| -> Result<Option<(String, Option<String>)>, DunkError> {
                    conn.query_row(
                        "SELECT status, depends_on FROM tasks WHERE id = ?",
                        params![t],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()
                    .map_err(db::map_sqlite)
                };
                let (current, raw) = exists(&id)?
                    .ok_or_else(|| DunkError::Refused(format!("no task with id {id:?}")))?;
                exists(&by)?
                    .ok_or_else(|| DunkError::Refused(format!("no task with id {by:?}")))?;
                if is_terminal(current.as_str()) {
                    return Err(DunkError::Refused(format!(
                        "task {id} is {current}; a finished task is not blocked (t854)")));
                }
                let mut deps = decode_list(raw.as_deref());
                if !deps.contains(&by) {
                    deps.push(by.clone());
                }
                let reason = format!("blocked-by:{by}");
                conn.execute(
                    "UPDATE tasks SET status = 'blocked', block_reason = ?, depends_on = ?, updated_at = ? WHERE id = ?",
                    params![reason, json!(deps).to_string(), now, id],
                )
                .map_err(db::map_sqlite)?;
                conn.execute(
                    "INSERT INTO task_events (task_id, event, at, status_from, status_to, meta) VALUES (?,?,?,?,?,?)",
                    params![id, "blocked", now, current, "blocked",
                            json!({"reason": reason}).to_string()],
                )
                .map_err(db::map_sqlite)?;
                // Same rule as `set_status`: a block ends the claim.
                conn.execute("DELETE FROM task_leases WHERE task_id = ?", params![id])
                    .map_err(db::map_sqlite)?;
                Ok(())
            })
            .await?;
        self.get(&wanted).await
    }

    /// Ids the `unblocked` event rows say were opened by `completed_id`'s completion.
    async fn unblocked_by(&self, completed_id: &str) -> Result<Vec<String>, DunkError> {
        let needle = format!("auto: {completed_id} completed");
        self.writer
            .call(move |conn| {
                let mut stmt = conn
                    .prepare("SELECT task_id, meta FROM task_events WHERE event = 'unblocked' AND actor = 'system' ORDER BY id DESC LIMIT 200")
                    .map_err(db::map_sqlite)?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))
                    .map_err(db::map_sqlite)?;
                let mut ids = Vec::new();
                for row in rows {
                    let (task, meta) = row.map_err(db::map_sqlite)?;
                    let reason = meta
                        .and_then(|m| serde_json::from_str::<serde_json::Value>(&m).ok())
                        .and_then(|v| v["reason"].as_str().map(str::to_string));
                    if reason.as_deref() == Some(needle.as_str()) {
                        ids.push(task);
                    }
                }
                Ok(ids)
            })
            .await
    }

    /// t1221 (t1218 C): the idempotent backstop the daemon tick runs. Opens every blocked
    /// task whose block is a dependency block (empty or `blocked-by:*`) and whose
    /// dependencies are all done AND landed, logs one `unblocked` event (actor `system`,
    /// reason `auto: dependencies satisfied (sweep)`) and fires the unblocked observer for
    /// each. A second sweep finds nothing. `needs-owner:`, `DEFERRED`, headroom and other
    /// free-text reasons, and tasks with a cancelled dependency, are never touched. Pure
    /// of the tick: it takes no clock or timer, so the T30 tick only has to call it.
    pub async fn sweep_auto_unblock(&self) -> Result<Vec<String>, DunkError> {
        let now = crate::clock::now_s();
        let opened = self
            .writer
            .call(move |conn| auto_unblock(conn, now, None))
            .await?;
        self.fire_unblocked(&opened);
        Ok(opened)
    }

    fn fire_unblocked(&self, ids: &[String]) {
        for id in ids {
            self.fire(id, "unblocked", Some("system"), None);
        }
    }

    /// `unblock`: back to open, reason cleared.
    pub async fn unblock(&self, id: &str) -> Result<Task, DunkError> {
        self.set_status(id, "open", None, None).await
    }

    /// `cancel`: withdrawn, reason kept, claim ended.
    pub async fn cancel(&self, id: &str, reason: &str) -> Result<Task, DunkError> {
        self.set_status(id, "cancelled", Some(reason), None).await
    }

    /// `complete`: done, claim ended.
    pub async fn complete(&self, id: &str) -> Result<Task, DunkError> {
        let before = self.get(id).await?;
        let done = self.set_status(id, "done", None, None).await?;
        // The Python returns early on an already-done task, so it fires nothing.
        if before.status != "done" {
            self.fire(&done.id, "completed", None, Some(&before.kind));
            // t1218 A: tell the assignee of each task this completion opened, through the
            // observer (the daemon turns it into a guarded inbox nudge).
            let opened = self.unblocked_by(&done.id).await?;
            self.fire_unblocked(&opened);
        }
        Ok(done)
    }

    /// `Flock.task_complete` (dunk_core): complete, then record `landed = false` in the
    /// same act when the work is done and NOT on main (t906, a stacked branch). `landed`
    /// true is the default and writes nothing extra; the dependents of an unlanded task
    /// stay closed until `mark_landed_for_pr` flips it back.
    pub async fn task_complete(
        &self,
        id: &str,
        by: Option<&str>,
        landed: bool,
    ) -> Result<Task, DunkError> {
        if landed {
            return self.complete(id).await;
        }
        // t1218: record the work as NOT on main BEFORE completing, because completion
        // opens the tasks that depend on this one (auto-unblock) and they must see
        // `landed = 0`; the Python order (complete, then set_landed) would open them first.
        self.set_landed(id, false, by, None, None).await?;
        match self.complete(id).await {
            Ok(done) => Ok(done),
            Err(error) => {
                // A failed completion must not leave a stray unlanded flag.
                let _ = self.set_landed(id, true, by, None, None).await;
                Err(error)
            }
        }
    }

    /// Insert a lease directly (test helper: claiming is T12's job).
    #[doc(hidden)]
    pub async fn claim_for_test(&self, id: &str, pane: &str) {
        let (id, pane) = (id.to_string(), pane.to_string());
        let now = crate::clock::now_s();
        self.writer.call(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO task_leases (task_id, pane_id, claimed_at, heartbeat_at, expires_at) VALUES (?,?,?,?,?)",
                params![id, pane, now, now, now + 1800.0],
            ).map_err(db::map_sqlite)?;
            Ok(())
        }).await.expect("insert a lease");
    }

    /// Insert a lease with explicit times (test helper).
    #[doc(hidden)]
    pub async fn insert_lease(&self, id: &str, pane: &str, claimed: f64, beat: f64, expires: f64) {
        let (id, pane) = (id.to_string(), pane.to_string());
        self.writer.call(move |conn| {
            conn.execute(
                "INSERT OR REPLACE INTO task_leases (task_id, pane_id, claimed_at, heartbeat_at, expires_at) VALUES (?,?,?,?,?)",
                params![id, pane, claimed, beat, expires],
            ).map_err(db::map_sqlite)?;
            Ok(())
        }).await.expect("insert a lease");
    }

    /// Live leases on a task (test helper).
    #[doc(hidden)]
    pub async fn leases(&self, id: &str) -> i64 {
        let id = id.to_string();
        self.writer
            .call(move |conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM task_leases WHERE task_id = ?",
                    params![id],
                    |row| row.get(0),
                )
                .map_err(db::map_sqlite)
            })
            .await
            .unwrap_or(-1)
    }

    /// Event count for a task (test helper).
    #[doc(hidden)]
    pub async fn events(&self, id: &str) -> i64 {
        let id = id.to_string();
        self.writer
            .call(move |conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM task_events WHERE task_id = ?",
                    params![id],
                    |row| row.get(0),
                )
                .map_err(db::map_sqlite)
            })
            .await
            .unwrap_or(-1)
    }

    /// Append one history event (`dunk_tasks._record` / `note`).
    pub async fn note(
        &self,
        id: &str,
        event: &str,
        actor: &str,
        reason: Option<&str>,
    ) -> Result<(), DunkError> {
        let (id, event, actor) = (id.to_string(), event.to_string(), actor.to_string());
        let reason = reason.map(|r| r.to_string());
        let now = crate::clock::now_s();
        self.writer
            .call(move |conn| {
                // t1018 finding 3: the meta is built BY serde_json, so a reason with a
                // newline or a tab is escaped the way the Python's json.dumps escapes it.
                // The hand-rolled escaper handled only backslash and quote, so a
                // multi-line evidence note wrote INVALID JSON - and recent_notes() skips
                // unparseable rows, which would make the normal shape of a reviewer's
                // note invisible (the t605 problem this API exists to fix). An empty meta
                // is "{}", not NULL, as the Python writes it.
                let meta = match reason.as_deref() {
                    Some(text) => json!({ "reason": text }).to_string(),
                    None => "{}".to_string(),
                };
                conn.execute(
                    "INSERT INTO task_events (task_id, event, at, actor, meta) VALUES (?,?,?,?,?)",
                    params![id, event, now, actor, meta],
                )
                .map_err(db::map_sqlite)?;
                Ok(())
            })
            .await
    }
    /// `assign` (t786 slice 4): add, remove or replace the assignees. The read-modify-write
    /// is ONE immediate transaction, the Python's F4: a concurrent assign must not be lost
    /// between the read and the update.
    pub async fn assign(
        &self,
        id: &str,
        assignees: &[&str],
        mode: &str,
    ) -> Result<Task, DunkError> {
        let incoming: Vec<String> = assignees
            .iter()
            .map(|item| item.trim().to_string())
            .filter(|item| !item.is_empty())
            .collect();
        if incoming.is_empty() {
            return Err(DunkError::Refused(
                "at least one assignee is required".into(),
            ));
        }
        let mode = match mode.trim().to_lowercase().as_str() {
            "" | "add" | "append" => "add",
            "remove" | "rm" => "remove",
            "set" | "replace" => "set",
            _ => {
                return Err(DunkError::Refused(
                    "assign mode must be add, remove or set".into(),
                ))
            }
        };
        let wanted = id.trim().to_string();
        let id = wanted.clone();
        let now = crate::clock::now_s();
        self.writer
            .call(move |conn| {
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(db::map_sqlite)?;
                let (scope, priority, raw): (String, String, Option<String>) = tx
                    .query_row(
                        "SELECT scope, priority, assignees FROM tasks WHERE id = ?",
                        params![id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()
                    .map_err(db::map_sqlite)?
                    .ok_or_else(|| DunkError::Refused(format!("no task with id {id:?}")))?;
                let current = decode_list(raw.as_deref());
                let merged: Vec<String> = match mode {
                    "add" => {
                        let mut out = current.clone();
                        for item in &incoming {
                            if !out.contains(item) {
                                out.push(item.clone());
                            }
                        }
                        out
                    }
                    "remove" => current.into_iter().filter(|item| !incoming.contains(item)).collect(),
                    _ => incoming.clone(),
                };
                tx.execute(
                    "UPDATE tasks SET assignees = ?, updated_at = ? WHERE id = ?",
                    params![json!(merged).to_string(), now, id],
                )
                .map_err(db::map_sqlite)?;
                tx.execute(
                    "INSERT INTO task_events (task_id, event, at, scope, priority, meta) VALUES (?,?,?,?,?,?)",
                    params![id, "assigned", now, scope, priority,
                            json!({ "assignees": merged }).to_string()],
                )
                .map_err(db::map_sqlite)?;
                tx.commit().map_err(db::map_sqlite)
            })
            .await?;
        self.get(&wanted).await
    }

    /// `delete` (t786 slice 4): refused while the task has children or dependents, and the
    /// checks and the delete are one immediate transaction (the Python's F4).
    pub async fn delete(&self, id: &str) -> Result<String, DunkError> {
        let wanted = id.trim().to_string();
        let now = crate::clock::now_s();
        self.writer
            .call(move |conn| {
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(db::map_sqlite)?;
                let id = wanted.clone();
                let (scope, priority, status): (String, String, String) = tx
                    .query_row(
                        "SELECT scope, priority, status FROM tasks WHERE id = ?",
                        params![id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()
                    .map_err(db::map_sqlite)?
                    .ok_or_else(|| DunkError::Refused(format!("no task with id {wanted:?}")))?;
                let children: i64 = tx
                    .query_row("SELECT COUNT(*) FROM tasks WHERE parent_id = ?", params![id],
                               |row| row.get(0))
                    .map_err(db::map_sqlite)?;
                if children > 0 {
                    return Err(DunkError::Refused(format!(
                        "task {id} has {children} child task(s); delete or re-parent them first")));
                }
                let mut dependents = Vec::new();
                {
                    let mut stmt = tx
                        .prepare("SELECT id, depends_on FROM tasks")
                        .map_err(db::map_sqlite)?;
                    let rows = stmt
                        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)))
                        .map_err(db::map_sqlite)?;
                    for row in rows {
                        let (other, raw) = row.map_err(db::map_sqlite)?;
                        if decode_list(raw.as_deref()).contains(&id) {
                            dependents.push(other);
                        }
                    }
                }
                if !dependents.is_empty() {
                    return Err(DunkError::Refused(format!(
                        "task {id} is a dependency of {}", dependents.join(", "))));
                }
                tx.execute(
                    "INSERT INTO task_events (task_id, event, at, scope, priority, status_from) VALUES (?,?,?,?,?,?)",
                    params![id, "deleted", now, scope, priority, status],
                )
                .map_err(db::map_sqlite)?;
                tx.execute("DELETE FROM tasks WHERE id = ?", params![id]).map_err(db::map_sqlite)?;
                tx.commit().map_err(db::map_sqlite)?;
                Ok(id)
            })
            .await
    }

    /// `update` for the non-status fields (t786 slice 5); see [`FieldEdit`].
    pub async fn update(
        &self,
        id: &str,
        edit: FieldEdit,
        actor: Option<&str>,
    ) -> Result<Task, DunkError> {
        if let Some(title) = &edit.title {
            if title.trim().is_empty() {
                return Err(DunkError::Refused("a task needs a title".into()));
            }
        }
        let priority = match &edit.priority {
            Some(value) => Some(validate_priority(value)?),
            None => None,
        };
        let wanted = id.trim().to_string();
        let actor = actor.map(|a| a.to_string());
        let now = crate::clock::now_s();
        let edit_for_call = edit.clone();
        self.writer
            .call(move |conn| {
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(db::map_sqlite)?;
                let id = wanted.clone();
                let (scope, old_priority, old_deps, old_assignees): (String, String, Option<String>, Option<String>) = tx
                    .query_row(
                        "SELECT scope, priority, depends_on, assignees FROM tasks WHERE id = ?",
                        params![id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .optional()
                    .map_err(db::map_sqlite)?
                    .ok_or_else(|| DunkError::Refused(format!("no task with id {wanted:?}")))?;
                let mut deps_after: Option<Vec<String>> = None;
                if let Some(deps) = &edit_for_call.depends_on {
                    let deps: Vec<String> = deps.iter().map(|d| d.trim().to_string()).filter(|d| !d.is_empty()).collect();
                    if deps.contains(&id) {
                        return Err(DunkError::Refused("a task cannot depend on itself".into()));
                    }
                    for dep in &deps {
                        let found: Option<String> = tx
                            .query_row("SELECT id FROM tasks WHERE id = ?", params![dep], |row| row.get(0))
                            .optional()
                            .map_err(db::map_sqlite)?;
                        if found.is_none() {
                            return Err(DunkError::Refused(format!("no task with id {dep:?}")));
                        }
                    }
                    deps_after = Some(deps);
                }
                let mut changed = false;
                if let Some(parent) = &edit_for_call.parent_id {
                    let parent = parent.as_deref().map(str::trim).filter(|p| !p.is_empty());
                    if let Some(parent) = parent {
                        if parent == id {
                            return Err(DunkError::Refused("a task cannot be its own parent".into()));
                        }
                        let found: Option<String> = tx
                            .query_row("SELECT id FROM tasks WHERE id = ?", params![parent], |row| row.get(0))
                            .optional()
                            .map_err(db::map_sqlite)?;
                        if found.is_none() {
                            return Err(DunkError::Refused(format!("no task with id {parent:?}")));
                        }
                    }
                    tx.execute("UPDATE tasks SET parent_id = ? WHERE id = ?", params![parent, id]).map_err(db::map_sqlite)?;
                    changed = true;
                }
                if let Some(flag) = edit_for_call.multi_assignee {
                    tx.execute("UPDATE tasks SET multi_assignee = ? WHERE id = ?", params![flag as i64, id]).map_err(db::map_sqlite)?;
                    changed = true;
                }
                if let Some(reason) = &edit_for_call.block_reason {
                    tx.execute("UPDATE tasks SET block_reason = ? WHERE id = ?", params![reason, id]).map_err(db::map_sqlite)?;
                    changed = true;
                }
                if let Some(list) = &edit_for_call.assignees {
                    let after: Vec<String> = list.iter().map(|a| a.trim().to_string()).filter(|a| !a.is_empty()).collect();
                    let before = decode_list(old_assignees.as_deref());
                    tx.execute("UPDATE tasks SET assignees = ? WHERE id = ?", params![json!(after).to_string(), id]).map_err(db::map_sqlite)?;
                    changed = true;
                    if before != after {
                        tx.execute(
                            "INSERT INTO task_events (task_id, event, at, actor, scope, priority, meta) VALUES (?,?,?,?,?,?,?)",
                            params![id, "assigned", now, actor, scope, old_priority,
                                    json!({ "assignees": after }).to_string()],
                        ).map_err(db::map_sqlite)?;
                    }
                }
                if let Some(title) = &edit_for_call.title {
                    tx.execute("UPDATE tasks SET title = ? WHERE id = ?", params![title.trim(), id]).map_err(db::map_sqlite)?;
                    changed = true;
                }
                if let Some(body) = &edit_for_call.body {
                    tx.execute("UPDATE tasks SET body = ? WHERE id = ?", params![body, id]).map_err(db::map_sqlite)?;
                    changed = true;
                }
                if let Some(new) = &priority {
                    tx.execute("UPDATE tasks SET priority = ? WHERE id = ?", params![new, id]).map_err(db::map_sqlite)?;
                    changed = true;
                    if *new != old_priority {
                        tx.execute(
                            "INSERT INTO task_events (task_id, event, at, actor, scope, priority, meta) VALUES (?,?,?,?,?,?,?)",
                            params![id, "priority", now, actor, scope, new,
                                    json!({ "from": old_priority, "to": new }).to_string()],
                        ).map_err(db::map_sqlite)?;
                    }
                }
                if let Some(after) = &deps_after {
                    let before = decode_list(old_deps.as_deref());
                    let encoded = json!(after).to_string();
                    tx.execute("UPDATE tasks SET depends_on = ? WHERE id = ?", params![encoded, id]).map_err(db::map_sqlite)?;
                    changed = true;
                    if before != *after {
                        tx.execute(
                            "INSERT INTO task_events (task_id, event, at, actor, scope, priority, meta) VALUES (?,?,?,?,?,?,?)",
                            params![id, "depends", now, actor, scope, old_priority,
                                    json!({
                                        "removed": before.iter().filter(|d| !after.contains(d)).collect::<Vec<_>>(),
                                        "added": after.iter().filter(|d| !before.contains(d)).collect::<Vec<_>>(),
                                        "depends_on": after,
                                    }).to_string()],
                        ).map_err(db::map_sqlite)?;
                    }
                }
                if changed {
                    tx.execute("UPDATE tasks SET updated_at = ? WHERE id = ?", params![now, id]).map_err(db::map_sqlite)?;
                }
                tx.commit().map_err(db::map_sqlite)
            })
            .await?;
        self.get(id.trim()).await
    }

    /// Resolve one task's dependencies (the dependency half of `_decorate`).
    pub async fn dependency_state(&self, id: &str) -> Result<DependencyState, DunkError> {
        let wanted = id.trim().to_string();
        self.writer
            .call(move |conn| dependency_state_conn(conn, &wanted).map(|(_, state)| state))
            .await
    }

    /// `leases_for`: live leases for a task (expired rows are ignored). T12 (t787) owns
    /// claim/heartbeat/release; this read shares its `task_leases` schema.
    pub async fn leases_for(&self, id: &str, now: Option<f64>) -> Result<Vec<Lease>, DunkError> {
        let id = id.to_string();
        let now = now.unwrap_or_else(crate::clock::now_s);
        self.writer
            .call(move |conn| leases_for_conn(conn, &id, now))
            .await
    }

    /// `_decorate`: the row plus everything a reader looks at (assignees, leases,
    /// claimable, eligible, allocation_waiting, the dependency fields). `pane_id` is the
    /// viewer: eligibility is per-viewer (F2).
    pub async fn decorate(&self, id: &str, pane_id: Option<&str>) -> Result<Decorated, DunkError> {
        let wanted = id.trim().to_string();
        let pane = pane_id.map(String::from);
        let now = crate::clock::now_s();
        self.writer
            .call(move |conn| decorate_conn(conn, &wanted, pane.as_deref(), now))
            .await
    }

    /// Record a review verdict (`dunk_tasks.set_verdict`, t662).
    ///
    /// The verdict is bound to the task's head by the row itself, so it is written here
    /// and not as free text: a review task's `verdict` is what the distinct-lane count
    /// reads. Only `accept` and `needs-fix` are verdicts - anything else is a refusal,
    /// because a typo that became a verdict would count as a lane's judgement.
    pub async fn set_verdict(
        &self,
        id: &str,
        verdict: &str,
        by: Option<&str>,
    ) -> Result<Task, DunkError> {
        let verdict = verdict.trim().to_lowercase();
        if verdict != "accept" && verdict != "needs-fix" {
            return Err(DunkError::Refused(format!(
                "verdict must be one of accept, needs-fix; got {verdict:?}"
            )));
        }
        let wanted = id.to_string();
        let id = wanted.clone();
        let by = by.map(|b| b.to_string());
        let now = crate::clock::now_s();
        self.writer
            .call(move |conn| {
                let changed = conn
                    .execute(
                        "UPDATE tasks SET verdict = ?, updated_at = ? WHERE id = ?",
                        params![verdict, now, id],
                    )
                    .map_err(db::map_sqlite)?;
                if changed == 0 {
                    return Err(DunkError::Refused(format!("no task with id {id:?}")));
                }
                conn.execute(
                    "INSERT INTO task_events (task_id, event, at, actor, meta) VALUES (?,?,?,?,?)",
                    params![
                        id,
                        "verdict",
                        now,
                        by,
                        format!("{{\"verdict\": \"{verdict}\"}}")
                    ],
                )
                .map_err(db::map_sqlite)?;
                Ok(())
            })
            .await?;
        self.get(&wanted).await
    }

    /// The verdict recorded on a task, if any (`Task` does not carry it: see `verdict_of`).
    pub async fn verdict_of(&self, id: &str) -> Result<Option<String>, DunkError> {
        let id = id.to_string();
        self.writer
            .call(move |conn| {
                conn.query_row(
                    "SELECT verdict FROM tasks WHERE id = ?",
                    params![id],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()
                .map_err(db::map_sqlite)
                .map(|found| found.flatten())
            })
            .await
    }

    /// `set_landed` (t906): say that a finished task's work is (or is not yet) on main.
    ///
    /// `landed = false` is what a lane writes when it completes a task whose PR is
    /// STACKED: the row is done, the work is not on main, and its dependents must not
    /// open. The merge path flips it back to true (`mark_landed_for_pr`).
    pub async fn set_landed(
        &self,
        id: &str,
        landed: bool,
        by: Option<&str>,
        repo: Option<&str>,
        pr: Option<i64>,
    ) -> Result<Task, DunkError> {
        let wanted = id.to_string();
        let id = wanted.clone();
        let by = by.map(|b| b.to_string());
        let repo = repo.map(|r| r.to_string());
        let now = crate::clock::now_s();
        self.writer
            .call(move |conn| {
                let tx = conn.unchecked_transaction().map_err(db::map_sqlite)?;
                let (scope, priority): (String, String) = tx
                    .query_row(
                        "SELECT scope, priority FROM tasks WHERE id = ?",
                        params![id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()
                    .map_err(db::map_sqlite)?
                    .ok_or_else(|| DunkError::Refused(format!("no task with id {id:?}")))?;
                tx.execute(
                    "UPDATE tasks SET landed = ?, updated_at = ? WHERE id = ?",
                    params![landed as i64, now, id],
                )
                .map_err(db::map_sqlite)?;
                let event = if landed { "landed" } else { "unlanded" };
                tx.execute(
                    "INSERT INTO task_events (task_id, event, at, actor, scope, priority, meta) VALUES (?,?,?,?,?,?,?)",
                    params![id, event, now, by, scope, priority,
                            json!({"repo": repo, "pr": pr}).to_string()],
                )
                .map_err(db::map_sqlite)?;
                tx.commit().map_err(db::map_sqlite)?;
                Ok(())
            })
            .await?;
        self.get(&wanted).await
    }

    /// `mark_landed_for_pr` (t906): flip every finished task that names `repo#pr` to
    /// landed, returning the ids it flipped.
    pub async fn mark_landed_for_pr(
        &self,
        repo: &str,
        pr: i64,
        by: Option<&str>,
    ) -> Result<Vec<String>, DunkError> {
        if repo.is_empty() || pr == 0 {
            return Ok(Vec::new());
        }
        let repo = repo.to_string();
        let by = by.map(|b| b.to_string());
        let now = crate::clock::now_s();
        self.writer
            .call(move |conn| {
                let tx = conn.unchecked_transaction().map_err(db::map_sqlite)?;
                let rows: Vec<(String, String, i64)> = {
                    let mut stmt = tx
                        .prepare("SELECT id, status, landed FROM tasks WHERE repo = ? AND pr = ?")
                        .map_err(db::map_sqlite)?;
                    let found = stmt
                        .query_map(params![repo, pr], |row| {
                            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                        })
                        .map_err(db::map_sqlite)?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(db::map_sqlite)?;
                    found
                };
                let mut flipped = Vec::new();
                for (id, status, landed) in rows {
                    if status == "done" && landed == 0 {
                        tx.execute(
                            "UPDATE tasks SET landed = 1, updated_at = ? WHERE id = ?",
                            params![now, id],
                        )
                        .map_err(db::map_sqlite)?;
                        tx.execute(
                            "INSERT INTO task_events (task_id, event, at, actor, scope, meta) VALUES (?,?,?,?,?,?)",
                            params![id, "landed", now, by, "global",
                                    json!({"repo": repo, "pr": pr, "reason": "the PR merged"}).to_string()],
                        )
                        .map_err(db::map_sqlite)?;
                        flipped.push(id);
                    }
                }
                tx.commit().map_err(db::map_sqlite)?;
                Ok(flipped)
            })
            .await
    }

    /// Whether a task's work is on main (`landed` column; `Task` does not carry it).
    pub async fn landed_of(&self, id: &str) -> Result<Option<bool>, DunkError> {
        let id = id.to_string();
        self.writer
            .call(move |conn| {
                conn.query_row(
                    "SELECT landed FROM tasks WHERE id = ?",
                    params![id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map_err(db::map_sqlite)
                .map(|found| found.map(|v| v != 0))
            })
            .await
    }
}

/// A status after which the task is not being worked (`dunk_tasks` terminal set).
fn is_terminal(status: &str) -> bool {
    matches!(status, "done" | "cancelled")
}

/// A status after which the CLAIM is over, terminal or not (`_end_claim`, t872).
///
/// `blocked` belongs here and not in `is_terminal`: a blocked task must be unblockable,
/// which is why the slice's single set refused `unblock` (t1018 finding 1).
fn ends_claim(status: &str) -> bool {
    matches!(status, "done" | "cancelled" | "blocked")
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
        // t1018 finding 4: the Python's _migrate adds `landed` (t906); without it here a
        // 25-column live file never gains the column the landed rule reads.
        ("landed", "INTEGER NOT NULL DEFAULT 1"),
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
    // t1018 finding 4: the SCHEMA comment cites the CoS's twelve duplicate reviews of
    // PR #117 as the reason for this index, and the index was not created - a fresh store
    // had NO indexes at all. Every index and the counters table the Python's _migrate
    // makes are created here, after the column migrations (the review index needs the
    // kind/repo/pr/head columns to exist).
    for statement in [
        "CREATE INDEX IF NOT EXISTS tasks_by_scope ON tasks (scope, status, created_at)",
        "CREATE INDEX IF NOT EXISTS tasks_by_parent ON tasks (parent_id)",
        "CREATE INDEX IF NOT EXISTS leases_by_pane ON task_leases (pane_id)",
        "CREATE INDEX IF NOT EXISTS task_events_by_time ON task_events (at)",
        "CREATE INDEX IF NOT EXISTS task_events_by_task ON task_events (task_id, at)",
        "CREATE INDEX IF NOT EXISTS task_events_by_event ON task_events (event, at)",
        "CREATE UNIQUE INDEX IF NOT EXISTS tasks_one_review_per_head ON tasks \
             (kind, repo, pr, head) WHERE kind = 'review' AND head IS NOT NULL \
             AND status IN ('open', 'in_progress', 'blocked')",
        // The Python allocates ids with a single-statement UPSERT on this table (the F1
        // fix); without it the port would fall back to the SELECT-then-INSERT the Python
        // comment names as the bug.
        "CREATE TABLE IF NOT EXISTS counters (name TEXT PRIMARY KEY, value INTEGER NOT NULL)",
    ] {
        conn.execute(statement, []).map_err(db::map_sqlite)?;
    }
    // The lease columns an older file may be missing (the Python migrates these too).
    let lease_columns: Vec<String> = conn
        .prepare("PRAGMA table_info(task_leases)")
        .map_err(db::map_sqlite)?
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(db::map_sqlite)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(db::map_sqlite)?;
    for (column, kind) in [
        ("caller_pid", "INTEGER"),
        ("caller_start", "TEXT"),
        ("caller_name", "TEXT"),
        ("assignee_added", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        if !lease_columns.iter().any(|have| have == column) {
            conn.execute(
                &format!("ALTER TABLE task_leases ADD COLUMN {column} {kind}"),
                [],
            )
            .map_err(db::map_sqlite)?;
        }
    }
    Ok(())
}

/// The validators (`validate_status` / `_priority` / `_kind` / `_verdict`), with the
/// Python's refusal sentences. Each returns the normalised value.
pub const STATUSES: [&str; 5] = ["open", "in_progress", "blocked", "done", "cancelled"];
pub const PRIORITIES: [&str; 4] = ["low", "medium", "high", "urgent"];
pub const KINDS: [&str; 4] = ["work", "review", "verify", "report"];
pub const VERDICTS: [&str; 2] = ["accept", "needs-fix"];

pub fn validate_status(status: &str) -> Result<String, DunkError> {
    let status = status.trim();
    if STATUSES.contains(&status) {
        return Ok(status.to_string());
    }
    Err(DunkError::Refused(format!(
        "status must be one of {}",
        STATUSES.join(", ")
    )))
}

pub fn validate_priority(priority: &str) -> Result<String, DunkError> {
    let priority = priority.trim();
    if PRIORITIES.contains(&priority) {
        return Ok(priority.to_string());
    }
    Err(DunkError::Refused(format!(
        "priority must be one of {}",
        PRIORITIES.join(", ")
    )))
}

pub fn validate_kind(kind: &str) -> Result<String, DunkError> {
    let kind = if kind.trim().is_empty() {
        DEFAULT_KIND.to_string()
    } else {
        kind.trim().to_lowercase()
    };
    if KINDS.contains(&kind.as_str()) {
        return Ok(kind);
    }
    Err(DunkError::Refused(format!(
        "kind must be one of {}; got {kind:?}",
        KINDS.join(", ")
    )))
}

pub fn validate_verdict(verdict: &str) -> Result<String, DunkError> {
    let verdict = verdict.trim().to_lowercase();
    if VERDICTS.contains(&verdict.as_str()) {
        return Ok(verdict);
    }
    Err(DunkError::Refused(format!(
        "verdict must be one of {}; got {verdict:?}",
        VERDICTS.join(", ")
    )))
}

/// `update`'s field edits (t786 slice 5). A status change is `set_status`, which carries
/// the t854/t891 guards; this edits title, body, priority and dependencies only, in ONE
/// immediate transaction, and writes the `priority` and `depends` events the Python writes.
#[derive(Debug, Default, Clone)]
pub struct FieldEdit {
    pub title: Option<String>,
    pub body: Option<String>,
    pub priority: Option<String>,
    pub depends_on: Option<Vec<String>>,
    /// `Some(None)` clears the parent; `None` leaves it alone (the Python's Ellipsis).
    pub parent_id: Option<Option<String>>,
    pub assignees: Option<Vec<String>>,
    pub multi_assignee: Option<bool>,
    /// `Some(None)` clears the reason; `None` leaves it alone.
    pub block_reason: Option<Option<String>>,
}

/// `validate_scope`: `global`, or a pane id such as `w8:p3` or `%4`.
pub fn validate_scope(scope: &str) -> Result<String, DunkError> {
    static PANE_ID: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let pane = PANE_ID.get_or_init(|| {
        regex::Regex::new(r"^(?:[A-Za-z0-9_.-]+:[A-Za-z0-9_.-]+|%\d+)$").expect("static pattern")
    });
    let scope = scope.trim();
    if scope == "global" || pane.is_match(scope) {
        return Ok(scope.to_string());
    }
    Err(DunkError::Refused(format!(
        "scope must be 'global' or a pane id such as 'w8:p3' or '%4' (got {scope:?})"
    )))
}

/// The dependency half of the Python's `_decorate` (t786 slice 7): what a task is waiting
/// on, and the t783/t878/t903/t906 distinctions the row must state out loud.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DependencyState {
    /// Dependencies still outstanding: not done/cancelled, or done but not landed (t906).
    pub blocked_by: Vec<String>,
    /// Done, but their PR is not on main (t906).
    pub unlanded_dependencies: Vec<String>,
    /// Cancelled dependencies: SATISFIED (t783), reported so the pass is not silent.
    pub dead_dependencies: Vec<String>,
    pub missing_dependencies: Vec<String>,
    /// A blocked task whose dependencies are all satisfied: only `unblock` is left (t903).
    pub unblock_needed: bool,
    pub dependency_note: Option<String>,
    /// open/in_progress and nothing outstanding. The per-viewer `claimable` half of the
    /// Python's `eligible` (assignees, leases) is NOT ported yet, so this is the
    /// generic-list value: unassigned-or-multi is not checked here.
    pub dependencies_clear: bool,
}

/// One live `task_leases` row.
#[derive(Debug, Clone, PartialEq)]
pub struct Lease {
    pub task_id: String,
    pub pane_id: String,
    pub claimed_at: f64,
    pub heartbeat_at: f64,
    pub expires_at: f64,
}

/// `_decorate` output: the row and its derived fields, same names as the Python dict.
#[derive(Debug, Clone, PartialEq)]
pub struct Decorated {
    pub task: Task,
    pub assignees: Vec<String>,
    pub depends_on: Vec<String>,
    /// An allocation offer is ready for exactly this assignee set.
    pub allocation_waiting: bool,
    pub multi_assignee: bool,
    pub is_global: bool,
    pub leases: Vec<Lease>,
    pub claimed_by: Vec<String>,
    pub lease_expires_at: Option<f64>,
    pub deps: DependencyState,
    pub claimable: bool,
    pub eligible: bool,
}

fn leases_for_conn(conn: &Connection, id: &str, now: f64) -> Result<Vec<Lease>, DunkError> {
    let mut stmt = conn
        .prepare("SELECT task_id, pane_id, claimed_at, heartbeat_at, expires_at FROM task_leases WHERE task_id = ?")
        .map_err(db::map_sqlite)?;
    let rows = stmt
        .query_map(params![id], |row| {
            Ok(Lease {
                task_id: row.get(0)?,
                pane_id: row.get(1)?,
                claimed_at: row.get(2)?,
                heartbeat_at: row.get(3)?,
                expires_at: row.get(4)?,
            })
        })
        .map_err(db::map_sqlite)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(db::map_sqlite)?;
    Ok(rows
        .into_iter()
        .filter(|lease| lease.expires_at > now)
        .collect())
}

/// `_claimable`: whether `pane_id` may take this task right now.
fn claimable_for(d: &Decorated, pane_id: &str) -> bool {
    if d.allocation_waiting {
        return false;
    }
    if !pane_id.is_empty() && d.leases.iter().any(|l| l.pane_id == pane_id) {
        return false; // already holds this one
    }
    if !d.leases.is_empty() {
        return d.multi_assignee;
    }
    let mine = !pane_id.is_empty() && d.assignees.iter().any(|a| a == pane_id);
    match d.task.status.as_str() {
        // F2: a named assignee can pull its own task.
        "open" => d.assignees.is_empty() || d.multi_assignee || mine,
        "in_progress" => d.multi_assignee || mine,
        _ => false,
    }
}

fn decorate_conn(
    conn: &Connection,
    id: &str,
    pane_id: Option<&str>,
    now: f64,
) -> Result<Decorated, DunkError> {
    let (task, state) = dependency_state_conn(conn, id)?;
    let multi: i64 = conn
        .query_row(
            "SELECT multi_assignee FROM tasks WHERE id = ?",
            params![id],
            |r| r.get(0),
        )
        .map_err(db::map_sqlite)?;
    let assignees = decode_list(Some(&task.assignees));
    let depends_on = decode_list(Some(&task.depends_on));
    // A missing allocation_offers table is "no offer", not an error.
    let owners: Option<String> = match conn
        .query_row(
            "SELECT owners FROM allocation_offers WHERE task_id=? AND state='ready'",
            params![id],
            |r| r.get(0),
        )
        .optional()
    {
        Ok(found) => found,
        Err(err) if err.to_string().contains("no such table") => None,
        Err(err) => return Err(db::map_sqlite(err)),
    };
    let mut sorted = assignees.clone();
    sorted.sort();
    let allocation_waiting = owners.is_some_and(|raw| sorted == decode_list(Some(&raw)));
    let leases = leases_for_conn(conn, id, now)?;
    let mut d = Decorated {
        is_global: task.scope == "global",
        claimed_by: leases.iter().map(|l| l.pane_id.clone()).collect(),
        lease_expires_at: leases.iter().map(|l| l.expires_at).reduce(f64::min),
        multi_assignee: multi != 0,
        allocation_waiting,
        leases,
        assignees,
        depends_on,
        deps: state,
        claimable: false,
        eligible: false,
        task,
    };
    d.claimable = match pane_id {
        Some(pane) if !pane.is_empty() => claimable_for(&d, pane),
        _ => d.assignees.is_empty() || d.multi_assignee,
    };
    d.eligible = matches!(d.task.status.as_str(), "open" | "in_progress")
        && d.deps.blocked_by.is_empty()
        && d.claimable;
    Ok(d)
}

fn dependency_state_conn(
    conn: &Connection,
    wanted: &str,
) -> Result<(Task, DependencyState), DunkError> {
    let task = conn
        .query_row(
            "SELECT id, scope, title, body, status, priority, kind, assignees, depends_on, block_reason, created_at, updated_at FROM tasks WHERE id = ?",
            params![wanted],
            row_to_task,
        )
        .optional()
        .map_err(db::map_sqlite)?
        .ok_or_else(|| DunkError::Refused(format!("no task with id {wanted:?}")))?;
    let status = task.status.clone();
    let deps = decode_list(Some(&task.depends_on));
    let mut state = DependencyState::default();
    let mut cancelled = Vec::new();
    for dep in &deps {
        let found: Option<(String, i64)> = conn
            .query_row(
                "SELECT status, landed FROM tasks WHERE id = ?",
                params![dep],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(db::map_sqlite)?;
        match found {
            None => {
                state.missing_dependencies.push(dep.clone());
                state.blocked_by.push(dep.clone());
            }
            Some((dep_status, landed)) => {
                let landed = landed != 0;
                if dep_status == "cancelled" {
                    state.dead_dependencies.push(dep.clone());
                    cancelled.push(dep.clone());
                } else if dep_status == "done" && !landed {
                    state.unlanded_dependencies.push(dep.clone());
                    state.blocked_by.push(dep.clone());
                } else if dep_status != "done" {
                    state.blocked_by.push(dep.clone());
                }
            }
        }
    }
    state.dependencies_clear =
        matches!(status.as_str(), "open" | "in_progress") && state.blocked_by.is_empty();
    if status == "blocked" && !deps.is_empty() && state.blocked_by.is_empty() {
        state.unblock_needed = true;
        state.dependency_note = Some(if !cancelled.is_empty() {
            format!("depends on a CANCELLED task ({}); a cancelled dependency counts as satisfied, so only the block is holding this task - 'task unblock' makes it claimable", cancelled.join(", "))
        } else {
            format!("every dependency is satisfied ({}); nothing but the block is holding this task - 'task unblock' makes it claimable", deps.join(", "))
        });
    }
    Ok((task, state))
}

/// `_order_key`: priority rank, then age, then id.
pub fn order_key(priority: &str, created_at: f64, id: &str) -> (u8, u64, String) {
    let rank = match priority {
        "urgent" => 0,
        "high" => 1,
        "medium" => 2,
        "low" => 3,
        _ => 99,
    };
    (rank, created_at.max(0.0).to_bits(), id.to_string())
}

/// `_decode`: a JSON list, or empty when the column is NULL, unparseable or not a list.
/// t1218: open every blocked task whose block is a DEPENDENCY block and whose dependencies
/// are all satisfied, inside the caller's writer call.
///
/// A dependency block is an empty reason or `blocked-by:*`; `needs-owner:`, `DEFERRED`,
/// headroom and any other free-text reason belong to the owner and are never touched.
/// Satisfied means every dependency exists, is `done` and is landed. A CANCELLED dependency
/// is NOT satisfied here (owner's t1218 test), unlike `dependency_state`'s note, which
/// treats cancelled as satisfied for a manual `task unblock`; the task stays blocked for a
/// human to decide. `completed` limits the event path to tasks that depend on it (reason
/// `auto: <id> completed`); `None` is the sweep (t1218 C, not wired yet: it needs the
/// daemon tick).
fn auto_unblock(
    conn: &Connection,
    now: f64,
    completed: Option<&str>,
) -> Result<Vec<String>, DunkError> {
    let candidates: Vec<(String, Option<String>, Option<String>)> = {
        let mut stmt = conn
            .prepare("SELECT id, depends_on, block_reason FROM tasks WHERE status = 'blocked' ORDER BY created_at, id")
            .map_err(db::map_sqlite)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .map_err(db::map_sqlite)?;
        rows.collect::<Result<_, _>>().map_err(db::map_sqlite)?
    };
    let mut opened = Vec::new();
    for (id, raw, reason) in candidates {
        let reason_text = reason.as_deref().unwrap_or("").trim();
        if !(reason_text.is_empty() || reason_text.starts_with("blocked-by:")) {
            continue;
        }
        let deps = decode_list(raw.as_deref());
        if deps.is_empty() || completed.is_some_and(|c| !deps.iter().any(|d| d == c)) {
            continue;
        }
        let mut satisfied = true;
        for dep in &deps {
            let found: Option<(String, i64)> = conn
                .query_row(
                    "SELECT status, landed FROM tasks WHERE id = ?",
                    params![dep],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(db::map_sqlite)?;
            if !matches!(found, Some((ref s, l)) if s == "done" && l != 0) {
                satisfied = false;
                break;
            }
        }
        if !satisfied {
            continue;
        }
        let changed = conn
            .execute(
                "UPDATE tasks SET status = 'open', block_reason = NULL, updated_at = ? WHERE id = ? AND status = 'blocked'",
                params![now, id],
            )
            .map_err(db::map_sqlite)?;
        if changed == 0 {
            continue;
        }
        let why = match completed {
            Some(c) => format!("auto: {c} completed"),
            None => "auto: dependencies satisfied (sweep)".to_string(),
        };
        conn.execute(
            "INSERT INTO task_events (task_id, event, at, actor, status_from, status_to, meta) VALUES (?,?,?,?,?,?,?)",
            params![id, "unblocked", now, "system", "blocked", "open",
                    json!({"reason": why}).to_string()],
        )
        .map_err(db::map_sqlite)?;
        opened.push(id);
    }
    Ok(opened)
}

fn decode_list(raw: Option<&str>) -> Vec<String> {
    match serde_json::from_str::<serde_json::Value>(raw.unwrap_or("[]")) {
        Ok(serde_json::Value::Array(items)) => items
            .into_iter()
            .map(|item| match item {
                serde_json::Value::String(text) => text,
                other => other.to_string(),
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// `dunk_api.h_task_complete`: an omitted `landed` means landed (hotfix 20530f2; the CLI's
/// `--unmerged` is the only way to send `Some(false)`).
pub async fn h_task_complete(
    store: &Tasks,
    id: &str,
    by: Option<&str>,
    landed: Option<bool>,
) -> Result<Task, DunkError> {
    store.task_complete(id, by, landed.unwrap_or(true)).await
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
    async fn a_terminal_status_ends_the_claim() {
        // t872/PR166 in the Python: block and complete both left the lease behind, the
        // reaper reopened the task and the push loop re-claimed it.
        let store = store("claim").await;
        let task = store.add("global", "duplicate", "").await.unwrap();
        store.claim_for_test(&task.id, "w2:p1").await;
        assert_eq!(1, store.leases(&task.id).await);
        store.block(&task.id, "duplicate").await.unwrap();
        assert_eq!(
            0,
            store.leases(&task.id).await,
            "a blocked task is not being worked"
        );
        let done = store.add("global", "finished", "").await.unwrap();
        store.claim_for_test(&done.id, "w2:p1").await;
        store.complete(&done.id).await.unwrap();
        assert_eq!(0, store.leases(&done.id).await);
    }

    #[tokio::test]
    async fn block_records_the_reason_and_unblock_clears_it() {
        let store = store("block").await;
        let task = store.add("global", "waiting", "").await.unwrap();
        let blocked = store
            .block(&task.id, "needs-owner: which path?")
            .await
            .unwrap();
        assert_eq!("blocked", blocked.status);
        assert_eq!(
            Some("needs-owner: which path?".to_string()),
            blocked.block_reason
        );
        let open = store.unblock(&task.id).await.unwrap();
        assert_eq!("open", open.status);
        assert_eq!(None, open.block_reason);
    }

    #[tokio::test]
    async fn marking_landed_is_recorded() {
        let store = store("landed-rec").await;
        let task = store.add("global", "stacked", "").await.unwrap();
        store.complete(&task.id).await.unwrap();
        store
            .set_landed(&task.id, false, Some("%109"), None, None)
            .await
            .unwrap();
        assert_eq!(Some(false), store.landed_of(&task.id).await.unwrap());
        store
            .set_landed(&task.id, true, Some("%109"), None, None)
            .await
            .unwrap();
        assert_eq!(Some(true), store.landed_of(&task.id).await.unwrap());
        assert_eq!(
            3,
            store.events(&task.id).await,
            "complete + unlanded + landed"
        );
    }

    #[tokio::test]
    async fn task_complete_defaults_to_landed_and_unmerged_records_unlanded() {
        let store = store("complete-landed").await;
        let a = store.add("global", "omitted", "").await.unwrap();
        h_task_complete(&store, &a.id, Some("%1"), None)
            .await
            .unwrap();
        assert_eq!(Some(true), store.landed_of(&a.id).await.unwrap());
        let b = store.add("global", "explicit", "").await.unwrap();
        h_task_complete(&store, &b.id, Some("%1"), Some(true))
            .await
            .unwrap();
        assert_eq!(Some(true), store.landed_of(&b.id).await.unwrap());
        let c = store.add("global", "unmerged", "").await.unwrap();
        let done = h_task_complete(&store, &c.id, Some("%1"), Some(false))
            .await
            .unwrap();
        assert_eq!("done", done.status);
        assert_eq!(Some(false), store.landed_of(&c.id).await.unwrap());
    }

    // ---- t1218: deterministic auto-unblock ----

    async fn blocked_on(store: &Tasks, title: &str, deps: &[&Task], reason: Option<&str>) -> Task {
        let task = store.add("global", title, "").await.unwrap();
        store
            .update(
                &task.id,
                FieldEdit {
                    depends_on: Some(deps.iter().map(|d| d.id.clone()).collect()),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        match reason {
            Some(r) => store.block(&task.id, r).await.unwrap(),
            None => store
                .set_status(&task.id, "blocked", None, None)
                .await
                .unwrap(),
        }
    }

    async fn unblocked_events(store: &Tasks, id: &str) -> Vec<String> {
        let id = id.to_string();
        store
            .writer
            .call(move |conn| {
                let mut stmt = conn
                    .prepare("SELECT actor || '|' || COALESCE(meta,'') FROM task_events WHERE task_id = ? AND event = 'unblocked'")
                    .map_err(db::map_sqlite)?;
                let rows = stmt.query_map(params![id], |r| r.get::<_, String>(0)).map_err(db::map_sqlite)?;
                Ok(rows.collect::<Result<Vec<_>, _>>().map_err(db::map_sqlite)?)
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn completing_a_task_unblocks_what_it_blocks_and_only_that() {
        let store = store("auto-unblock-a").await;
        let a = store.add("global", "a", "").await.unwrap();
        let c = store.add("global", "c", "").await.unwrap();
        let only_a = blocked_on(&store, "only a", &[&a], Some("blocked-by:t0")).await;
        let a_and_c = blocked_on(&store, "a and c", &[&a, &c], None).await;
        store.complete(&a.id).await.unwrap();
        let opened = store.get(&only_a.id).await.unwrap();
        assert_eq!("open", opened.status);
        assert_eq!(None, opened.block_reason);
        assert_eq!("blocked", store.get(&a_and_c.id).await.unwrap().status);
        let events = unblocked_events(&store, &only_a.id).await;
        assert_eq!(1, events.len());
        assert!(
            events[0].starts_with("system|")
                && events[0].contains(&format!("auto: {} completed", a.id))
        );
        store.complete(&c.id).await.unwrap();
        assert_eq!("open", store.get(&a_and_c.id).await.unwrap().status);
    }

    #[tokio::test]
    async fn the_unblocked_observer_fires_for_each_opened_task() {
        let store = store("auto-unblock-obs").await;
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        store.on_event(Box::new(move |id, event, by, _| {
            if event == "unblocked" {
                sink.lock()
                    .unwrap()
                    .push((id.to_string(), by.map(str::to_string)));
            }
        }));
        let a = store.add("global", "a", "").await.unwrap();
        let b = blocked_on(&store, "b", &[&a], None).await;
        store.complete(&a.id).await.unwrap();
        assert_eq!(
            vec![(b.id.clone(), Some("system".to_string()))],
            *seen.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn a_cancelled_dependency_does_not_unblock() {
        let store = store("auto-unblock-cancel").await;
        let a = store.add("global", "a", "").await.unwrap();
        let dead = store.add("global", "dead", "").await.unwrap();
        let b = blocked_on(&store, "b", &[&a, &dead], None).await;
        store.cancel(&dead.id, "no").await.unwrap();
        store.complete(&a.id).await.unwrap();
        assert_eq!("blocked", store.get(&b.id).await.unwrap().status);
    }

    #[tokio::test]
    async fn owner_and_free_text_blocks_are_never_touched() {
        let store = store("auto-unblock-manual").await;
        let a = store.add("global", "a", "").await.unwrap();
        let mut held = Vec::new();
        for reason in [
            "needs-owner: which path?",
            "DEFERRED until Q1",
            "headroom: load 30",
        ] {
            held.push(blocked_on(&store, reason, &[&a], Some(reason)).await);
        }
        store.complete(&a.id).await.unwrap();
        for task in held {
            let got = store.get(&task.id).await.unwrap();
            assert_eq!("blocked", got.status, "{:?}", got.block_reason);
            assert_eq!(task.block_reason, got.block_reason);
        }
    }

    #[tokio::test]
    async fn an_unlanded_dependency_keeps_the_task_blocked() {
        let store = store("auto-unblock-unlanded").await;
        let a = store.add("global", "a", "").await.unwrap();
        let b = blocked_on(&store, "b", &[&a], None).await;
        store.task_complete(&a.id, Some("%1"), false).await.unwrap();
        assert_eq!("blocked", store.get(&b.id).await.unwrap().status);
    }

    #[tokio::test]
    async fn sweep_opens_the_t810_shape_once_and_a_second_sweep_is_a_noop() {
        let store = store("sweep-t810").await;
        let a = store.add("global", "a", "").await.unwrap();
        store.complete(&a.id).await.unwrap();
        // t810: blocked with an empty reason AFTER its dependency was done: no event
        // ever opens it, only the sweep.
        let b = blocked_on(&store, "b", &[&a], None).await;
        assert_eq!("blocked", store.get(&b.id).await.unwrap().status);
        assert_eq!(
            vec![b.id.clone()],
            store.sweep_auto_unblock().await.unwrap()
        );
        let opened = store.get(&b.id).await.unwrap();
        assert_eq!("open", opened.status);
        assert_eq!(None, opened.block_reason);
        assert!(store.sweep_auto_unblock().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn sweep_fires_the_unblocked_observer_once_per_task() {
        let store = store("sweep-observer").await;
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        store.on_event(Box::new(move |id, event, _by, _kind| {
            if event == "unblocked" {
                sink.lock().unwrap().push(id.to_string());
            }
        }));
        let a = store.add("global", "a", "").await.unwrap();
        store.complete(&a.id).await.unwrap();
        let b = blocked_on(&store, "b", &[&a], Some("blocked-by:t1")).await;
        store.sweep_auto_unblock().await.unwrap();
        store.sweep_auto_unblock().await.unwrap();
        assert_eq!(vec![b.id], *seen.lock().unwrap());
    }

    #[tokio::test]
    async fn sweep_leaves_cancelled_dependencies_and_manual_reasons_alone() {
        let store = store("sweep-untouched").await;
        let done = store.add("global", "done", "").await.unwrap();
        store.complete(&done.id).await.unwrap();
        let dead = store.add("global", "dead", "").await.unwrap();
        store.cancel(&dead.id, "no").await.unwrap();
        let cancelled_dep = blocked_on(&store, "c", &[&dead], None).await;
        let owner = blocked_on(&store, "o", &[&done], Some("needs-owner: pick one")).await;
        let deferred = blocked_on(&store, "d", &[&done], Some("DEFERRED")).await;
        let headroom = blocked_on(&store, "h", &[&done], Some("headroom 71%")).await;
        assert!(store.sweep_auto_unblock().await.unwrap().is_empty());
        for task in [&cancelled_dep, &owner, &deferred, &headroom] {
            assert_eq!("blocked", store.get(&task.id).await.unwrap().status);
        }
    }

    #[tokio::test]
    async fn block_by_records_the_dependency_and_completion_clears_it() {
        let store = store("block-by").await;
        let a = store.add("global", "a", "").await.unwrap();
        let b = store.add("global", "b", "").await.unwrap();
        let blocked = store.block_by(&b.id, &a.id).await.unwrap();
        assert_eq!("blocked", blocked.status);
        assert_eq!(Some(format!("blocked-by:{}", a.id)), blocked.block_reason);
        assert_eq!(vec![a.id.clone()], decode_list(Some(&blocked.depends_on)));
        assert!(store.block_by(&b.id, &b.id).await.is_err());
        assert!(store.block_by(&b.id, "t999").await.is_err());
        store.complete(&a.id).await.unwrap();
        assert_eq!("open", store.get(&b.id).await.unwrap().status);
    }

    #[tokio::test]
    async fn set_landed_on_an_unknown_id_is_a_refusal() {
        let store = store("landed-unknown").await;
        assert!(store
            .set_landed("t999", true, None, None, None)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn mark_landed_for_pr_flips_only_finished_unlanded_tasks() {
        let store = store("landed-pr").await;
        let done = store.add("global", "done stacked", "").await.unwrap();
        let open = store.add("global", "still open", "").await.unwrap();
        for id in [&done.id, &open.id] {
            store
                .writer
                .call({
                    let id = id.clone();
                    move |conn| {
                        conn.execute(
                            "UPDATE tasks SET repo = 'o/r', pr = 7, landed = 0 WHERE id = ?",
                            params![id],
                        )
                        .map_err(db::map_sqlite)?;
                        Ok(())
                    }
                })
                .await
                .unwrap();
        }
        store.complete(&done.id).await.unwrap();
        assert!(store
            .mark_landed_for_pr("", 7, None)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            vec![done.id.clone()],
            store
                .mark_landed_for_pr("o/r", 7, Some("daemon"))
                .await
                .unwrap()
        );
        assert_eq!(Some(true), store.landed_of(&done.id).await.unwrap());
        assert_eq!(Some(false), store.landed_of(&open.id).await.unwrap());
    }

    #[tokio::test]
    async fn note_appends_an_event() {
        let store = store("note").await;
        let task = store.add("global", "evidence", "").await.unwrap();
        store
            .note(&task.id, "note", "w2:p1", Some("ran ctest"))
            .await
            .unwrap();
        assert_eq!(1, store.events(&task.id).await);
    }

    #[tokio::test]
    async fn an_unknown_id_is_a_refusal_not_a_silent_no_op() {
        let store = store("missing").await;
        let error = store.block("t999", "x").await.unwrap_err();
        assert!(error.to_string().contains("no task with id"));
    }

    #[tokio::test]
    async fn a_verdict_is_bound_and_a_typo_is_refused() {
        let store = store("verdict").await;
        let task = store.add("global", "review it", "").await.unwrap();
        store
            .set_verdict(&task.id, "ACCEPT", Some("w2:p2"))
            .await
            .unwrap();
        assert_eq!(
            Some("accept".to_string()),
            store.verdict_of(&task.id).await.unwrap()
        );
        let error = store
            .set_verdict(&task.id, "approved", None)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("verdict must be one of"));
    }

    #[tokio::test]
    async fn a_copy_of_the_live_store_lists_the_same_rows() {
        // T11's acceptance, as far as this slice can honestly reach it: open a COPY of
        // the live tasks.db (never the live file) and compare the rows the store reads
        // against a direct query on the same copy. It does NOT compare against the
        // Python's list output - that needs the differential harness (T01) - so this
        // proves the reader, not the parity.
        let live = std::path::Path::new(&std::env::var("HOME").unwrap_or_default())
            .join(".config/dunkingsheep/tasks.db");
        if !live.exists() {
            return; // no live store on this host: nothing to copy, and a skip is honest
        }
        let dir = std::env::temp_dir().join(format!("ds-store-copy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let copy = dir.join("tasks-copy.db");
        let _ = std::fs::remove_file(&copy);
        std::fs::copy(&live, &copy).unwrap();
        let store = Tasks::open(&copy).await.unwrap();
        let rows = store.list("global").await.unwrap_or_default();
        // the same copy, read directly: the store must agree with its own source
        let conn = Connection::open(&copy).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM tasks WHERE scope = 'global'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count as usize,
            rows.len(),
            "the store lists every global row the file holds"
        );
        for task in rows.iter().take(5) {
            let (title, status): (String, String) = conn
                .query_row(
                    "SELECT title, status FROM tasks WHERE id = ?",
                    params![task.id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(title, task.title);
            assert_eq!(status, task.status);
        }
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
            let conn = Connection::open(&path).unwrap();
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

    /// t1018 finding 1: a terminal status is FINAL (the t854 rule the slice lost).
    /// Reproduced there: block() after complete() gave "blocked"; unblock() after
    /// complete() gave "open".
    #[tokio::test]
    async fn a_finished_task_is_not_reopened_by_an_edit() {
        let tasks = store("terminal-guard").await;
        let task = tasks.add("global", "finished work", "").await.unwrap();
        tasks
            .set_status(&task.id, "done", None, Some("%109"))
            .await
            .unwrap();
        let blocked = tasks.block(&task.id, "late").await.unwrap_err();
        assert!(
            blocked
                .to_string()
                .contains("is done; a finished task is not reopened"),
            "{blocked}"
        );
        let cancelled = tasks.add("global", "cancelled work", "").await.unwrap();
        tasks
            .set_status(&cancelled.id, "cancelled", Some("duplicate"), None)
            .await
            .unwrap();
        let unblocked = tasks.unblock(&cancelled.id).await.unwrap_err();
        assert!(
            unblocked.to_string().contains("is cancelled"),
            "{unblocked}"
        );
        // ...and a BLOCKED task is still unblockable: 'blocked' ends the claim, it is not
        // terminal (the slice had it in the terminal set, which refused unblock).
        let waiting = tasks.add("global", "waiting work", "").await.unwrap();
        tasks
            .block(&waiting.id, "waiting on the owner")
            .await
            .unwrap();
        assert_eq!("open", tasks.unblock(&waiting.id).await.unwrap().status);
    }

    /// t1018 finding 2: a transition writes the columns and ONE event row - the metrics
    /// are computed from those rows, so a silent transition is invisible to them.
    #[tokio::test]
    async fn a_transition_records_its_columns_and_its_event() {
        let tasks = store("transition-columns").await;
        let task = tasks.add("global", "work", "").await.unwrap();
        let done = tasks
            .set_status(&task.id, "done", None, Some("%109"))
            .await
            .unwrap();
        let row = tasks
            .writer
            .call({
                let id = task.id.clone();
                move |conn| {
                    Ok(conn
                        .query_row(
                            "SELECT completed_at, completed_by FROM tasks WHERE id = ?",
                            params![id],
                            |row| {
                                Ok((
                                    row.get::<_, Option<f64>>(0)?,
                                    row.get::<_, Option<String>>(1)?,
                                ))
                            },
                        )
                        .unwrap())
                }
            })
            .await
            .unwrap();
        assert!(row.0.is_some(), "completed_at is written");
        assert_eq!(Some("%109".to_string()), row.1, "completed_by is written");
        assert!(!done.status.is_empty());
        let events: Vec<String> = tasks
            .writer
            .call({
                let id = task.id.clone();
                move |conn| {
                    let mut stmt = conn
                        .prepare("SELECT event FROM task_events WHERE task_id = ? ORDER BY id")
                        .unwrap();
                    Ok(stmt
                        .query_map(params![id], |row| row.get(0))
                        .unwrap()
                        .collect::<Result<Vec<String>, _>>()
                        .unwrap())
                }
            })
            .await
            .unwrap();
        assert_eq!(
            vec!["completed".to_string()],
            events,
            "exactly one event row per transition, named as the Python names it"
        );

        let cancelled = tasks.add("global", "withdrawn", "").await.unwrap();
        tasks.cancel(&cancelled.id, "duplicate").await.unwrap();
        let cancelled_at: Option<f64> = tasks
            .writer
            .call({
                let id = cancelled.id.clone();
                move |conn| {
                    Ok(conn
                        .query_row(
                            "SELECT cancelled_at FROM tasks WHERE id = ?",
                            params![id],
                            |row| row.get(0),
                        )
                        .unwrap())
                }
            })
            .await
            .unwrap();
        assert!(cancelled_at.is_some(), "cancelled_at is written");
    }

    /// t1018 finding 3: a multi-line reason is VALID json (the normal shape of an
    /// evidence note) and an empty meta is "{}", not NULL - recent_notes() skips rows it
    /// cannot parse, so an escaped-wrong note would be invisible in task read.
    #[tokio::test]
    async fn a_multi_line_note_is_valid_json() {
        let tasks = store("note-json").await;
        let task = tasks.add("global", "reviewed work", "").await.unwrap();
        let reason = "line one\nline two\twith a tab \\ and a \" quote";
        tasks
            .note(&task.id, "note", "%109", Some(reason))
            .await
            .unwrap();
        let meta: String = tasks
            .writer
            .call({
                let id = task.id.clone();
                move |conn| {
                    Ok(conn
                        .query_row(
                            "SELECT meta FROM task_events WHERE task_id = ?",
                            params![id],
                            |row| row.get(0),
                        )
                        .unwrap())
                }
            })
            .await
            .unwrap();
        let parsed: serde_json::Value =
            serde_json::from_str(&meta).expect("a note with a newline must write valid JSON");
        assert_eq!(reason, parsed["reason"].as_str().unwrap());
        // An empty note writes "{}" (the Python does), not NULL.
        let empty = tasks.add("global", "no reason", "").await.unwrap();
        tasks.note(&empty.id, "note", "%109", None).await.unwrap();
        let meta: String = tasks
            .writer
            .call({
                let id = empty.id.clone();
                move |conn| {
                    Ok(conn
                        .query_row(
                            "SELECT meta FROM task_events WHERE task_id = ?",
                            params![id],
                            |row| row.get(0),
                        )
                        .unwrap())
                }
            })
            .await
            .unwrap();
        assert_eq!("{}", meta);
    }

    /// t1018 finding 4: a fresh store has the indexes and the counters table the
    /// Python's _migrate creates - including the unique review index whose absence let
    /// the CoS file twelve identical reviews of PR #117.
    #[tokio::test]
    async fn a_fresh_store_has_the_indexes_and_the_counters_table() {
        let tasks = store("indexes").await;
        let (indexes, tables): (Vec<String>, Vec<String>) = tasks
            .writer
            .call(|conn| {
                let mut stmt = conn
                    .prepare("SELECT name FROM sqlite_master WHERE type = 'index' ORDER BY name")
                    .unwrap();
                let indexes = stmt
                    .query_map([], |row| row.get::<_, String>(0))
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();
                let mut stmt = conn
                    .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
                    .unwrap();
                let tables = stmt
                    .query_map([], |row| row.get::<_, String>(0))
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();
                Ok((indexes, tables))
            })
            .await
            .unwrap();
        for index in [
            "tasks_by_scope",
            "tasks_by_parent",
            "leases_by_pane",
            "task_events_by_time",
            "task_events_by_task",
            "task_events_by_event",
            "tasks_one_review_per_head",
        ] {
            assert!(
                indexes.iter().any(|have| have == index),
                "{index} is missing"
            );
        }
        assert!(
            tables.iter().any(|table| table == "counters"),
            "counters is missing"
        );
    }

    /// t1018 finding 5: ids come from the counter UPSERT, so two writers cannot compute
    /// the same number (the Python's F1 fix; the slice used SELECT MAX + 1).
    #[tokio::test]
    async fn ids_come_from_the_counter_not_from_a_select_max() {
        let tasks = store("ids").await;
        let first = tasks.add("global", "one", "").await.unwrap();
        let second = tasks.add("global", "two", "").await.unwrap();
        assert_eq!("t1", first.id);
        assert_eq!("t2", second.id, "the counter, not a MAX scan");
        let value: i64 = tasks
            .writer
            .call(|conn| {
                Ok(conn
                    .query_row(
                        "SELECT value FROM counters WHERE name = 'task'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap())
            })
            .await
            .unwrap();
        assert_eq!(2, value);
    }

    #[tokio::test]
    async fn assign_adds_removes_and_replaces() {
        let tasks = store("assign").await;
        let task = tasks.add("global", "t", "").await.unwrap();
        let a = tasks.assign(&task.id, &["%1", "%2"], "add").await.unwrap();
        assert_eq!(decode_list(Some(&a.assignees)), vec!["%1", "%2"]);
        let b = tasks
            .assign(&task.id, &["%2", "%3"], "append")
            .await
            .unwrap();
        assert_eq!(decode_list(Some(&b.assignees)), vec!["%1", "%2", "%3"]);
        let c = tasks.assign(&task.id, &["%2"], "rm").await.unwrap();
        assert_eq!(decode_list(Some(&c.assignees)), vec!["%1", "%3"]);
        let d = tasks.assign(&task.id, &["%9"], "set").await.unwrap();
        assert_eq!(decode_list(Some(&d.assignees)), vec!["%9"]);
        assert_eq!(tasks.events(&task.id).await, 4);
        assert!(tasks.assign(&task.id, &[" "], "add").await.is_err());
        assert!(tasks.assign(&task.id, &["%1"], "bogus").await.is_err());
        assert!(tasks.assign("t999", &["%1"], "add").await.is_err());
    }

    #[tokio::test]
    async fn delete_refuses_a_dependency_and_records_the_deletion() {
        let tasks = store("delete").await;
        let first = tasks.add("global", "first", "").await.unwrap();
        let second = tasks.add("global", "second", "").await.unwrap();
        tasks
            .writer
            .call({
                let (a, b) = (first.id.clone(), second.id.clone());
                move |conn| {
                    conn.execute(
                        "UPDATE tasks SET depends_on = ? WHERE id = ?",
                        params![json!([a]).to_string(), b],
                    )
                    .map_err(db::map_sqlite)?;
                    Ok(())
                }
            })
            .await
            .unwrap();
        let refused = tasks.delete(&first.id).await.unwrap_err().to_string();
        assert!(
            refused.contains(&format!("is a dependency of {}", second.id)),
            "{refused}"
        );
        assert_eq!(tasks.delete(&second.id).await.unwrap(), second.id);
        assert!(tasks.get(&second.id).await.is_err());
        assert_eq!(tasks.events(&second.id).await, 1);
        assert_eq!(tasks.delete(&first.id).await.unwrap(), first.id);
    }

    #[test]
    fn the_validators_refuse_with_the_pythons_sentences() {
        assert_eq!(validate_priority(" high ").unwrap(), "high");
        assert!(validate_priority("x")
            .unwrap_err()
            .to_string()
            .contains("priority must be one of low, medium, high, urgent"));
        assert!(validate_status("nope")
            .unwrap_err()
            .to_string()
            .contains("status must be one of open, in_progress, blocked, done, cancelled"));
        assert_eq!(validate_kind("").unwrap(), "work");
        assert_eq!(validate_kind(" Review ").unwrap(), "review");
        assert!(validate_kind("x")
            .unwrap_err()
            .to_string()
            .contains("kind must be one of work, review, verify, report; got \"x\""));
        assert_eq!(validate_verdict("ACCEPT").unwrap(), "accept");
        assert!(validate_verdict("maybe").is_err());
    }

    #[tokio::test]
    async fn update_edits_fields_and_records_priority_and_depends_events() {
        let tasks = store("update").await;
        let a = tasks.add("global", "a", "").await.unwrap();
        let b = tasks.add("global", "b", "").await.unwrap();
        let edit = FieldEdit {
            title: Some(" renamed ".into()),
            body: Some("new".into()),
            priority: Some("urgent".into()),
            depends_on: Some(vec![a.id.clone()]),
            ..Default::default()
        };
        let got = tasks.update(&b.id, edit, Some("%1")).await.unwrap();
        assert_eq!(
            (got.title.as_str(), got.body.as_str(), got.priority.as_str()),
            ("renamed", "new", "urgent")
        );
        assert_eq!(decode_list(Some(&got.depends_on)), vec![a.id.clone()]);
        assert_eq!(tasks.events(&b.id).await, 2);
        let own = FieldEdit {
            depends_on: Some(vec![b.id.clone()]),
            ..Default::default()
        };
        assert!(tasks
            .update(&b.id, own, None)
            .await
            .unwrap_err()
            .to_string()
            .contains("cannot depend on itself"));
        let ghost = FieldEdit {
            depends_on: Some(vec!["t999".into()]),
            ..Default::default()
        };
        assert!(tasks.update(&b.id, ghost, None).await.is_err());
        let blank = FieldEdit {
            title: Some("  ".into()),
            ..Default::default()
        };
        assert!(tasks.update(&b.id, blank, None).await.is_err());
        assert_eq!(
            tasks.events(&b.id).await,
            2,
            "a refused edit writes nothing"
        );
    }

    #[test]
    fn validate_scope_takes_global_or_a_pane_id() {
        for ok in ["global", " %4 ", "w8:p3"] {
            assert!(validate_scope(ok).is_ok(), "{ok}");
        }
        assert_eq!(validate_scope(" %4 ").unwrap(), "%4");
        let refused = validate_scope("nowhere").unwrap_err().to_string();
        assert!(
            refused.contains(
                "scope must be 'global' or a pane id such as 'w8:p3' or '%4' (got \"nowhere\")"
            ),
            "{refused}"
        );
    }

    #[tokio::test]
    async fn update_handles_parent_assignees_flag_and_block_reason() {
        let tasks = store("update2").await;
        let a = tasks.add("global", "a", "").await.unwrap();
        let b = tasks.add("global", "b", "").await.unwrap();
        let edit = FieldEdit {
            parent_id: Some(Some(a.id.clone())),
            assignees: Some(vec!["%1".into(), " ".into()]),
            multi_assignee: Some(true),
            block_reason: Some(Some("waiting".into())),
            ..Default::default()
        };
        let got = tasks.update(&b.id, edit, None).await.unwrap();
        assert_eq!(decode_list(Some(&got.assignees)), vec!["%1"]);
        assert_eq!(got.block_reason.as_deref(), Some("waiting"));
        assert_eq!(
            tasks.events(&b.id).await,
            1,
            "assignment is the only event-worthy change"
        );
        let own = FieldEdit {
            parent_id: Some(Some(b.id.clone())),
            ..Default::default()
        };
        assert!(tasks
            .update(&b.id, own, None)
            .await
            .unwrap_err()
            .to_string()
            .contains("its own parent"));
        assert!(tasks
            .delete(&a.id)
            .await
            .unwrap_err()
            .to_string()
            .contains("child task"));
        let clear = FieldEdit {
            parent_id: Some(None),
            block_reason: Some(None),
            ..Default::default()
        };
        let cleared = tasks.update(&b.id, clear, None).await.unwrap();
        assert_eq!(cleared.block_reason, None);
        assert_eq!(tasks.delete(&a.id).await.unwrap(), a.id);
    }

    #[test]
    fn order_key_sorts_by_priority_then_age_then_id() {
        let mut rows = vec![
            ("low", 1.0, "t1"),
            ("urgent", 9.0, "t3"),
            ("urgent", 2.0, "t2"),
            ("weird", 0.5, "t4"),
        ];
        rows.sort_by_key(|(p, c, i)| order_key(p, *c, i));
        let ids: Vec<&str> = rows.iter().map(|r| r.2).collect();
        assert_eq!(ids, vec!["t2", "t3", "t1", "t4"]);
    }

    #[tokio::test]
    async fn dependency_state_names_what_a_task_waits_for() {
        let tasks = store("depstate").await;
        let (done, open, dead, stacked) = (
            tasks.add("global", "done", "").await.unwrap(),
            tasks.add("global", "open", "").await.unwrap(),
            tasks.add("global", "dead", "").await.unwrap(),
            tasks.add("global", "stacked", "").await.unwrap(),
        );
        tasks
            .set_status(&done.id, "done", None, None)
            .await
            .unwrap();
        tasks
            .set_status(&dead.id, "cancelled", Some("dup"), None)
            .await
            .unwrap();
        tasks
            .set_status(&stacked.id, "done", None, None)
            .await
            .unwrap();
        tasks
            .set_landed(&stacked.id, false, None, None, None)
            .await
            .unwrap();
        let child = tasks.add("global", "child", "").await.unwrap();
        let edit = FieldEdit {
            depends_on: Some(vec![
                done.id.clone(),
                open.id.clone(),
                dead.id.clone(),
                stacked.id.clone(),
            ]),
            ..Default::default()
        };
        tasks.update(&child.id, edit, None).await.unwrap();
        let state = tasks.dependency_state(&child.id).await.unwrap();
        assert_eq!(state.blocked_by, vec![open.id.clone(), stacked.id.clone()]);
        assert_eq!(state.unlanded_dependencies, vec![stacked.id.clone()]);
        assert_eq!(state.dead_dependencies, vec![dead.id.clone()]);
        assert!(!state.dependencies_clear);
        // t903: a blocked task with every dependency satisfied says only the block is left.
        let solo = tasks.add("global", "solo", "").await.unwrap();
        let edit = FieldEdit {
            depends_on: Some(vec![done.id.clone(), dead.id.clone()]),
            ..Default::default()
        };
        tasks.update(&solo.id, edit, None).await.unwrap();
        tasks
            .set_status(&solo.id, "blocked", Some("hold"), None)
            .await
            .unwrap();
        let state = tasks.dependency_state(&solo.id).await.unwrap();
        assert!(state.unblock_needed);
        assert!(state.dependency_note.unwrap().contains("CANCELLED task"));
        assert!(tasks.dependency_state("t999").await.is_err());
    }

    #[tokio::test]
    async fn unassigned_is_claimable_and_decorate_reads_leases() {
        let tasks = store("decorate").await;
        let t = tasks.add("global", "t", "").await.unwrap();
        let d = tasks.decorate(&t.id, None).await.unwrap();
        assert!(d.claimable && d.eligible && d.is_global && d.leases.is_empty());
        assert_eq!(d.lease_expires_at, None);
        let now = crate::clock::now_s();
        tasks.insert_lease(&t.id, "%1", now, now, now + 100.0).await;
        let d = tasks.decorate(&t.id, Some("%2")).await.unwrap();
        assert_eq!(d.claimed_by, vec!["%1".to_string()]);
        assert!(
            !d.claimable,
            "a leased single-assignee task is not claimable"
        );
        assert_eq!(d.lease_expires_at, Some(now + 100.0));
        assert!(!tasks.decorate(&t.id, Some("%1")).await.unwrap().claimable);
        // expired leases are ignored
        tasks
            .insert_lease(&t.id, "%1", now - 200.0, now - 200.0, now - 100.0)
            .await;
        assert!(tasks.decorate(&t.id, Some("%2")).await.unwrap().claimable);
        // a named assignee can pull its own task; others cannot
        tasks
            .update(
                &t.id,
                FieldEdit {
                    assignees: Some(vec!["%3".into()]),
                    ..Default::default()
                },
                None,
            )
            .await
            .unwrap();
        assert!(tasks.decorate(&t.id, Some("%3")).await.unwrap().claimable);
        assert!(!tasks.decorate(&t.id, Some("%4")).await.unwrap().claimable);
        assert!(!tasks.decorate(&t.id, None).await.unwrap().claimable);
        assert!(tasks.decorate("t999", None).await.is_err());
    }

    #[tokio::test]
    async fn observers_fire_after_commit_and_a_failing_one_is_contained() {
        use std::sync::{Arc, Mutex};
        let tasks = store("fire").await;
        let seen: Arc<Mutex<Vec<(String, String, Option<String>)>>> = Arc::default();
        tasks.on_event(Box::new(|_, _, _, _| panic!("a broken observer")));
        let sink = seen.clone();
        tasks.on_event(Box::new(move |id, event, _, kind| {
            sink.lock()
                .unwrap()
                .push((id.into(), event.into(), kind.map(String::from)));
        }));
        let task = tasks.add("global", "t", "").await.unwrap();
        tasks.complete(&task.id).await.unwrap();
        tasks.complete(&task.id).await.unwrap();
        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![
                (task.id.clone(), "created".to_string(), None),
                (
                    task.id.clone(),
                    "completed".to_string(),
                    Some("work".to_string())
                ),
            ],
            "created once, completed once: completing a done task fires nothing"
        );
    }
}
