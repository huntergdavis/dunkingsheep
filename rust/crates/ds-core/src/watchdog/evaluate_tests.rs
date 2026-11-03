//! Ports of `evaluate`, skipped/error handling, filtering and ordering in
//! `tests/test_dunk_watchdog.py`.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;

use rusqlite::Connection;
use serde_json::{json, Value};

use super::*;

const NOW: f64 = 1_800_000_000.0;
const EXAMPLE: &str = include_str!("../../../../../packaging/watchdog.example.json");

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn config(&self, enabled: &[&str]) -> Value {
        let mut config: Value = serde_json::from_str(EXAMPLE).unwrap();
        config["stores"]["config_dir"] = json!(self.dir.path().to_string_lossy());
        for rule in config["rules"].as_object_mut().unwrap().values_mut() {
            rule["enabled"] = json!(false);
        }
        for id in enabled {
            config["rules"][*id]["enabled"] = json!(true);
        }
        config
    }

    fn tasks_db(&self) -> PathBuf {
        let path = self.path("tasks.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE tasks (id TEXT, scope TEXT, title TEXT, body TEXT, status TEXT, \
             assignees TEXT, created_at REAL, updated_at REAL, block_reason TEXT); \
             CREATE TABLE task_leases (task_id TEXT, pane_id TEXT, claimed_at REAL, \
             heartbeat_at REAL, expires_at REAL); \
             CREATE TABLE task_events (id INTEGER PRIMARY KEY, task_id TEXT, event TEXT, \
             at REAL, actor TEXT, meta TEXT);",
        )
        .unwrap();
        path
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn insert_task(&self, id: &str, status: &str, at: f64) {
        Connection::open(self.path("tasks.db"))
            .unwrap()
            .execute(
                "INSERT INTO tasks (id, scope, title, body, status, assignees, created_at, \
                 updated_at, block_reason) VALUES (?1, 'global', 'title', 'body', ?2, '[]', \
                 ?3, ?3, NULL)",
                rusqlite::params![id, status, at],
            )
            .unwrap();
    }
}

#[test]
fn test_evaluate_only_runs_named_enabled_rules_and_sorts_findings() {
    let fixture = Fixture::new();
    fixture.tasks_db();
    fixture.insert_task("z-task", "blocked", NOW - 10_000.0);
    fixture.insert_task("a-task", "blocked", NOW - 10_000.0);
    Connection::open(fixture.path("tasks.db"))
        .unwrap()
        .execute(
            "INSERT INTO task_leases VALUES ('expired-task', '%9', ?1, ?1, ?2)",
            rusqlite::params![NOW - 10_000.0, NOW - 1.0],
        )
        .unwrap();

    let mut config = fixture.config(&["task.blocked_stale", "task.lease_stale"]);
    config["rules"]["task.blocked_stale"]["threshold_minutes"] = json!(1);
    config["rules"]["task.blocked_stale"]["severity"] = json!("warning");
    config["rules"]["task.lease_stale"]["never_renewed_minutes"] = json!(1);
    config["rules"]["task.lease_stale"]["lookback_minutes"] = json!(10);
    config["rules"]["task.lease_stale"]["fresh_minutes"] = json!(10);
    config["rules"]["task.lease_stale"]["severity"] = json!("critical");

    let result = evaluate(
        &config,
        NOW,
        None,
        None,
        Some(&["task.blocked_stale", "task.lease_stale"]),
    );
    assert!(result["errors"].as_array().unwrap().is_empty());
    assert!(result["skipped"].as_array().unwrap().is_empty());
    let found = result["findings"].as_array().unwrap();
    assert_eq!(3, found.len());
    assert_eq!("task.lease_stale", found[0]["rule"]);
    assert_eq!("a-task", found[1]["key"]);
    assert_eq!("z-task", found[2]["key"]);

    let filtered = evaluate(&config, NOW, None, None, Some(&["task.blocked_stale"]));
    assert!(filtered["findings"]
        .as_array()
        .unwrap()
        .iter()
        .all(|finding| finding["rule"] == "task.blocked_stale"));
}

#[test]
fn test_evaluate_skips_unavailable_metering_even_when_disabled() {
    let fixture = Fixture::new();
    let config = fixture.config(&[]);
    let result = evaluate(&config, NOW, None, None, Some(&["money.spend_projection"]));
    assert_eq!(
        json!([{"rule": "money.spend_projection", "error": "no metering source"}]),
        result["skipped"]
    );
    assert!(result["findings"].as_array().unwrap().is_empty());
    assert!(result["errors"].as_array().unwrap().is_empty());
}

#[test]
fn test_evaluate_reports_rule_failures_instead_of_returning_a_quiet_pass() {
    let fixture = Fixture::new();
    fixture.tasks_db();
    Connection::open(fixture.path("tasks.db"))
        .unwrap()
        .execute(
            "INSERT INTO task_events (task_id, event, at, meta) VALUES ('t1', 'requeued', ?1, '{bad')",
            [NOW - 10.0],
        )
        .unwrap();
    let mut config = fixture.config(&["task.lease_stale"]);
    config["rules"]["task.lease_stale"]["never_renewed_minutes"] = json!(1);
    config["rules"]["task.lease_stale"]["lookback_minutes"] = json!(10);
    config["rules"]["task.lease_stale"]["fresh_minutes"] = json!(10);

    let result = evaluate(&config, NOW, None, None, Some(&["task.lease_stale"]));
    assert!(result["findings"].as_array().unwrap().is_empty());
    assert!(result["skipped"].as_array().unwrap().is_empty());
    assert_eq!(1, result["errors"].as_array().unwrap().len());
    assert_eq!("task.lease_stale", result["errors"][0]["rule"]);
    assert_eq!(python_json_load_error("{bad"), result["errors"][0]["error"]);
}

