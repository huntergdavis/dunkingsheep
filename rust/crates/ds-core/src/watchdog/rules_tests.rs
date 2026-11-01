//! Ports of the task-rule tests of tests/test_dunk_watchdog.py (RuleBoundaryTests) plus the
//! `Inputs.tasks` loader. The other rules, config validation and the escalation tests are
//! NOT ported in this slice.

use super::rules::*;
use super::*;
use serde_json::json;

const NOW: f64 = 1_800_000_000.0;
const MIN: f64 = 60.0;
const HOUR: f64 = 3600.0;

fn blocked_rule(threshold: f64) -> Value {
    json!({"severity": "warning", "class": "ops", "threshold_minutes": threshold as i64})
}

fn lease_rule(never: i64, lookback: i64, fresh: i64) -> Value {
    json!({"severity": "warning", "class": "ops", "never_renewed_minutes": never,
           "lookback_minutes": lookback, "fresh_minutes": fresh})
}

fn task(id: &str, status: &str, updated_at: f64, block_reason: Option<&str>) -> Value {
    json!({"id": id, "scope": "global", "status": status, "updated_at": updated_at,
           "assignees": [], "block_reason": block_reason})
}

fn lease(task: &str, pane: &str, claimed: f64, heartbeat: f64, expires: f64) -> Value {
    json!({"task_id": task, "pane_id": pane, "claimed_at": claimed,
           "heartbeat_at": heartbeat, "expires_at": expires})
}

fn event(id: i64, task: &str, name: &str, at: f64, meta: Option<Value>) -> Value {
    json!({"id": id, "task_id": task, "event": name, "at": at,
           "meta": meta.map(|m| m.to_string())})
}

