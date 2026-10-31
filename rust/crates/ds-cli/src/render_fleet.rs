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

// ---- slice 3b: printers with no clock, no json.dumps and no task-row dependency -----------

/// `_clip(text, width)`: `…` replaces the tail when longer (Python lines 1009-1011).
pub fn clip(v: &Value, width: usize) -> String {
    let text = if v.is_null() {
        String::new()
    } else {
        py_str(v)
    };
    if text.chars().count() <= width {
        text
    } else {
        format!("{}…", first_chars(&text, width - 1))
    }
}

fn rjust(s: &str, width: usize) -> String {
    let n = s.chars().count();
    format!("{}{s}", " ".repeat(width.saturating_sub(n)))
}

/// `d.get(key, default)`: the default only when the key is ABSENT (a present null stays None).
fn get_or(v: &Value, key: &str, default: Value) -> Value {
    v.get(key).cloned().unwrap_or(default)
}

fn yes_no(b: bool, yes: &str, no: &str) -> String {
    (if b { yes } else { no }).to_string()
}

pub fn render_message(result: &Value) -> String {
    let (label, pane) = (
        py_str(get(result, "target_label")),
        py_str(get(result, "pane_id")),
    );
    if truthy(get(result, "queued")) {
        format!(
            "{} queued for {label} ({pane}): someone is typing\n",
            py_str(get(result, "message_id"))
        )
    } else if truthy(get(result, "ok")) {
        format!("sent to {label} ({pane})\n")
    } else {
        format!(
            "FAILED for {label} ({pane}): {}\n",
            py_str(get(result, "message"))
        )
    }
}

pub fn render_capability(result: &Value) -> String {
    let mut out = format!(
        "{}  mailbox={}  declared={}  agent={}\n",
        py_str(get(result, "pane_id")),
        py_repr(get(result, "mailbox")),
        yes_no(truthy(get(result, "declared")), "yes", "-"),
        or(get(result, "agent"), "-")
    );
    out.push_str(&format!(
        "  drains={}  send={}  evidence={}\n",
        yes_no(truthy(get(result, "drains")), "yes", "no"),
        py_str(get(result, "send")),
        or(get(result, "drain_evidence"), "-")
    ));
    out.push_str(&format!("  {}\n", py_str(get(result, "expect"))));
    if !get(result, "desk_credential").is_null() {
        out.push_str(&format!(
            "  desk: credential={}  may type here={}\n",
            yes_no(truthy(get(result, "desk_credential")), "yes", "no"),
            yes_no(truthy(get(result, "desk_can_type")), "yes", "no")
        ));
    }
    out
}

pub fn render_lanes(result: &Value) -> String {
    let rows = items(result, "lanes");
    if rows.is_empty() {
        return "the lane registry is empty\n".to_string();
    }
    let mut out = format!(
        "{} {} {} ALIVE\n",
        ljust("PANE", 8),
        ljust("MAILBOX", 42),
        ljust("DECLARED", 9)
    );
    for row in rows {
        let declared = if truthy(get(row, "declared")) {
            "yes"
        } else {
            "-"
        };
        let alive = match get(row, "alive") {
            Value::Bool(true) => "yes",
            Value::Bool(false) => "NO",
            _ => "-",
        };
        let name = first_chars(&or(get(row, "mailbox"), "-"), 42);
        out.push_str(&format!(
            "{} {} {} {alive}\n",
            ljust(&py_str(get(row, "pane_id")), 8),
            ljust(&name, 42),
            ljust(declared, 9)
        ));
    }
    out
}

pub fn render_lane(result: &Value) -> String {
    format!(
        "{} is {} (declared)\n",
        py_str(get(result, "pane_id")),
        py_repr(get(result, "mailbox"))
    )
}

pub fn render_inbox_stats(r: &Value) -> String {
    let boxes = items(r, "mailboxes");
    if boxes.is_empty() {
        return "no mailboxes\n".to_string();
    }
    let mut out = format!(
        "{} {} {} {} {} OLDEST\n",
        ljust("MAILBOX", 30),
        rjust("UNREAD", 7),
        rjust("ARCH", 6),
        rjust("UNDELIV", 8),
        rjust("TOTAL", 7)
    );
    for b in boxes {
        let oldest = if truthy(get(b, "oldest_unread_s")) {
            format!("{}s", py_str(get(b, "oldest_unread_s")))
        } else {
            "-".to_string()
        };
        out.push_str(&format!(
            "{} {} {} {} {} {oldest}\n",
            ljust(&clip(get(b, "destination"), 30), 30),
            rjust(&py_str(get(b, "unread")), 7),
            rjust(&py_str(&get_or(b, "archived", Value::from(0))), 6),
            rjust(&py_str(&get_or(b, "undeliverable", Value::from(0))), 8),
            rjust(&py_str(get(b, "total")), 7)
        ));
    }
    out
}

