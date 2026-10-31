//! One real usage reading per provider (owner #1110), ported from `tools/usage_providers.py`.
//!
//! Every reader returns ONE JSON object with `status` ok|unknown|error. No cache, no
//! estimate dressed as a reading: a stale snapshot is `unknown` with a named reason.
//! `claude` (needs an HTTPS client; no new crate allowed) is not ported here and
//! reports a named `error` rather than a guess.

#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

use regex::Regex;
use serde_json::{json, Value};

fn err(provider: &str, message: &str, now: f64) -> Value {
    json!({"provider": provider, "status": "error", "measured_at": now,
           "meaning": "unknown", "error": message})
}

/// Epoch seconds from a number or an ISO-8601 string.
pub fn epoch(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => chrono::DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|d| d.timestamp_millis() as f64 / 1000.0),
        _ => None,
    }
}

/// First `rate_limits` object found anywhere in the event.
pub fn find_rate_limits(value: &Value) -> Option<Value> {
    match value {
        Value::Object(o) => {
            if let Some(r) = o.get("rate_limits").filter(|r| r.is_object()) {
                return Some(r.clone());
            }
            o.values().find_map(find_rate_limits)
        }
        Value::Array(a) => a.iter().find_map(find_rate_limits),
        _ => None,
    }
}

fn mtime(path: &Path) -> f64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// The newest `n` `*.jsonl` files under `dir`, newest first (Python's glob `**`).
pub fn newest_jsonl(dir: &Path, n: usize) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "jsonl") {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    walk(dir, &mut files);
    files.sort_by(|a, b| mtime(b).total_cmp(&mtime(a)));
    files.truncate(n);
    files
}

