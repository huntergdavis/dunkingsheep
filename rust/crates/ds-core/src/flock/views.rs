//! `dunk_views.py` (T38, slice 2): the read-only Command Center compositions that do not
//! need a store this crate lacks yet: `cell`, `_iso`, `_median`, `_app_row`, `_app_state`,
//! `app_messages`, `git_merges`, `message_state`, `reply_stats`. Every number is a cell
//! `{value, as_of, reason}` so a missing fact is never a zero.
//!
//! The inbox and task stores are parameters (closures), not dependencies: the Python reads
//! `flock.inbox` and `flock.task_list`, which T38's later slices wire. NOT in this slice:
//! `roster` (needs `resolve_lanes`, task leases, `dunk_activity`, `dunk_load`) and
//! `project_summary` (needs the context store, lanes and `task_list`).

use std::path::Path;

use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Map, Value};

/// The application's delivery column does DOUBLE DUTY: transport on an owner message
/// (sent -> read -> acked), reply lifecycle on an agent message (posted). It is split by
/// role here, never passed through raw.
pub const TRANSPORT_STATES: [&str; 3] = ["sent", "read", "acked"];
pub const REPLY_STATES: [&str; 1] = ["posted"];

/// `PROJECT_REPOS`: the repositories a project page can show merges for.
pub const PROJECT_REPOS: [(&str, &str); 3] = [
    ("dunkingsheep", "~/workspace/dunkingsheep"),
    ("vp-of-agentry", "~/workspace/vp-of-agentry"),
    ("herdr-webui", "~/workspace/herdr-webui"),
];

/// `cell`.
pub fn cell(value: Value, as_of: Option<&str>, reason: Option<&str>) -> Value {
    json!({"value": value, "as_of": as_of, "reason": reason})
}

/// `_iso`: `%Y-%m-%dT%H:%M:%SZ` in UTC; 0 and None are None (Python's falsy `at`).
pub fn iso(at: Option<f64>) -> Option<String> {
    let at = at.filter(|a| *a != 0.0)?;
    chrono::DateTime::from_timestamp(at.floor() as i64, 0)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

fn iso_value(at: Option<f64>) -> Value {
    iso(at).map(Value::String).unwrap_or(Value::Null)
}

/// `_median`: rounded to one place (Python `round(x, 1)`, half-to-even on the exact value).
pub fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut ordered = values.to_vec();
    ordered.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let middle = ordered.len() / 2;
    let raw = if ordered.len() % 2 == 1 {
        ordered[middle]
    } else {
        (ordered[middle - 1] + ordered[middle]) / 2.0
    };
    Some(format!("{raw:.1}").parse().unwrap_or(raw))
}

fn s<'a>(row: &'a Value, key: &str) -> &'a str {
    row.get(key).and_then(Value::as_str).unwrap_or("")
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(t)) => !t.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Python `repr(str)` for the quoted state in a reason.
fn py_repr(t: &str) -> String {
    if t.contains('\'') && !t.contains('"') {
        format!("\"{t}\"")
    } else {
        format!("'{}'", t.replace('\\', "\\\\").replace('\'', "\\'"))
    }
}

