//! Human formatting: the `print_*` functions of the Python `dunkingsheep` script (T43).
//!
//! Each printer is `render_x(&Value) -> String` (the exact text `print` would write,
//! trailing newline included) plus a `print_x` that writes it. Pure functions of the JSON
//! result, so they are tested without a terminal. Slices 1-3 of t811 cover the Python lines
//! 1004-1427 (`_clip` .. `print_recover`); lines 1629+ are %138's (render_fleet.rs).
use serde_json::Value;

/// Python truthiness of a JSON value (`x or default`).
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

fn get<'a>(v: &'a Value, key: &str) -> &'a Value {
    v.get(key).unwrap_or(&Value::Null)
}

/// `str(value)`: strings raw, None/True/False as Python spells them.
pub fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `str(x if x is not None else "")`.
fn text_or_empty(v: &Value) -> String {
    if v.is_null() {
        String::new()
    } else {
        py_str(v)
    }
}

/// `_clip`: at most `width` characters, the last one an ellipsis when cut.
pub fn clip(v: &Value, width: usize) -> String {
    clip_str(&text_or_empty(v), width)
}

pub fn clip_str(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        text.to_string()
    } else {
        let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// `f"{s:<w}"`.
pub fn ljust(s: &str, w: usize) -> String {
    let n = s.chars().count();
    format!("{s}{}", " ".repeat(w.saturating_sub(n)))
}

/// `f"{s:>w}"`.
pub fn rjust(s: &str, w: usize) -> String {
    let n = s.chars().count();
    format!("{}{s}", " ".repeat(w.saturating_sub(n)))
}

/// `f"{x:g}"` for the magnitudes an interval takes (6 significant digits, trimmed).
fn fmt_g(x: f64) -> String {
    if x == 0.0 {
        return "0".into();
    }
    let exp = x.abs().log10().floor() as i32;
    if !(-4..6).contains(&exp) {
        let s = format!("{x:.5e}");
        let (mant, e) = s.split_once('e').unwrap_or((&s, "0"));
        let mant = if mant.contains('.') {
            mant.trim_end_matches('0').trim_end_matches('.')
        } else {
            mant
        };
        let e: i32 = e.parse().unwrap_or(0);
        return format!("{mant}e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs());
    }
    let decimals = (5 - exp).max(0) as usize;
    let s = format!("{x:.decimals$}");
    if s.contains('.') {
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        s
    }
}

/// `dunk_core.format_interval`.
pub fn format_interval(minutes: f64) -> String {
    if 0.0 < minutes && minutes < 1.0 {
        return format!("{}s", fmt_g(minutes * 60.0));
    }
    if minutes == minutes.trunc() {
        return format!("{}", minutes as i64);
    }
    fmt_g(minutes)
}

/// `_every`.
pub fn every(minutes: f64) -> String {
    let text = format_interval(minutes);
    if text.ends_with('s') {
        text
    } else {
        format!("{text}m")
    }
}

fn num(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}

/// `_gate`.
pub fn gate(dunk: &Value) -> String {
    let mut parts = Vec::new();
    if get(dunk, "only_when") == "idle" {
        parts.push("idle");
    }
    if truthy(get(dunk, "skip_if_unconsumed")) {
        parts.push("skip");
    }
    if parts.is_empty() {
        "-".into()
    } else {
        parts.join("+")
    }
}

/// `print_dunks`.
pub fn render_dunks(dunks: &[Value]) -> String {
    if dunks.is_empty() {
        return "no dunks\n".into();
    }
    let mut out = format!(
        "{:<5} {:<4} {:<20} {:<28} {:>6} {:>6} {:>4} {:<9} TEXT\n",
        "ID", "RUN", "STATUS", "TARGET", "EVERY", "SENT", "SKIP", "GATE"
    );
    for d in dunks {
        let mut sent = if truthy(get(d, "send_count")) {
            py_str(get(d, "send_count"))
        } else {
            "0".into()
        };
        if truthy(get(d, "max_sends")) {
            sent += &format!("/{}", py_str(get(d, "max_sends")));
        }
        let name = if truthy(get(d, "name")) {
            format!(" ({})", py_str(get(d, "name")))
        } else {
            String::new()
        };
        let text = if truthy(get(d, "text")) {
            py_str(get(d, "text"))
        } else {
            String::new()
        };
        let skip = if truthy(get(d, "skip_count")) {
            py_str(get(d, "skip_count"))
        } else {
            "0".into()
        };
        out += &format!(
            "{} {} {} {} {} {} {} {} {}\n",
            ljust(&clip(get(d, "id"), 5), 5),
            ljust(
                if truthy(get(d, "running")) {
                    "yes"
                } else {
                    "no"
                },
                4
            ),
            ljust(&clip(get(d, "status"), 20), 20),
            ljust(&clip(get(d, "target_label"), 28), 28),
            rjust(&every(num(get(d, "interval_minutes"))), 6),
            rjust(&sent, 6),
            rjust(&skip, 4),
            ljust(&gate(d), 9),
            clip_str(&(text.replace('\n', " ") + &name), 60),
        );
    }
    out
}

/// Python `repr(str)` for the `text` line of `print_dunk` (single quotes unless the text
/// has a single quote and no double quote).
pub fn py_repr_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `print_dunk`.
pub fn render_dunk(dunk: &Value) -> String {
    const ORDER: [&str; 18] = [
        "id",
        "name",
        "running",
        "status",
        "target_label",
        "target_pane_id",
        "interval_minutes",
        "only_when",
        "max_sends",
        "skip_if_unconsumed",
        "hold_while_typing",
        "send_count",
        "skip_count",
        "hold_count",
        "awaiting_consumption",
        "next_send_in_s",
        "last_error",
        "text",
    ];
    let mut out = String::new();
    for key in ORDER {
        // Python: `key in dunk and dunk[key] not in (None, "", False, 0) or key in (...)`.
        // `not in (None, "", False, 0)` is equality, so 0.0 and False both match 0/False.
        let always = key == "skip_if_unconsumed" || key == "hold_while_typing";
        let present = dunk.get(key);
        let shown = present.is_some_and(|v| {
            !(v.is_null() || v == "" || matches!(v, Value::Bool(false)) || v.as_f64() == Some(0.0))
        });
        if shown || always {
            let value = present.unwrap_or(&Value::Null);
            let text = if key == "text" {
                py_repr_str(&py_str(value))
            } else {
                py_str(value)
            };
            out += &format!("{} {}\n", ljust(key, 16), text);
        }
    }
    out
}

/// `print_panes`.
pub fn render_panes(result: &Value) -> String {
    let panes = get(result, "panes").as_array().cloned().unwrap_or_default();
    if panes.is_empty() {
        return "no panes (is the terminal backend reachable? on the herdr rollback: is the herdr server running?)\n".into();
    }
    let or_dash = |v: &Value| {
        if truthy(v) {
            v.clone()
        } else {
            Value::String("-".into())
        }
    };
    let mut out = format!(
        "{:<8} {:<20} {:<24} {:<8} {:<8} DIRECTORY\n",
        "PANE", "WORKSPACE", "TAB", "AGENT", "STATUS"
    );
    for p in &panes {
        let marker = if truthy(get(p, "is_self")) { "*" } else { " " };
        out += &format!(
            "{marker}{} {} {} {} {} {}\n",
            ljust(&clip(get(p, "pane_id"), 7), 7),
            ljust(&clip(get(p, "workspace_label"), 20), 20),
            ljust(&clip(get(p, "tab_label"), 24), 24),
            ljust(&clip(&or_dash(get(p, "agent")), 8), 8),
            ljust(&clip(&or_dash(get(p, "agent_status")), 8), 8),
            clip(get(p, "cwd"), 50),
        );
    }
    if truthy(get(result, "self_pane_id")) {
        out += &format!("* = this pane ({})\n", py_str(get(result, "self_pane_id")));
    }
    let gap = get(result, "registry_gap")
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !gap.is_empty() {
        // t714: the pane list is the terminal's opinion; the registry knows the PROCESS.
        out += "\n";
        out += &format!(
            "{} registered lane(s) the backend pane list does not contain:\n",
            gap.len()
        );
        for row in &gap {
            let state = if truthy(get(row, "alive")) {
                format!("process ALIVE (pid {})", py_str(get(row, "pid")))
            } else if truthy(get(row, "pid")) {
                format!("process gone (pid {})", py_str(get(row, "pid")))
            } else {
                "process gone (pid not recorded)".to_string()
            };
            out += &format!(
                "  {} {} {} {}\n",
                ljust(&py_str(get(row, "pane")), 8),
                ljust(&clip(&or_dash(get(row, "label")), 24), 24),
                ljust(&clip(&or_dash(get(row, "role")), 9), 9),
                state
            );
        }
        if gap.iter().any(|r| truthy(get(r, "alive"))) {
            out += "  an ALIVE lane that is not in the list cannot be typed into: mail to it fails with reason=backend_missing_pane (restore the pane, or address the lane by mailbox name).\n";
        }
    }
    out
}

/// `print_inbox_archive`.
pub fn render_inbox_archive(r: &Value) -> String {
    let when = if truthy(get(r, "before")) {
        format!(" before {}", py_str(get(r, "before")))
    } else {
        String::new()
    };
    let states: Vec<String> = get(r, "states")
        .as_array()
        .map(|a| a.iter().map(py_str).collect())
        .unwrap_or_default();
    format!(
        "archived {} message(s) in {} (states {}{when})\n",
        py_str(get(r, "archived")),
        py_str(get(r, "mailbox")),
        states.join(",")
    )
}

/// `print_load`.
pub fn render_load(r: &Value) -> String {
    let mut out = format!(
        "window {}s | host load1 {} | attributed {} cores | unattributed {} cores\n",
        py_str(get(r, "elapsed_s")),
        py_str(get(r, "host_load1")),
        py_str(get(r, "attributed_cores")),
        py_str(get(r, "unattributed_cores"))
    );
    let callers = get(r, "callers").as_array().cloned().unwrap_or_default();
    if callers.is_empty() {
        out += "no registered callers with pid+starttime (nothing is assumed)\n";
    }
    for row in &callers {
        let gpu = get(row, "gpu_pct");
        let gpu_text = if !get(gpu, "value").is_null() {
            format!("{}%", py_str(get(gpu, "value")))
        } else {
            format!("null ({})", py_str(get(gpu, "reason")))
        };
        out += &format!(
            "  {} {}%  peak {}%  rss {}MB  procs {}  gpu {}\n",
            ljust(&py_str(get(row, "name")), 24),
            rjust(&py_str(get(row, "cpu_pct")), 6),
            py_str(get(row, "peak_cpu_pct")),
            py_str(get(row, "rss_mb")),
            py_str(get(row, "process_count")),
            gpu_text
        );
    }
    for item in get(r, "would_kill").as_array().cloned().unwrap_or_default() {
        let tag = if truthy(get(&item, "kill")) {
            ""
        } else {
            " [alert-only]"
        };
        out += &format!("  {}{tag}\n", py_str(get(&item, "line")));
    }
    for skip in get(r, "skipped").as_array().cloned().unwrap_or_default() {
        out += &format!(
            "  {} skipped: {}\n",
            ljust(&py_str(get(&skip, "name")), 24),
            py_str(get(&skip, "reason"))
        );
    }
    out
}

/// `print_pm_digest`.
pub fn render_pm_digest(r: &Value) -> String {
    let action = if truthy(get(r, "action")) {
        py_str(get(r, "action"))
    } else {
        "updated".into()
    };
    format!(
        "{action} {} -> {} ({}, {} bytes)\n",
        py_str(get(r, "digest_id")),
        py_str(get(r, "destination")),
        py_str(get(r, "state")),
        py_str(get(r, "body_bytes"))
    )
}

/// `print_pm_routing`.
pub fn render_pm_routing(r: &Value) -> String {
    let routing = get(r, "routing").as_object().cloned().unwrap_or_default();
    if routing.is_empty() {
        return format!(
            "no PM routing configured at {}\n",
            py_str(get(r, "registry"))
        );
    }
    let mut keys: Vec<&String> = routing.keys().collect();
    keys.sort();
    let mut out = String::new();
    for project in keys {
        let entry = &routing[project];
        let members: Vec<String> = get(entry, "members")
            .as_array()
            .map(|a| a.iter().map(py_str).collect())
            .unwrap_or_default();
        let members = if members.is_empty() {
            "-".to_string()
        } else {
            members.join(", ")
        };
        out += &format!(
            "{project}: pm {}  mode {}  lanes {members}\n",
            py_str(get(entry, "pm")),
            py_str(get(entry, "mode"))
        );
    }
    out
}

/// `print_inbox_count`.
pub fn render_inbox_count(r: &Value) -> String {
    let mut line = format!(
        "{}: {} unread of {}",
        py_str(get(r, "mailbox")),
        py_str(get(r, "unread")),
        py_str(get(r, "total"))
    );
    if truthy(get(r, "oldest_unread_s")) {
        line += &format!(
            ", oldest waiting {}",
            every(num(get(r, "oldest_unread_s")) / 60.0)
        );
    }
    for (key, tail) in [
        ("archived", " archived"),
        ("undeliverable", " undeliverable"),
        ("from_lanes", " from lanes (bypassed a PM)"),
        ("attachments_unread", " attachment(s)"),
    ] {
        if truthy(get(r, key)) {
            line += &format!(", {}{tail}", py_str(get(r, key)));
        }
    }
    line + "\n"
}

/// `print_inbox_sent`.
pub fn render_inbox_sent(r: &Value) -> String {
    let mut line = format!(
        "{} -> {}",
        py_str(get(r, "message_id")),
        py_str(get(r, "destination"))
    );
    if let Some(a) = get(r, "attachments").as_array().filter(|a| !a.is_empty()) {
        line += &format!(" with {} attachment(s)", a.len());
    }
    line + if truthy(get(r, "notified")) {
        "; recipient nudged\n"
    } else {
        "; no nudge\n"
    }
}

/// `_inbox_rows`.
pub fn render_inbox_rows(messages: &[Value]) -> String {
    if messages.is_empty() {
        return "no messages\n".into();
    }
    let mut out = format!(
        "{:<6} {:<9} {:<22} {:>8} {:>7} SUBJECT\n",
        "ID", "STATE", "FROM", "WAITING", "BYTES"
    );
    for m in messages {
        let waiting = if get(m, "waiting_s").is_null() {
            "-".to_string()
        } else {
            format!("{}s", py_str(get(m, "waiting_s")))
        };
        let mut subject = if truthy(get(m, "subject")) {
            py_str(get(m, "subject"))
        } else {
            String::new()
        };
        if let Some(a) = get(m, "attachments").as_array().filter(|a| !a.is_empty()) {
            subject += &format!("  [{} attachment(s)]", a.len());
        }
        let sender = if truthy(get(m, "sender")) {
            get(m, "sender").clone()
        } else {
            Value::String("-".into())
        };
        out += &format!(
            "{} {} {} {} {} {}\n",
            ljust(&py_str(get(m, "message_id")), 6),
            ljust(&py_str(get(m, "state")), 9),
            ljust(&clip(&sender, 22), 22),
            rjust(&waiting, 8),
            rjust(&py_str(get(m, "body_bytes")), 7),
            clip_str(&subject, 44)
        );
    }
    out
}

/// `_print_capped` (t1165): a desk-capped result with no `messages` key.
pub fn render_capped(r: &Value) -> String {
    let note = if truthy(get(r, "note")) {
        py_str(get(r, "note"))
    } else {
        String::new()
    };
    let head = if truthy(get(r, "head")) {
        py_str(get(r, "head"))
    } else {
        String::new()
    };
    format!(
        "result capped by the desk role ({} bytes > {}): {note}\n{head}\n",
        py_str(get(r, "bytes")),
        py_str(get(r, "cap"))
    )
}

/// `print_inbox_list`.
pub fn render_inbox_list(r: &Value) -> String {
    if r.get("messages").is_none() {
        return render_capped(r);
    }
    let messages = get(r, "messages").as_array().cloned().unwrap_or_default();
    format!(
        "mailbox {}\n{}",
        py_str(get(r, "mailbox")),
        render_inbox_rows(&messages)
    )
}

// ---- slice 2: tasks and memory (Python lines 1207-1365) --------------------------------

/// `time.strftime('%Y-%m-%d %H:%M:%S', time.localtime(at))`. The CLI passes a closure that
/// applies the local zone; `utc_stamp` is the zone-free default used by tests.
pub type Stamp<'a> = &'a dyn Fn(f64) -> String;

/// UTC `%Y-%m-%d %H:%M:%S` (civil-from-days; no time crate).
pub fn utc_stamp(at: f64) -> String {
    let secs = at.floor() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn join(v: &Value, sep: &str) -> String {
    v.as_array()
        .map(|a| a.iter().map(py_str).collect::<Vec<_>>().join(sep))
        .unwrap_or_default()
}

/// `_task_row`.
pub fn task_row(task: &Value) -> String {
    let scope = if truthy(get(task, "is_global")) {
        Value::String("global".into())
    } else {
        get(task, "scope").clone()
    };
    let assignees = join(get(task, "assignees"), ",");
    let assignees = if assignees.is_empty() {
        "-".to_string()
    } else {
        assignees
    };
    let mut row = format!(
        "{} {} {} {} {} {}",
        ljust(&py_str(get(task, "id")), 5),
        ljust(&py_str(get(task, "status")), 11),
        ljust(&py_str(get(task, "priority")), 6),
        ljust(&clip(&scope, 10), 10),
        ljust(&clip_str(&assignees, 18), 18),
        clip(get(task, "title"), 50)
    );
    // t878/#164: a cancelled dependency is SATISFIED (t879); the note stays visible.
    if truthy(get(task, "dead_dependencies")) {
        row += &format!(
            "  [depends on cancelled {}: satisfied (t164), claimable]",
            join(get(task, "dead_dependencies"), ", ")
        );
    }
    row
}

/// `print_task`.
pub fn render_task(task: &Value, stamp: Stamp) -> String {
    let mut out = format!(
        "{} [{}] {}  {}\n",
        py_str(get(task, "id")),
        py_str(get(task, "priority")),
        py_str(get(task, "status")),
        py_str(get(task, "title"))
    );
    if let Some(dep) = get(task, "blocked_dependents")
        .as_array()
        .filter(|a| !a.is_empty())
    {
        out += &format!(
            "warning     cancelling this leaves {} unclaimable dependent(s): {} - a cancelled dependency can never be satisfied, so repoint or cancel them\n",
            dep.len(), join(get(task, "blocked_dependents"), ", ")
        );
    }
    out += &format!("scope       {}\n", py_str(get(task, "scope")));
    if truthy(get(task, "assignees")) {
        out += &format!("assignees   {}\n", join(get(task, "assignees"), ", "));
    }
    if truthy(get(task, "claimed_by")) {
        out += &format!("claimed by  {}\n", join(get(task, "claimed_by"), ", "));
    }
    if truthy(get(task, "pr")) {
        let acct = if truthy(get(task, "accounting_pr")) {
            format!(" (accounting pr #{})", py_str(get(task, "accounting_pr")))
        } else {
            String::new()
        };
        out += &format!("body pr     #{}{acct}\n", py_str(get(task, "pr")));
    }
    if truthy(get(task, "dead_dependencies")) {
        out += &format!(
            "cancelled deps {} (satisfied since t164, so this row IS claimable; repoint with `task update <id> --depends-on <good-id>` if you want a live edge)\n",
            join(get(task, "dead_dependencies"), ", ")
        );
    }
    if truthy(get(task, "depends_on")) {
        let blocked = if truthy(get(task, "blocked_by")) {
            format!(" (blocked by {})", join(get(task, "blocked_by"), ", "))
        } else {
            String::new()
        };
        out += &format!(
            "depends on  {}{blocked}\n",
            join(get(task, "depends_on"), ", ")
        );
    }
    if truthy(get(task, "unblock_needed")) && truthy(get(task, "dependency_note")) {
        // t903: only the block is holding this task, and nothing used to say so.
        out += &format!("unblock     {}\n", py_str(get(task, "dependency_note")));
    }
    if truthy(get(task, "parent_id")) {
        out += &format!("parent      {}\n", py_str(get(task, "parent_id")));
    }
    if truthy(get(task, "block_reason")) {
        out += &format!("blocked     {}\n", py_str(get(task, "block_reason")));
    }
    if truthy(get(task, "body")) {
        out += &format!("\n{}\n", py_str(get(task, "body")));
    }
    if let Some(children) = get(task, "children").as_array().filter(|a| !a.is_empty()) {
        out += "\nsubtasks:\n";
        for child in children {
            out += &format!("  {}\n", task_row(child));
        }
    }
    if truthy(get(task, "lease_expires_at")) {
        out += &format!(
            "lease       {}\n",
            stamp(num(get(task, "lease_expires_at")))
        );
    }
    if let Some(notes) = get(task, "notes").as_array().filter(|a| !a.is_empty()) {
        // t605: the evidence a lane recorded, in the view the next reader uses.
        out += &format!("\nnotes (last {}):\n", notes.len());
        for row in notes {
            let actor = if truthy(get(row, "actor")) {
                py_str(get(row, "actor"))
            } else {
                "-".into()
            };
            let event = if truthy(get(row, "event")) {
                py_str(get(row, "event"))
            } else {
                "note".into()
            };
            out += &format!("  {} {actor} {event}\n", stamp(num(get(row, "at"))));
            let reason = if truthy(get(row, "reason")) {
                py_str(get(row, "reason"))
            } else {
                String::new()
            };
            let lines = py_splitlines(&reason);
            if lines.is_empty() {
                out += "    \n";
            }
            for line in lines {
                out += &format!("    {line}\n");
            }
        }
    }
    out
}

/// `str.splitlines()` for the separators a note can carry (\n, \r\n, \r).
pub fn py_splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        match rest.find(['\n', '\r']) {
            Some(i) => {
                out.push(&rest[..i]);
                let skip = if rest[i..].starts_with("\r\n") { 2 } else { 1 };
                rest = &rest[i + skip..];
            }
            None => {
                out.push(rest);
                rest = "";
            }
        }
    }
    out
}

/// `print_task_list`.
pub fn render_task_list(result: &Value) -> String {
    let tasks = get(result, "tasks").as_array().cloned().unwrap_or_default();
    let pane = if truthy(get(result, "pane_id")) {
        py_str(get(result, "pane_id"))
    } else {
        "-".into()
    };
    let mut out = format!("scope {} (pane {pane})\n", py_str(get(result, "scope")));
    if tasks.is_empty() {
        return out + "no tasks\n";
    }
    out += &format!(
        "{:<5} {:<11} {:<6} {:<10} {:<18} TITLE\n",
        "ID", "STATUS", "PRIORITY", "SCOPE", "ASSIGNEES"
    );
    for task in &tasks {
        out += &task_row(task);
        out += "\n";
    }
    out
}

/// `print_task_next`.
pub fn render_task_next(result: &Value, stamp: Stamp) -> String {
    let task = get(result, "task");
    if !truthy(task) {
        let mut out = String::new();
        let refused = get(result, "refused");
        let reason = py_str(get(refused, "reason"));
        let has_reason = truthy(get(refused, "reason"));
        let ids = join(get(refused, "task_ids"), ", ");
        let owners = join(get(refused, "owned_by"), ", ");
        if has_reason && reason == "busy" {
            let held = if ids.is_empty() {
                "in-progress work".to_string()
            } else {
                ids
            };
            out += &format!("refused (busy): this pane already holds {held}; pass --allow-busy to take parallel work\n");
        } else if has_reason && reason == "empty" {
            out += "no eligible task: the queue is empty for this scope\n";
        } else if has_reason {
            let mut detail = if ids.is_empty() {
                String::new()
            } else {
                format!(" ({ids})")
            };
            if !owners.is_empty() {
                detail += &format!(" owned by {owners}");
            }
            out += &format!("no eligible task: refused ({reason}){detail}\n");
        } else {
            out += "no eligible task\n";
        }
        if truthy(get(result, "following")) {
            out +=
                "following the queue: the daemon will push the next task when this pane is idle\n";
        }
        return out;
    }
    format!(
        "claimed {} (lease {}s)\n{}",
        py_str(get(task, "id")),
        num(get(result, "lease_seconds")) as i64,
        render_task(task, stamp)
    )
}

/// `print_task_followers`.
pub fn render_task_followers(result: &Value) -> String {
    let followers = get(result, "followers")
        .as_array()
        .cloned()
        .unwrap_or_default();
    if followers.is_empty() {
        return "no followers\n".into();
    }
    followers
        .iter()
        .map(|f| {
            format!(
                "{} {}\n",
                ljust(&py_str(get(f, "pane_id")), 12),
                py_str(get(f, "scope"))
            )
        })
        .collect()
}

/// `print_task_stats`. serde_json keeps the daemon's key order only with preserve_order;
/// the counts print in the map's iteration order (sorted without it) - see the PR note.
pub fn render_task_stats(result: &Value) -> String {
    let kv = |m: &serde_json::Map<String, Value>, skip_zero: bool| {
        m.iter()
            .filter(|(_, v)| !skip_zero || truthy(v))
            .map(|(k, v)| format!("{k}={}", py_str(v)))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut out = format!(
        "{}\n",
        get(result, "totals")
            .as_object()
            .map(|m| kv(m, false))
            .unwrap_or_default()
    );
    if let Some(by) = get(result, "by_scope").as_object() {
        for (scope, counts) in by {
            out += &format!(
                "  {scope}: {}\n",
                counts.as_object().map(|m| kv(m, true)).unwrap_or_default()
            );
        }
    }
    out
}

fn first_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn or_dash(v: &Value) -> String {
    if truthy(v) {
        py_str(v)
    } else {
        "-".into()
    }
}

/// `print_memory`.
pub fn render_memory(rows: &[Value]) -> String {
    if rows.is_empty() {
        return "no matches\n".into();
    }
    let mut out = String::new();
    for row in rows {
        out += &format!(
            "{:.3} {} {} #{} [{}]\n",
            num(get(row, "score")),
            ljust(&or_dash(get(row, "harness")), 8),
            ljust(&or_dash(get(row, "project")), 20),
            py_str(get(row, "session_id")),
            or_dash(get(row, "role"))
        );
        let snippet = if truthy(get(row, "snippet")) {
            py_str(get(row, "snippet"))
        } else {
            String::new()
        };
        out += &format!("  {}\n", first_chars(&snippet.replace('\n', " "), 200));
    }
    out
}

/// `print_memory_context`.
pub fn render_memory_context(result: &Value) -> String {
    let counts = get(result, "counts");
    let mut out = format!(
        "query {}  sessions {}  snippets {}\n",
        py_repr_str(&py_str(get(result, "query"))),
        py_str(get(counts, "sessions")),
        py_str(get(counts, "snippets"))
    );
    for item in get(result, "sessions")
        .as_array()
        .cloned()
        .unwrap_or_default()
    {
        let title = if truthy(get(&item, "title")) {
            py_str(get(&item, "title"))
        } else {
            String::new()
        };
        out += &format!(
            "- {} {} #{} {title} (score {:.3})\n",
            py_str(get(&item, "harness")),
            or_dash(get(&item, "project")),
            py_str(get(&item, "session_id")),
            num(get(&item, "score"))
        );
        for s in get(&item, "snippets")
            .as_array()
            .cloned()
            .unwrap_or_default()
        {
            let text = if truthy(get(&s, "snippet")) {
                py_str(get(&s, "snippet"))
            } else {
                String::new()
            };
            out += &format!("    {}\n", first_chars(&text.replace('\n', " "), 160));
        }
    }
    out
}

/// `print_memory_hook_context`: the Claude SessionStart hook JSON.
pub fn render_memory_hook_context(result: &Value) -> String {
    let mut text = if truthy(get(result, "text")) {
        py_str(get(result, "text"))
    } else {
        String::new()
    };
    if !text.trim().is_empty() {
        text =
            format!("Dunking Sheep memory - relevant past sessions for this workspace:\n\n{text}");
    }
    // Built by hand: Python keeps insertion order (hookEventName first); serde_json's
    // default map would sort the keys.
    format!(
        "{{\"hookSpecificOutput\": {{\"hookEventName\": \"SessionStart\", \"additionalContext\": {}}}}}\n",
        py_json_dumps(&Value::String(text))
    )
}

/// Python `json.dumps` default (ensure_ascii, ", " and ": " separators, insertion order).
pub fn py_json_dumps(v: &Value) -> String {
    match v {
        Value::String(s) => {
            let mut out = String::from("\"");
            for c in s.chars() {
                match c {
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    '\u{8}' => out.push_str("\\b"),
                    '\u{c}' => out.push_str("\\f"),
                    c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                        let mut buf = [0u16; 2];
                        for unit in c.encode_utf16(&mut buf) {
                            out.push_str(&format!("\\u{unit:04x}"));
                        }
                    }
                    c => out.push(c),
                }
            }
            out + "\""
        }
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(py_json_dumps).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, v)| format!(
                    "{}: {}",
                    py_json_dumps(&Value::String(k.clone())),
                    py_json_dumps(v)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => other.to_string(),
    }
}

/// `print_memory_stats`.
pub fn render_memory_stats(result: &Value) -> String {
    let p = |k: &str| py_str(get(result, k));
    let q = get(result, "queries");
    let qq = |k: &str| py_str(get(q, k));
    let mut out = format!(
        "path {}  schema {}  ok {}\nsources {}  sessions {}  chunks {}\nqueue depth {}  {}\ndb bytes {}\nqueries n={} p50={} p95={} p99={} errors={}\n",
        p("path"), p("schema_version"), p("ok"), p("sources"), p("sessions"), p("chunks"), p("queue_depth"), p("queue"),
        p("db_bytes"), qq("count"), qq("p50"), qq("p95"), qq("p99"), qq("errors")
    );
    if truthy(get(result, "ingestor")) {
        out += &format!("ingestor {}\n", py_repr_value(get(result, "ingestor")));
    }
    for item in get(result, "harness")
        .as_array()
        .cloned()
        .unwrap_or_default()
    {
        out += &format!(
            "  {}: {} chunks / {} sessions\n",
            py_str(get(&item, "harness")),
            py_str(get(&item, "chunks")),
            py_str(get(&item, "sessions"))
        );
    }
    out
}

/// Python `repr` of a JSON-derived value (dict/list/str/None/bool/number).
pub fn py_repr_value(v: &Value) -> String {
    match v {
        Value::String(s) => py_repr_str(s),
        Value::Array(a) => format!(
            "[{}]",
            a.iter().map(py_repr_value).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, v)| format!("{}: {}", py_repr_str(k), py_repr_value(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        other => py_str(other),
    }
}

/// `print_memory_backfill`.
pub fn render_memory_backfill(result: &Value) -> String {
    let mut out = String::new();
    if result.get("enqueued").is_some() {
        let throttle = get(get(get(result, "status"), "ingestor"), "throttle_s");
        out += &format!(
            "queued {} of {} source(s); paused {}; throttle {}\n",
            py_str(get(result, "enqueued")),
            py_str(get(result, "discovered")),
            py_str(get(result, "paused")),
            py_str(throttle)
        );
    } else if result.get("paused").is_some() {
        out += &format!("backfill paused {}\n", py_str(get(result, "paused")));
    }
    let status = if truthy(get(result, "status")) {
        get(result, "status")
    } else {
        result
    };
    if truthy(status) {
        out += &render_memory_stats(status);
    }
    out
}

// ---- slice 3: health, doctor, recover (Python lines 1383-1440) -------------------------

fn field(v: &Value, k: &str) -> String {
    py_str(get(v, k))
}

/// `print_health`.
pub fn render_health(result: &Value) -> String {
    let state = if truthy(get(result, "ok")) {
        "ok"
    } else {
        "DEGRADED"
    };
    let mut out = format!(
        "health {state}  duration {} ms\n",
        field(result, "duration_ms")
    );
    for item in get(result, "problems")
        .as_array()
        .cloned()
        .unwrap_or_default()
    {
        out += &format!("  problem: {}\n", py_str(&item));
    }
    for item in get(result, "repairs")
        .as_array()
        .cloned()
        .unwrap_or_default()
    {
        out += &format!("  repaired: {}\n", py_str(&item));
    }
    let checks = get(result, "checks");
    let memory = get(checks, "memory");
    out += &format!(
        "  memory ok={} sources={} sessions={} chunks={} queue={}\n",
        field(memory, "ok"),
        field(memory, "sources"),
        field(memory, "sessions"),
        field(memory, "chunks"),
        field(memory, "queue_depth")
    );
    out += &format!(
        "  integrity={} stale={} wal={} disk_free={} tasks={}\n",
        field(checks, "integrity"),
        field(checks, "stale_leases"),
        field(checks, "wal_bytes"),
        field(checks, "disk_free_bytes"),
        field(checks, "tasks")
    );
    let dispatch = get(checks, "dispatch");
    if truthy(dispatch) {
        // 'the feeder is dead' as a number, not an absence; minutes because humans read those.
        let age = get(dispatch, "age_s");
        let human = if age.is_null() {
            "never".to_string()
        } else if num(age) >= 60.0 {
            format!("{:.1}m ago ({}s)", num(age) / 60.0, py_str(age))
        } else {
            format!("{}s ago", py_str(age))
        };
        let dry = if truthy(get(dispatch, "dry_run")) {
            " (dry run)"
        } else {
            ""
        };
        out += &format!(
            "  dispatcher={} last_tick={human}{dry}\n",
            field(dispatch, "status")
        );
    }
    out
}

fn warnings(result: &Value) -> String {
    get(result, "warnings")
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|w| format!("  warning {}: {}\n", field(w, "kind"), field(w, "detail")))
        .collect()
}

/// `print_doctor`.
pub fn render_doctor(result: &Value) -> String {
    let state = if truthy(get(result, "ok")) {
        "ok"
    } else {
        "WARN"
    };
    let panes = get(result, "panes");
    let mut out = format!(
        "doctor {state}  backend {}  launches {}\n  panes {} (agents {}, sessions {})\n",
        field(result, "herdr_version"),
        field(result, "launches"),
        field(panes, "total"),
        field(panes, "with_agent"),
        field(panes, "with_session")
    );
    if let Some(surfaces) = get(result, "surfaces").as_object() {
        for (key, value) in surfaces {
            if key == "guard" && value.is_object() {
                out += &format!(
                    "  guard installed={} shadowing={} active_in_this_shell={}\n",
                    field(value, "installed"),
                    field(value, "shadowing_herdr"),
                    field(value, "active_in_this_shell")
                );
            } else {
                // Python prints str(value): dicts/lists as repr.
                let shown = if value.is_string() {
                    py_str(value)
                } else {
                    py_repr_value(value)
                };
                out += &format!("  {key} {shown}\n");
            }
        }
    }
    out + &warnings(result)
}

/// `print_recover`.
pub fn render_recover(result: &Value) -> String {
    let mode = if truthy(get(result, "apply")) {
        "apply"
    } else {
        "dry-run"
    };
    let mut out = format!(
        "recover {mode}  backend {}  ok {}  launches {}\n",
        field(result, "herdr"),
        field(result, "ok"),
        field(result, "launches")
    );
    for item in get(result, "plan").as_array().cloned().unwrap_or_default() {
        let agent = or_dash(get(&item, "agent"));
        let detail = if truthy(get(&item, "command")) {
            field(&item, "command")
        } else if truthy(get(&item, "reason")) {
            field(&item, "reason")
        } else {
            String::new()
        };
        out += &format!(
            "  {} {} {} {detail}\n",
            ljust(&field(&item, "pane_id"), 8),
            ljust(&field(&item, "action"), 15),
            ljust(&agent, 8)
        );
        if !get(&item, "applied").is_null() {
            let why = if truthy(get(&item, "apply_reason")) {
                field(&item, "apply_reason")
            } else if truthy(get(&item, "apply_detail")) {
                field(&item, "apply_detail")
            } else {
                String::new()
            };
            out += &format!("           applied={} {why}\n", field(&item, "applied"));
        }
    }
    out + &warnings(result)
}

macro_rules! printer {
    ($name:ident, $render:ident) => {
        pub fn $name(v: &Value) {
            print!("{}", $render(v));
        }
    };
}
printer!(print_dunk, render_dunk);
printer!(print_panes, render_panes);
printer!(print_inbox_archive, render_inbox_archive);
printer!(print_load, render_load);
printer!(print_pm_digest, render_pm_digest);
printer!(print_pm_routing, render_pm_routing);
printer!(print_inbox_count, render_inbox_count);
printer!(print_inbox_sent, render_inbox_sent);
printer!(print_inbox_list, render_inbox_list);

pub fn print_dunks(dunks: &[Value]) {
    print!("{}", render_dunks(dunks));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn clip_cuts_with_an_ellipsis() {
        assert_eq!(clip_str("abcdef", 4), "abc…");
        assert_eq!(clip_str("abcd", 4), "abcd");
        assert_eq!(clip(&Value::Null, 3), "");
    }

    #[test]
    fn intervals_print_like_format_interval() {
        assert_eq!(every(10.0), "10m");
        assert_eq!(every(0.5), "30s");
        assert_eq!(every(1.5), "1.5m");
        assert_eq!(every(0.0), "0m");
    }

    #[test]
    fn dunks_table_and_empty() {
        assert_eq!(render_dunks(&[]), "no dunks\n");
        let d = json!({"id": "d1", "running": true, "status": "ok", "target_label": "w1:p1",
            "interval_minutes": 5, "send_count": 3, "max_sends": 10, "skip_count": 1,
            "only_when": "idle", "skip_if_unconsumed": true, "text": "hi\nthere", "name": "nm"});
        let out = render_dunks(&[d]);
        let mut lines = out.lines();
        assert!(lines.next().unwrap().starts_with("ID    RUN  STATUS"));
        assert_eq!(
            lines.next().unwrap(),
            "d1    yes  ok                   w1:p1                            5m   3/10    1 idle+skip hi there (nm)"
        );
    }

    #[test]
    fn dunk_detail_hides_empty_fields_but_keeps_the_two_flags() {
        let out = render_dunk(&json!({"id": "d1", "running": false, "text": "it's"}));
        assert_eq!(
            out,
            "id               d1\nskip_if_unconsumed None\nhold_while_typing None\ntext             \"it's\"\n"
                .replace("skip_if_unconsumed None", &format!("{} None", ljust("skip_if_unconsumed", 16)))
                .replace("hold_while_typing None", &format!("{} None", ljust("hold_while_typing", 16)))
        );
    }

    #[test]
    fn the_cli_prints_the_gap_line() {
        // test_dunk_pane_registry_gap.py::test_the_cli_prints_the_gap_line
        let result =
            json!({"panes": [], "registry_gap": [{"pane": "w2:p1", "alive": true, "pid": 42}]});
        // no panes short-circuits, exactly as the Python does
        assert!(render_panes(&result).starts_with("no panes"));
        let result = json!({"panes": [{"pane_id": "%1", "is_self": true}], "self_pane_id": "%1",
            "registry_gap": [{"pane": "w2:p1", "alive": true, "pid": 42}]});
        let text = render_panes(&result);
        assert!(text.contains("registered lane(s) the backend pane list does not contain"));
        assert!(
            text.contains("w2:p1")
                && text.contains("ALIVE")
                && text.contains("backend_missing_pane")
        );
        assert!(text.contains("* = this pane (%1)"));
    }

    #[test]
    fn inbox_count_sent_and_list() {
        let c =
            json!({"mailbox": "a", "unread": 2, "total": 5, "oldest_unread_s": 120, "archived": 1});
        assert_eq!(
            render_inbox_count(&c),
            "a: 2 unread of 5, oldest waiting 2m, 1 archived\n"
        );
        let s =
            json!({"message_id": "i1", "destination": "b", "attachments": ["x"], "notified": true});
        assert_eq!(
            render_inbox_sent(&s),
            "i1 -> b with 1 attachment(s); recipient nudged\n"
        );
        assert_eq!(
            render_inbox_list(&json!({"mailbox": "a", "messages": []})),
            "mailbox a\nno messages\n"
        );
        let capped = json!({"bytes": 9, "cap": 5, "head": "hd"});
        assert_eq!(
            render_inbox_list(&capped),
            "result capped by the desk role (9 bytes > 5): \nhd\n"
        );
    }

    #[test]
    fn pm_printers() {
        assert_eq!(
            render_pm_digest(
                &json!({"digest_id": "g1", "destination": "pm", "state": "queued", "body_bytes": 7})
            ),
            "updated g1 -> pm (queued, 7 bytes)\n"
        );
        assert_eq!(
            render_pm_routing(&json!({"routing": {}, "registry": "/r"})),
            "no PM routing configured at /r\n"
        );
        let r = json!({"routing": {"b": {"pm": "x", "mode": "m", "members": []}}});
        assert_eq!(render_pm_routing(&r), "b: pm x  mode m  lanes -\n");
    }

    #[test]
    fn task_detail_and_rows() {
        let child = json!({"id": "t2", "status": "open", "priority": "low", "scope": "%1", "assignees": [], "title": "kid", "dead_dependencies": ["t9"]});
        let task = json!({"id": "t1", "priority": "high", "status": "blocked", "title": "T", "scope": "global",
            "assignees": ["%1", "%2"], "claimed_by": ["%1"], "pr": 5, "accounting_pr": 6, "depends_on": ["t3"], "blocked_by": ["t3"],
            "block_reason": "why", "body": "b1", "children": [child], "lease_expires_at": 86400.0,
            "notes": [{"at": 0.0, "actor": null, "event": null, "reason": "a\nb"}]});
        let out = render_task(&task, &utc_stamp);
        assert_eq!(out, "t1 [high] blocked  T\nscope       global\nassignees   %1, %2\nclaimed by  %1\nbody pr     #5 (accounting pr #6)\ndepends on  t3 (blocked by t3)\nblocked     why\n\nb1\n\nsubtasks:\n  t2    open        low    %1         -                  kid  [depends on cancelled t9: satisfied (t164), claimable]\nlease       1970-01-02 00:00:00\n\nnotes (last 1):\n  1970-01-01 00:00:00 - note\n    a\n    b\n");
    }

    #[test]
    fn task_list_next_followers_stats() {
        assert_eq!(
            render_task_list(&json!({"scope": "global", "tasks": []})),
            "scope global (pane -)\nno tasks\n"
        );
        let busy = json!({"refused": {"reason": "busy", "task_ids": ["t1"]}});
        assert_eq!(
            render_task_next(&busy, &utc_stamp),
            "refused (busy): this pane already holds t1; pass --allow-busy to take parallel work\n"
        );
        let owned = json!({"refused": {"reason": "owned_elsewhere", "task_ids": ["t1", "t2"], "owned_by": ["%1"]}, "following": true});
        assert_eq!(render_task_next(&owned, &utc_stamp), "no eligible task: refused (owned_elsewhere) (t1, t2) owned by %1\nfollowing the queue: the daemon will push the next task when this pane is idle\n");
        assert_eq!(
            render_task_next(&json!({}), &utc_stamp),
            "no eligible task\n"
        );
        assert_eq!(render_task_followers(&json!({})), "no followers\n");
        assert_eq!(
            render_task_followers(&json!({"followers": [{"pane_id": "%4", "scope": "global"}]})),
            "%4           global\n"
        );
        let stats = json!({"totals": {"open": 2}, "by_scope": {"global": {"open": 2, "done": 0}}});
        assert_eq!(render_task_stats(&stats), "open=2\n  global: open=2\n");
    }

    #[test]
    fn memory_printers() {
        assert_eq!(render_memory(&[]), "no matches\n");
        let row = json!({"score": 1.23456, "harness": null, "project": "p", "session_id": 7, "role": "user", "snippet": "a\nb"});
        assert_eq!(
            render_memory(&[row]),
            "1.235 -        p                    #7 [user]\n  a b\n"
        );
        let ctx = json!({"query": "it's", "counts": {"sessions": 1, "snippets": 1},
            "sessions": [{"harness": "pi", "project": null, "session_id": 3, "score": 0.5, "snippets": [{"snippet": "x"}]}]});
        assert_eq!(
            render_memory_context(&ctx),
            "query \"it's\"  sessions 1  snippets 1\n- pi - #3  (score 0.500)\n    x\n"
        );
        let hook = render_memory_hook_context(&json!({"text": "é"}));
        assert_eq!(hook, "{\"hookSpecificOutput\": {\"hookEventName\": \"SessionStart\", \"additionalContext\": \"Dunking Sheep memory - relevant past sessions for this workspace:\\n\\n\\u00e9\"}}\n");
        assert_eq!(render_memory_backfill(&json!({"paused": true})), "backfill paused True\npath None  schema None  ok None\nsources None  sessions None  chunks None\nqueue depth None  None\ndb bytes None\nqueries n=None p50=None p95=None p99=None errors=None\n");
    }

    #[test]
    fn utc_stamp_and_splitlines() {
        assert_eq!(utc_stamp(1_793_461_428.0), "2026-10-31 15:43:48");
        assert_eq!(py_splitlines("a\r\nb\n"), vec!["a", "b"]);
        assert!(py_splitlines("").is_empty());
    }

    #[test]
    fn health_doctor_recover() {
        let h = json!({"ok": false, "duration_ms": 12, "problems": ["p"], "repairs": ["r"],
            "checks": {"memory": {"ok": true, "sources": 1, "sessions": 2, "chunks": 3, "queue_depth": 0},
            "integrity": "ok", "stale_leases": 0, "wal_bytes": 9, "disk_free_bytes": 8, "tasks": 4,
            "dispatch": {"status": "ok", "age_s": 120, "dry_run": true}}});
        assert_eq!(render_health(&h), "health DEGRADED  duration 12 ms\n  problem: p\n  repaired: r\n  memory ok=True sources=1 sessions=2 chunks=3 queue=0\n  integrity=ok stale=0 wal=9 disk_free=8 tasks=4\n  dispatcher=ok last_tick=2.0m ago (120s) (dry run)\n");
        let never = json!({"ok": true, "checks": {"dispatch": {"status": "dead", "age_s": null}}});
        assert!(render_health(&never).contains("last_tick=never\n"));
        let d = json!({"ok": true, "herdr_version": "1", "launches": 2, "panes": {"total": 3, "with_agent": 2, "with_session": 1},
            "surfaces": {"guard": {"installed": true, "shadowing_herdr": false, "active_in_this_shell": true}, "mcp": "yes"},
            "warnings": [{"kind": "k", "detail": "d"}]});
        assert_eq!(render_doctor(&d), "doctor ok  backend 1  launches 2\n  panes 3 (agents 2, sessions 1)\n  guard installed=True shadowing=False active_in_this_shell=True\n  mcp yes\n  warning k: d\n");
        let r = json!({"apply": true, "herdr": "h", "ok": true, "launches": 1,
            "plan": [{"pane_id": "%1", "action": "relaunch", "agent": null, "command": "go", "applied": false, "apply_reason": "nope"}]});
        assert_eq!(render_recover(&r), "recover apply  backend h  ok True  launches 1\n  %1       relaunch        -        go\n           applied=False nope\n");
    }
}
