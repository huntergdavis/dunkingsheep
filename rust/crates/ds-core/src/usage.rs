//! Configurable real-usage runner (owner #1110), ported from `dunk_usage.py`.
//!
//! The usage config maps each provider to a command line, a timeout and a parse
//! spec. A command must print one canonical JSON object; it may exit non-zero for
//! an unknown/error state, and the runner surfaces that state instead of hiding
//! it. There is no cache: every call runs the command again.
//!
//! `max_age_s` is passed to the provider command as `DUNKINGSHEEP_USAGE_MAX_AGE_S`;
//! a provider that cannot honour it still returns its own status/reason.

#![forbid(unsafe_code)]

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::error::DunkError;

pub const SCHEMA: i64 = 1;

fn refuse<T>(message: String) -> Result<T, DunkError> {
    Err(DunkError::Refused(message))
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Python `shlex.split` (POSIX mode): whitespace splits, quotes group, `\` escapes.
pub fn shlex_split(text: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(x) => cur.push(x),
                        None => return Err("No closing quotation".into()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            // POSIX shlex unescapes ONLY `"` and `\` inside double quotes and keeps
                            // the backslash before anything else (`$`, a backtick): bash's rule is
                            // wider, and the difference changes what a pane would execute (t805
                            // review t1348).
                            Some(x @ ('"' | '\\')) => cur.push(x),
                            Some(x) => {
                                cur.push('\\');
                                cur.push(x);
                            }
                            None => return Err("No closing quotation".into()),
                        },
                        Some(x) => cur.push(x),
                        None => return Err("No closing quotation".into()),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match chars.next() {
                    Some(x) => cur.push(x),
                    None => return Err("No escaped character".into()),
                }
            }
            x => {
                in_word = true;
                cur.push(x);
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    Ok(words)
}

fn number(value: &Value, name: &str) -> Result<f64, DunkError> {
    match value {
        Value::Number(n) => Ok(n.as_f64().unwrap_or(0.0)),
        _ => refuse(format!("{name} must be a number")),
    }
}

fn expand_user(path: &str) -> String {
    match (path.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => path.to_string(),
    }
}

/// Load and validate the usage config; every message is the Python's.
pub fn load_config(path: &str) -> Result<Value, DunkError> {
    let path = expand_user(path);
    if !Path::new(&path).is_file() {
        return refuse(format!(
            "no usage config at {path}; copy packaging/usage.example.json"
        ));
    }
    let config: Value = std::fs::read_to_string(&path)
        .map_err(|e| e.to_string())
        .and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string()))
        .or_else(|e| refuse(format!("cannot read {path}: {e}")))?;
    if config.get("schema").and_then(Value::as_i64) != Some(SCHEMA) {
        return refuse(format!("{path}: schema must be {SCHEMA}"));
    }
    let providers = match config.get("providers").and_then(Value::as_object) {
        Some(p) if !p.is_empty() => p,
        _ => return refuse(format!("{path}: providers must be a non-empty object")),
    };
    for (name, spec) in providers {
        let Some(spec) = spec.as_object() else {
            return refuse(format!("{path}: providers.{name} must be an object"));
        };
        let command = spec.get("command").and_then(Value::as_str).unwrap_or("");
        if command.trim().is_empty() {
            return refuse(format!(
                "{path}: providers.{name}.command must be a command line"
            ));
        }
        if let Err(e) = shlex_split(command) {
            return refuse(format!("{path}: providers.{name}.command: {e}"));
        }
        let timeout = spec.get("timeout_s").cloned().unwrap_or(json!(30));
        if number(&timeout, &format!("providers.{name}.timeout_s"))? <= 0.0 {
            return refuse(format!("{path}: providers.{name}.timeout_s must be > 0"));
        }
        if let Some(age) = spec.get("max_age_s") {
            number(age, &format!("providers.{name}.max_age_s"))?;
        }
        if let Some(parse) = spec.get("parse") {
            let ok = parse
                .as_object()
                .is_some_and(|m| m.values().all(Value::is_string));
            if !ok {
                return refuse(format!(
                    "{path}: providers.{name}.parse must map names to dotted paths"
                ));
            }
        }
    }
    Ok(config)
}