/// `_app_state`: sent / delivered / seen / replied for an APPLICATION message, with evidence.
/// `replied` is `answer_id IS NOT NULL`, NOT `answered_at` (456 owner rows carry answered_at,
/// 70 have no answer_id: answered_at is 'picked up').
pub fn app_state(row: &Value) -> Value {
    let delivery = s(row, "delivery").trim().to_lowercase();
    let role = s(row, "role").trim().to_lowercase();
    let (transport, lifecycle, reason): (Option<String>, Option<String>, Option<String>);
    if role == "owner" {
        transport = TRANSPORT_STATES
            .contains(&delivery.as_str())
            .then(|| delivery.clone());
        lifecycle = None;
        reason = if transport.is_some() {
            None
        } else if !delivery.is_empty() {
            Some(format!("unexpected transport state {}", py_repr(&delivery)))
        } else {
            Some("no transport state recorded".into())
        };
    } else {
        transport = None;
        lifecycle = REPLY_STATES
            .contains(&delivery.as_str())
            .then(|| delivery.clone());
        reason = if lifecycle.is_some() {
            None
        } else if !delivery.is_empty() {
            Some(format!("unexpected reply state {}", py_repr(&delivery)))
        } else {
            Some("no reply state recorded".into())
        };
    }
    let replied = if truthy(row.get("answer_id")) {
        let reply = row.get("answer").cloned().unwrap_or_else(|| json!({}));
        let at = reply.get("at").cloned().unwrap_or(Value::Null);
        json!({"at": at, "reply_id": row["answer_id"], "reply_excerpt": reply.get("excerpt").cloned().unwrap_or(Value::Null),
               "reason": if truthy(reply.get("at")) { Value::Null } else { json!("answer_id is set but the reply row is missing") }})
    } else {
        cell(Value::Null, None, Some("awaiting reply (no answer_id yet)"))
    };
    let seen = if truthy(row.get("answered_at")) {
        json!({"at": row["answered_at"], "reason": Value::Null})
    } else {
        cell(Value::Null, None, Some("not picked up yet"))
    };
    let detail = if truthy(row.get("delivery_detail")) {
        row["delivery_detail"].clone()
    } else {
        Value::Null
    };
    json!({
        "id": row.get("id").cloned().unwrap_or(Value::Null), "found": true, "source": "application",
        "mailbox": row.get("channel").cloned().unwrap_or(Value::Null),
        "role": row.get("role").cloned().unwrap_or(Value::Null),
        "sent": {"at": row.get("created_at").cloned().unwrap_or(Value::Null), "by": row.get("role").cloned().unwrap_or(Value::Null)},
        "delivered": {"ok": transport.is_some() || lifecycle.is_some(), "transport": transport, "reply_lifecycle": lifecycle,
                      "detail": detail, "reason": reason},
        "seen": seen,
        "replied": replied,
        "state": transport.clone().or(lifecycle.clone()),
    })
}

fn sql_value(v: rusqlite::types::ValueRef<'_>) -> Value {
    use rusqlite::types::ValueRef::*;
    match v {
        Null => Value::Null,
        Integer(i) => json!(i),
        Real(f) => json!(f),
        Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        Blob(_) => Value::Null,
    }
}

fn open_app(path: &Path) -> Option<Connection> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .ok()
}

/// `_app_row`: one row from the application's log, or None. Never fails on a missing DB.
pub fn app_row(app_db: &Path, message_id: &str) -> Option<Value> {
    let id: i64 = message_id.trim().parse().ok()?;
    let conn = open_app(app_db)?;
    let columns = [
        "id",
        "role",
        "channel",
        "body",
        "delivery",
        "delivery_detail",
        "created_at",
        "answer_id",
        "answered_at",
    ];
    let mut row: Map<String, Value> = conn
        .query_row(
            "SELECT id, role, channel, body, delivery, delivery_detail, created_at, answer_id, answered_at FROM messages WHERE id = ?",
            [id],
            |r| Ok(columns.iter().enumerate().map(|(i, c)| (c.to_string(), sql_value(r.get_ref_unwrap(i)))).collect()),
        )
        .ok()?;
    let mut answer = Value::Null;
    if let Some(answer_id) = row.get("answer_id").and_then(Value::as_i64) {
        let reply = conn.query_row(
            "SELECT id, role, channel, body, created_at FROM messages WHERE id = ?",
            [answer_id],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get_ref_unwrap(4).as_str().ok().map(String::from),
                ))
            },
        );
        if let Ok((rid, role, channel, body, at)) = reply {
            let body = body.unwrap_or_default();
            let excerpt = body
                .trim()
                .lines()
                .next()
                .map(|l| l.chars().take(120).collect::<String>());
            answer = json!({"id": rid, "role": role, "channel": channel, "at": at, "excerpt": if body.trim().is_empty() { None } else { excerpt }});
        }
    }
    row.insert("answer".into(), answer);
    Some(Value::Object(row))
}

