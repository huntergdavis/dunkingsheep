//! T53 (t1400 slice D): the host, health, retired-pane and money rules of `dunk_watchdog.py`
//! (`host.load`, `host.tmp`, `host.disk_full`, `host.heavy_slot_stale`, `health.failed`,
//! `pane.retired`, `money.spend_projection`) and the inputs they read (`sample_host`,
//! `_heavy_jobs`, `_slot_holder_alive`, `health.json`).
//!
//! The Python rules take `inputs`; here each rule takes the already-read value it judges (a
//! [`HostSample`], the `health.json` value, the retired records), so a test builds the fixture
//! directly and the evaluator (a later slice) wires the real readers.

use std::collections::BTreeMap;
use std::path::Path;

use regex::Regex;
use serde_json::{json, Value};

use super::{finding, minutes, py_float, py_number, SOURCE_RULE};
use crate::error::DunkError;
use crate::panes::Retired;

/// Pane id -> `agent_status`, as `Inputs.panes()` builds it.
pub type Panes = BTreeMap<String, String>;

/// `sample_host`: one host reading for a path.
#[derive(Debug, Clone, PartialEq)]
pub struct HostSample {
    pub load1: f64,
    /// `host.get("load5", host["load1"])`: a sampler that records no load5 falls back to load1.
    pub load5: Option<f64>,
    pub cpus: i64,
    pub used_fraction: f64,
    pub free_bytes: i64,
    pub inode_used_fraction: f64,
}

/// `sample_host(path)`: `statvfs` plus the load averages. Raises (here `Err`) when the path cannot be
/// read, like the Python.
///
/// `cpus` is `std::thread::available_parallelism()`, which honours affinity and cgroup limits where
/// Python's `os.cpu_count()` does not: they can differ inside a restricted container.
pub fn sample_host(path: &Path) -> Result<HostSample, DunkError> {
    let stat = nix::sys::statvfs::statvfs(path)
        .map_err(|error| DunkError::Refused(format!("{}: {error}", path.display())))?;
    let blocks = stat.blocks() as f64;
    let files = stat.files() as f64;
    let loadavg = std::fs::read_to_string("/proc/loadavg")?;
    let mut fields = loadavg
        .split_whitespace()
        .map(|f| f.parse::<f64>().unwrap_or(0.0));
    let (load1, load5) = (fields.next().unwrap_or(0.0), fields.next().unwrap_or(0.0));
    Ok(HostSample {
        load1,
        load5: Some(load5),
        cpus: std::thread::available_parallelism().map_or(1, |n| n.get() as i64),
        used_fraction: if blocks > 0.0 {
            (blocks - stat.blocks_free() as f64) / blocks
        } else {
            1.0
        },
        free_bytes: (stat.blocks_available() as u64 * stat.fragment_size() as u64) as i64,
        inode_used_fraction: if files > 0.0 {
            (files - stat.files_free() as f64) / files
        } else {
            0.0
        },
    })
}

/// `round(x, ndigits)` the way Python does it: the exact binary value, half to even.
fn round_to(value: f64, digits: usize) -> f64 {
    format!("{value:.digits$}").parse().unwrap_or(0.0)
}

fn cfg_f64(rule: &Value, key: &str) -> f64 {
    rule[key].as_f64().unwrap_or(0.0)
}

/// Is `pid` a live process? `Some(true)` alive or not ours (EPERM), `Some(false)` gone.
fn pid_alive(pid: i64) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    match kill(Pid::from_raw(pid as i32), None) {
        Ok(()) => true,
        Err(Errno::ESRCH) => false,
        // exists, just not ours; any other error is treated like Python's "no ProcessLookupError"
        Err(_) => true,
    }
}

/// `_heavy_jobs`: heavy slot holders under `tmp` (`/tmp` in production). Best effort: an unreadable
/// directory reports 0 rather than breaking the rule.
pub fn heavy_jobs(tmp: &Path) -> i64 {
    let Ok(entries) = std::fs::read_dir(tmp) else {
        return 0;
    };
    let mut count = 0;
    for entry in entries.flatten() {
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with("poolrad-heavy.")
        {
            continue;
        }
        let holder = entry.path().join("pid");
        match std::fs::read_to_string(holder)
            .ok()
            .and_then(|text| text.trim().parse::<i64>().ok())
        {
            // an unreadable or malformed holder file counts as a job
            None => count += 1,
            Some(pid) => {
                if pid_alive(pid) {
                    count += 1;
                }
            }
        }
    }
    count
}

