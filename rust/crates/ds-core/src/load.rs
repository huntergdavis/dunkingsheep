//! `load.rs` (T54, slice 2): per-caller CPU from /proc, with a pid+starttime guard and process
//! trees. Ports `dunk_load.py`.
//!
//! Identity is the registered caller (pid + starttime). A recycled pid produces no sample.
//! CPU is the caller's PROCESS TREE (the caller plus every descendant), found by walking the
//! ppid map. A child that reparents to init escapes the tree: that is the stated limit, and the
//! unattributed remainder is what makes it visible. No subprocesses.
//!
//! KNOWN PYTHON DEFECT, ported as is (port, do not redesign): `collect` reads the previous
//! sample from `state["last"]` but writes only `state["at"]` and `state["callers"]`, so the
//! previous sample is always empty, `cpu_pct` is always `None` and `peak_cpu_pct` always 0.0.
//! The tests assert the Python's behaviour; a fix is a Python+Rust change of its own.
//!
//! The policy evaluation is injected (`Evaluate`) so this module does not own the choice of
//! evaluator; production passes `policy::evaluate` mapped to its `PolicyError` text.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::config::{config_dir, Env};
use crate::error::DunkError;

type Object = Map<String, Value>;

/// `(name, pid, starttime)`: one registered caller.
pub type Caller = (String, i64, i64);

/// The policy evaluation `collect` calls for a caller with a policy (`dunk_policy.evaluate`).
/// `Err` carries the `PolicyError` text.
pub type Evaluate<'a> = &'a dyn Fn(&Object, &Object) -> Result<Vec<Value>, String>;

/// One entry of the aux vector (`/proc/self/auxv`): the kernel's own answer for `SC_CLK_TCK`
/// (`AT_CLKTCK`, 17) and `SC_PAGE_SIZE` (`AT_PAGESZ`, 6). Read here because `nix::unistd::sysconf`
/// sits behind a nix feature this crate does not enable and a Cargo change is outside this task.
fn auxv(key: u64) -> Option<f64> {
    let bytes = fs::read("/proc/self/auxv").ok()?;
    bytes.chunks_exact(16).find_map(|entry| {
        let (k, v) = entry.split_at(8);
        let k = u64::from_ne_bytes(k.try_into().ok()?);
        let v = u64::from_ne_bytes(v.try_into().ok()?);
        (k == key && v != 0).then_some(v as f64)
    })
}

/// `TICKS`: `SC_CLK_TCK`, else 100.
pub fn ticks() -> f64 {
    auxv(17).unwrap_or(100.0)
}

fn page_size() -> f64 {
    auxv(6).unwrap_or(4096.0)
}

/// `STATE`: `DUNKINGSHEEP_LOAD_STATE`, else `load-state.json` in the config directory.
pub fn default_state_path(env: &Env) -> Result<PathBuf, DunkError> {
    match env.get("DUNKINGSHEEP_LOAD_STATE") {
        Some(path) if !path.is_empty() => Ok(PathBuf::from(path)),
        _ => Ok(config_dir(env)?.join("load-state.json")),
    }
}

/// The fields of `/proc/<pid>/stat` the sampler needs (`_stat`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stat {
    pub state: String,
    pub ppid: i64,
    pub utime: i64,
    pub stime: i64,
    pub starttime: i64,
    pub rss_pages: i64,
}

/// `_stat`: `None` when the process is gone or the line does not parse. The comm may contain
/// spaces and parentheses, so everything is parsed after the LAST `)`.
pub fn stat(pid: i64) -> Option<Stat> {
    let raw = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest: Vec<&str> = raw.rsplit_once(')')?.1.split_whitespace().collect();
    Some(Stat {
        state: (*rest.first()?).to_string(),
        ppid: rest.get(1)?.parse().ok()?,
        utime: rest.get(11)?.parse().ok()?,
        stime: rest.get(12)?.parse().ok()?,
        starttime: rest.get(19)?.parse().ok()?,
        rss_pages: rest.get(21)?.parse().ok()?,
    })
}

/// `_ppid_map`: one walk, pid -> ppid for every visible process.
pub fn ppid_map() -> BTreeMap<i64, i64> {
    let mut out = BTreeMap::new();
    let Ok(entries) = fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name
            .to_str()
            .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        else {
            continue;
        };
        let Ok(pid) = name.parse::<i64>() else {
            continue;
        };
        if let Some(info) = stat(pid) {
            out.insert(pid, info.ppid);
        }
    }
    out
}

/// `tree_of`: the caller plus descendants, via the ppid map, sorted.
pub fn tree_of(pid: i64, ppids: &BTreeMap<i64, i64>) -> Vec<i64> {
    let mut children: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for (child, parent) in ppids {
        children.entry(*parent).or_default().push(*child);
    }
    let mut seen = BTreeSet::new();
    let mut stack = vec![pid];
    while let Some(current) = stack.pop() {
        if !seen.insert(current) {
            continue;
        }
        stack.extend(children.get(&current).into_iter().flatten().copied());
    }
    seen.into_iter().collect()
}

