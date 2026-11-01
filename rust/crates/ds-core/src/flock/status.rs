//! `status`, `poll_status`, `view` and `h_status` (T38b, t1397), ported from
//! `dunk_core.Flock.status/poll_status/view/_invalidate_view` and `dunk_api.h_status`.
//!
//! OPEN DECISION (not code): `code_status()` in the Python is the SOURCE identity of the
//! running process (`code_hash`, `code_mtime`, `code_stale`, `code_changed_modules`,
//! `code_identity_errors`): "is the running daemon the code I just wrote". A Rust binary has
//! no module files to compare, so the Rust meaning is the leader's to choose; it sits behind
//! [`StatusEnv::code_status`] so either choice is a wiring change, not a change here. The keys
//! it returns are spliced in where the Python's `**code_status()` sits (after
//! `federated_input`).
//!
//! Everything else the status reads (dunk list, outbox length, inbox stats, task totals,
//! waiters, quarantine, last message, backend availability) is a hook too: those stores have
//! no Rust owner on `Flock` yet.

use std::sync::Mutex;

use serde_json::{json, Map, Value};

use super::dunks::DunkList;
use crate::error::DunkError;

/// `VIEW_CACHE_S` default (`DUNKINGSHEEP_VIEW_CACHE_S`).
pub const VIEW_CACHE_S: f64 = 5.0;

/// What `status`/`poll_status`/`view` read from outside the dunk list.
pub trait StatusEnv {
    fn version(&self) -> String;
    /// `self.federated_input`.
    fn federated_input(&self) -> Value;
    /// `code_status()`: the open decision above. Keys are spliced in order.
    fn code_status(&self) -> Map<String, Value>;
    fn dunks(&self) -> &DunkList;
    fn started_at(&self) -> f64;
    fn pid(&self) -> u32;
    fn now(&self) -> f64;
    /// `sum(len(q) for q in self.outbox.values())`.
    fn queued_messages(&self) -> usize;
    /// `sum(m["unread"] for m in self.inbox.stats())`: scans the whole store.
    fn inbox_unread(&self) -> Result<i64, DunkError>;
    /// `self.tasks.stats()["totals"]`: scans the whole store.
    fn task_totals(&self) -> Result<Value, DunkError>;
    /// `len(self.task_waiters)`.
    fn task_followers(&self) -> usize;
    /// `self.quarantined_mailboxes or None`.
    fn quarantined_mailboxes(&self) -> Option<Value>;
    /// `self.last_message`.
    fn last_message(&self) -> String;
    /// `self.herdr.is_available()`: a subprocess on the Python side, so only when asked.
    fn herdr_available(&self) -> bool;
    /// `self.herdr.server_error_hint()`.
    fn herdr_hint(&self) -> String;
    /// `self.state_file if self.persist_enabled else None`.
    fn state_file(&self) -> Option<String>;
    /// `self.inbox_count(None, self_pane_id=...)` as the wire JSON; an error is its text.
    fn inbox_count(&self, self_pane_id: &str) -> Result<Value, String>;
}

fn dunk_counts(env: &dyn StatusEnv) -> (usize, usize) {
    let list = env.dunks().list();
    let running = list
        .iter()
        .filter(|d| d.get("running").and_then(Value::as_bool).unwrap_or(false))
        .count();
    (list.len(), running)
}

fn uptime_s(env: &dyn StatusEnv) -> i64 {
    (env.now() - env.started_at()) as i64
}

fn put(map: &mut Map<String, Value>, key: &str, value: Value) {
    map.insert(key.to_string(), value);
}