/// A band fitted to our own recorded work (2026-10-22), not to a guess.
///
/// Fit over 461 samples / 8.3 h of host-watch history, on load5/cpu:
///
/// ```text
/// load5/cpu = 0.49 + 0.190*busy + 0.336*heavy     R2 0.52, RMS 0.33
/// ```
///
/// `live` was 27 in every sample (nobody closed a pane all night), so its coefficient is
/// unidentifiable from this data. The band is set from the observed envelope of legitimate work, not
/// from the point prediction: the machine reached 2.79/cpu with four lanes and a heavy job, so a
/// threshold of 2.0/2.6 paged on our own productivity. warning 3.0/cpu, critical 3.5/cpu (2 sigma of
/// the fit residual above the observed maximum).
///
/// `panes` is `inputs.panes() or {}`: `None` (source disabled) and empty are the same here.
pub fn rule_host_load(
    host: &HostSample,
    panes: Option<&Panes>,
    heavy: i64,
    rule: &Value,
) -> Vec<Value> {
    let cpus = host.cpus.max(1);
    let empty = Panes::new();
    let panes = panes.unwrap_or(&empty);
    let live = panes.len();
    let busy = panes
        .values()
        .filter(|s| s.to_lowercase() == "working")
        .count();
    let warning = cfg_f64(rule, "load1_per_cpu");
    let critical = cfg_f64(rule, "critical_load1_per_cpu");
    // Judge on load5, not load1: a CRITICAL fired at load1 30.5 with busy 1 / heavy 0 and load1 was
    // 18.9 a minute later. A one-minute spike is the wrong measurement; load5 plus must_hold_runs
    // (consecutive spaced runs) is a persisted condition. The config keys still say load1_* for
    // compatibility; the value they hold is per-cpu load5.
    let load5 = host.load5.unwrap_or(host.load1);
    let per_cpu = load5 / cpus as f64;
    if per_cpu < warning {
        return Vec::new();
    }
    let level = if per_cpu >= critical {
        "critical"
    } else {
        "warning"
    };
    let mut item = finding(
        "host.load",
        rule,
        &format!("load5:{level}"),
        format!(
            "load5 {load5:.1} = {per_cpu:.2}/cpu, band {warning:.1}-{critical:.1}/cpu (fitted 2026-10-22), \
             {live} live / {busy} busy / {heavy} heavy"
        ),
        format!("{warning:.1}/cpu (load5 {:.1})", warning * cpus as f64),
        format!("{per_cpu:.2}/cpu (load5 {load5:.1})"),
        "load5 (5 min)".into(),
        json!({
            "load1": round_to(host.load1, 2), "load5": round_to(load5, 2), "cpus": cpus,
            "per_cpu": round_to(per_cpu, 2), "live": live, "busy": busy, "heavy": heavy,
            "warning_per_cpu": rule["load1_per_cpu"].clone(),
            "critical_per_cpu": rule["critical_load1_per_cpu"].clone(),
        }),
        SOURCE_RULE,
        false,
    );
    if level == "critical" {
        item["severity"] = json!("critical");
        item["threshold"] = json!(format!(
            "{critical:.1}/cpu (sustained {} runs)",
            py_number(&rule["critical_hold_runs"])
        ));
    }
    vec![item]
}

/// `host.tmp`: used fraction, free bytes and inode fraction of one path.
pub fn rule_host_tmp(host: &HostSample, rule: &Value) -> Vec<Value> {
    let path = rule["path"].as_str().unwrap_or("");
    let max_used = cfg_f64(rule, "max_used_fraction");
    let max_inode = cfg_f64(rule, "max_inode_fraction");
    let checks: [(&str, bool, String, String); 3] = [
        (
            "used_fraction",
            host.used_fraction >= max_used,
            py_float(round_to(host.used_fraction, 4)),
            py_number(&rule["max_used_fraction"]),
        ),
        (
            "free_bytes",
            (host.free_bytes as f64) <= cfg_f64(rule, "min_free_bytes"),
            // an int stays an int under `round(value, 4)`, and the unit follows
            format!("{}B", host.free_bytes),
            format!("{}B", py_number(&rule["min_free_bytes"])),
        ),
        (
            "inode_fraction",
            host.inode_used_fraction >= max_inode,
            py_float(round_to(host.inode_used_fraction, 4)),
            py_number(&rule["max_inode_fraction"]),
        ),
    ];
    let mut out = Vec::new();
    for (name, fired, measured, threshold) in checks {
        if !fired {
            continue;
        }
        let mut item = finding(
            "host.tmp",
            rule,
            &format!("{path}:{name}"),
            format!("{path} {name} breaches its limit"),
            threshold,
            measured,
            "current sample".into(),
            json!({"path": path, "metric": name}),
            SOURCE_RULE,
            false,
        );
        if name == "used_fraction" && host.used_fraction >= cfg_f64(rule, "critical_used_fraction")
        {
            // The 90-98% band is VP critical (VP i19821); >=98% is host.disk_full's.
            item["severity"] = json!("critical");
            item["threshold"] = json!(format!(
                "{} (critical)",
                py_number(&rule["critical_used_fraction"])
            ));
        }
        out.push(item);
    }
    out
}