pub fn render_owner_push(result: &Value) -> String {
    format!(
        "pushed owner message {} for {} (no inbox row, nothing typed)\n",
        py_str(get(result, "id")),
        py_str(get(result, "pane_id"))
    )
}

pub fn render_report_run(result: &Value) -> String {
    let mut out = format!(
        "report {}  (generated {})\n",
        py_str(get(result, "path")),
        py_str(get(result, "generated_at"))
    );
    let posted = get(result, "posted");
    if truthy(get(posted, "message_id")) {
        out.push_str(&format!(
            "posted to the owner mailbox as {} (transport {})\n",
            py_str(get(posted, "message_id")),
            or(get(posted, "transport"), "none")
        ));
    } else if truthy(get(posted, "error")) {
        out.push_str(&format!("NOT posted: {}\n", py_str(get(posted, "error"))));
    }
    for problem in items(result, "prose_rejected") {
        out.push_str(&format!("prose rejected: {}\n", py_str(problem)));
    }
    out
}

pub fn render_owner_reply(result: &Value) -> String {
    let reply = get(result, "reply");
    format!(
        "answered {} with {} on {} (question {} acked)\n",
        py_str(get(result, "question")),
        py_str(get(reply, "message_id")),
        or(get(result, "transport"), "no transport"),
        py_str(get(result, "acked"))
    )
}

pub fn render_clear(result: &Value) -> String {
    let label = if truthy(get(result, "target_label")) {
        py_str(get(result, "target_label"))
    } else {
        py_str(get(result, "pane_id"))
    };
    let pane = py_str(get(result, "pane_id"));
    if truthy(get(result, "flushed")) {
        let before = py_str(get(get(result, "before"), "state"));
        let after = py_str(get(get(result, "after"), "state"));
        let reason = if truthy(get(result, "reason")) {
            format!(" [{}]", py_str(get(result, "reason")))
        } else {
            String::new()
        };
        format!("cleared {label} ({pane}): {before} -> {after}{reason}\n")
    } else {
        format!(
            "nothing cleared on {label} ({pane}): {}\n",
            py_str(get(result, "reason"))
        )
    }
}

fn one_line(v: &Value) -> Value {
    let text = if truthy(v) { py_str(v) } else { String::new() };
    Value::String(text.replace('\n', " "))
}

pub fn render_messages(result: &Value) -> String {
    let queued = items(result, "queued");
    let recent = items(result, "recent");
    if queued.is_empty() && recent.is_empty() {
        return "no messages\n".to_string();
    }
    let mut out = String::new();
    if !queued.is_empty() {
        out.push_str(&format!(
            "{} {} {} {} TEXT\n",
            ljust("ID", 5),
            rjust("WAITING", 8),
            ljust("TARGET", 28),
            ljust("FROM", 10)
        ));
        for m in queued {
            out.push_str(&format!(
                "{} {} {} {} {}\n",
                ljust(&py_str(get(m, "message_id")), 5),
                rjust(&format!("{}s", py_str(get(m, "waiting_s"))), 8),
                ljust(&clip(get(m, "target_label"), 28), 28),
                ljust(&clip(get(m, "source"), 10), 10),
                clip(&one_line(get(m, "text")), 50)
            ));
            if truthy(get(m, "blocked_by")) {
                out.push_str(&format!(
                    "{:<5} {:>8} blocked by {}\n",
                    "",
                    "",
                    py_str(get(m, "blocked_by"))
                ));
            } else if truthy(get(m, "unblock_needed")) {
                out.push_str(&format!(
                    "{:<5} {:>8} dependencies satisfied - UNBLOCK to claim\n",
                    "", ""
                ));
            }
        }
    }
    if !recent.is_empty() {
        if !queued.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!(
            "{} {} {} {} TEXT\n",
            ljust("ID", 5),
            ljust("STATUS", 10),
            ljust("TARGET", 28),
            ljust("FROM", 10)
        ));
        for m in recent {
            out.push_str(&format!(
                "{} {} {} {} {}\n",
                ljust(&py_str(get(m, "message_id")), 5),
                ljust(&py_str(get(m, "status")), 10),
                ljust(&clip(get(m, "target_label"), 28), 28),
                ljust(&clip(get(m, "source"), 10), 10),
                clip(&one_line(get(m, "text")), 50)
            ));
        }
    }
    out
}