/// `app_messages`: recent application messages, newest first (what the owner chat renders).
pub fn app_messages(app_db: &Path, channel: Option<&str>, limit: i64) -> Value {
    let Some(conn) = open_app(app_db) else {
        return json!({"error": format!("application message log unavailable: unable to open database file {}", app_db.display()), "messages": []});
    };
    let columns = [
        "id",
        "role",
        "channel",
        "body",
        "delivery",
        "created_at",
        "answer_id",
        "answered_at",
    ];
    let (where_sql, params): (&str, Vec<rusqlite::types::Value>) =
        match channel.filter(|c| !c.is_empty()) {
            Some(c) => (
                " WHERE channel = ?",
                vec![c.to_string().into(), limit.into()],
            ),
            None => ("", vec![limit.into()]),
        };
    let sql = format!("SELECT id, role, channel, body, delivery, created_at, answer_id, answered_at FROM messages{where_sql} ORDER BY id DESC LIMIT ?");
    let rows: Result<Vec<Value>, rusqlite::Error> = (|| {
        let mut stmt = conn.prepare(&sql)?;
        let mapped = stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| {
            Ok(Value::Object(
                columns
                    .iter()
                    .enumerate()
                    .map(|(i, c)| (c.to_string(), sql_value(r.get_ref_unwrap(i))))
                    .collect(),
            ))
        })?;
        mapped.collect()
    })();
    match rows {
        Ok(rows) => json!({"count": rows.len(), "messages": rows, "channel": channel}),
        Err(error) => {
            json!({"error": format!("application message log unreadable: {error}"), "messages": []})
        }
    }
}

/// `git_merges`: recent merges for one repository, with its own as_of. Deterministic.
/// `git log` runs under the Python's 10 s deadline (`subprocess.run(..., timeout=10)`); a
/// stuck git is a named reason, never a blocked caller.
pub fn git_merges(repo: &str, limit: i64) -> Value {
    git_merges_with(repo, limit, "git", 10.0)
}

/// `git_merges` with the program and deadline as parameters (the test uses a fake git).
pub fn git_merges_with(repo: &str, limit: i64, program: &str, timeout_s: f64) -> Value {
    let path = expand_home(repo);
    if !Path::new(&path).join(".git").is_dir() {
        return json!({"repo": path, "merges": [], "as_of": null, "reason": format!("{path} is not a git repository")});
    }
    let n = format!("-n{limit}");
    let args = [
        "-C",
        path.as_str(),
        "log",
        "--merges",
        n.as_str(),
        "--format=%h%x1f%s%x1f%cI",
    ];
    let (code, stdout, stderr) = match crate::usage_providers::run_captured(
        program, &args, timeout_s,
    ) {
        Ok(out) => out,
        Err(error) => {
            return json!({"repo": path, "merges": [], "as_of": null, "reason": format!("git log failed: {error}")})
        }
    };
    if code != 0 {
        let err = if stderr.trim().is_empty() {
            "git log failed".to_string()
        } else {
            stderr.trim().chars().take(200).collect()
        };
        return json!({"repo": path, "merges": [], "as_of": null, "reason": err});
    }
    let merges: Vec<Value> = stdout
        .lines()
        .filter_map(|line| {
            let p: Vec<&str> = line.split('\u{1f}').collect();
            (p.len() == 3).then(|| json!({"sha": p[0], "subject": p[1], "at": p[2]}))
        })
        .collect();
    let as_of = merges
        .first()
        .map(|m| m["at"].clone())
        .unwrap_or(Value::Null);
    let reason = if merges.is_empty() {
        json!("no merge commits in this repository")
    } else {
        Value::Null
    };
    json!({"repo": path, "merges": merges, "as_of": as_of, "reason": reason})
}

fn expand_home(path: &str) -> String {
    match (path.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => path.to_string(),
    }
}

