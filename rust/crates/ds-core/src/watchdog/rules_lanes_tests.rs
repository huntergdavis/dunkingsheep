//! Ports of the lane tests of `tests/test_dunk_watchdog.py` and the rule-level tests of
//! `tests/test_dunk_watchdog_starvation.py` (t1400 slice E). The Python runs them through `evaluate`
//! and `run` (later slices), so each calls the rule on the same fixture; the expectations are the
//! Python's. The stores are real SQLite files with the real schemas. The delivery/run tests of the
//! starvation file (hold, group delivery, dedupe, partial retry, unprotected recipient, dry run) and
//! the daemon-inventory test belong to those later slices.

use std::path::PathBuf;

use rusqlite::Connection;
use serde_json::{json, Value};

use super::*;

const NOW: f64 = 1_800_000_000.0;
const MIN: f64 = 60.0;
const PATTERN: &str = "^w[0-9A-Za-z]+:p[0-9A-Za-z]+$";

struct Fleet {
    dir: tempfile::TempDir,
}

impl Fleet {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let run = |db: &str, sql: &str| {
            Connection::open(dir.path().join(db))
                .unwrap()
                .execute(sql, [])
                .unwrap();
        };
        run(
            "tasks.db",
            "CREATE TABLE tasks (id TEXT PRIMARY KEY, scope TEXT NOT NULL, title TEXT NOT NULL, \
            body TEXT NOT NULL DEFAULT '', status TEXT NOT NULL DEFAULT 'open', \
            priority TEXT NOT NULL DEFAULT 'medium', parent_id TEXT, \
            multi_assignee INTEGER NOT NULL DEFAULT 0, attempts INTEGER NOT NULL DEFAULT 0, \
            assignees TEXT NOT NULL DEFAULT '[]', depends_on TEXT NOT NULL DEFAULT '[]', \
            created_at REAL NOT NULL, updated_at REAL NOT NULL, created_by TEXT, \
            completed_by TEXT, completed_at REAL, cancelled_at REAL, block_reason TEXT)",
        );
        run("tasks.db", "CREATE TABLE task_leases (task_id TEXT NOT NULL, pane_id TEXT NOT NULL, \
            claimed_at REAL NOT NULL, heartbeat_at REAL NOT NULL, expires_at REAL NOT NULL, \
            caller_pid INTEGER, caller_start TEXT, caller_name TEXT, PRIMARY KEY (task_id, pane_id))");
        run(
            "tasks.db",
            "CREATE TABLE task_events (id INTEGER PRIMARY KEY AUTOINCREMENT, \
            task_id TEXT NOT NULL, event TEXT NOT NULL, at REAL NOT NULL, actor TEXT, scope TEXT, \
            priority TEXT, status_from TEXT, status_to TEXT, duration_ms INTEGER, meta TEXT)",
        );
        run("inbox.db", "CREATE TABLE messages (id TEXT PRIMARY KEY, mailbox TEXT NOT NULL, \
            mailbox_label TEXT, pane_id TEXT, sender TEXT, subject TEXT, body TEXT NOT NULL, \
            state TEXT NOT NULL, created_at REAL NOT NULL, read_at REAL, acked_at REAL, \
            cancelled_at REAL, reason TEXT, notify_state TEXT, notify_reason TEXT, archived_at REAL)");
        run(
            "metrics.db",
            "CREATE TABLE events (id INTEGER PRIMARY KEY AUTOINCREMENT, at REAL NOT NULL, \
            kind TEXT NOT NULL, source TEXT, pane_id TEXT, reason TEXT)",
        );
        Fleet { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn sql(&self, db: &str, sql: &str, params: &[&dyn rusqlite::ToSql]) {
        Connection::open(self.path(db))
            .unwrap()
            .execute(sql, params)
            .unwrap();
    }

    fn task(
        &self,
        id: &str,
        status: &str,
        updated_at: f64,
        assignees: &[&str],
        reason: Option<&str>,
    ) {
        self.sql(
            "tasks.db",
            "INSERT INTO tasks (id, scope, title, body, status, assignees, created_at, updated_at, \
             block_reason) VALUES (?, 'global', 't', '', ?, ?, ?, ?, ?)",
            &[&id, &status, &json!(assignees).to_string(), &updated_at, &updated_at, &reason],
        );
    }

    fn task_event(&self, task: &str, event: &str, at: f64, actor: Option<&str>) {
        self.sql(
            "tasks.db",
            "INSERT INTO task_events (task_id, event, at, actor) VALUES (?,?,?,?)",
            &[&task, &event, &at, &actor],
        );
    }

    fn lease(&self, task: &str, pane: &str, claimed: f64, heartbeat: f64, expires: f64) {
        self.sql(
            "tasks.db",
            "INSERT INTO task_leases (task_id, pane_id, claimed_at, heartbeat_at, expires_at) \
             VALUES (?,?,?,?,?)",
            &[&task, &pane, &claimed, &heartbeat, &expires],
        );
    }

    fn message(
        &self,
        id: &str,
        sender: Option<&str>,
        created_at: f64,
        pane: Option<&str>,
        read_at: Option<f64>,
    ) {
        self.sql(
            "inbox.db",
            "INSERT INTO messages (id, mailbox, pane_id, sender, body, state, created_at, read_at) \
             VALUES (?,?,?,?,?,?,?,?)",
            &[&id, &"x", &pane, &sender, &"BODY", &"read", &created_at, &read_at],
        );
    }

    fn metric(&self, kind: &str, pane: &str, at: f64) {
        self.sql(
            "metrics.db",
            "INSERT INTO events (at, kind, pane_id, source) VALUES (?,?,?,?)",
            &[&at, &kind, &pane, &"pi-extension"],
        );
    }

    fn tasks(&self) -> TaskData {
        TaskData::load(&self.path("tasks.db"), NOW).unwrap()
    }

    fn inbox(&self) -> (PathBuf, BTreeMap<String, String>) {
        let path = self.path("inbox.db");
        let names = InboxDb { path: &path }.name_to_pane(NOW).unwrap();
        (path, names)
    }
}

fn panes(list: &[(&str, &str)]) -> Panes {
    list.iter()
        .map(|(p, s)| (p.to_string(), s.to_string()))
        .collect()
}

fn keys(found: &[Value]) -> Vec<(String, String)> {
    found
        .iter()
        .map(|f| {
            (
                f["rule"].as_str().unwrap().to_string(),
                f["key"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[test]
fn test_lane_rules() {
    let fleet = Fleet::new();
    let p = panes(&[
        ("w1:p1", "working"),
        ("w1:p2", "working"),
        ("w1:p3", "idle"),
        ("w1:p4", "idle"),
    ]);
    fleet.task_event("tx", "status", NOW - 59.0 * MIN, Some("w1:p2"));
    fleet.task("t3", "in_progress", NOW - 99.0 * MIN, &["w1:p3"], None);
    fleet.task("t4", "in_progress", NOW - 99.0 * MIN, &["w1:p4"], None);
    fleet.message("i1", Some("w1:p4"), NOW - 59.0 * MIN, None, None);
    let data = fleet.tasks();
    let (path, names) = fleet.inbox();
    let inbox = InboxDb { path: &path };
    let working = json!({"severity": "warning", "class": "ops", "quiet_minutes": 60});
    let idle = json!({"severity": "warning", "class": "ops", "quiet_minutes": 60,
                      "idle_statuses": ["idle"], "holding_statuses": ["in_progress"]});
    let mut found =
        rule_lane_idle_holding_task(Some(&p), &data, &inbox, &names, PATTERN, &idle, NOW).unwrap();
    found.extend(
        rule_lane_working_without_task(Some(&p), &data, &names, PATTERN, &working, NOW).unwrap(),
    );
    let mut got = keys(&found);
    got.sort();
    assert_eq!(
        vec![
            ("lane.idle_holding_task".to_string(), "t3@w1:p3".to_string()),
            ("lane.working_without_task".to_string(), "w1:p1".to_string())
        ],
        got
    );
    assert_eq!(
        "pane w1:p3 holds task t3 while idle with no activity",
        found[0]["summary"]
    );
    assert_eq!("idle, no activity", found[0]["measured"]);
    assert_eq!("60min quiet", found[0]["threshold"]);
}

fn starve_rule(overrides: Value) -> Value {
    let mut rule = json!({"severity": "warning", "class": "ops", "worker_panes": ["%55", "%56"],
        "superuser_panes": ["%2", "%15"], "quiet_minutes": 10, "min_empty_workers": 2,
        "must_hold_runs": 2});
    for (k, v) in overrides.as_object().unwrap() {
        rule[k] = v.clone();
    }
    rule
}

/// `setup_rule` of the starvation suite: three panes and one old blocked global task.
fn setup() -> (Fleet, Panes) {
    let fleet = Fleet::new();
    fleet.task(
        "t1",
        "blocked",
        NOW - 60.0 * MIN,
        &["%33"],
        Some("Owner approval required"),
    );
    (
        fleet,
        panes(&[("%55", "idle"), ("%56", "done"), ("%15", "working")]),
    )
}

fn evaluate(fleet: &Fleet, p: &Panes, rule: &Value) -> Vec<Value> {
    let since = NOW - rule["quiet_minutes"].as_f64().unwrap() * 60.0;
    let snapshot = Starvation::load(&fleet.path("tasks.db"), NOW, since).unwrap();
    let metrics = fleet.path("metrics.db");
    rule_fleet_assignment_starvation(
        &snapshot,
        &|| turn_panes(&metrics, since, NOW),
        &|| Ok(Some(p.clone())),
        rule,
        NOW,
    )
    .unwrap()
}

#[test]
fn test_blocked_backlog_with_empty_workers_even_while_reviews_complete() {
    let (fleet, p) = setup();
    fleet.task_event("review", "completed", NOW - MIN, Some("%31"));
    let found = evaluate(&fleet, &p, &starve_rule(json!({})));
    assert_eq!(1, found.len());
    let evidence = &found[0]["evidence"];
    assert_eq!(json!(["%55", "%56"]), evidence["idle_workers"]);
    assert_eq!(json!(["t1"]), evidence["blocked_tasks"]);
    assert_eq!(json!(1), evidence["done_records_in_window"]);
    assert!(found[0]["summary"]
        .as_str()
        .unwrap()
        .contains("preserve owner holds"));
    assert_eq!("2 empty queues quiet 10min", found[0]["threshold"]);
    assert_eq!("2 idle; 0 unknown; 0 open; 1 blocked", found[0]["measured"]);
    assert_eq!("last 10min", found[0]["window"]);
}

#[test]
fn test_busy_unknown_missing_recent_and_assigned_do_not_count() {
    let (fleet, mut p) = setup();
    let rule = starve_rule(json!({}));
    for status in ["working", "blocked"] {
        p.insert("%55".into(), status.into());
        assert!(evaluate(&fleet, &p, &rule).is_empty(), "{status}");
    }
    p.insert("%55".into(), "idle".into());
    fleet.metric("turn.end", "%55", NOW - MIN);
    assert!(
        evaluate(&fleet, &p, &rule).is_empty(),
        "a recent turn makes the pane busy"
    );
    fleet.sql("metrics.db", "DELETE FROM events", &[]);
    fleet.lease("other", "%55", NOW - 2.0 * MIN, NOW - MIN, NOW + MIN);
    assert!(
        evaluate(&fleet, &p, &rule).is_empty(),
        "a live lease makes the pane busy"
    );
    fleet.sql("tasks.db", "DELETE FROM task_leases", &[]);
    fleet.task("ready", "open", NOW - MIN, &["%55"], None);
    assert!(
        evaluate(&fleet, &p, &rule).is_empty(),
        "assigned work is not an empty queue"
    );
}

#[test]
fn test_no_backlog_private_backlog_and_fresh_backlog_do_not_alert() {
    let (fleet, p) = setup();
    let rule = starve_rule(json!({}));
    fleet.sql("tasks.db", "UPDATE tasks SET status='done'", &[]);
    assert!(evaluate(&fleet, &p, &rule).is_empty());
    fleet.sql(
        "tasks.db",
        "UPDATE tasks SET status='blocked',scope='%33'",
        &[],
    );
    assert!(evaluate(&fleet, &p, &rule).is_empty());
    fleet.sql(
        "tasks.db",
        "UPDATE tasks SET scope='global',created_at=?",
        &[&(NOW - MIN)],
    );
    assert!(evaluate(&fleet, &p, &rule).is_empty());
}

#[test]
fn test_future_events_and_expired_leases_do_not_mask_current_starvation() {
    let (fleet, p) = setup();
    fleet.lease("old", "%55", NOW - 60.0 * MIN, NOW - 40.0 * MIN, NOW - MIN);
    fleet.task_event("future", "claimed", NOW + MIN, Some("%55"));
    fleet.metric("turn.start", "%55", NOW + MIN);
    assert_eq!(1, evaluate(&fleet, &p, &starve_rule(json!({}))).len());
}

#[test]
fn test_unknown_is_visibility_gap_not_idle_and_missing_pane_is_not_capacity() {
    let (fleet, mut p) = setup();
    let rule = starve_rule(json!({}));
    p.insert("%55".into(), "unknown".into()); // the pane source maps a null status to "unknown"
    let found = evaluate(&fleet, &p, &rule);
    let evidence = &found[0]["evidence"];
    assert_eq!(json!(["%56"]), evidence["idle_workers"]);
    assert_eq!(json!(["%55"]), evidence["activity_unknown_workers"]);
    p.remove("%55");
    assert!(evaluate(&fleet, &p, &rule).is_empty());
}

#[test]
fn test_private_assigned_work_suppresses_empty_queue() {
    let (fleet, p) = setup();
    fleet.task("private", "open", NOW - 60.0 * MIN, &["%55"], None);
    fleet.sql(
        "tasks.db",
        "UPDATE tasks SET scope='%55' WHERE id='private'",
        &[],
    );
    assert!(evaluate(&fleet, &p, &starve_rule(json!({}))).is_empty());
}

// -- extra: the branches the Python suites do not name -----------------------------------------

#[test]
fn extra_idle_holder_is_quiet_when_it_has_read_or_sent_mail_or_has_no_known_status() {
    let fleet = Fleet::new();
    fleet.task("t3", "in_progress", NOW - 99.0 * MIN, &["w1:p3"], None);
    fleet.task("t4", "in_progress", NOW - 99.0 * MIN, &["w1:p4"], None);
    fleet.task("t5", "in_progress", NOW - 99.0 * MIN, &["w1:p5"], None);
    fleet.message(
        "i1",
        None,
        NOW - 200.0 * MIN,
        Some("w1:p3"),
        Some(NOW - 5.0 * MIN),
    ); // a recent READ
    let p = panes(&[("w1:p3", "idle"), ("w1:p4", "idle")]); // w1:p5 is not in the inventory
    let data = fleet.tasks();
    let (path, names) = fleet.inbox();
    let rule = json!({"severity": "warning", "class": "ops", "quiet_minutes": 60,
                      "idle_statuses": ["idle"], "holding_statuses": ["in_progress"]});
    let found = rule_lane_idle_holding_task(
        Some(&p),
        &data,
        &InboxDb { path: &path },
        &names,
        PATTERN,
        &rule,
        NOW,
    )
    .unwrap();
    assert_eq!(
        vec![("lane.idle_holding_task".to_string(), "t4@w1:p4".to_string())],
        keys(&found)
    );
}

#[test]
fn extra_a_lease_or_a_mailbox_name_assignee_makes_a_pane_a_holder() {
    let fleet = Fleet::new();
    fleet.message("i1", None, NOW - 500.0 * MIN, Some("w1:p9"), None);
    // 'codex' is a mailbox NAME resolving to w1:p9 through the inbox's routing history
    fleet.sql("inbox.db", "UPDATE messages SET mailbox='codex'", &[]);
    fleet.task("t1", "in_progress", NOW, &["codex"], None);
    fleet.task("t2", "in_progress", NOW, &[], None);
    fleet.lease("t2", "w1:p8", NOW - MIN, NOW - MIN, NOW + MIN);
    let data = fleet.tasks();
    let (_, names) = fleet.inbox();
    let held = holders(&data, &names, PATTERN, &["in_progress"]).unwrap();
    assert_eq!(
        vec![
            ("t1".to_string(), vec!["w1:p9".to_string()]),
            ("t2".to_string(), vec!["w1:p8".to_string()])
        ],
        held
    );
}

#[test]
fn extra_a_disabled_pane_source_is_an_error_like_the_pythons_attribute_error() {
    let fleet = Fleet::new();
    let data = fleet.tasks();
    let (path, names) = fleet.inbox();
    let rule = json!({"severity": "warning", "class": "ops", "quiet_minutes": 60,
                      "idle_statuses": ["idle"], "holding_statuses": ["in_progress"]});
    assert_eq!(
        "'NoneType' object has no attribute 'items'",
        rule_lane_working_without_task(None, &data, &names, PATTERN, &rule, NOW)
            .unwrap_err()
            .to_string()
    );
    assert_eq!(
        "'NoneType' object has no attribute 'get'",
        rule_lane_idle_holding_task(
            None,
            &data,
            &InboxDb { path: &path },
            &names,
            PATTERN,
            &rule,
            NOW
        )
        .unwrap_err()
        .to_string()
    );
}

#[test]
fn extra_starvation_reads_panes_and_metrics_only_when_there_is_a_backlog() {
    let empty = Starvation::default();
    let boom = || Err(DunkError::Refused("must not be read".into()));
    let found = rule_fleet_assignment_starvation(
        &empty,
        &boom,
        &|| Err(DunkError::Refused("no".into())),
        &starve_rule(json!({})),
        NOW,
    );
    assert!(
        found.unwrap().is_empty(),
        "no backlog: nothing is read, nothing can fail"
    );
    // with a backlog, an unreadable pane source is reported, never swallowed
    let (fleet, _) = setup();
    let snapshot = Starvation::load(&fleet.path("tasks.db"), NOW, NOW - 600.0).unwrap();
    let error = rule_fleet_assignment_starvation(
        &snapshot,
        &|| Ok(vec![]),
        &|| {
            Err(DunkError::Refused(
                "daemon pane inventory is unavailable".into(),
            ))
        },
        &starve_rule(json!({})),
        NOW,
    )
    .unwrap_err();
    assert_eq!("daemon pane inventory is unavailable", error.to_string());
}

#[test]
fn extra_superusers_are_never_counted_as_empty_workers_and_min_empty_workers_gates() {
    let (fleet, p) = setup();
    // %15 is working AND a superuser; make the superuser list cover %56 as well
    let covered = starve_rule(json!({"superuser_panes": ["%2", "%56"]}));
    assert!(
        evaluate(&fleet, &p, &covered).is_empty(),
        "only %55 is a worker now: below 2"
    );
    let one = starve_rule(json!({"superuser_panes": ["%2", "%56"], "min_empty_workers": 1}));
    assert_eq!(1, evaluate(&fleet, &p, &one).len());
}

#[test]
fn extra_a_malformed_assignees_value_is_an_error_not_a_pass() {
    let (fleet, _) = setup();
    fleet.sql("tasks.db", "UPDATE tasks SET assignees='{not json'", &[]);
    assert!(Starvation::load(&fleet.path("tasks.db"), NOW, NOW - 600.0).is_err());
}