/// Walk a dotted path through objects and list indexes; `None` is the Python's `_MISSING`.
fn path<'a>(value: &'a Value, dotted: &str) -> Option<&'a Value> {
    let mut cur = value;
    for part in dotted.split('.') {
        cur = match cur {
            Value::Array(a) => a.get(part.parse::<usize>().ok()?)?,
            Value::Object(o) => o.get(part)?,
            _ => return None,
        };
    }
    Some(cur)
}

fn error(name: &str, message: &str, extra: Value) -> Value {
    let mut out = json!({"provider": name, "status": "error", "meaning": "unknown",
                         "measured_at": now(), "error": message});
    if let (Some(o), Some(e)) = (out.as_object_mut(), extra.as_object()) {
        o.extend(e.clone());
    }
    out
}

fn clip(text: &str) -> String {
    text.chars().take(1000).collect()
}

/// Run one provider command and surface its state (no cache, no guessing).
pub fn run_provider(name: &str, spec: &Value, max_age: Option<f64>) -> Value {
    let command = spec["command"].as_str().unwrap_or("").to_string();
    let timeout = spec
        .get("timeout_s")
        .and_then(Value::as_f64)
        .unwrap_or(30.0);
    let age = max_age.unwrap_or_else(|| {
        spec.get("max_age_s")
            .and_then(Value::as_f64)
            .unwrap_or(3600.0)
    });
    let argv = shlex_split(&command).unwrap_or_default();
    let Some((program, args)) = argv.split_first() else {
        return error(
            name,
            "cannot run command: empty command",
            json!({"command": command}),
        );
    };
    let child = Command::new(program)
        .args(args)
        .env("DUNKINGSHEEP_USAGE_MAX_AGE_S", format!("{age}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            return error(
                name,
                &format!("cannot run command: {e}"),
                json!({"command": command}),
            )
        }
    };
    // Drain both pipes on threads so a chatty child cannot block on a full pipe.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            String::from_utf8_lossy(&buf).into_owned()
        })
    };
    let out_t = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let err_t = drain(child.stderr.take().map(|p| Box::new(p) as _));
    let deadline = Instant::now() + Duration::from_secs_f64(timeout);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return error(
                    name,
                    &format!("command timed out after {timeout}s"),
                    json!({"command": command}),
                );
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(e) => {
                return error(
                    name,
                    &format!("cannot run command: {e}"),
                    json!({"command": command}),
                )
            }
        }
    };
    let stdout = out_t.join().unwrap_or_default();
    let stderr = err_t.join().unwrap_or_default();
    let (stdout, stderr) = (stdout.trim(), stderr.trim());
    let code = crate::usage_providers::return_code(&status);
    let parsed: Option<Map<String, Value>> =
        serde_json::from_str::<Value>(stdout).ok().and_then(|v| {
            if let Value::Object(m) = v {
                Some(m)
            } else {
                None
            }
        });
    let Some(mut parsed) = parsed else {
        return error(
            name,
            &format!("command exited {code} without canonical JSON"),
            json!({"command": command, "stderr": clip(stderr), "exit_code": code}),
        );
    };
    parsed.entry("provider").or_insert_with(|| json!(name));
    parsed.entry("measured_at").or_insert_with(|| json!(now()));
    parsed.entry("meaning").or_insert_with(|| json!("unknown"));
    parsed
        .entry("status")
        .or_insert_with(|| json!(if code != 0 { "error" } else { "unknown" }));
    parsed.insert("exit_code".into(), json!(code));
    if !stderr.is_empty() && parsed["status"] != json!("ok") {
        parsed.insert("stderr".into(), json!(clip(stderr)));
    }
    let mut result = Value::Object(parsed);
    if let Some(parse) = spec.get("parse").and_then(Value::as_object) {
        for (label, dotted) in parse {
            let dotted = dotted.as_str().unwrap_or("");
            match path(&result, dotted).cloned() {
                None => {
                    // A successful reading must carry every parsed field; an
                    // unknown/error result may omit value fields and keeps its reason.
                    if result["status"] == json!("ok") {
                        return error(
                            name,
                            &format!("parse {label}: path '{dotted}' is missing"),
                            json!({"command": command, "stderr": clip(stderr),
                                   "exit_code": code}),
                        );
                    }
                }
                Some(v) => {
                    let o = result.as_object_mut().expect("object");
                    o.entry("parsed")
                        .or_insert_with(|| json!({}))
                        .as_object_mut()
                        .expect("object")
                        .insert(label.clone(), v);
                }
            }
        }
    }
    result
}