/// The last rate_limits event in a rollout file (read whole; scanned backwards).
pub fn last_rate_limits(path: &Path) -> Option<Value> {
    let text = String::from_utf8_lossy(&fs::read(path).ok()?).into_owned();
    for line in text.lines().rev() {
        if !line.contains("rate_limits") {
            continue;
        }
        let Ok(mut event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(limits) = find_rate_limits(&event) {
            event["rate_limits"] = limits;
            return Some(event);
        }
    }
    None
}

fn rel(path: &Path, home: &Path) -> String {
    path.strip_prefix(home)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

/// Codex weekly limit from the newest rate_limits event; stale => unknown.
pub fn codex(home: &Path, max_age_s: f64, now: f64) -> Value {
    let mut best: Option<(f64, Value, Value, PathBuf)> = None;
    for path in newest_jsonl(&home.join(".codex/sessions"), 8) {
        let Some(event) = last_rate_limits(&path) else {
            continue;
        };
        let limits = event["rate_limits"].clone();
        let primary = limits["primary"].clone();
        if !primary["used_percent"].is_number() {
            continue;
        }
        let measured = epoch(&event["timestamp"]).unwrap_or_else(|| mtime(&path));
        if best.as_ref().is_none_or(|b| measured > b.0) {
            best = Some((measured, limits, primary, path));
        }
    }
    let Some((measured, limits, primary, path)) = best else {
        return err(
            "codex",
            "no Codex rate_limits snapshot in ~/.codex/sessions",
            now,
        );
    };
    let age = now - measured;
    let used = primary["used_percent"].as_f64().unwrap_or(0.0);
    let window = primary["window_minutes"].clone();
    let name = if window == json!(10080) {
        "weekly".to_string()
    } else {
        format!(
            "{}min",
            window.as_i64().map_or("null".into(), |w| w.to_string())
        )
    };
    let credits = limits["credits"]["balance"].clone();
    if age > max_age_s {
        return json!({"provider": "codex", "status": "unknown", "meaning": "percent_used",
            "measured_at": measured, "source": rel(&path, home),
            "reason": format!("latest Codex snapshot is {:.0} min old (max {:.0}); no live account endpoint",
                              age / 60.0, max_age_s / 60.0),
            "last_snapshot": {"percent_used": used, "window_minutes": window,
                "resets_at": primary["resets_at"], "credits": credits,
                "plan_type": limits["plan_type"]}});
    }
    json!({"provider": "codex", "status": "ok", "meaning": "percent_used",
        "measured_at": measured, "source": rel(&path, home), "snapshot_age_s": round1(age),
        "windows": [{"name": name, "percent_used": used,
            "percent_remaining": round1(100.0 - used), "resets_at": primary["resets_at"],
            "window_minutes": window}],
        "credits": credits, "plan_type": limits["plan_type"]})
}

fn walk_usage<'a>(value: &'a Value, out: &mut Vec<&'a Value>) {
    match value {
        Value::Object(o) => {
            if let Some(u) = o.get("usage").filter(|u| u.is_object()) {
                out.push(u);
            }
            o.values().for_each(|c| walk_usage(c, out));
        }
        Value::Array(a) => a.iter().for_each(|c| walk_usage(c, out)),
        _ => {}
    }
}

/// Local pi session token/cost (NOT plan quota); stale => unknown with a reason.
pub fn pi(home: &Path, max_age_s: f64, now: f64) -> Value {
    let files = newest_jsonl(&home.join(".pi/agent/sessions"), 3);
    let Some(path) = files.first() else {
        return err("pi", "no pi session JSONL under ~/.pi/agent/sessions", now);
    };
    let text = match fs::read(path) {
        Ok(b) => String::from_utf8_lossy(&b).into_owned(),
        Err(e) => return err("pi", &format!("cannot read pi session: {e}"), now),
    };
    let (mut cost, mut inp, mut out, mut cr, mut cw, mut count) =
        (0.0, 0i64, 0i64, 0i64, 0i64, 0i64);
    let (mut last, mut last_ts): (Option<Value>, Option<f64>) = (None, None);
    for line in text.lines().filter(|l| l.contains("\"usage\"")) {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let mut usages = Vec::new();
        walk_usage(&event, &mut usages);
        for u in usages {
            count += 1;
            cost += u["cost"]["total"].as_f64().unwrap_or(0.0);
            let n = |k: &str| u[k].as_f64().unwrap_or(0.0) as i64;
            inp += n("input");
            out += n("output");
            cr += n("cacheRead");
            cw += n("cacheWrite");
            last = Some(u.clone());
            last_ts = epoch(&event["timestamp"]);
        }
    }
    let Some(last) = last.filter(|_| count > 0) else {
        return err("pi", "no usage records in the latest pi session", now);
    };
    let measured = last_ts.unwrap_or_else(|| mtime(path));
    let age = now - measured;
    let mut r = json!({"provider": "pi",
        "status": if age <= max_age_s { "ok" } else { "unknown" },
        "meaning": "local_session_estimate", "measured_at": measured,
        "source": rel(path, home), "session_messages_with_usage": count,
        "session_cost_usd": (cost * 1e6).round() / 1e6,
        "input_tokens": inp, "output_tokens": out,
        "cache_read_tokens": cr, "cache_write_tokens": cw,
        "last_context_tokens": last["totalTokens"], "last_cost_usd": last["cost"]["total"],
        "notes": ["local pi session token/cost; not the OpenCode Go plan quota"]});
    if age > max_age_s {
        r["reason"] = json!(format!(
            "latest pi usage is {:.0} min old (max {:.0})",
            age / 60.0,
            max_age_s / 60.0
        ));
    }
    r
}

/// `Label  $1.25 / 36.9K / 2M` -> number, with K/M suffix scaling.
pub fn stat(text: &str, label: &str) -> Option<f64> {
    let re = Regex::new(&format!(
        r"{}\s+\$?([0-9]+(?:\.[0-9]+)?)\s*([KM])?",
        regex::escape(label)
    ))
    .ok()?;
    let c = re.captures(text)?;
    let v: f64 = c[1].parse().ok()?;
    Some(match c.get(2).map(|m| m.as_str()) {
        Some("K") => v * 1e3,
        Some("M") => v * 1e6,
        _ => v,
    })
}

/// The honest OpenCode Go answer: local stats as context, plan quota UNKNOWN.
pub fn opencode_from_stats(stdout: &str, now: f64) -> Value {
    let clean = Regex::new(r"\x1b\[[0-9;]*m")
        .expect("static regex")
        .replace_all(stdout, "");
    let s = |l: &str| stat(&clean, l);
    json!({"provider": "opencode-go", "status": "unknown", "meaning": "unknown",
        "measured_at": now, "source": "opencode stats --days 1",
        "reason": "opencode stats reports local token/cost, not account quota; no documented OpenCode Go usage endpoint was found",
        "local": {"sessions": s("Sessions"), "messages": s("Messages"),
            "total_cost_usd": s("Total Cost"), "input_tokens": s("Input"),
            "output_tokens": s("Output"), "cache_read_tokens": s("Cache Read"),
            "cache_write_tokens": s("Cache Write")}})
}

/// Run `opencode stats --days 1`; a missing binary or non-zero exit is a named error.
pub fn opencode(now: f64) -> Value {
    match Command::new("opencode")
        .args(["stats", "--days", "1"])
        .output()
    {
        Err(e) => err("opencode", &format!("opencode stats failed: {e}"), now),
        Ok(o) if !o.status.success() => err(
            "opencode",
            &format!(
                "opencode stats exited {}: {}",
                o.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&o.stderr)
                    .trim()
                    .chars()
                    .take(300)
                    .collect::<String>()
            ),
            now,
        ),
        Ok(o) => opencode_from_stats(&String::from_utf8_lossy(&o.stdout), now),
    }
}

/// Claude needs HTTPS (no client crate is approved): a named error, never a guess.
pub fn claude(now: f64) -> Value {
    err(
        "claude",
        "claude probe not ported to Rust yet (needs an HTTPS client crate)",
        now,
    )
}

#[cfg(test)]
#[path = "usage_providers_tests.rs"]
mod tests;
