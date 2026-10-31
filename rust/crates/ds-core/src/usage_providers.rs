//! One real usage reading per provider (owner #1110), ported from `tools/usage_providers.py`.
//!
//! Every reader returns ONE JSON object with `status` ok|unknown|error. No cache, no
//! estimate dressed as a reading: a stale snapshot is `unknown` with a named reason.
//! `claude` (needs an HTTPS client; no new crate allowed) is not ported here and
//! reports a named `error` rather than a guess.

#![forbid(unsafe_code)]

use std::fs;
use std::io::Read;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, UNIX_EPOCH};

use regex::Regex;
use serde_json::{json, Value};

fn err(provider: &str, message: &str, now: f64) -> Value {
    json!({"provider": provider, "status": "error", "measured_at": now,
           "meaning": "unknown", "error": message})
}

/// Epoch seconds from a number or an ISO-8601 string (Python `_epoch`).
///
/// `datetime.fromisoformat(str(value).replace("Z", "+00:00")).timestamp()`: an offset is
/// honoured, and a NAIVE timestamp is read in the machine's local zone, as Python does.
/// Date-only and space-separated forms parse too (Python 3.11+). Anything else is `None`.
pub fn epoch(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => parse_iso(s),
        _ => None,
    }
}

fn parse_iso(text: &str) -> Option<f64> {
    use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone};
    let text = text.trim().replace('Z', "+00:00");
    if let Ok(d) = chrono::DateTime::parse_from_rfc3339(&text) {
        return Some(d.timestamp_micros() as f64 / 1e6);
    }
    // `2026-10-31 12:00:00+00:00` (space) and `...T...` with an offset but no `:` colon etc.
    for fmt in [
        "%Y-%m-%d %H:%M:%S%.f%:z",
        "%Y-%m-%dT%H:%M:%S%.f%z",
        "%Y-%m-%d %H:%M:%S%.f%z",
    ] {
        if let Ok(d) = chrono::DateTime::parse_from_str(&text, fmt) {
            return Some(d.timestamp_micros() as f64 / 1e6);
        }
    }
    let naive = [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ]
    .iter()
    .find_map(|fmt| NaiveDateTime::parse_from_str(&text, fmt).ok())
    .or_else(|| {
        NaiveDate::parse_from_str(&text, "%Y-%m-%d")
            .ok()
            .and_then(|d| d.and_hms_opt(0, 0, 0))
    })?;
    let local = Local.from_local_datetime(&naive).earliest()?;
    Some(local.timestamp_micros() as f64 / 1e6)
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
            // Python's glob `**` and `*` never match a dot-file or dot-directory.
            if e.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
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
        // Python: `_epoch(...) or getmtime(path)`: a zero timestamp is as missing as None.
        let measured = epoch(&event["timestamp"])
            .filter(|t| *t != 0.0)
            .unwrap_or_else(|| mtime(&path));
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
    let name = window_name(&window);
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

/// `"weekly" if window == 10080 else f"{window}min"`: Python compares numerically (a float
/// 10080.0 is weekly) and formats `None` as `None`, a float with its `repr`.
fn window_name(window: &Value) -> String {
    if window.as_f64() == Some(10080.0) {
        return "weekly".to_string();
    }
    let shown = match window {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::String(s) => s.clone(),
        Value::Number(n) if n.is_f64() => format!("{:?}", n.as_f64().unwrap_or(0.0)),
        other => other.to_string(),
    };
    format!("{shown}min")
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

/// Python `returncode`: the exit code, or minus the signal that killed the child.
pub(crate) fn return_code(status: &std::process::ExitStatus) -> i32 {
    status
        .code()
        .or_else(|| status.signal().map(|s| -s))
        .unwrap_or(-1)
}

/// `subprocess.run(argv, capture_output=True, text=True, timeout=...)`: stdin closed, both
/// pipes drained on threads, the DIRECT child killed at the deadline (as Python does).
fn run_captured(
    program: &str,
    args: &[&str],
    timeout_s: f64,
) -> Result<(i32, String, String), String> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            String::from_utf8_lossy(&buf).into_owned()
        })
    };
    let out = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let err = drain(child.stderr.take().map(|p| Box::new(p) as _));
    let deadline = Instant::now() + Duration::from_secs_f64(timeout_s);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let quoted: Vec<String> = std::iter::once(program)
                    .chain(args.iter().copied())
                    .map(|a| format!("'{a}'"))
                    .collect();
                return Err(format!(
                    "Command '[{}]' timed out after {timeout_s} seconds",
                    quoted.join(", ")
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => return Err(e.to_string()),
        }
    };
    Ok((
        return_code(&status),
        out.join().unwrap_or_default(),
        err.join().unwrap_or_default(),
    ))
}

/// Run `opencode stats --days 1` (30 s timeout, as Python); a missing binary, a timeout or a
/// non-zero exit is a named error.
pub fn opencode(now: f64) -> Value {
    opencode_with("opencode", 30.0, now)
}

fn opencode_with(program: &str, timeout_s: f64, now: f64) -> Value {
    match run_captured(program, &["stats", "--days", "1"], timeout_s) {
        Err(e) => err("opencode", &format!("opencode stats failed: {e}"), now),
        Ok((code, _, stderr)) if code != 0 => err(
            "opencode",
            &format!(
                "opencode stats exited {code}: {}",
                stderr.trim().chars().take(300).collect::<String>()
            ),
            now,
        ),
        Ok((_, stdout, _)) => opencode_from_stats(&stdout, now),
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