/// `tree_cpu`'s result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeCpu {
    pub ticks: i64,
    pub rss_pages: i64,
    pub process_count: i64,
    pub starttime: i64,
}

/// `tree_cpu`: summed utime+stime and RSS for the tree, or `None` if the root is gone.
pub fn tree_cpu(pid: i64, ppids: &BTreeMap<i64, i64>) -> Option<TreeCpu> {
    let members = tree_of(pid, ppids);
    let root = stat(pid)?;
    let (mut ticks, mut rss_pages, mut process_count) = (0, 0, 0);
    for member in members {
        if let Some(info) = stat(member) {
            ticks += info.utime + info.stime;
            rss_pages += info.rss_pages;
            process_count += 1;
        }
    }
    Some(TreeCpu {
        ticks,
        rss_pages,
        process_count,
        starttime: root.starttime,
    })
}

/// `registered_callers`: `[(name, pid, starttime)]` from task leases that carry caller identity.
pub fn registered_callers(tasks: &[Value]) -> Vec<Caller> {
    // Insertion-ordered, last name wins per (pid, start), like the Python dict.
    let mut seen: Vec<((i64, i64), String)> = Vec::new();
    for lease in tasks {
        let pid = lease
            .get("caller_pid")
            .and_then(Value::as_i64)
            .filter(|p| *p != 0);
        let start = lease.get("caller_start").and_then(Value::as_i64);
        let (Some(pid), Some(start)) = (pid, start) else {
            continue;
        };
        let name = match lease
            .get("caller_name")
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty())
        {
            Some(name) => name.to_string(),
            None => format!("pid {pid}"),
        };
        match seen.iter_mut().find(|(key, _)| *key == (pid, start)) {
            Some(slot) => slot.1 = name,
            None => seen.push(((pid, start), name)),
        }
    }
    seen.into_iter()
        .map(|((pid, start), name)| (name, pid, start))
        .collect()
}

/// `sample`: per caller, CPU ticks now and the pid-reuse verdict. Returns
/// `{"at", "callers": [...], "skipped": [...]}`.
pub fn sample(callers: &[Caller], now: f64) -> Value {
    let ppids = ppid_map();
    let (mut rows, mut skipped) = (Vec::new(), Vec::new());
    let gone =
        |name: &str, pid: i64, reason: &str| json!({"name": name, "pid": pid, "reason": reason});
    for (name, pid, start) in callers {
        let Some(info) = stat(*pid) else {
            skipped.push(gone(name, *pid, "process gone"));
            continue;
        };
        if info.starttime != *start {
            skipped.push(gone(name, *pid, "pid reused (starttime differs)"));
            continue;
        }
        let Some(cpu) = tree_cpu(*pid, &ppids) else {
            skipped.push(gone(name, *pid, "process gone"));
            continue;
        };
        rows.push(
            json!({"name": name, "pid": pid, "starttime": start, "cpu_ticks": cpu.ticks,
            "rss_pages": cpu.rss_pages, "process_count": cpu.process_count}),
        );
    }
    json!({"at": now, "callers": rows, "skipped": skipped})
}

/// `gpu`: per-process GPU is a NULL with a reason, never a fabricated number.
pub fn gpu() -> Value {
    if let Ok(entries) = fs::read_dir("/proc") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if !name
                .to_str()
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
            {
                continue;
            }
            let fdinfo = entry.path().join("fdinfo");
            if !fdinfo.is_dir() {
                continue;
            }
            let Ok(fds) = fs::read_dir(&fdinfo) else {
                continue;
            };
            for fd in fds.flatten() {
                let Ok(bytes) = fs::read(fd.path()) else {
                    continue;
                };
                if String::from_utf8_lossy(&bytes).contains("drm-engine-") {
                    return json!({"value": null, "reason":
                        "drm fdinfo exposes engine time, not a per-process utilisation percentage"});
                }
            }
        }
    }
    json!({"value": null, "reason": "no per-process GPU counter on this hardware (Intel UHD 620)"})
}

/// `_load_state`: anything unreadable or malformed is an empty state.
fn load_state(path: &Path) -> Value {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| json!({}))
}

/// `round(x, n)` the way Python does it: correctly rounded from the exact binary value.
fn round_to(value: f64, places: usize) -> f64 {
    format!("{value:.places$}").parse().unwrap_or(value)
}