/// `Flock.status`. The sidebar poll runs often; asking the backend for availability spawns a
/// subprocess, so `check_herdr = false` keeps that out of the hot path and leaves the field
/// null (not "down").
pub fn status(env: &dyn StatusEnv, check_herdr: bool) -> Result<Map<String, Value>, DunkError> {
    let (total, running) = dunk_counts(env);
    let available = check_herdr.then(|| env.herdr_available());
    let mut out = Map::new();
    put(&mut out, "version", json!(env.version()));
    put(&mut out, "federated_input", env.federated_input());
    for (key, value) in env.code_status() {
        out.insert(key, value);
    }
    put(&mut out, "started_at", json!(env.started_at()));
    put(&mut out, "pid", json!(env.pid()));
    put(&mut out, "dunkers", json!(total));
    put(&mut out, "running", json!(running));
    put(&mut out, "herdr_available", json!(available));
    put(
        &mut out,
        "herdr_hint",
        if available == Some(false) {
            json!(env.herdr_hint())
        } else {
            Value::Null
        },
    );
    put(&mut out, "state_file", json!(env.state_file()));
    put(&mut out, "uptime_s", json!(uptime_s(env)));
    put(&mut out, "queued_messages", json!(env.queued_messages()));
    put(&mut out, "inbox_unread", json!(env.inbox_unread()?));
    put(&mut out, "tasks", env.task_totals()?);
    put(&mut out, "task_followers", json!(env.task_followers()));
    put(
        &mut out,
        "quarantined_mailboxes",
        env.quarantined_mailboxes().unwrap_or(Value::Null),
    );
    put(&mut out, "message", json!(env.last_message()));
    Ok(out)
}

/// `Flock.poll_status`: the cheap half of status, counts only, no global inbox/task scans.
/// The sidebar poll must not pay for `inbox.stats()` and `tasks.stats()`.
pub fn poll_status(env: &dyn StatusEnv) -> Map<String, Value> {
    let (total, running) = dunk_counts(env);
    let mut out = Map::new();
    put(&mut out, "version", json!(env.version()));
    put(&mut out, "federated_input", env.federated_input());
    for (key, value) in env.code_status() {
        out.insert(key, value);
    }
    put(&mut out, "started_at", json!(env.started_at()));
    put(&mut out, "pid", json!(env.pid()));
    put(&mut out, "dunkers", json!(total));
    put(&mut out, "running", json!(running));
    put(&mut out, "queued_messages", json!(env.queued_messages()));
    put(&mut out, "message", json!(env.last_message()));
    put(&mut out, "herdr_available", Value::Null);
    put(&mut out, "herdr_hint", Value::Null);
    put(&mut out, "uptime_s", json!(uptime_s(env)));
    out
}

#[derive(Debug, Default)]
struct Cached {
    at: f64,
    pane: Option<String>,
    data: Option<Value>,
}

/// `_view_cache`. A mutation (add/update/remove/start/stop, inbox send/ack/read, task change)
/// calls [`ViewCache::invalidate`], so the next read rebuilds; the TTL bounds staleness for
/// external writers.
#[derive(Debug, Default)]
pub struct ViewCache {
    inner: Mutex<Cached>,
}

impl ViewCache {
    pub fn new() -> ViewCache {
        ViewCache::default()
    }

    /// `_invalidate_view`.
    pub fn invalidate(&self) {
        let mut cached = self.inner.lock().expect("view cache lock");
        cached.at = 0.0;
        cached.data = None;
    }

    /// `Flock.view`: status + dunks + inbox count, derived from `poll_status`, the in-memory
    /// dunk list and the inbox count. `fresh` rebuilds now. A view never breaks: an inbox
    /// failure is `inbox_error`.
    pub fn view(
        &self,
        env: &dyn StatusEnv,
        self_pane_id: Option<&str>,
        fresh: bool,
        ttl_s: f64,
    ) -> Value {
        let now = env.now();
        {
            let cached = self.inner.lock().expect("view cache lock");
            if let Some(data) = &cached.data {
                if !fresh && cached.pane.as_deref() == self_pane_id && now - cached.at < ttl_s {
                    return data.clone();
                }
            }
        }
        let mut status = poll_status(env);
        status.insert("self_pane_id".to_string(), json!(self_pane_id));
        let mut data = Map::new();
        data.insert("status".to_string(), Value::Object(status));
        data.insert(
            "dunks".to_string(),
            Value::Array(env.dunks().list().into_iter().map(Value::Object).collect()),
        );
        if let Some(pane) = self_pane_id.filter(|p| !p.is_empty()) {
            match env.inbox_count(pane) {
                Ok(count) => {
                    data.insert("inbox_count".to_string(), count);
                }
                Err(error) => {
                    data.insert("inbox_error".to_string(), json!(error));
                }
            }
        }
        let data = Value::Object(data);
        let mut cached = self.inner.lock().expect("view cache lock");
        *cached = Cached {
            at: now,
            pane: self_pane_id.map(str::to_string),
            data: Some(data.clone()),
        };
        data
    }
}

