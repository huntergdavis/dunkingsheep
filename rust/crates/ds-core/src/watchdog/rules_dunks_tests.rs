//! Ports of the dunk-rule tests of `tests/test_dunk_watchdog.py` (t1400 slice B). The Python tests
//! run them through `evaluate`, which is a later slice, so each calls the rule on the same fixture
//! instead; the expectations are the Python's. The metrics are a real read-only `metrics.db`.

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde_json::{json, Value};

use super::*;

const NOW: f64 = 1_800_000_000.0;
const MIN: f64 = 60.0;
const HOUR: f64 = 3600.0;

struct Fleet {
    _dir: tempfile::TempDir,
    metrics: PathBuf,
}

impl Fleet {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let metrics = dir.path().join("metrics.db");
        let conn = Connection::open(&metrics).unwrap();
        conn.execute(
            "CREATE TABLE events (id INTEGER PRIMARY KEY AUTOINCREMENT, at REAL, kind TEXT, source TEXT)",
            [],
        )
        .unwrap();
        Fleet { _dir: dir, metrics }
    }

    fn metric(&self, kind: &str, source: &str, at: f64) {
        Connection::open(&self.metrics)
            .unwrap()
            .execute(
                "INSERT INTO events (at, kind, source) VALUES (?,?,?)",
                rusqlite::params![at, kind, source],
            )
            .unwrap();
    }

    fn skip_loop(&self, dunks: &[Value], retired: &BTreeMap<String, Retired>) -> Vec<Value> {
        let db = MetricsDb {
            path: &self.metrics,
        };
        rule_dunk_skip_loop(dunks, &db, retired, &skip_rule(), NOW).unwrap()
    }
}