/// Run every provider, or only `only`; an unknown name is a clear error.
pub fn run(
    config: &Value,
    only: Option<&str>,
    max_age: Option<f64>,
) -> Result<Vec<Value>, DunkError> {
    let providers = config["providers"].as_object().cloned().unwrap_or_default();
    let names: Vec<&String> = match only {
        Some(o) => {
            if !providers.contains_key(o) {
                let known: Vec<&str> = providers.keys().map(String::as_str).collect();
                return refuse(format!(
                    "unknown provider '{o}'; known: {}",
                    known.join(", ")
                ));
            }
            providers.keys().filter(|k| k.as_str() == o).collect()
        }
        None => providers.keys().collect(),
    };
    Ok(names
        .into_iter()
        .map(|n| run_provider(n, &providers[n], max_age))
        .collect())
}

fn fmt1(v: &Value) -> String {
    format!("{:.1}", v.as_f64().unwrap_or(0.0))
}

fn windows(reading: &Value) -> String {
    let mut parts = Vec::new();
    for w in reading["windows"].as_array().into_iter().flatten() {
        let name = match &w["name"] {
            Value::String(s) => s.clone(),
            Value::Null => "None".into(),
            v => v.to_string(),
        };
        let mut text = format!("{name}: ");
        if !w["percent_used"].is_null() {
            text += &format!("{}% used", fmt1(&w["percent_used"]));
        }
        if !w["percent_remaining"].is_null() {
            text += &format!(" / {}% left", fmt1(&w["percent_remaining"]));
        }
        if let Some(r) = w["resets_at"].as_str().filter(|s| !s.is_empty()) {
            text += &format!(" (resets {r})");
        }
        parts.push(text);
    }
    parts.join("; ")
}

/// Python's `repr(float)`: shortest round-trip digits, fixed notation for exponents in
/// `-4 ..= 15`, otherwise `1e-07` / `1e+16` (sign and at least two exponent digits).
pub fn py_float_repr(value: f64) -> String {
    if value.is_nan() {
        return "nan".into();
    }
    if value.is_infinite() {
        return if value < 0.0 { "-inf" } else { "inf" }.into();
    }
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0"
        } else {
            "0.0"
        }
        .into();
    }
    let exponent = {
        let sci = format!("{value:e}");
        sci.split_once('e')
            .and_then(|(_, e)| e.parse::<i32>().ok())
            .unwrap_or(0)
    };
    if (-4..16).contains(&exponent) {
        let text = format!("{value}");
        if text.contains('.') {
            text
        } else {
            format!("{text}.0")
        }
    } else {
        let sci = format!("{value:e}");
        let (mantissa, _) = sci.split_once('e').unwrap_or((&sci, "0"));
        format!(
            "{mantissa}e{}{:02}",
            if exponent < 0 { '-' } else { '+' },
            exponent.abs()
        )
    }
}