pub fn render_workspaces(result: &Value) -> String {
    let workspaces = items(result, "workspaces");
    if workspaces.is_empty() {
        return "no workspaces (is the terminal backend reachable? on the herdr rollback: is the herdr server running?)\n"
            .to_string();
    }
    let own = get(result, "self");
    let mut out = String::new();
    for ws in workspaces {
        let mark = if get(ws, "workspace_id") == get(own, "workspace_id") {
            "*"
        } else {
            " "
        };
        let tabs = items(ws, "tabs");
        let tab_count = get_or(ws, "tab_count", Value::from(tabs.len() as i64));
        out.push_str(&format!(
            "{mark}{} {} {} tabs  {}\n",
            ljust(&py_str(get(ws, "workspace_id")), 6),
            ljust(&clip(get(ws, "label"), 30), 30),
            rjust(&py_str(&tab_count), 3),
            or(get(ws, "agent_status"), "-")
        ));
        for tab in tabs {
            let mark = if get(tab, "tab_id") == get(own, "tab_id") {
                "*"
            } else {
                " "
            };
            out.push_str(&format!(
                "   {mark}{} {} {} panes {}\n",
                ljust(&py_str(get(tab, "tab_id")), 8),
                ljust(&clip(get(tab, "label"), 30), 30),
                rjust(&py_str(&get_or(tab, "pane_count", Value::from(0))), 3),
                or(get(tab, "agent_status"), "-")
            ));
        }
    }
    if truthy(own) {
        out.push_str("* = this pane's workspace/tab\n");
    }
    out
}

/// stdout half of `print_created` (its `error` goes to stderr, see `created_error`).
pub fn render_created(result: &Value) -> String {
    let (ws, tab) = (get(result, "workspace"), get(result, "tab"));
    let pane = get(result, "pane");
    let mut parts: Vec<String> = Vec::new();
    if truthy(ws) {
        parts.push(format!(
            "workspace {} {}",
            py_str(get(ws, "workspace_id")),
            py_repr(get(ws, "label"))
        ));
    }
    if truthy(tab) {
        parts.push(format!(
            "tab {} {}",
            py_str(get(tab, "tab_id")),
            py_repr(get(tab, "label"))
        ));
    }
    parts.push(format!("pane {}", py_str(get(pane, "pane_id"))));
    let mut line = format!("created {}", parts.join(", "));
    if truthy(get(result, "ran")) {
        line.push_str(&format!("; running {}", py_repr(get(result, "ran"))));
    }
    line.push('\n');
    line
}

/// The stderr line `print_created` adds when the result carries an error.
pub fn created_error(result: &Value) -> Option<String> {
    truthy(get(result, "error")).then(|| format!("{}\n", py_str(get(result, "error"))))
}

pub fn render_poll(result: &Value) -> String {
    let status = get(result, "status");
    let count = get(result, "inbox_count");
    let mut out = format!(
        "self {}  v{}  dunks {}/{} running  inbox {} unread / {}\n",
        or(get(status, "self_pane_id"), "?"),
        py_str(get(status, "version")),
        py_str(&get_or(status, "running", Value::from(0))),
        py_str(&get_or(status, "dunkers", Value::from(0))),
        py_str(&get_or(count, "unread", Value::from(0))),
        py_str(&get_or(count, "total", Value::from(0)))
    );
    if truthy(get(result, "error")) {
        out.push_str(&format!("  note: {}\n", py_str(get(result, "error"))));
    }
    let dunks = get(result, "dunks");
    for message in items(dunks, "messages").iter().take(5) {
        out.push_str(&format!(
            "  #{} {}: {}\n",
            py_str(get(message, "message_id")),
            py_str(get(message, "sender")),
            or(get(message, "subject"), "(no subject)")
        ));
    }
    for dunk in items(dunks, "dunks").iter().take(10) {
        let state = if truthy(get(dunk, "running")) {
            "running"
        } else {
            "stopped"
        };
        let target = if truthy(get(dunk, "target_label")) {
            py_str(get(dunk, "target_label"))
        } else {
            py_str(get(dunk, "target_pane_id"))
        };
        out.push_str(&format!(
            "  {} {state} every {}m -> {target}\n",
            py_str(get(dunk, "id")),
            py_str(get(dunk, "interval_minutes"))
        ));
    }
    out
}