fn skip_rule() -> Value {
    json!({"severity": "warning", "class": "ops", "consecutive_skips": 3,
           "lookback_hours": 168, "fresh_minutes": 120})
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

fn dunk(id: &str, target: &str) -> Value {
    json!({"id": id, "text": "go", "target": target, "target_pane_id": target, "running": true})
}

#[test]
fn test_a_running_streak_keeps_one_stable_key_and_ends_with_the_run() {
    let fleet = Fleet::new();
    let dunks = vec![
        json!({"id": "d1", "running": true}),
        json!({"id": "d2", "running": true}),
    ];
    for n in 0..3 {
        fleet.metric("dunk.skipped", "d1", NOW - 30.0 * MIN + f64::from(n));
    }
    let first = fleet.skip_loop(&dunks, &BTreeMap::new());
    fleet.metric("dunk.skipped", "d1", NOW - 25.0 * MIN); // the run grows
    let grown = fleet.skip_loop(&dunks, &BTreeMap::new());
    assert_eq!(
        keys(&first),
        keys(&grown),
        "a growing loop keeps its key (one incident)"
    );
    fleet.metric("dunk.sent", "d1", NOW - 20.0 * MIN); // the run ends
    for n in 0..2 {
        fleet.metric("dunk.skipped", "d2", NOW - HOUR + f64::from(n));
    }
    let found = fleet.skip_loop(&dunks, &BTreeMap::new());
    let who: Vec<&str> = found
        .iter()
        .map(|f| f["evidence"]["dunk"].as_str().unwrap())
        .collect();
    assert_eq!(vec!["d1"], who);
    assert_eq!("4 consecutive skips", found[0]["measured"]);
}

#[test]
fn test_events_after_now_are_never_seen() {
    let fleet = Fleet::new();
    let dunks = vec![json!({"id": "d1", "running": true})];
    for n in 0..5 {
        fleet.metric("dunk.skipped", "d1", NOW + 1.0 + f64::from(n));
    }
    assert!(fleet.skip_loop(&dunks, &BTreeMap::new()).is_empty());
}

#[test]
fn test_dunk_overdue() {
    let rule = json!({"severity": "warning", "class": "ops", "missed_intervals": 3});
    let dunks = vec![
        json!({"id": "d1", "running": true, "interval_minutes": 10, "next_send_at": NOW - 30.0 * MIN}),
        json!({"id": "d2", "running": true, "interval_minutes": 10, "next_send_at": NOW - 30.0 * MIN + 1.0}),
        json!({"id": "d3", "running": false, "interval_minutes": 10, "next_send_at": NOW - 99.0 * MIN}),
        json!({"id": "d4", "running": true, "interval_minutes": 10, "next_send_at": null}),
    ];
    let found = rule_dunk_overdue(&dunks, &rule, NOW);
    assert_eq!(
        vec![
            ("dunk.overdue".to_string(), "d1".to_string()),
            ("dunk.overdue".to_string(), "d4".to_string())
        ],
        keys(&found)
    );
    // the texts a dry-run line prints
    assert_eq!("3 intervals", found[0]["threshold"]);
    assert_eq!("3.0 intervals", found[0]["measured"]);
    assert_eq!("interval 10min", found[0]["window"]);
    assert_eq!(json!(30.0), found[0]["evidence"]["overdue_minutes"]);
    assert_eq!(
        "dunk d4 is running with no scheduled send",
        found[1]["summary"]
    );
}

#[test]
fn test_a_skip_streak_that_stopped_is_history_not_a_condition() {
    // d52 paged with 33 skips whose last one was 76 hours old.
    let fleet = Fleet::new();
    for index in 0..5 {
        fleet.metric(
            "dunk.skipped",
            "d52",
            NOW - 76.0 * HOUR + f64::from(index) * MIN,
        );
    }
    let found = fleet.skip_loop(&[dunk("d52", "w1:p1")], &BTreeMap::new());
    assert!(
        found.is_empty(),
        "a streak that ended 76h ago is not a current condition"
    );
}

#[test]
fn test_a_skip_streak_that_is_still_running_pages() {
    let fleet = Fleet::new();
    for index in 0..5 {
        fleet.metric(
            "dunk.skipped",
            "d9",
            NOW - 30.0 * MIN + f64::from(index) * MIN,
        );
    }
    let found = fleet.skip_loop(&[dunk("d9", "w1:p1")], &BTreeMap::new());
    assert_eq!(1, found.len());
    let summary = found[0]["summary"].as_str().unwrap();
    assert!(summary.contains("5 times in a row"), "{summary}");
    assert!(summary.contains("min ago"), "{summary}");
    assert_eq!("3 consecutive skips", found[0]["threshold"]);
    assert_eq!("last skip within 120min", found[0]["window"]);
    assert_eq!(json!(5), found[0]["evidence"]["current_streak"]);
}

#[test]
fn test_a_dunk_aimed_at_a_retired_pane_is_obsolete_once() {
    let fleet = Fleet::new();
    for index in 0..5 {
        fleet.metric(
            "dunk.skipped",
            "d52",
            NOW - 10.0 * MIN + f64::from(index) * MIN,
        );
    }
    let mut retired = BTreeMap::new();
    retired.insert(
        "w9:p9".to_string(),
        Retired {
            pane_id: "w9:p9".into(),
            reason: "superseded-by:w9:p10".into(),
            by: None,
            labels: vec![],
            mailboxes: vec![],
            retired_at: NOW,
        },
    );
    let found = fleet.skip_loop(&[dunk("d52", "w9:p9")], &retired);
    assert_eq!(1, found.len());
    assert_eq!("info", found[0]["severity"]);
    assert!(found[0]["summary"]
        .as_str()
        .unwrap()
        .contains("retired pane w9:p9"));
    assert_eq!(
        json!(true),
        found[0]["once"],
        "an obsolete dunk is reported once, not every run"
    );
    assert_eq!("d52:obsolete", found[0]["key"]);
}

// -- extra: the loaders and the rounding the Python does ---------------------------------------

#[test]
fn extra_load_dunks_reads_the_dunkers_list_and_refuses_a_missing_or_bad_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dunks.json");
    assert!(
        load_dunks(&path).is_err(),
        "a missing store is an error, never a silent pass"
    );
    std::fs::write(&path, "{not json").unwrap();
    assert!(load_dunks(&path).is_err());
    std::fs::write(&path, r#"{"version": 1, "next_id": 1}"#).unwrap();
    assert!(load_dunks(&path).unwrap().is_empty());
    std::fs::write(&path, r#"{"dunkers": [{"id": "d1"}]}"#).unwrap();
    assert_eq!(vec![json!({"id": "d1"})], load_dunks(&path).unwrap());
}

#[test]
fn extra_the_metrics_store_must_exist() {
    let db = MetricsDb {
        path: Path::new("/nonexistent/metrics.db"),
    };
    assert!(db.dunk_events("d1", 0.0, NOW).is_err());
}

#[test]
fn extra_minutes_ago_rounds_half_to_even_like_python() {
    // round(2.5) == 2 and round(3.5) == 4 in Python.
    assert_eq!(2, round_half_even(2.5));
    assert_eq!(4, round_half_even(3.5));
    assert_eq!(0, round_half_even(0.5));
}

#[test]
fn extra_a_sent_event_ends_the_run_and_the_longest_run_wins() {
    let fleet = Fleet::new();
    for n in 0..2 {
        fleet.metric("dunk.skipped", "d1", NOW - 50.0 * MIN + f64::from(n));
    }
    fleet.metric("dunk.sent", "d1", NOW - 45.0 * MIN);
    for n in 0..4 {
        fleet.metric("dunk.skipped", "d1", NOW - 40.0 * MIN + f64::from(n));
    }
    let found = fleet.skip_loop(&[json!({"id": "d1", "running": true})], &BTreeMap::new());
    assert_eq!("4 consecutive skips", found[0]["measured"]);
    assert_eq!(json!(4), found[0]["evidence"]["current_streak"]);
}
