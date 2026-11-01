//! `health.rs` (T54, slice 4): deterministic self-check and self-repair. Ports `dunk_health.py`.
//!
//! There is no model in this loop. Every check has a fixed threshold and, when `repair` is set,
//! a fixed repair. The daemon's maintenance tick and the `dunkingsheep health --repair` timer
//! call the same code. Checks: dispatcher freshness, memory DB presence + `PRAGMA quick_check`,
//! the ok flag, stale ingest leases, WAL size, free disk space, task counts and the registry vs
//! pane-list gap (t714). Repairs: requeue stale leases, checkpoint a large WAL.
//!
//! Everything the Python reaches through the `flock` (the memory service, the task stats, the
//! registry/pane gap) is a [`HealthEnv`] method, so the check runs without a daemon.
//!
//! Texts that embed a Python exception string (sqlite3.Error in `quick_check`) cannot be
//! byte-identical; they carry the Rust error text instead (named in the PR).

use std::fs;
use std::path::Path;

use nix::sys::statvfs::statvfs;
use serde_json::{json, Map, Value};

use crate::config::{config_dir, Env};
use crate::db;
use crate::error::DunkError;

/// The Python module constants, as a value so a test can lower one (`mock.patch`).
#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    /// `WAL_WARN_BYTES`: 64 MiB.
    pub wal_warn_bytes: i64,
    /// `DISPATCH_STALE_S`: 16 minutes (two missed 5-minute ticks plus slack).
    pub dispatch_stale_s: i64,
    /// `DISK_MIN_FREE_BYTES`: 512 MiB.
    pub disk_min_free_bytes: u64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            wal_warn_bytes: 64 * 1024 * 1024,
            dispatch_stale_s: 16 * 60,
            disk_min_free_bytes: 512 * 1024 * 1024,
        }
    }
}

/// What `run_health` reads from the flock. `Err(text)` is the Python exception's `str()`.
pub trait HealthEnv {
    /// `getattr(flock, "memory", None) is not None`.
    fn memory_enabled(&self) -> bool;
    /// `flock.memory_stats()`.
    fn memory_stats(&self) -> Result<Value, String>;
    /// `flock.memory_status()`.
    fn memory_status(&self) -> Result<Value, String>;
    /// `store.recover_leases()`.
    fn recover_leases(&self) -> Result<(), DunkError>;
    /// `store.requeue_partial()`.
    fn requeue_partial(&self) -> Result<(), DunkError>;
    /// `store.checkpoint()`.
    fn checkpoint(&self) -> Result<(), DunkError>;
    /// `flock.tasks.stats().get("totals", {})`.
    fn task_totals(&self) -> Result<Value, String>;
    /// `flock.registry_pane_gap()`; `None` when the flock has no such reader.
    fn registry_pane_gap(&self) -> Option<Result<Vec<Value>, String>> {
        None
    }
}

/// `HEALTH_PATH`: `~/.config/dunkingsheep/health.json` (the Python expands `~`, it does not
/// consult `DUNKINGSHEEP_DIR`; kept).
pub fn default_health_path(env: &Env) -> std::path::PathBuf {
    Path::new(env.get("HOME").unwrap_or("/")).join(".config/dunkingsheep/health.json")
}

/// `_stale_leases`: ingest rows a dead worker left behind. Read-only, fixed query.
pub fn stale_leases(path: &Path, now: f64) -> Result<Value, DunkError> {
    let conn = db::open_reader(path)?;
    let expired: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM ingest_queue WHERE state='leased' \
             AND (lease_until IS NULL OR lease_until < ?)",
            [now],
            |row| row.get(0),
        )
        .map_err(db::map_sqlite)?;
    let partial: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM ingest_queue q JOIN sources s \
             ON s.id=q.source_id WHERE q.state='skipped' AND s.state='partial'",
            [],
            |row| row.get(0),
        )
        .map_err(db::map_sqlite)?;
    Ok(json!({"expired_leases": expired, "partial_skipped": partial}))
}

/// `_quick_check`: the first row of `PRAGMA quick_check`.
pub fn quick_check(path: &Path) -> Result<String, DunkError> {
    let conn = db::open_reader(path)?;
    conn.query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(db::map_sqlite)
}