fn host_load1() -> Option<f64> {
    fs::read_to_string("/proc/loadavg")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Python's `str()` of a JSON scalar, for the would-kill line.
fn py_str(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".to_string(),
        Some(Value::Bool(true)) => "True".to_string(),
        Some(Value::Bool(false)) => "False".to_string(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => other.to_string(),
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

/// `collect`: sample and turn ticks into a window percentage of one core.
///
/// `state_path` lets a second reader (the watchdog) keep its own sample window instead of
/// advancing this one. `policies` is keyed by caller name (or the pid as text).
pub fn collect(
    callers: &[Caller],
    now: f64,
    policies: Option<&Object>,
    evaluate: Evaluate<'_>,
    state_path: &Path,
) -> Result<Value, DunkError> {
    let state = load_state(state_path);
    // The Python reads state["last"], which it never writes (see the module note).
    let previous = state.get("last").cloned().unwrap_or_else(|| json!({}));
    let previous = if truthy(&previous) {
        previous
    } else {
        json!({})
    };
    let previous_at = state
        .get("at")
        .and_then(Value::as_f64)
        .filter(|at| *at != 0.0);
    let current = sample(callers, now);
    let elapsed = previous_at.map(|at| now - at);
    let ticks = ticks();
    let current_callers = current["callers"].as_array().cloned().unwrap_or_default();
    let mut rows: Vec<Object> = Vec::new();
    for row in &current_callers {
        let key = format!("{}:{}", row["pid"], row["starttime"]);
        let before = previous.get("callers").and_then(|c| c.get(&key)).cloned();
        let cpu_ticks = row["cpu_ticks"].as_i64().unwrap_or(0);
        let mut pct: Option<f64> = None;
        if let (Some(before), Some(elapsed)) = (&before, elapsed) {
            if elapsed != 0.0 && elapsed > 0.0 {
                let delta =
                    cpu_ticks - before.get("cpu_ticks").and_then(Value::as_i64).unwrap_or(0);
                if delta >= 0 {
                    pct = Some(round_to(100.0 * delta as f64 / (elapsed * ticks), 1));
                }
            }
        }
        let peak = match before.as_ref().filter(|b| truthy(b)) {
            Some(before) => {
                let old = before
                    .get("peak_cpu_pct")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0);
                pct.unwrap_or(0.0).max(old)
            }
            None => pct.unwrap_or(0.0),
        };
        let rss_pages = row["rss_pages"].as_f64().unwrap_or(0.0);
        let mut out = Object::new();
        out.insert("name".into(), row["name"].clone());
        out.insert("pid".into(), row["pid"].clone());
        out.insert("cpu_pct".into(), pct.map_or(Value::Null, |p| json!(p)));
        out.insert("peak_cpu_pct".into(), json!(peak));
        out.insert(
            "rss_mb".into(),
            json!(round_to(rss_pages * page_size() / 1_048_576.0, 1)),
        );
        out.insert("process_count".into(), row["process_count"].clone());
        out.insert("gpu_pct".into(), gpu());
        rows.push(out);
    }
    let mut state_callers = Object::new();
    for row in &current_callers {
        let peak = rows
            .iter()
            .find(|r| r["pid"] == row["pid"])
            .map_or(json!(0.0), |r| r["peak_cpu_pct"].clone());
        state_callers.insert(
            format!("{}:{}", row["pid"], row["starttime"]),
            json!({"cpu_ticks": row["cpu_ticks"], "peak_cpu_pct": peak}),
        );
    }
    crate::atomic_json::write(state_path, &json!({"at": now, "callers": state_callers}))?;
    // The honest remainder: host load minus what we attributed (in cores).
    let load1 = host_load1();
    let attributed = round_to(
        rows.iter()
            .map(|r| r["cpu_pct"].as_f64().unwrap_or(0.0))
            .sum::<f64>()
            / 100.0,
        2,
    );
    let unattributed = load1.map(|l| round_to((l - attributed).max(0.0), 2));
    let mut would_kill = Vec::new();
    if let Some(policies) = policies.filter(|p| !p.is_empty()) {
        for row in &rows {
            let name = row["name"].as_str().unwrap_or_default();
            let pid_text = row["pid"].to_string();
            let policy = policies
                .get(name)
                .filter(|p| truthy(p))
                .or_else(|| policies.get(&pid_text).filter(|p| truthy(p)))
                .and_then(Value::as_object);
            let Some(policy) = policy else { continue };
            let findings = evaluate(row, policy)
                .unwrap_or_else(|reason| vec![json!({"rule": "policy", "reason": reason})]);
            if findings.is_empty() {
                continue;
            }
            let reason = findings
                .iter()
                .filter(|f| f.get("measured").is_some())
                .map(|f| {
                    format!(
                        "{} {} (limit {})",
                        py_str(f.get("rule")),
                        py_str(f.get("measured")),
                        py_str(f.get("threshold"))
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            would_kill.push(json!({
                "agent": row["name"],
                "pid": row["pid"],
                "line": format!("would-kill {name} (pid {}, {reason})", row["pid"]),
                "findings": findings,
                "kill": policy.get("kill").is_some_and(truthy),
                "dry_run": policy.get("dry_run").cloned().unwrap_or(json!(true)),
            }));
        }
    }
    Ok(json!({
        "at": now,
        "elapsed_s": elapsed.filter(|e| *e != 0.0).map_or(Value::Null, |e| json!(round_to(e, 1))),
        "callers": rows,
        "skipped": current["skipped"],
        "host_load1": load1,
        "attributed_cores": attributed,
        "unattributed_cores": unattributed,
        "would_kill": would_kill,
    }))
}

#[cfg(test)]
mod tests;