pub fn render_turn_end(result: &Value) -> String {
    let mut line = format!(
        "turn end {} (ended={})",
        py_str(get(result, "pane_id")),
        py_str(get(result, "ended"))
    );
    let dispatch = get(result, "dispatch");
    if get(dispatch, "action").as_str() == Some("nudge") {
        let outcome = if truthy(get(dispatch, "delivered")) {
            "delivered".to_string()
        } else {
            format!("HELD, not delivered ({})", py_str(get(dispatch, "outcome")))
        };
        line.push_str(&format!(
            "; trigger=turn.end nudged {}: {outcome}",
            py_str(get(dispatch, "task_id"))
        ));
    } else if truthy(dispatch) {
        line.push_str(&format!(
            "; trigger=turn.end no nudge ({})",
            py_str(get(dispatch, "reason"))
        ));
    }
    line.push('\n');
    line
}

// ---- slice 3c: printers that read the local clock ---------------------------------------
//
// `time.strftime(fmt, time.localtime(ts))` is `local(ts, fmt)`: chrono's `Local`, which
// follows the host's TZ like Python does. Tests pin TZ explicitly. `now` is passed in so
// "Ns ago" is deterministic.

fn local(ts: &Value, fmt: &str) -> String {
    use chrono::TimeZone;
    let secs = ts.as_f64().unwrap_or(0.0).floor() as i64;
    match chrono::Local.timestamp_opt(secs, 0).single() {
        Some(t) => t.format(fmt).to_string(),
        None => String::new(),
    }
}

/// Python's `str.splitlines()[0]`: up to the first line boundary (`\n \r \v \f \x1c-\x1e
/// \x85    `); an empty text has no first line, which Python raises on, so "".
fn first_line(text: &str) -> &str {
    let end = text
        .find(|c: char| {
            matches!(
                c,
                '\n' | '\r' | '\x0b' | '\x0c' | '\x1c'
                    ..='\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}'
            )
        })
        .unwrap_or(text.len());
    &text[..end]
}

pub fn render_inbox_status(r: &Value) -> String {
    let mut out = String::new();
    for key in [
        "message_id",
        "state",
        "destination",
        "pane_id",
        "sender",
        "waiting_s",
        "body_bytes",
        "attachments",
        "attachment_bytes",
        "reason",
    ] {
        let v = get(r, key);
        if !(v.is_null() || v.as_str() == Some("")) {
            out.push_str(&format!("{} {}\n", ljust(key, 17), py_str(v)));
        }
    }
    for (key, label) in [
        ("created_at", "sent"),
        ("read_at", "read"),
        ("acked_at", "acked"),
        ("cancelled_at", "cancelled"),
    ] {
        let v = get(r, key);
        if truthy(v) {
            out.push_str(&format!(
                "{} {}\n",
                ljust(label, 17),
                local(v, "%Y-%m-%d %H:%M:%S")
            ));
        }
    }
    let notify = get(r, "notify");
    if truthy(get(notify, "state")) {
        let reason = if truthy(get(notify, "reason")) {
            format!(" ({})", py_str(get(notify, "reason")))
        } else {
            String::new()
        };
        out.push_str(&format!(
            "{} {}{reason}\n",
            ljust("notify", 17),
            py_str(get(notify, "state"))
        ));
    }
    out
}

pub fn render_retired(result: &Value) -> String {
    if result.get("retired").is_some() {
        let records = items(result, "retired");
        if records.is_empty() {
            return "no retired panes\n".to_string();
        }
        let mut out = format!("{} {} REASON\n", ljust("PANE", 10), ljust("RETIRED", 20));
        for record in records {
            let stamp = local(get(record, "retired_at"), "%Y-%m-%d %H:%M");
            out.push_str(&format!(
                "{} {} {}\n",
                ljust(&py_str(get(record, "pane_id")), 10),
                ljust(&stamp, 20),
                py_str(get(record, "reason"))
            ));
        }
        return out;
    }
    if result.get("unread_kept").is_some() {
        let kept = if !get(result, "unread_kept").is_null() {
            format!(
                "; {} unread message(s) kept as the record",
                py_str(get(result, "unread_kept"))
            )
        } else {
            String::new()
        };
        return format!(
            "retired {} ({}){kept}\n",
            py_str(get(result, "pane_id")),
            py_str(get(result, "reason"))
        );
    }
    format!("unretired {}\n", py_str(get(result, "pane_id")))
}