#[test]
fn test_evaluate_reuses_host_samples_by_exact_path() {
    let fixture = Fixture::new();
    let mut config = fixture.config(&["host.load", "host.tmp", "host.disk_full"]);
    config["panes"]["source"] = json!("none");
    config["rules"]["host.disk_full"]["paths"] = json!(["/", "/tmp"]);
    let calls = RefCell::new(BTreeMap::<String, usize>::new());
    let sampler = |path: &std::path::Path| {
        *calls
            .borrow_mut()
            .entry(path.to_string_lossy().into_owned())
            .or_default() += 1;
        Ok(HostSample {
            load1: 0.1,
            load5: Some(0.1),
            cpus: 4,
            used_fraction: 0.5,
            free_bytes: 1_000_000,
            inode_used_fraction: 0.5,
        })
    };

    let result = evaluate(
        &config,
        NOW,
        None,
        Some(&sampler),
        Some(&["host.load", "host.tmp", "host.disk_full"]),
    );
    assert!(result["errors"].as_array().unwrap().is_empty());
    assert_eq!(1, calls.borrow().get("/").copied().unwrap_or_default());
    assert_eq!(1, calls.borrow().get("/tmp").copied().unwrap_or_default());
}

#[test]
fn test_evaluate_preserves_json_decode_error_for_other_rule_inputs() {
    let fixture = Fixture::new();
    std::fs::write(fixture.path("health.json"), "{bad").unwrap();
    let config = fixture.config(&["health.failed"]);
    let result = evaluate(&config, NOW, None, None, Some(&["health.failed"]));
    assert_eq!(1, result["errors"].as_array().unwrap().len());
    assert_eq!(python_json_load_error("{bad"), result["errors"][0]["error"]);
}

#[test]
fn test_evaluate_preserves_file_error_for_json_input() {
    let fixture = Fixture::new();
    let missing = fixture.path("health.json");
    let config = fixture.config(&["health.failed"]);
    let result = evaluate(&config, NOW, None, None, Some(&["health.failed"]));
    assert_eq!(1, result["errors"].as_array().unwrap().len());
    assert_eq!(python_open_error(&missing), result["errors"][0]["error"]);
}

#[test]
fn test_evaluate_preserves_missing_read_only_database_error() {
    let fixture = Fixture::new();
    let tasks_path = fixture.path("tasks.db");
    let inbox_path = fixture.path("inbox.db");

    let tasks_config = fixture.config(&["task.blocked_stale"]);
    let tasks_result = evaluate(
        &tasks_config,
        NOW,
        None,
        None,
        Some(&["task.blocked_stale"]),
    );
    assert_eq!(
        python_missing_store_error(&tasks_path),
        tasks_result["errors"][0]["error"]
    );

    let inbox_config = fixture.config(&["inbox.undeliverable"]);
    let inbox_result = evaluate(
        &inbox_config,
        NOW,
        None,
        None,
        Some(&["inbox.undeliverable"]),
    );
    assert_eq!(
        python_missing_store_error(&inbox_path),
        inbox_result["errors"][0]["error"]
    );
}

#[test]
fn test_evaluate_skips_pane_rules_when_pane_source_is_disabled() {
    let fixture = Fixture::new();
    let mut config = fixture.config(&["lane.working_without_task"]);
    config["panes"]["source"] = json!("none");
    let result = evaluate(
        &config,
        NOW,
        None,
        None,
        Some(&["lane.working_without_task"]),
    );
    assert_eq!(
        "pane source disabled (panes.source=none)",
        result["skipped"][0]["error"]
    );
    assert!(result["errors"].as_array().unwrap().is_empty());
}

#[test]
fn test_self_error_contains_all_raw_problems_once() {
    let problems = vec![json!({"rule": "x", "error": "one"}), json!("two")];
    let finding = self_error(&problems);
    assert_eq!("watchdog.self_error", finding["rule"]);
    assert_eq!("self", finding["severity"]);
    assert_eq!(json!(problems), finding["evidence"]["problems"]);
    assert_eq!("2 problem(s)", finding["measured"]);
}

fn python_json_load_error(raw: &str) -> String {
    let output = Command::new("python3")
        .args([
            "-c",
            "import json,sys\ntry: json.loads(sys.argv[1])\nexcept Exception as error: print(type(error).__name__ + ': ' + str(error))",
            raw,
        ])
        .output()
        .expect("python3 JSON error oracle");
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn python_open_error(path: &std::path::Path) -> String {
    let output = Command::new("python3")
        .args([
            "-c",
            "import sys\ntry: open(sys.argv[1]).read()\nexcept Exception as error: print(type(error).__name__ + ': ' + str(error))",
        ])
        .arg(path)
        .output()
        .expect("python3 file error oracle");
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn python_missing_store_error(path: &std::path::Path) -> String {
    let output = Command::new("python3")
        .args([
            "-c",
            "import os,sys\ntry:\n if not os.path.exists(sys.argv[1]): raise FileNotFoundError(sys.argv[1])\nexcept Exception as error: print(type(error).__name__ + ': ' + str(error))",
        ])
        .arg(path)
        .output()
        .expect("python3 missing store error oracle");
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}