/// Python's `float(x)` for the value of `tick_at` (`None` = ValueError/TypeError).
fn py_float(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
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

/// `_dispatch_freshness`: how long since a dispatcher tick completed: "the feeder is dead" as a
/// number. Absence is not evidence: a tick that has never run and a tick that stopped are
/// different, so the check reports both, with the age.
pub fn dispatch_freshness(config_dir: &Path, now: f64, thresholds: &Thresholds) -> Value {
    let absent = |reason: &str| json!({"status": "absent", "age_s": null, "reason": reason});
    let path = config_dir.join("dispatch-state.json");
    let state: Option<Value> = fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    let Some(state) = state else {
        return absent("no dispatcher tick has ever run");
    };
    let tick_at = match state.get("tick_at").filter(|v| truthy(v)) {
        None => 0.0,
        Some(value) => match py_float(value) {
            Some(tick) => tick,
            None => return absent("no dispatcher tick has ever run"),
        },
    };
    if tick_at == 0.0 {
        return absent("the dispatch state has no tick_at");
    }
    let age = (now - tick_at).max(0.0);
    json!({
        "status": if age <= thresholds.dispatch_stale_s as f64 { "ok" } else { "stale" },
        "age_s": age as i64,
        "dry_run": state.get("dry_run").is_some_and(truthy),
        "reason": null,
    })
}

fn py_str(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".to_string(),
        Some(Value::Bool(true)) => "True".to_string(),
        Some(Value::Bool(false)) => "False".to_string(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
    }
}

/// `run_health(flock, repair, deep)`.
pub fn run_health(
    env: &dyn HealthEnv,
    repair: bool,
    deep: bool,
    config_dir: &Path,
    now: f64,
    thresholds: &Thresholds,
) -> Result<Value, DunkError> {
    let started = now;
    let mut problems: Vec<String> = Vec::new();
    let mut repairs: Vec<String> = Vec::new();
    let mut checks = Map::new();
    let dispatch = dispatch_freshness(config_dir, started, thresholds);
    // ABSENT IS NOT A PROBLEM: a fresh host has no tick yet, and a health check that calls that
    // DEGRADED is an alarm nobody can satisfy until the timer runs. It becomes a problem only
    // when a tick EXISTS and has stopped.
    if dispatch["status"] == "stale" {
        problems.push(format!(
            "dispatcher: last successful tick {}s ago (stale after {}s) - the feeder is not running",
            dispatch["age_s"], thresholds.dispatch_stale_s
        ));
    }
    checks.insert("dispatch".into(), dispatch);

    if !env.memory_enabled() {
        problems.push("memory service disabled".into());
    } else {
        let stats = env.memory_stats().unwrap_or_else(|error| {
            problems.push(format!("memory_stats failed: {error}"));
            json!({})
        });
        let status = env.memory_status().unwrap_or_else(|error| {
            problems.push(format!("memory_status failed: {error}"));
            json!({})
        });
        let mut memory = Map::new();
        for key in [
            "path",
            "schema_version",
            "sources",
            "sessions",
            "chunks",
            "queue_depth",
        ] {
            memory.insert(key.into(), stats.get(key).cloned().unwrap_or(Value::Null));
        }
        memory.insert(
            "ok".into(),
            status.get("ok").cloned().unwrap_or(Value::Null),
        );
        checks.insert("memory".into(), Value::Object(memory));
        let db_bytes = stats
            .get("db_bytes")
            .filter(|v| truthy(v))
            .cloned()
            .unwrap_or_else(|| json!({}));
        checks.insert("db_bytes".into(), db_bytes.clone());
        if status.get("ok") == Some(&Value::Bool(false)) {
            problems.push("memory status reports ok=false".into());
        }
        let path = stats
            .get("path")
            .and_then(Value::as_str)
            .filter(|p| !p.is_empty());
        if let Some(path) = path.filter(|p| Path::new(p).exists()) {
            let path = Path::new(path);
            let integrity = if deep {
                // A full quick_check scans the whole DB; run it only when asked.
                let integrity = quick_check(path).unwrap_or_else(|error| format!("error: {error}"));
                if integrity != "ok" {
                    problems.push(format!("sqlite quick_check: {integrity}"));
                }
                integrity
            } else {
                // Cheap liveness: the header and freelist answer, and stats already succeeded.
                "ok (shallow)".to_string()
            };
            checks.insert("integrity".into(), json!(integrity));
            let stale = stale_leases(path, started)?;
            let expired = stale["expired_leases"].as_i64().unwrap_or(0);
            let partial = stale["partial_skipped"].as_i64().unwrap_or(0);
            if expired != 0 || partial != 0 {
                if repair {
                    env.recover_leases()?;
                    env.requeue_partial()?;
                    repairs.push(format!(
                        "requeued {expired} expired + {partial} partial ingest lease(s)"
                    ));
                } else {
                    problems.push(format!(
                        "stale ingest leases: {{'expired_leases': {expired}, 'partial_skipped': {partial}}}"
                    ));
                }
            }
            checks.insert("stale_leases".into(), stale);
            let wal = db_bytes.get("-wal").and_then(Value::as_i64).unwrap_or(0);
            checks.insert("wal_bytes".into(), json!(wal));
            if wal > thresholds.wal_warn_bytes {
                if repair {
                    env.checkpoint()?;
                    repairs.push(format!("checkpointed WAL ({wal} bytes)"));
                } else {
                    problems.push(format!("WAL large: {wal} bytes"));
                }
            }
        }
    }

    let usage = statvfs(config_dir).map_err(|e| {
        DunkError::Refused(format!(
            "[Errno {}] {}: '{}'",
            e as i32,
            e.desc(),
            config_dir.display()
        ))
    })?;
    let free = usage.blocks_available() as u64 * usage.fragment_size() as u64;
    checks.insert("disk_free_bytes".into(), json!(free));
    if free < thresholds.disk_min_free_bytes {
        problems.push(format!("disk free below threshold: {free} bytes"));
    }

    match env.task_totals() {
        Ok(totals) => {
            checks.insert("tasks".into(), totals);
        }
        Err(error) => problems.push(format!("task stats failed: {error}")),
    }

    // REGISTRY vs PANE LIST (t714). A registered lane whose process is alive but which the
    // backend pane list does not contain is unreachable. The check reports the disagreement
    // instead of asserting a cause; a registration whose process is GONE is a stale row, not a
    // lost lane.
    if let Some(gap) = env.registry_pane_gap() {
        let gap = gap.unwrap_or_else(|error| {
            problems.push(format!("registry/pane-list comparison failed: {error}"));
            Vec::new()
        });
        let alive: Vec<&Value> = gap
            .iter()
            .filter(|row| row.get("alive").is_some_and(truthy))
            .collect();
        if !alive.is_empty() {
            let names: Vec<String> = alive
                .iter()
                .map(|row| {
                    let label = row
                        .get("label")
                        .filter(|v| truthy(v))
                        .or_else(|| row.get("role"));
                    format!(
                        "{} ({}, pid {})",
                        py_str(row.get("pane")),
                        py_str(label),
                        py_str(row.get("pid"))
                    )
                })
                .collect();
            problems.push(format!(
                "backend pane list is missing {} registered lane(s) whose process is alive: {} - they cannot be typed into and read as dead to the close/dead guards (t699/t714)",
                alive.len(),
                names.join(", ")
            ));
        }
        checks.insert("registry_pane_gap".into(), Value::Array(gap));
    }

    let finished = crate::clock::now_s();
    Ok(json!({
        "ok": problems.is_empty(),
        "checked_at": started,
        "duration_ms": ((finished - started).max(0.0) * 1000.0 * 10.0).round() / 10.0,
        "problems": problems,
        "repairs": repairs,
        "checks": checks,
    }))
}

/// `write_report`: write the last report where ops tooling (and the VP) can read it
/// (`indent=2, sort_keys=True`, no trailing newline).
pub fn write_report(result: &Value, path: &Path) -> Result<std::path::PathBuf, DunkError> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(&crate::atomic_json::sorted_keys(result.clone()))?;
    fs::write(path, text)?;
    Ok(path.to_path_buf())
}

/// The default config directory for a check (`DUNKINGSHEEP_DIR` or `~/.config/dunkingsheep`).
pub fn check_config_dir(env: &Env) -> Result<std::path::PathBuf, DunkError> {
    config_dir(env)
}

#[cfg(test)]
mod tests;