/// `dunk_api.h_status`. The cheap top of status is the cached view; the expensive
/// availability probe stays opt-in (`check_herdr`) and `full=true` keeps the old answer.
pub fn h_status(
    env: &dyn StatusEnv,
    cache: &ViewCache,
    full: bool,
    fresh: bool,
    check_herdr: bool,
    self_pane_id: Option<&str>,
) -> Result<Value, DunkError> {
    if !full {
        let view = cache.view(env, self_pane_id, fresh, VIEW_CACHE_S);
        let mut result = view["status"].as_object().cloned().unwrap_or_default();
        if check_herdr {
            result.insert("herdr_available".to_string(), json!(env.herdr_available()));
        }
        return Ok(Value::Object(result));
    }
    let mut result = status(env, true)?;
    result.insert("self_pane_id".to_string(), json!(self_pane_id));
    Ok(Value::Object(result))
}

// -- code identity (the Rust meaning of `code_status`, decided by the leader on t1397) -----

/// The identity of the running binary, frozen once at start (the Python freezes
/// `CODE_IDENTITY` before importing application code).
///
/// Decision (leader, t1397): `code_hash` is the first 12 hex of sha256 over the binary's BYTES
/// read once at start (a rebuild with the same git sha and different bytes must read as
/// different); `code_mtime` is the binary's mtime at start; `code_stale` compares
/// `(mtime, size, inode)` of the file now with the start values (one stat, no re-hash); when
/// stale, `code_changed_modules` is `[<file name>]` so a stale daemon says WHAT changed;
/// `code_identity_errors` lists read failures. As in the Python, a read failure at start
/// leaves `code_hash` null and `code_stale` null (unknown, not false).
#[derive(Debug, Clone)]
pub struct CodeIdentity {
    exe: std::path::PathBuf,
    name: String,
    hash: Option<String>,
    mtime: Option<f64>,
    stamp: Option<(i128, u64, u64)>,
    errors: Vec<String>,
}

/// `(mtime in ns, size, inode)`: one stat.
fn stamp_of(path: &std::path::Path) -> std::io::Result<(i128, u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path)?;
    let nanos = i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec());
    Ok((nanos, meta.size(), meta.ino()))
}

impl CodeIdentity {
    /// Read `exe` once: hash its bytes and record its stat stamp.
    pub fn at_start(exe: impl Into<std::path::PathBuf>) -> CodeIdentity {
        use sha2::{Digest, Sha256};
        let exe = exe.into();
        let name = exe
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "<binary>".to_string());
        let mut errors = Vec::new();
        let stamp = stamp_of(&exe).ok();
        let hash = match std::fs::read(&exe) {
            Ok(bytes) if stamp.is_some() => Some(
                Sha256::digest(&bytes)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()[..12]
                    .to_string(),
            ),
            _ => {
                errors.push(name.clone());
                None
            }
        };
        let mtime = stamp.map(|(nanos, _, _)| nanos as f64 / 1e9);
        CodeIdentity {
            exe,
            name,
            hash,
            mtime,
            stamp,
            errors,
        }
    }

    /// `code_status()`: one stat per call, never a re-hash.
    pub fn code_status(&self) -> Map<String, Value> {
        let now = stamp_of(&self.exe);
        let mut errors = self.errors.clone();
        if now.is_err() {
            errors.push(self.name.clone());
        }
        errors.sort();
        errors.dedup();
        let stale = match (&self.hash, &self.stamp, &now) {
            (Some(_), Some(start), Ok(current)) => Some(start != current),
            _ => None,
        };
        let changed: Vec<&str> = if stale == Some(true) && !errors.contains(&self.name) {
            vec![self.name.as_str()]
        } else {
            vec![]
        };
        let mut out = Map::new();
        put(&mut out, "code_hash", json!(self.hash));
        put(&mut out, "code_mtime", json!(self.mtime));
        put(&mut out, "code_stale", json!(stale));
        put(&mut out, "code_changed_modules", json!(changed));
        put(&mut out, "code_identity_errors", json!(errors));
        out
    }
}
