//! T43 printers, group "fleet" (t811, slice 3): `print_poolrad_sync`, `print_poolrad_progress`,
//! `print_review_sync`, `print_review_list`, `print_whoami`, `print_superuser` from
//! `dunkingsheep` (Python lines 1629-1709). Each is `render_x(&Value) -> String` (the exact
//! text the Python prints, trailing newline included) so it can be compared byte for byte.
//!
//! Python's f-strings print a missing or null field as `None`, a bool as `True`/`False`;
//! `py_str` does the same. LIMIT: where Python would raise `KeyError` (a required key
//! absent from a malformed payload) these print `None` instead. Float reprs are Python's
//! only for whole numbers and plain decimals (no `1e+16` forms).

use serde_json::Value;

fn get<'a>(v: &'a Value, key: &str) -> &'a Value {
    v.get(key).unwrap_or(&Value::Null)
}

/// Python truthiness of a JSON value.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn py_repr_string(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

fn py_float(f: f64) -> String {
    if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e16 {
        format!("{f:.1}")
    } else {
        format!("{f}")
    }
}

fn py_repr(v: &Value) -> String {
    match v {
        Value::String(s) => py_repr_string(s),
        Value::Array(a) => format!("[{}]", a.iter().map(py_repr).collect::<Vec<_>>().join(", ")),
        Value::Object(o) => format!(
            "{{{}}}",
            o.iter()
                .map(|(k, v)| format!("{}: {}", py_repr_string(k), py_repr(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => py_str(other),
    }
}

/// `str(value)` / what an f-string `{value}` prints.
pub fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => match n.as_i64() {
            Some(i) => i.to_string(),
            None => py_float(n.as_f64().unwrap_or(0.0)),
        },
        Value::String(s) => s.clone(),
        other => py_repr(other),
    }
}

/// `value or default`.
fn or(v: &Value, default: &str) -> String {
    if truthy(v) {
        py_str(v)
    } else {
        default.to_string()
    }
}

fn ljust(s: &str, width: usize) -> String {
    let n = s.chars().count();
    format!("{s}{}", " ".repeat(width.saturating_sub(n)))
}

fn items<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    get(v, key).as_array().map_or(&[], Vec::as_slice)
}

pub fn render_poolrad_sync(result: &Value) -> String {
    let skipped = get(result, "skipped");
    let mut out = format!(
        "base {}; queue rows {}, credited {}, already filed {}\n",
        or(get(result, "base"), "?"),
        py_str(get(skipped, "queue_rows")),
        py_str(get(skipped, "credited")),
        py_str(get(skipped, "already_filed")),
    );
    for item in items(result, "filed") {
        let task = if item.get("task").is_some() {
            py_str(get(item, "task"))
        } else {
            "(dry)".to_string()
        };
        out.push_str(&format!(
            "  filed {task} {} rank {}\n",
            py_str(get(item, "id")),
            py_str(get(item, "rank"))
        ));
    }
    for error in items(result, "errors") {
        out.push_str(&format!("  ! {}\n", py_str(error)));
    }
    out
}

pub fn render_poolrad_progress(result: &Value) -> String {
    let mut out = format!(
        "credited strict bodies: {}  identities: {}  CEILING {}\n",
        py_str(get(result, "credited")),
        py_str(get(result, "identities")),
        py_str(get(result, "ceiling"))
    );
    out.push_str(&format!("  {}\n", py_str(get(result, "ceiling_note"))));
    out.push_str(&format!(
        "  composable in queue: {} of {} candidates; main {}\n",
        py_str(get(result, "composable_bodies")),
        py_str(get(result, "queue_candidates")),
        py_str(get(result, "main"))
    ));
    if get(result, "replaced").is_null() {
        out.push_str(&format!(
            "  replaced: n/a ({})\n",
            py_str(get(result, "replaced_note"))
        ));
    }
    out
}

fn first_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

pub fn render_review_sync(result: &Value) -> String {
    let mut out = String::new();
    for repo in items(result, "repos") {
        out.push_str(&format!(
            "{}: {} open PR(s)\n",
            py_str(get(repo, "repo")),
            py_str(get(repo, "open"))
        ));
    }
    for item in items(result, "filed") {
        let task = if item.get("task").is_some() {
            py_str(get(item, "task"))
        } else {
            "(dry)".to_string()
        };
        out.push_str(&format!(
            "  filed {task} {}#{} @ {} author {}\n",
            py_str(get(item, "repo")),
            py_str(get(item, "pr")),
            first_chars(&py_str(get(item, "head")), 12),
            or(get(item, "author"), "?")
        ));
    }
    for task in items(result, "cancelled") {
        out.push_str(&format!("  cancelled {}\n", py_str(task)));
    }
    for status in items(result, "statuses") {
        out.push_str(&format!(
            "  status {}@{} {}\n",
            py_str(get(status, "repo")),
            py_str(get(status, "head")),
            py_str(get(status, "state"))
        ));
    }
    for error in items(result, "errors") {
        out.push_str(&format!("  ! {}\n", py_str(error)));
    }
    if !["filed", "cancelled", "errors"]
        .iter()
        .any(|k| truthy(get(result, k)))
    {
        out.push_str("  nothing to do\n");
    }
    out
}

pub fn render_review_list(result: &Value) -> String {
    let rows = items(result, "reviews");
    if rows.is_empty() {
        return "no live reviews\n".to_string();
    }
    let mut out = String::new();
    for row in rows {
        let who = {
            let names: Vec<String> = get(row, "reviewer")
                .as_array()
                .map(|a| a.iter().map(py_str).collect())
                .unwrap_or_default();
            if names.is_empty() {
                "-".to_string()
            } else {
                names.join(",")
            }
        };
        // `{row['age_s'] // 60}m`: floor division (Python floors toward -inf).
        let age = match get(row, "age_s") {
            Value::Number(n) if n.is_i64() => (n.as_i64().unwrap().div_euclid(60)).to_string(),
            // float // int stays a float in Python: 7200.5 // 60 == 120.0
            Value::Number(n) => py_float((n.as_f64().unwrap_or(0.0) / 60.0).floor()),
            _ => "None".to_string(),
        };
        out.push_str(&format!(
            "{} {} {}#{} @ {} author {} reviewer {} age {}m\n",
            ljust(&py_str(get(row, "task")), 6),
            ljust(&py_str(get(row, "status")), 11),
            py_str(get(row, "repo")),
            py_str(get(row, "pr")),
            py_str(get(row, "head")),
            ljust(&or(get(row, "author"), "?"), 8),
            ljust(&who, 10),
            age
        ));
    }
    out
}

pub fn render_whoami(result: &Value) -> String {
    let mut out = format!(
        "pane {}  role {}{}\n",
        or(get(result, "pane_id"), "-"),
        py_str(get(result, "role")),
        if truthy(get(result, "via_token")) {
            "  (service token)"
        } else {
            ""
        }
    );
    if truthy(get(result, "identity_note")) {
        out.push_str(&format!(
            "  identity: {}\n",
            py_str(get(result, "identity_note"))
        ));
    }
    if truthy(get(result, "binding_note")) {
        out.push_str(&format!(
            "  binding: {}\n",
            py_str(get(result, "binding_note"))
        ));
    }
    let reg = get(result, "registered");
    if truthy(reg) {
        out.push_str(&format!(
            "  registered: {} pid {} provider {} by {}\n",
            py_str(get(reg, "role")),
            py_str(get(reg, "pane_pid")),
            or(get(reg, "provider"), "-"),
            or(get(reg, "by"), "-")
        ));
    }
    out
}

pub fn render_superuser(result: &Value) -> String {
    let mut out = format!("roles {}\n", py_str(get(result, "path")));
    out.push_str(&format!(
        "  owner: {} (pane {}, role {})\n",
        if truthy(get(result, "superuser")) {
            "yes"
        } else {
            "no"
        },
        or(get(result, "pane_id"), "-"),
        py_str(get(result, "role"))
    ));
    let lanes = get(result, "lanes").as_object().filter(|o| !o.is_empty());
    match lanes {
        Some(lanes) => {
            let mut keys: Vec<&String> = lanes.keys().collect();
            keys.sort();
            for pane in keys {
                let entry = &lanes[pane];
                let line = format!(
                    "  {} {} pid {} {} {}",
                    ljust(pane, 8),
                    ljust(&py_str(get(entry, "role")), 9),
                    or(get(entry, "pane_pid"), "-"),
                    if truthy(get(entry, "provider")) {
                        py_str(get(entry, "provider"))
                    } else {
                        String::new()
                    },
                    if truthy(get(entry, "label")) {
                        py_str(get(entry, "label"))
                    } else {
                        String::new()
                    },
                );
                out.push_str(line.trim_end());
                out.push('\n');
            }
        }
        None => out.push_str("  (no lanes registered; `lane register --role owner` bootstraps)\n"),
    }
    let tokens = items(result, "tokens");
    if !tokens.is_empty() {
        let joined: Vec<String> = tokens
            .iter()
            .map(|t| format!("{}={}", py_str(get(t, "label")), py_str(get(t, "role"))))
            .collect();
        out.push_str(&format!("  tokens: {}\n", joined.join(", ")));
    }
    if result.get("added").is_some() {
        out.push_str(&format!("  added: {}\n", py_str(get(result, "added"))));
    }
    if result.get("removed").is_some() {
        out.push_str(&format!(
            "  removed: {} ({})\n",
            py_str(get(result, "removed")),
            py_str(get(result, "entry"))
        ));
    }
    if result.get("recovered").is_some() {
        out.push_str(&format!(
            "  recovered: {} ({})\n",
            py_str(get(result, "pane_id")),
            py_str(get(result, "name"))
        ));
    }
    out
}
