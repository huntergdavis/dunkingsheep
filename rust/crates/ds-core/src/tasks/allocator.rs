//! `dunk_allocator.py` (T55, slice 1): registered, deterministic allocation. The task DB is
//! the ownership authority; registration and offers are explicit contracts, never inferred
//! from task prose. This slice: the schema, `tags`, `register`, `status`, `offer`.
//!
//! Slice 2 adds `review`. NOT yet: `allocate` (the one
//! transaction that assigns an offered task to an idle registered worker), and the
//! `Allocator.tick` watchdog pass (needs the Flock: lanes, turns, outbox, inbox, delivery
//! state, next-job materialisation).
//!
//! This is a child of `tasks` because the Python reaches into the store's lock, connection,
//! `_record` and `_decorate`; here the same transaction runs on the store's writer.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Map, Value};

use super::next_job::{handoffs_in, record_in};
use super::{decorate_conn, Tasks};
use crate::db;
use crate::error::DunkError;

pub const ALLOCATION_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS allocation_workers (
 pane_id TEXT PRIMARY KEY, pools TEXT NOT NULL, roles TEXT NOT NULL,
 skills TEXT NOT NULL, enabled INTEGER NOT NULL, registered_by TEXT NOT NULL,
 updated_at REAL NOT NULL, last_assigned REAL NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS allocation_offers (
 task_id TEXT PRIMARY KEY, pool TEXT NOT NULL, role TEXT NOT NULL,
 requires TEXT NOT NULL, excluded TEXT NOT NULL, owners TEXT NOT NULL,
 offered_by TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'ready');
CREATE TABLE IF NOT EXISTS allocation_reviews (
 source_id TEXT PRIMARY KEY, task_id TEXT NOT NULL UNIQUE,
 revision TEXT NOT NULL, author TEXT NOT NULL);
";

fn refuse<T>(message: &str) -> Result<T, DunkError> {
    Err(DunkError::Refused(message.to_string()))
}

fn simple_name(text: &str) -> bool {
    (1..=80).contains(&text.chars().count())
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-'))
}

/// `tags`: a list of simple names, de-duplicated and sorted. `required` refuses an empty list.
pub fn tags(values: &Value, required: bool) -> Result<Vec<String>, DunkError> {
    let bad = || DunkError::Refused("tags must be a list of nonempty simple names".into());
    let list = values.as_array().ok_or_else(bad)?;
    let mut out: Vec<String> = Vec::new();
    for item in list {
        match item.as_str() {
            Some(text) if simple_name(text) => out.push(text.to_string()),
            _ => return Err(bad()),
        }
    }
    out.sort();
    out.dedup();
    if required && out.is_empty() {
        return refuse("at least one tag is required");
    }
    Ok(out)
}

/// `t._record(task_id, event, actor=, meta=)`: one `task_events` row. The Python records an
/// empty task id as `""` for registry events.
fn record(
    conn: &Connection,
    task_id: &str,
    event: &str,
    actor: &str,
    meta: &Value,
    now: f64,
) -> Result<(), DunkError> {
    conn.execute(
        "INSERT INTO task_events (task_id, event, at, actor, meta) VALUES (?,?,?,?,?)",
        params![task_id, event, now, actor, meta.to_string()],
    )
    .map_err(db::map_sqlite)?;
    Ok(())
}

fn rows(conn: &Connection, sql: &str, columns: &[&str]) -> Result<Vec<Value>, DunkError> {
    let mut stmt = conn.prepare(sql).map_err(db::map_sqlite)?;
    let mapped = stmt
        .query_map([], |row| {
            let mut out = Map::new();
            for (i, name) in columns.iter().enumerate() {
                out.insert(
                    (*name).to_string(),
                    match row.get_ref(i)? {
                        rusqlite::types::ValueRef::Null => Value::Null,
                        rusqlite::types::ValueRef::Integer(n) => json!(n),
                        rusqlite::types::ValueRef::Real(f) => json!(f),
                        rusqlite::types::ValueRef::Text(t) => json!(String::from_utf8_lossy(t)),
                        rusqlite::types::ValueRef::Blob(_) => Value::Null,
                    },
                );
            }
            Ok(Value::Object(out))
        })
        .map_err(db::map_sqlite)?;
    mapped
        .collect::<Result<Vec<_>, _>>()
        .map_err(db::map_sqlite)
}

/// `status`: the registry and the offers, raw columns as the Python's `dict(row)`.
fn status_in(conn: &Connection) -> Result<Value, DunkError> {
    let workers = rows(
        conn,
        "SELECT pane_id, pools, roles, skills, enabled, registered_by, updated_at, last_assigned FROM allocation_workers ORDER BY pane_id",
        &["pane_id", "pools", "roles", "skills", "enabled", "registered_by", "updated_at", "last_assigned"],
    )?;
    let offers = rows(
        conn,
        "SELECT task_id, pool, role, requires, excluded, owners, offered_by, state FROM allocation_offers ORDER BY task_id",
        &["task_id", "pool", "role", "requires", "excluded", "owners", "offered_by", "state"],
    )?;
    Ok(json!({"workers": workers, "offers": offers}))
}

/// `_offer`: validate the task and write (or re-open) its offer. Runs inside the caller's
/// transaction.
fn offer_in(
    conn: &Connection,
    task_id: &str,
    pool: &str,
    role: &str,
    requires: &Value,
    excluded: &[String],
    actor: &str,
    now: f64,
) -> Result<(), DunkError> {
    let task = decorate_conn(conn, task_id, None, now)?;
    if task.task.scope != "global" || !matches!(task.task.status.as_str(), "open" | "blocked") {
        return refuse("only open/blocked global backlog can be offered");
    }
    if task.task.body.trim().is_empty()
        || task.assignees.is_empty()
        || !task.leases.is_empty()
        || task.multi_assignee
    {
        return refuse("offer requires body, owner, exclusive task and no lease");
    }
    if role != "worker" && role != "reviewer" {
        return refuse("invalid allocation role");
    }
    tags(&json!([pool]), true)?;
    let requires = tags(requires, false)?;
    if handoffs_in(conn, None)?.iter().any(|h| {
        h.get("task_id").and_then(Value::as_str) == Some(task_id)
            && h.get("state").and_then(Value::as_str) != Some("cancelled")
    }) {
        return refuse("task already has a delivery intent");
    }
    let mut excluded: Vec<String> = excluded.to_vec();
    excluded.sort();
    excluded.dedup();
    let mut owners = task.assignees.clone();
    owners.sort();
    conn.execute(
        "INSERT INTO allocation_offers VALUES (?,?,?,?,?,?,?,'ready') \
         ON CONFLICT(task_id) DO UPDATE SET pool=excluded.pool,role=excluded.role,\
         requires=excluded.requires,excluded=excluded.excluded,owners=excluded.owners,\
         offered_by=excluded.offered_by,state='ready'",
        params![
            task_id,
            pool,
            role,
            json!(requires).to_string(),
            json!(excluded).to_string(),
            json!(owners).to_string(),
            actor
        ],
    )
    .map_err(db::map_sqlite)?;
    record(
        conn,
        task_id,
        "allocation_offered",
        actor,
        &json!({"pool": pool, "role": role, "requires": requires}),
        now,
    )
}

fn full_revision(revision: &str) -> bool {
    (40..=64).contains(&revision.len())
        && revision.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
}

/// `AllocationStore.review` inside the caller's transaction: one canonical review task per
/// source, an immutable author exclusion, and an idempotent same-revision resubmission. A new
/// revision reopens the SAME completed review task, but cannot replace a revision that is
/// being reviewed or is in delivery. Returns the review task's id.
#[allow(clippy::too_many_arguments)]
fn review_in(
    conn: &Connection,
    source_id: &str,
    revision: &str,
    body: &str,
    pool: &str,
    requires: &Value,
    author: &str,
    now: f64,
) -> Result<String, DunkError> {
    let body = format!("Frozen revision: {revision}\nSource task: {source_id}\n\n{body}");
    let source: (String, String, String, String, Option<String>, Option<String>) = conn
        .query_row(
            "SELECT scope, title, priority, assignees, created_by, completed_by FROM tasks WHERE id = ?",
            params![source_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .optional()
        .map_err(db::map_sqlite)?
        .ok_or_else(|| DunkError::Refused(format!("no task with id {source_id:?}")))?;
    let (scope, title, priority, assignees_raw, created_by, completed_by) = source;
    let assignees: Vec<String> = serde_json::from_str(&assignees_raw).unwrap_or_default();
    if scope != "global" || !assignees.iter().any(|a| a == author) {
        return refuse("only source owner may submit global work for review");
    }
    let old: Option<(String, String, String)> = conn
        .query_row(
            "SELECT task_id, revision, author FROM allocation_reviews WHERE source_id=?",
            params![source_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()
        .map_err(db::map_sqlite)?;
    if let Some((_, _, old_author)) = &old {
        if old_author != author {
            return refuse("review author changed; operator reconciliation required");
        }
    }
    if let Some((old_task, old_revision, _)) = &old {
        if old_revision == revision {
            return Ok(old_task.clone());
        }
    }
    let task_id = if let Some((old_task, _, _)) = &old {
        let previous: String = conn
            .query_row(
                "SELECT status FROM tasks WHERE id = ?",
                params![old_task],
                |r| r.get(0),
            )
            .map_err(db::map_sqlite)?;
        if previous != "done" {
            return refuse("prior review is not finished");
        }
        // Completion is durable; retained terminal leases are not active review work and
        // must not delay a corrected revision for 30 minutes.
        conn.execute("DELETE FROM task_leases WHERE task_id=?", params![old_task])
            .map_err(db::map_sqlite)?;
        conn.execute(
            "UPDATE tasks SET status='open',body=?,assignees=?,updated_at=?,completed_at=NULL,completed_by=NULL WHERE id=?",
            params![body, json!([author]).to_string(), now, old_task],
        )
        .map_err(db::map_sqlite)?;
        old_task.clone()
    } else {
        let next: i64 = conn
            .query_row(
                "INSERT INTO counters (name, value) VALUES ('task', 1) ON CONFLICT(name) DO UPDATE SET value = value + 1 RETURNING value",
                [],
                |r| r.get(0),
            )
            .map_err(db::map_sqlite)?;
        let id = format!("t{next}");
        conn.execute(
            "INSERT INTO tasks (id,scope,title,body,status,priority,parent_id,assignees,created_at,updated_at,created_by) VALUES (?,'global',?,?,'open',?,?,?,?,?,?)",
            params![id, format!("Review: {title}"), body, priority, source_id, json!([author]).to_string(), now, now, author],
        )
        .map_err(db::map_sqlite)?;
        conn.execute(
            "INSERT INTO task_events (task_id, event, at, actor, scope, priority, status_to) VALUES (?,?,?,?,?,?,?)",
            params![id, "created", now, author, "global", priority, "open"],
        )
        .map_err(db::map_sqlite)?;
        id
    };
    // Cancel only a terminal review's historical intents before reopening.
    if old.is_some() {
        for handoff in handoffs_in(conn, None)? {
            if handoff.get("task_id").and_then(Value::as_str) == Some(task_id.as_str())
                && handoff.get("state").and_then(Value::as_str) != Some("cancelled")
            {
                let mut details = handoff.clone();
                details.insert("state".into(), json!("cancelled"));
                details.insert("reason".into(), json!("review_revision_superseded"));
                let pane = handoff.get("pane_id").and_then(Value::as_str).unwrap_or("");
                let from_task = handoff.get("from_task").and_then(Value::as_str);
                record_in(
                    conn, &task_id, pane, from_task, None, 0, None, now, &details,
                )?;
            }
        }
    }
    let mut excluded: Vec<String> = assignees.clone();
    excluded.extend(
        [created_by, completed_by]
            .into_iter()
            .flatten()
            .filter(|p| !p.is_empty()),
    );
    let prior: Option<String> = conn
        .query_row(
            "SELECT excluded FROM allocation_offers WHERE task_id=?",
            params![task_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(db::map_sqlite)?;
    if let Some(raw) = prior {
        excluded.extend(serde_json::from_str::<Vec<String>>(&raw).unwrap_or_default());
    }
    excluded.sort();
    excluded.dedup();
    offer_in(
        conn, &task_id, pool, "reviewer", requires, &excluded, author, now,
    )?;
    conn.execute(
        "INSERT INTO allocation_reviews VALUES (?,?,?,?) ON CONFLICT(source_id) DO UPDATE SET revision=excluded.revision",
        params![source_id, task_id, revision, author],
    )
    .map_err(db::map_sqlite)?;
    record(
        conn,
        source_id,
        "review_submitted",
        author,
        &json!({"review_task": task_id, "revision": revision}),
        now,
    )?;
    Ok(task_id)
}

impl Tasks {
    /// `AllocationStore.__init__`: create the allocation tables (idempotent).
    pub async fn allocation_init(&self) -> Result<(), DunkError> {
        self.writer
            .call(|conn| {
                conn.execute_batch(ALLOCATION_SCHEMA)
                    .map_err(db::map_sqlite)
            })
            .await
    }

    /// `AllocationStore.status`.
    pub async fn allocation_status(&self) -> Result<Value, DunkError> {
        self.writer.call(|conn| status_in(conn)).await
    }

    /// `AllocationStore.register`: an explicit, operator-attributed registration of a worker.
    pub async fn allocation_register(
        &self,
        pane: &str,
        pools: &Value,
        roles: &Value,
        skills: &Value,
        actor: &str,
        enabled: &Value,
    ) -> Result<Value, DunkError> {
        let Some(enabled) = enabled.as_bool() else {
            return refuse("enabled must be boolean");
        };
        let (pools, roles, skills) = (tags(pools, true)?, tags(roles, true)?, tags(skills, false)?);
        if !roles.iter().all(|r| r == "worker" || r == "reviewer") {
            return refuse("roles must be worker or reviewer");
        }
        if pane.is_empty() || actor.is_empty() {
            return refuse("registration requires pane and operator");
        }
        let (pane, actor) = (pane.to_string(), actor.to_string());
        let now = crate::clock::now_s();
        self.writer
            .call(move |conn| {
                let tx = conn.unchecked_transaction().map_err(db::map_sqlite)?;
                tx.execute(
                    "INSERT INTO allocation_workers (pane_id,pools,roles,skills,enabled,registered_by,updated_at) \
                     VALUES (?,?,?,?,?,?,?) ON CONFLICT(pane_id) DO UPDATE SET \
                     pools=excluded.pools,roles=excluded.roles,skills=excluded.skills,\
                     enabled=excluded.enabled,registered_by=excluded.registered_by,updated_at=excluded.updated_at",
                    params![pane, json!(pools).to_string(), json!(roles).to_string(), json!(skills).to_string(), enabled as i64, actor, now],
                )
                .map_err(db::map_sqlite)?;
                record(&tx, "", "allocation_registered", &actor, &json!({"pane_id": pane, "enabled": enabled, "pools": pools, "roles": roles, "skills": skills}), now)?;
                tx.commit().map_err(db::map_sqlite)?;
                status_in(conn)
            })
            .await
    }

    /// `AllocationStore.offer`: offer an open global task, owned and exclusive, as worker work.
    /// One immediate transaction; a canonical review can never be re-offered as worker work.
    pub async fn allocation_offer(
        &self,
        task_id: &str,
        pool: &str,
        requires: &Value,
        actor: &str,
    ) -> Result<super::Task, DunkError> {
        let (id, pool, requires, actor) = (
            task_id.to_string(),
            pool.to_string(),
            requires.clone(),
            actor.to_string(),
        );
        let now = crate::clock::now_s();
        let wanted = id.clone();
        self.writer
            .call(move |conn| {
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(db::map_sqlite)?;
                let reviewed: Option<i64> = tx
                    .query_row(
                        "SELECT 1 FROM allocation_reviews WHERE task_id=?",
                        params![id],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(db::map_sqlite)?;
                if reviewed.is_some() {
                    return refuse("canonical review cannot be reoffered as worker work");
                }
                offer_in(&tx, &id, &pool, "worker", &requires, &[], &actor, now)?;
                tx.commit().map_err(db::map_sqlite)
            })
            .await?;
        self.get(&wanted).await
    }
    /// `AllocationStore.review`: submit a source task's work for canonical review. The
    /// revision must be a full hash and the body real evidence/acceptance text.
    pub async fn allocation_review(
        &self,
        source_id: &str,
        revision: &str,
        body: &str,
        pool: &str,
        requires: &Value,
        author: &str,
    ) -> Result<super::Task, DunkError> {
        if !full_revision(revision) || body.trim().is_empty() {
            return refuse("review requires full revision hash and evidence/acceptance body");
        }
        let (source, revision, body, pool, requires, author) = (
            source_id.to_string(),
            revision.to_string(),
            body.to_string(),
            pool.to_string(),
            requires.clone(),
            author.to_string(),
        );
        let now = crate::clock::now_s();
        let task_id = self
            .writer
            .call(move |conn| {
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(db::map_sqlite)?;
                let id = review_in(
                    &tx, &source, &revision, &body, &pool, &requires, &author, now,
                )?;
                tx.commit().map_err(db::map_sqlite)?;
                Ok(id)
            })
            .await?;
        self.get(&task_id).await
    }
}

#[cfg(test)]
#[path = "allocator_tests.rs"]
mod allocator_tests;