pub fn render_report_list(result: &Value) -> String {
    let mut out = format!(
        "{} report(s) in {}\n",
        py_str(&get_or(result, "count", Value::from(0))),
        py_str(get(result, "dir"))
    );
    for row in items(result, "reports") {
        out.push_str(&format!(
            "  {} {} B  {}\n",
            local(get(row, "at"), "%Y-%m-%d %H:%M"),
            rjust(&py_str(get(row, "size")), 8),
            py_str(get(row, "path"))
        ));
    }
    out
}

pub fn render_owner_conversation(result: &Value) -> String {
    let mut out = format!(
        "mailbox {}  messages {}  waiting {}\n",
        py_str(get(result, "mailbox")),
        py_str(&get_or(result, "count", Value::from(0))),
        items(result, "waiting").len()
    );
    for row in items(result, "messages") {
        let mark = if truthy(get(row, "reply_to")) {
            "->"
        } else if get(row, "state").as_str() == Some("queued") {
            "? "
        } else {
            "  "
        };
        let when = local(get(row, "created_at"), "%m-%d %H:%M");
        let body = if truthy(get(row, "body")) {
            py_str(get(row, "body"))
        } else {
            String::new()
        };
        let sender = or(get(row, "sender"), "-");
        out.push_str(&format!(
            "{mark} {} {when} {} {} {}\n",
            rjust(&py_str(get(row, "id")), 6),
            ljust(&or(get(row, "transport"), "-"), 8),
            ljust(&sender, 8),
            first_chars(first_line(&body), 90)
        ));
    }
    out
}

pub fn render_owner_take(result: &Value) -> String {
    let mut out = String::new();
    for message in items(result, "messages") {
        let subject = if truthy(get(message, "subject")) {
            format!("[{}] ", py_str(get(message, "subject")))
        } else {
            String::new()
        };
        out.push_str(&format!(
            "--- owner message {} at {}\n",
            py_str(get(message, "id")),
            local(get(message, "created_at"), "%H:%M:%S")
        ));
        out.push_str(&format!("{subject}{}\n", py_str(get(message, "body"))));
    }
    out
}

/// `print_status`; `now` is `time.time()` for the "Ns ago" part.
pub fn render_status(result: &Value, now: f64) -> String {
    let mut out = String::new();
    let (version, code) = (get(result, "version"), get(result, "code_hash"));
    if truthy(version) && truthy(code) {
        out.push_str(&format!(
            "{} {} (code {})\n",
            ljust("version", 16),
            py_str(version),
            py_str(code)
        ));
    } else if truthy(version) {
        out.push_str(&format!("{} {}\n", ljust("version", 16), py_str(version)));
    }
    for key in [
        "code_mtime",
        "started_at",
        "code_stale",
        "pid",
        "dunkers",
        "running",
        "queued_messages",
        "herdr_available",
        "herdr_hint",
        "state_file",
        "uptime_s",
        "self_pane_id",
        "message",
    ] {
        let value = get(result, key);
        if (value.is_null() || value.as_str() == Some("")) && key != "code_stale" {
            continue;
        }
        let shown = match key {
            "code_mtime" | "started_at" => format!(
                "{} ({}s ago)",
                local(value, "%Y-%m-%d %H:%M:%S"),
                (now - value.as_f64().unwrap_or(0.0)).trunc() as i64
            ),
            "code_stale" => {
                if value.is_null() {
                    let errors: Vec<String> = items(result, "code_identity_errors")
                        .iter()
                        .map(py_str)
                        .collect();
                    let errors = errors.join(", ");
                    format!(
                        "UNKNOWN - source identity unavailable{}",
                        if errors.is_empty() {
                            String::new()
                        } else {
                            format!(": {errors}")
                        }
                    )
                } else if truthy(value) {
                    let names: Vec<String> = items(result, "code_changed_modules")
                        .iter()
                        .map(py_str)
                        .collect();
                    let names = names.join(", ");
                    let mut text =
                        "YES - code on disk differs from this process; restart it".to_string();
                    if !names.is_empty() {
                        text.push_str(&format!(" ({names})"));
                    }
                    text
                } else {
                    "no".to_string()
                }
            }
            _ => py_str(value),
        };
        out.push_str(&format!("{} {shown}\n", ljust(key, 16)));
    }
    out
}