fn keys(findings: &[Value]) -> Vec<(String, String)> {
    findings
        .iter()
        .map(|f| {
            (
                f["rule"].as_str().unwrap().to_string(),
                f["key"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn pair(rule: &str, key: &str) -> (String, String) {
    (rule.to_string(), key.to_string())
}

#[test]
fn a_task_held_for_the_owner_does_not_page_but_is_counted() {
    let data = TaskData {
        tasks: vec![
            task(
                "held",
                "blocked",
                NOW - 5.0 * 24.0 * HOUR,
                Some("HELD FOR OWNER DECISION: A2 needs 3-7 days of funding"),
            ),
            task("stale", "blocked", NOW - 2.0 * HOUR, None),
        ],
        ..Default::default()
    };
    let findings = rule_task_blocked_stale(&data, &blocked_rule(60.0), NOW);
    let paged: Vec<_> = findings
        .iter()
        .filter(|f| f["key"] != "held-owner")
        .collect();
    assert_eq!(
        vec!["stale"],
        paged
            .iter()
            .map(|f| f["key"].as_str().unwrap())
            .collect::<Vec<_>>()
    );
    let digest: Vec<_> = findings
        .iter()
        .filter(|f| f["key"] == "held-owner")
        .collect();
    assert_eq!(1, digest.len());
    assert!(digest[0]["summary"]
        .as_str()
        .unwrap()
        .contains("1 task(s) held on owner decisions"));
    assert_eq!("info", digest[0]["severity"], "the digest never pages");
    assert_eq!("digest", digest[0]["source"]);
    assert_eq!("5.0d", digest[0]["measured"]);
    assert_eq!(json!(5.0), digest[0]["evidence"]["oldest_days"]);
}

#[test]
fn the_hold_marker_is_declared_not_prose() {
    let data = TaskData {
        tasks: vec![task(
            "prose",
            "blocked",
            NOW - 2.0 * HOUR,
            Some("waiting for the owner to decide"),
        )],
        ..Default::default()
    };
    let findings = rule_task_blocked_stale(&data, &blocked_rule(60.0), NOW);
    assert_eq!(vec![pair("task.blocked_stale", "prose")], keys(&findings));
    // The marker is matched case-insensitively after trimming, as `_is_held` does.
    assert!(is_held(&task(
        "x",
        "blocked",
        0.0,
        Some("  held for owner decision: y")
    )));
    assert!(!is_held(&task("x", "blocked", 0.0, None)));
}

#[test]
fn task_blocked_stale_fires_at_its_boundary_and_a_later_event_resets_it() {
    let mut events = vec![event(1, "t3", "status", NOW - 10.0 * MIN, None)];
    events.reverse();
    let data = TaskData {
        tasks: vec![
            task("t1", "blocked", NOW - 60.0 * MIN, None),
            task("t2", "blocked", NOW - 60.0 * MIN + 1.0, None),
            task("t3", "blocked", NOW - 120.0 * MIN, None),
            task("t4", "open", NOW - 999.0 * MIN, None),
        ],
        events,
        ..Default::default()
    };
    let findings = rule_task_blocked_stale(&data, &blocked_rule(60.0), NOW);
    assert_eq!(vec![pair("task.blocked_stale", "t1")], keys(&findings));
    let f = &findings[0];
    assert_eq!("60min", f["threshold"]);
    assert_eq!("60.0min", f["measured"]);
    assert_eq!("since last activity", f["window"]);
    assert_eq!(json!(false), f["evidence"]["block_reason_set"]);
    assert_eq!(
        json!({"task": "t1", "assignees": [], "block_reason_set": false}),
        f["evidence"]
    );
    assert!(f.get("once").is_none());
}

#[test]
fn task_lease_expired_unrenewed_and_reaped() {
    let data = TaskData {
        leases: vec![
            lease("a", "w1:p1", NOW - 99.0 * MIN, NOW - 50.0 * MIN, NOW),
            lease("b", "w1:p1", NOW - 30.0 * MIN, NOW - 30.0 * MIN, NOW + HOUR),
            lease("c", "w1:p1", NOW - 29.0 * MIN, NOW - 29.0 * MIN, NOW + HOUR),
            lease("d", "w1:p1", NOW - 99.0 * MIN, NOW - 1.0 * MIN, NOW + HOUR),
        ],
        events: vec![
            event(
                1,
                "e",
                "requeued",
                NOW - 60.0 * MIN,
                Some(json!({"reason": "expired"})),
            ),
            event(
                2,
                "f",
                "requeued",
                NOW - 61.0 * MIN,
                Some(json!({"reason": "expired"})),
            ),
            event(
                3,
                "g",
                "released",
                NOW - 5.0 * MIN,
                Some(json!({"reason": "manual"})),
            ),
        ],
        ..Default::default()
    };
    let mut findings = rule_task_lease_stale(&data, &lease_rule(30, 60, 60), NOW);
    sort_findings(&mut findings);
    assert_eq!(
        vec![
            pair("task.lease_stale", "expired:a@w1:p1"),
            pair("task.lease_stale", "reaped:1"),
            pair("task.lease_stale", "unrenewed:b@w1:p1"),
        ],
        keys(&findings)
    );
    let expired = findings
        .iter()
        .find(|f| f["key"] == "expired:a@w1:p1")
        .unwrap();
    assert_eq!("expired 0.0min ago", expired["measured"]);
    let reaped = findings.iter().find(|f| f["key"] == "reaped:1").unwrap();
    assert_eq!(json!(true), reaped["once"], "a reap is reported once ever");
    assert_eq!("60.0min old", reaped["measured"]);
    assert_eq!("60min fresh window", reaped["window"]);
    let unrenewed = findings
        .iter()
        .find(|f| f["key"] == "unrenewed:b@w1:p1")
        .unwrap();
    assert_eq!("30min", unrenewed["threshold"]);
    assert_eq!("30.0min", unrenewed["measured"]);
}

#[test]
fn old_reap_ages_out_of_the_fresh_window() {
    let data = TaskData {
        events: vec![event(
            1,
            "e",
            "requeued",
            NOW - 90.0 * MIN,
            Some(json!({"reason": "expired"})),
        )],
        ..Default::default()
    };
    assert!(
        rule_task_lease_stale(&data, &lease_rule(30, 1440, 60), NOW).is_empty(),
        "a 90-minute-old reap is history, not news"
    );
}

#[test]
fn a_pane_gone_reap_counts_and_a_reasonless_one_does_not() {
    let data = TaskData {
        events: vec![
            event(
                1,
                "e",
                "released",
                NOW - MIN,
                Some(json!({"reason": "pane_gone"})),
            ),
            event(2, "f", "released", NOW - MIN, None),
        ],
        ..Default::default()
    };
    let findings = rule_task_lease_stale(&data, &lease_rule(30, 60, 60), NOW);
    assert_eq!(vec![pair("task.lease_stale", "reaped:1")], keys(&findings));
}

#[test]
fn minutes_rounds_half_to_even_like_python() {
    // Python: round(15 / 60.0, 1) == 0.2 (0.25 is exact, ties go to even); round(0.35, 1) == 0.3
    // (0.35 is below the tie in binary); a naive (x * 10).round() / 10 gives 0.3 for the first.
    assert_eq!(0.2, minutes(15.0));
    assert_eq!(0.3, minutes(21.0));
    assert_eq!(60.0, minutes(3600.0));
    assert_eq!("60.0", py_float(minutes(3600.0)));
    assert_eq!("0.01", py_number(&json!(0.01)));
    assert_eq!("60", py_number(&json!(60)));
    assert_eq!("60.0", py_number(&json!(60.0)));
}

#[test]
fn findings_sort_by_severity_then_rule_then_key() {
    let mk = |sev: &str, rule: &str, key: &str| json!({"severity": sev, "rule": rule, "key": key});
    let mut list = vec![
        mk("info", "a", "1"),
        mk("critical", "z", "9"),
        mk("warning", "b", "2"),
        mk("warning", "b", "1"),
        mk("weird", "a", "0"),
    ];
    sort_findings(&mut list);
    let order: Vec<_> = list
        .iter()
        .map(|f| {
            format!(
                "{}{}",
                f["severity"].as_str().unwrap(),
                f["key"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(
        vec!["critical9", "warning1", "warning2", "info1", "weird0"],
        order
    );
}

fn seed_tasks_db(path: &std::path::Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE tasks (id TEXT PRIMARY KEY, scope TEXT NOT NULL, status TEXT NOT NULL,
            assignees TEXT NOT NULL DEFAULT '[]', created_at REAL NOT NULL, updated_at REAL NOT NULL,
            block_reason TEXT);
         CREATE TABLE task_leases (task_id TEXT NOT NULL, pane_id TEXT NOT NULL, claimed_at REAL NOT NULL,
            heartbeat_at REAL NOT NULL, expires_at REAL NOT NULL, PRIMARY KEY (task_id, pane_id));
         CREATE TABLE task_events (id INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT NOT NULL,
            event TEXT NOT NULL, at REAL NOT NULL, actor TEXT, meta TEXT);",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO tasks VALUES ('t1','global','blocked','[\"w1:p1\"]',1.0,2.0,NULL)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO task_leases VALUES ('t1','w1:p1',1.0,1.0,9.0)",
        [],
    )
    .unwrap();
    for (task, at) in [("t1", 10.0), ("t1", 20.0), ("t1", 30.0)] {
        conn.execute(
            "INSERT INTO task_events (task_id, event, at) VALUES (?, 'status', ?)",
            rusqlite::params![task, at],
        )
        .unwrap();
    }
}

#[test]
fn the_loader_reads_the_store_read_only_and_never_sees_events_after_now() {
    let dir = std::env::temp_dir().join(format!("ds-wd-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tasks.db");
    let _ = std::fs::remove_file(&path);
    seed_tasks_db(&path);
    let data = TaskData::load(&path, 20.0).unwrap();
    assert_eq!(1, data.tasks.len());
    assert_eq!(
        json!(["w1:p1"]),
        data.tasks[0]["assignees"],
        "assignees are decoded"
    );
    assert_eq!(Value::Null, data.tasks[0]["block_reason"]);
    assert_eq!(1, data.leases.len());
    assert_eq!(
        2,
        data.events.len(),
        "the event at 30 is the future at now=20"
    );
    assert!(
        TaskData::load(&dir.join("missing.db"), 20.0).is_err(),
        "an unreadable store is an error"
    );
}