/// `message_state`: sent / delivered / seen / replied. A numeric id is an APPLICATION id
/// (answer link lives there); an inbox id ('i123') is looked up through `inbox_get`.
pub fn message_state(
    app_db: &Path,
    inbox_get: &dyn Fn(&str) -> Option<Value>,
    ids: &[String],
    now: f64,
) -> Value {
    let mut out = Vec::new();
    for id in ids {
        if !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) {
            match app_row(app_db, id) {
                None => out.push(json!({"id": id, "found": false, "source": "application", "reason": "no application row with this id"})),
                Some(row) => out.push(app_state(&row)),
            }
            continue;
        }
        let Some(row) = inbox_get(id).filter(|r| truthy(Some(r))) else {
            out.push(json!({"id": id, "found": false}));
            continue;
        };
        let created = row
            .get("created_at")
            .and_then(Value::as_f64)
            .filter(|c| *c != 0.0)
            .unwrap_or(now);
        let notify = row
            .get("notify_state")
            .and_then(Value::as_str)
            .unwrap_or("");
        let at = |key: &str| row.get(key).and_then(Value::as_f64).filter(|v| *v != 0.0);
        let id_out = row
            .get("id")
            .filter(|v| truthy(Some(v)))
            .or_else(|| row.get("message_id"))
            .cloned()
            .unwrap_or(Value::Null);
        out.push(json!({
            "id": id_out, "found": true, "source": "inbox",
            "mailbox": row.get("mailbox").cloned().unwrap_or(Value::Null),
            "sent": {"at": iso(Some(created)), "by": row.get("sender").cloned().unwrap_or(Value::Null)},
            // DELIVERED is about the NUDGE, not the mail: the mail is the row being read.
            "delivered": {"ok": notify == "notified", "state": row.get("notify_state").cloned().unwrap_or(Value::Null),
                          "reason": row.get("notify_reason").cloned().unwrap_or(Value::Null), "at": null,
                          "at_reason": "the store records the notify state, not when it changed"},
            "seen": {"at": iso_value(at("read_at")), "acked_at": iso_value(at("acked_at"))},
            "replied": cell(Value::Null, None, Some("the COS/VP application records its own ids; this inbox message has no application row (its log has no external_id for inbox ids)")),
            "state": row.get("state").cloned().unwrap_or(Value::Null),
            "age_s": (now - created) as i64,
        }));
    }
    json!({"as_of": iso(Some(now)), "messages": out})
}