/// `json.dumps(value, sort_keys=True)`: ", " and ": " separators, keys sorted at every
/// depth, non-ASCII escaped as `\uXXXX`, floats as Python prints them.
pub fn dumps_sorted(value: &Value) -> String {
    fn walk(value: &Value, out: &mut String) {
        match value {
            Value::Null => out.push_str("null"),
            Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
            Value::Number(n) => match n.as_f64().filter(|_| n.is_f64()) {
                Some(f) => out.push_str(&py_float_repr(f)),
                None => out.push_str(&n.to_string()),
            },
            Value::String(text) => {
                out.push('"');
                for c in text.chars() {
                    match c {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        '\n' => out.push_str("\\n"),
                        '\r' => out.push_str("\\r"),
                        '\t' => out.push_str("\\t"),
                        '\u{8}' => out.push_str("\\b"),
                        '\u{c}' => out.push_str("\\f"),
                        ' '..='\u{7f}' => out.push(c),
                        other => {
                            let mut units = [0u16; 2];
                            for unit in other.encode_utf16(&mut units) {
                                out.push_str(&format!("\\u{unit:04x}"));
                            }
                        }
                    }
                }
                out.push('"');
            }
            Value::Array(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    walk(item, out);
                }
                out.push(']');
            }
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                out.push('{');
                for (i, key) in keys.iter().enumerate() {
                    if i > 0 {
                        out.push_str(", ");
                    }
                    walk(&Value::String((*key).clone()), out);
                    out.push_str(": ");
                    walk(&map[*key], out);
                }
                out.push('}');
            }
        }
    }
    let mut out = String::new();
    walk(value, &mut out);
    out
}

/// Python's `str()` of a JSON value as an f-string interpolates it.
fn show(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(n) if n.is_f64() => n.as_f64().map(py_float_repr).unwrap_or_default(),
        Value::Number(n) => n.to_string(),
        other => dumps_sorted(other),
    }
}

fn take(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// The operator table. Unknown stays unknown: a failed command prints UNKNOWN + why.
pub fn render_text(results: &[Value]) -> String {
    let mut lines = vec![format!(
        "{:<10} {:<8} {:<22} READING",
        "PROVIDER", "STATUS", "MEANING"
    )];
    for r in results {
        let status = r["status"].as_str().unwrap_or("error");
        let body = if status == "ok" {
            if r["meaning"] == json!("percent_used")
                && r["windows"].as_array().is_some_and(|a| !a.is_empty())
            {
                windows(r)
            } else if r["meaning"] == json!("local_session_estimate") {
                format!(
                    "${} session; {} ctx tokens; {} messages (local; not plan quota)",
                    show(&r["session_cost_usd"]),
                    show(&r["last_context_tokens"]),
                    show(&r["session_messages_with_usage"])
                )
            } else {
                let skip = ["provider", "status", "meaning", "measured_at", "parsed"];
                let rest: Map<String, Value> = r
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter(|(k, _)| !skip.contains(&k.as_str()))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                take(&dumps_sorted(&Value::Object(rest)), 220)
            }
        } else if status == "unknown" {
            let reason = ["reason", "error"]
                .iter()
                .filter_map(|k| r[*k].as_str().filter(|s| !s.is_empty()))
                .next()
                .unwrap_or("no reading available");
            let mut b = format!("UNKNOWN: {reason}");
            if !r["last_snapshot"]["percent_used"].is_null() {
                b += &format!(
                    " | last snapshot {}% used",
                    show(&r["last_snapshot"]["percent_used"])
                );
            }
            b
        } else {
            let mut b = format!(
                "UNKNOWN: {}",
                r["error"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .unwrap_or("command failed")
            );
            if let Some(s) = r["stderr"].as_str().filter(|s| !s.is_empty()) {
                b += &format!(" | stderr: {}", take(s, 200));
            }
            b
        };
        lines.push(format!(
            "{:<10} {:<8} {:<22} {}",
            show(&r["provider"]),
            status,
            show(&r["meaning"]),
            body
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
#[path = "usage_tests.rs"]
mod tests;