/// `host.disk_full`: any configured path at or past `max_used_fraction`. `sample` reads one path
/// (`inputs.host(path)`), so a failure on any path is an `Err` like the Python's raise.
pub fn rule_host_disk_full(
    sample: &dyn Fn(&str) -> Result<HostSample, DunkError>,
    rule: &Value,
) -> Result<Vec<Value>, DunkError> {
    let mut paths: Vec<&str> = rule["paths"]
        .as_array()
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    paths.sort_unstable();
    let max_used = cfg_f64(rule, "max_used_fraction");
    let mut out = Vec::new();
    for path in paths {
        let host = sample(path)?;
        if host.used_fraction >= max_used {
            out.push(finding(
                "host.disk_full",
                rule,
                &format!("{path}:{}", py_number(&rule["max_used_fraction"])),
                format!(
                    "{path} is {}% full",
                    py_float(round_to(host.used_fraction * 100.0, 1))
                ),
                py_number(&rule["max_used_fraction"]),
                py_float(round_to(host.used_fraction, 4)),
                "current sample".into(),
                json!({"path": path}),
                SOURCE_RULE,
                false,
            ));
        }
    }
    Ok(out)
}

/// `_slot_holder_alive`: `Some(true)` for a live holder pid, `Some(false)` for a dead or unparseable
/// one, `None` for a legacy slot with no pid file.
fn slot_holder_alive(slot: &Path) -> Option<bool> {
    let raw = std::fs::read_to_string(slot.join("pid")).ok()?;
    let Ok(pid) = raw.trim().parse::<i64>() else {
        return Some(false);
    };
    if pid <= 0 {
        return Some(false);
    }
    Some(pid_alive(pid))
}

/// A poolrad-heavy slot is stale only when its holder is gone.
///
/// poolrad-heavy.sh records the holder pid in the slot. Age alone is not staleness: a legitimate
/// >30min compile kept its slot and the watchdog woke the VP for a healthy job (t31). Legacy slots
/// with no pid file fall back to the age threshold for one migration.
///
/// A SLOT IS `<prefix>.<digits>`, AND IT IS A FILE OR DIRECTORY, NEVER A SYMLINK: the live wrapper is
/// symlinked into /tmp as `/tmp/poolrad-heavy.sh` and a prefix match alone reported it as "legacy slot,
/// age 1981min" on every check-in. The whole name must match with a literal separator (reviewer's
/// correction on #33), and no trailing newline is accepted (`\Z`).
pub fn rule_host_heavy_slot_stale(rule: &Value, now: f64) -> Result<Vec<Value>, DunkError> {
    let directory = rule["directory"].as_str().unwrap_or("");
    let base = rule["prefix"].as_str().unwrap_or("").trim_end_matches('.');
    let max_age_minutes = cfg_f64(rule, "max_age_minutes");
    let pattern = Regex::new(&format!("^{}\\.[0-9]+$", regex::escape(base)))
        .map_err(|error| DunkError::Refused(error.to_string()))?;
    let mut names: Vec<String> = std::fs::read_dir(directory)?
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    let mut out = Vec::new();
    for name in names {
        // `$` without the multiline flag only matches at the very end, so "slot.1\n" is refused.
        if !pattern.is_match(&name) {
            continue;
        }
        let slot = Path::new(directory).join(&name);
        let link = std::fs::symlink_metadata(&slot);
        if link.as_ref().map_or(true, |m| m.file_type().is_symlink()) {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&slot) else {
            continue;
        };
        if !(meta.is_dir() || meta.is_file()) {
            continue;
        }
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0.0, |d| d.as_secs_f64());
        let age = now - mtime;
        let holder = slot_holder_alive(&slot);
        if holder == Some(true) {
            continue;
        }
        if holder.is_none() && age < max_age_minutes * 60.0 {
            continue;
        }
        let detail = if holder == Some(false) {
            "holder gone".to_string()
        } else {
            format!("legacy slot, age {}min", py_float(minutes(age)))
        };
        out.push(finding(
            "host.heavy_slot_stale",
            rule,
            &name,
            format!("heavy-job slot {name} is stale"),
            format!(
                "holder alive or <{}min",
                py_number(&rule["max_age_minutes"])
            ),
            detail,
            "holder liveness".into(),
            json!({"slot": slot.to_string_lossy(), "holder": holder}),
            SOURCE_RULE,
            false,
        ));
    }
    Ok(out)
}

