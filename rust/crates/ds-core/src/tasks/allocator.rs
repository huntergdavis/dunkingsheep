//! `dunk_allocator.py` (T55, slice 1): registered, deterministic allocation. The task DB is
//! the ownership authority; registration and offers are explicit contracts, never inferred
//! from task prose. This slice: the schema, `tags`, `register`, `status`, `offer`.
//!
//! NOT in this slice: `review` (one canonical review per source), `allocate` (the one
//! transaction that assigns an offered task to an idle registered worker), and the
//! `Allocator.tick` watchdog pass (needs the Flock: lanes, turns, outbox, inbox, delivery
//! state, next-job materialisation).
//!
//! This is a child of `tasks` because the Python reaches into the store's lock, connection,
//! `_record` and `_decorate`; here the same transaction runs on the store's writer.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Map, Value};

use super::next_job::handoffs_in;
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
}

#[cfg(test)]
#[path = "allocator_tests.rs"]
mod allocator_tests;
