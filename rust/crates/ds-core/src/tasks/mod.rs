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
                Ok(())
            })
            .await?;
        self.get(&wanted).await
    }

    /// `block`: a task that waits on somebody, with the reason recorded.
    pub async fn block(&self, id: &str, reason: &str) -> Result<Task, DunkError> {
        self.set_status(id, "blocked", Some(reason), None).await
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
        self.set_status(id, "done", None, None).await
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
        let conn = rusqlite::Connection::open(&copy).unwrap();
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
}