/// Python's `str(value)` for the JSON scalar an f-string prints (`None` for null/absent).
fn py_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".into(),
        Some(Value::Bool(true)) => "True".into(),
        Some(Value::Bool(false)) => "False".into(),
        Some(Value::String(text)) => text.clone(),
        Some(other) => py_number(other),
    }
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
    }
}

/// `health.failed`: `health.json` is stale, or reports not-ok / problems. `health` is the parsed
/// file (`Inputs.health`, which raises when it is missing or malformed).
pub fn rule_health_failed(health: &Value, rule: &Value, now: f64) -> Vec<Value> {
    let mut out = Vec::new();
    // `float(health.get("checked_at") or 0)`
    let checked_at = if truthy(health.get("checked_at")) {
        health["checked_at"].as_f64().unwrap_or(0.0)
    } else {
        0.0
    };
    let age = now - checked_at;
    if age >= cfg_f64(rule, "stale_minutes") * 60.0 {
        out.push(finding(
            "health.failed",
            rule,
            "stale",
            "health.json is not being refreshed".into(),
            format!("{}min", py_number(&rule["stale_minutes"])),
            format!("{}min", py_float(minutes(age))),
            "since last check".into(),
            json!({}),
            SOURCE_RULE,
            false,
        ));
    }
    let problems = health.get("problems");
    // `health.get("ok", False)`: an absent key is False; a present null is falsy too.
    if !truthy(health.get("ok")) || truthy(problems) {
        let list: Vec<Value> = problems
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        out.push(finding(
            "health.failed",
            rule,
            "problems",
            "health check reports problems".into(),
            "ok=true and no problems".into(),
            format!(
                "ok={}, {} problem(s)",
                py_text(health.get("ok")),
                list.len()
            ),
            "last check".into(),
            json!({ "problems": list }),
            SOURCE_RULE,
            false,
        ));
    }
    out
}

/// Report how many panes are retired, in the info digest.
///
/// Retiring a pane stops it paging (its mail is the record of what was tried), but a silence nobody
/// sees is how a lane vanishes from the fleet unnoticed. So the count is stated every digest:
/// `retired panes: 2 (wW:p4, wW:p1)`.
pub fn rule_pane_retired(records: &[Retired], rule: &Value) -> Vec<Value> {
    if records.is_empty() {
        return Vec::new();
    }
    let names: Vec<&str> = records.iter().map(|r| r.pane_id.as_str()).collect();
    let reasons: Vec<String> = records
        .iter()
        .map(|r| format!("{}: {}", r.pane_id, r.reason))
        .collect();
    let by_pane: serde_json::Map<String, Value> = records
        .iter()
        .map(|r| (r.pane_id.clone(), json!(r.reason)))
        .collect();
    let mut item = finding(
        "pane.retired",
        rule,
        "count",
        format!("retired panes: {} ({})", records.len(), names.join(", ")),
        "informational".into(),
        format!("{} retired", records.len()),
        "current".into(),
        json!({"panes": names, "reasons": by_pane}),
        "digest",
        false,
    );
    item["severity"] = json!("info");
    item["measured"] = json!(reasons.join("; "));
    vec![item]
}

/// A rule that has no input source: reported SKIPPED, never a pass or a page (`RuleUnavailable`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleUnavailable(pub String);

impl std::fmt::Display for RuleUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// `money.spend_projection` has no metering reader: it always raises `RuleUnavailable`, with the
/// reason the Python gives (`source!r` for a configured source).
pub fn rule_money_spend_projection(rule: &Value) -> Result<Vec<Value>, RuleUnavailable> {
    let source = &rule["source"];
    if source.is_null() || rule["cap"].is_null() {
        return Err(RuleUnavailable("no metering source".into()));
    }
    let shown = source.as_str().map_or_else(
        || py_number(source),
        |s| {
            if s.contains('\'') && !s.contains('"') {
                format!("\"{s}\"")
            } else {
                format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
            }
        },
    );
    Err(RuleUnavailable(format!(
        "no metering reader implemented for source {shown}"
    )))
}

#[cfg(test)]
#[path = "rules_host_tests.rs"]
mod rules_host_tests;