/// `reply_stats`: time from a message landing to the lane reading it, last and median.
/// `rows` is `inbox.stats()` (one object per mailbox); `latencies(mailbox)` is
/// `_seen_latencies` (ascending). A true reply needs an answer link (null with a reason).
pub fn reply_stats(
    rows: &[Value],
    latencies: &dyn Fn(&str) -> Vec<f64>,
    wanted: &[String],
    now: f64,
) -> Value {
    let as_of = iso(Some(now));
    let mut out: Vec<Value> = Vec::new();
    for row in rows {
        let mailbox = row.get("mailbox").and_then(Value::as_str);
        if !wanted.is_empty() && !mailbox.is_some_and(|m| wanted.iter().any(|w| w == m)) {
            continue;
        }
        let l = latencies(mailbox.unwrap_or(""));
        let none_yet = if l.is_empty() {
            Some("no message has been read yet")
        } else {
            None
        };
        out.push(json!({
            "mailbox": mailbox,
            "seen_latency_s": cell(median(&l).map(|m| json!(m)).unwrap_or(Value::Null), as_of.as_deref(), none_yet),
            "seen_latency_last_s": cell(l.last().map(|v| json!(v)).unwrap_or(Value::Null), as_of.as_deref(), none_yet),
            "samples": l.len(),
            "reply_latency_s": cell(Value::Null, None, Some("the answer link lives in the application, not this store")),
        }));
    }
    out.sort_by(|a, b| {
        a["mailbox"]
            .as_str()
            .unwrap_or("")
            .cmp(b["mailbox"].as_str().unwrap_or(""))
    });
    json!({"as_of": as_of, "mailboxes": out})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn state(patch: Value) -> Value {
        let mut base = json!({"id": 1, "role": "owner", "channel": "vp", "created_at": "2026-10-24T00:00:00Z",
            "delivery": "acked", "delivery_detail": "Handled by agent", "answer_id": null, "answered_at": null, "answer": null});
        for (k, v) in patch.as_object().unwrap() {
            base[k] = v.clone();
        }
        app_state(&base)
    }

    #[test]
    fn replied_requires_answer_id_not_answered_at() {
        let st = state(json!({"answered_at": "2026-10-24T00:10:00Z", "answer_id": null}));
        assert_eq!(st["replied"]["reason"], "awaiting reply (no answer_id yet)");
        assert_eq!(
            st["seen"]["at"], "2026-10-24T00:10:00Z",
            "answered_at is 'picked up' and belongs under seen"
        );
    }

    #[test]
    fn replied_carries_the_reply_row_and_its_time() {
        let st = state(
            json!({"answer_id": 99, "answered_at": "2026-10-24T00:10:00Z", "answer": {"at": "2026-10-24T00:20:00Z", "excerpt": "done"}}),
        );
        assert_eq!(st["replied"]["at"], "2026-10-24T00:20:00Z");
        assert_eq!(st["replied"]["reply_id"], 99);
        assert_eq!(st["replied"]["reply_excerpt"], "done");
    }

    #[test]
    fn an_answer_id_with_a_missing_reply_row_says_so() {
        let st = state(json!({"answer_id": 99, "answer": null}));
        assert!(st["replied"]["at"].is_null());
        assert!(st["replied"]["reason"]
            .as_str()
            .unwrap()
            .contains("reply row is missing"));
    }

    #[test]
    fn the_delivery_column_is_split_by_role() {
        let owner = state(json!({"role": "owner", "delivery": "acked"}));
        assert_eq!(owner["delivered"]["transport"], "acked");
        assert!(owner["delivered"]["reply_lifecycle"].is_null());
        let agent = state(json!({"role": "agent", "delivery": "posted"}));
        assert_eq!(agent["delivered"]["reply_lifecycle"], "posted");
        assert!(agent["delivered"]["transport"].is_null());
    }

    #[test]
    fn an_unexpected_state_is_null_with_a_reason() {
        let st = state(json!({"role": "owner", "delivery": "posted"}));
        assert!(st["delivered"]["transport"].is_null());
        assert_eq!(
            st["delivered"]["reason"],
            "unexpected transport state 'posted'"
        );
        assert_eq!(
            state(json!({"role": "agent", "delivery": ""}))["delivered"]["reason"],
            "no reply state recorded"
        );
    }

    #[test]
    fn a_non_repo_is_null_with_a_reason() {
        let dir = std::env::temp_dir().join(format!("ds-views-norepo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = git_merges(dir.to_str().unwrap(), 10);
        assert!(out["as_of"].is_null());
        assert!(out["reason"]
            .as_str()
            .unwrap()
            .contains("is not a git repository"));
        assert_eq!(out["merges"], json!([]));
    }

    #[test]
    fn a_stuck_git_times_out_with_the_pythons_reason() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("ds-views-stuck-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let fake = dir.join("fakegit");
        std::fs::write(&fake, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let started = std::time::Instant::now();
        let out = git_merges_with(dir.to_str().unwrap(), 10, fake.to_str().unwrap(), 0.3);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "the deadline killed the fake git"
        );
        assert!(out["merges"].as_array().unwrap().is_empty());
        assert!(out["as_of"].is_null());
        let reason = out["reason"].as_str().unwrap();
        assert!(
            reason.starts_with("git log failed: Command '[")
                && reason.ends_with("timed out after 0.3 seconds"),
            "{reason}"
        );
    }

    #[test]
    fn a_missing_git_binary_is_a_named_reason() {
        let dir = std::env::temp_dir().join(format!("ds-views-nogit-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let out = git_merges_with(dir.to_str().unwrap(), 10, "/nonexistent/git", 5.0);
        assert!(out["reason"]
            .as_str()
            .unwrap()
            .starts_with("git log failed: "));
    }

    #[test]
    fn merges_are_read_from_a_real_repository() {
        let dir = std::env::temp_dir().join(format!("ds-views-repo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                ok.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&ok.stderr)
            );
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["commit", "-q", "--allow-empty", "-m", "base"]);
        git(&["checkout", "-q", "-b", "feat"]);
        git(&["commit", "-q", "--allow-empty", "-m", "work"]);
        git(&["checkout", "-q", "main"]);
        git(&["commit", "-q", "--allow-empty", "-m", "main moves"]);
        git(&["merge", "-q", "--no-ff", "feat", "-m", "Merge feat"]);
        let out = git_merges(dir.to_str().unwrap(), 10);
        assert_eq!(out["merges"].as_array().unwrap().len(), 1);
        assert_eq!(out["merges"][0]["subject"], "Merge feat");
        assert_eq!(out["as_of"], out["merges"][0]["at"]);
        assert!(out["reason"].is_null());
    }

    #[test]
    fn iso_and_median_follow_the_python() {
        assert_eq!(iso(Some(86400.0)).as_deref(), Some("1970-01-02T00:00:00Z"));
        assert_eq!(iso(Some(0.0)), None);
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[1.0, 2.0]), Some(1.5));
        assert_eq!(
            median(&[0.25]),
            Some(0.2),
            "round(0.25, 1) is 0.2 in Python (half to even)"
        );
    }

    #[test]
    fn message_state_reports_each_id_from_the_store_that_knows_it() {
        let missing = Path::new("/nonexistent/app.sqlite3");
        let inbox = |id: &str| -> Option<Value> {
            (id == "i1").then(|| json!({"id": "i1", "mailbox": "a", "sender": "b", "created_at": 100.0, "notify_state": "notified", "read_at": 160.0, "state": "read"}))
        };
        let ids: Vec<String> = ["7", "i1", "i9"].iter().map(|s| s.to_string()).collect();
        let out = message_state(missing, &inbox, &ids, 200.0);
        let m = out["messages"].as_array().unwrap();
        assert_eq!(
            m[0],
            json!({"id": "7", "found": false, "source": "application", "reason": "no application row with this id"})
        );
        assert_eq!(m[1]["delivered"]["ok"], true);
        assert_eq!(m[1]["seen"]["at"], "1970-01-01T00:02:40Z");
        assert!(m[1]["seen"]["acked_at"].is_null());
        assert_eq!(m[1]["age_s"], 100);
        assert_eq!(m[2], json!({"id": "i9", "found": false}));
    }

    #[test]
    fn reply_stats_filters_sorts_and_states_what_it_lacks() {
        let rows = vec![
            json!({"mailbox": "b"}),
            json!({"mailbox": "a"}),
            json!({"mailbox": "c"}),
        ];
        let lat = |m: &str| {
            if m == "a" {
                vec![1.0, 3.0, 5.0]
            } else {
                vec![]
            }
        };
        let out = reply_stats(&rows, &lat, &["a".into(), "b".into()], 0.0);
        let boxes = out["mailboxes"].as_array().unwrap();
        assert_eq!(
            boxes
                .iter()
                .map(|b| b["mailbox"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(boxes[0]["seen_latency_s"]["value"], 3.0);
        assert_eq!(boxes[0]["seen_latency_last_s"]["value"], 5.0);
        assert_eq!(boxes[0]["samples"], 3);
        assert_eq!(
            boxes[1]["seen_latency_s"]["reason"],
            "no message has been read yet"
        );
        assert!(boxes[1]["reply_latency_s"]["value"].is_null());
    }

    #[test]
    fn app_rows_are_read_from_the_applications_database() {
        let dir = std::env::temp_dir().join(format!("ds-views-app-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("messages.sqlite3");
        let _ = std::fs::remove_file(&path);
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, role TEXT, channel TEXT, body TEXT, delivery TEXT, delivery_detail TEXT, created_at TEXT, answer_id INTEGER, answered_at TEXT);
             INSERT INTO messages VALUES (1, 'owner', 'vp', 'q', 'acked', NULL, '2026-10-24T00:00:00Z', 2, '2026-10-24T00:10:00Z');
             INSERT INTO messages VALUES (2, 'agent', 'vp', E'first line\nsecond', 'posted', NULL, '2026-10-24T00:20:00Z', NULL, NULL);".replace("E'first line\nsecond'", "'first line\nsecond'").as_str(),
        ).unwrap();
        drop(conn);
        let row = app_row(&path, "1").unwrap();
        assert_eq!(row["answer"]["excerpt"], "first line");
        let st = app_state(&row);
        assert_eq!(st["replied"]["at"], "2026-10-24T00:20:00Z");
        assert!(app_row(&path, "99").is_none());
        let listed = app_messages(&path, Some("vp"), 10);
        assert_eq!(listed["count"], 2);
        assert_eq!(listed["messages"][0]["id"], 2, "newest first");
        assert!(
            app_messages(Path::new("/nonexistent/x.sqlite3"), None, 5)["error"]
                .as_str()
                .unwrap()
                .contains("unavailable")
        );
    }
}
