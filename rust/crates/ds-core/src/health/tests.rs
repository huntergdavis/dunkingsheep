//! Ported 1:1 from tests/test_dunk_health.py, same order and names. Not ported:
//! `test_the_human_line_reports_minutes` (the CLI's `print_health`, ds-cli) and
//! `DispatchHealthWiringTests` (builds a whole Flock; the wiring is the daemon's).

use std::io::Write;
use std::sync::Mutex;

use rusqlite::Connection;

use super::*;
use crate::memory::ingest;

struct StoreEnv {
    path: std::path::PathBuf,
    conn: Mutex<Connection>,
    totals: Value,
}

impl StoreEnv {
    fn new(dir: &Path) -> StoreEnv {
        let path = dir.join("memory.db");
        let conn = db::open_writer(&path).expect("writer");
        ingest::apply_full_schema(&conn).expect("schema");
        StoreEnv {
            path,
            conn: Mutex::new(conn),
            totals: json!({}),
        }
    }
    fn sample(&self, dir: &Path, name: &str) -> String {
        let path = dir.join(name);
        let mut file = std::fs::File::create(&path).expect("create");
        writeln!(file, "{{\"type\":\"user\"}}").expect("write");
        path.to_string_lossy().into_owned()
    }
    fn count(&self, state: &str) -> i64 {
        self.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM ingest_queue WHERE state=?",
                [state],
                |r| r.get(0),
            )
            .expect("count")
    }
}

impl HealthEnv for StoreEnv {
    fn memory_enabled(&self) -> bool {
        true
    }
    fn memory_stats(&self) -> Result<Value, String> {
        let size = |suffix: &str| {
            fs::metadata(format!("{}{suffix}", self.path.display())).map_or(0, |m| m.len() as i64)
        };
        Ok(
            json!({"path": self.path.to_string_lossy(), "schema_version": ingest::SCHEMA_VERSION,
            "db_bytes": {"": size(""), "-wal": size("-wal")}}),
        )
    }
    fn memory_status(&self) -> Result<Value, String> {
        Ok(json!({"ok": true}))
    }
    fn recover_leases(&self) -> Result<(), DunkError> {
        ingest::recover_leases(&mut self.conn.lock().unwrap()).map(|_| ())
    }
    fn requeue_partial(&self) -> Result<(), DunkError> {
        ingest::requeue_partial(&mut self.conn.lock().unwrap()).map(|_| ())
    }
    fn checkpoint(&self) -> Result<(), DunkError> {
        let _ = self
            .conn
            .lock()
            .unwrap()
            .query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |_| Ok(()));
        Ok(())
    }
    fn task_totals(&self) -> Result<Value, String> {
        Ok(self.totals.clone())
    }
}

fn now() -> f64 {
    crate::clock::now_s()
}

fn run(env: &StoreEnv, dir: &Path, repair: bool, deep: bool, thresholds: &Thresholds) -> Value {
    run_health(env, repair, deep, dir, now(), thresholds).expect("health")
}

fn problems(result: &Value) -> Vec<String> {
    result["problems"]
        .as_array()
        .expect("problems")
        .iter()
        .map(|p| p.as_str().unwrap().to_string())
        .collect()
}

// -- HealthTests ----------------------------------------------------------------------

#[test]
fn test_fresh_store_is_ok() {
    let dir = tempfile::tempdir().expect("tempdir");
    let env = StoreEnv::new(dir.path());
    let result = run(&env, dir.path(), false, false, &Thresholds::default());
    assert_eq!(result["ok"], true, "{:?}", problems(&result));
    assert_eq!(result["checks"]["integrity"], "ok (shallow)");
    assert_eq!(result["repairs"], json!([]));
}

#[test]
fn test_deep_check_runs_quick_check() {
    let dir = tempfile::tempdir().expect("tempdir");
    let env = StoreEnv::new(dir.path());
    let result = run(&env, dir.path(), false, true, &Thresholds::default());
    assert_eq!(result["ok"], true, "{:?}", problems(&result));
    assert_eq!(result["checks"]["integrity"], "ok");
}

#[test]
fn test_repairs_a_stale_ingest_lease() {
    let dir = tempfile::tempdir().expect("tempdir");
    let env = StoreEnv::new(dir.path());
    let sample = env.sample(dir.path(), "s.jsonl");
    {
        let mut conn = env.conn.lock().unwrap();
        ingest::enqueue(&mut conn, &sample, Some("generic"), Some("demo")).expect("enqueue");
        ingest::claim_next(&mut conn, 0.01).expect("claim");
    }
    std::thread::sleep(std::time::Duration::from_millis(50));

    let dry = run(&env, dir.path(), false, false, &Thresholds::default());
    assert_eq!(dry["ok"], false);
    assert!(problems(&dry).iter().any(|p| p.contains("stale")));

    let fixed = run(&env, dir.path(), true, false, &Thresholds::default());
    assert_eq!(fixed["ok"], true, "{:?}", problems(&fixed));
    assert!(fixed["repairs"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r.as_str().unwrap().contains("requeued")));
    assert_eq!(env.count("queued"), 1);
}

#[test]
fn test_flags_and_checkpoints_a_large_wal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let env = StoreEnv::new(dir.path());
    let sample = env.sample(dir.path(), "w.jsonl");
    ingest::enqueue(
        &mut env.conn.lock().unwrap(),
        &sample,
        Some("generic"),
        Some("demo"),
    )
    .expect("enqueue");
    let low = Thresholds {
        wal_warn_bytes: 1,
        ..Thresholds::default()
    };
    let dry = run(&env, dir.path(), false, false, &low);
    assert!(problems(&dry).iter().any(|p| p.contains("WAL")));
    let fixed = run(&env, dir.path(), true, false, &low);
    assert!(fixed["repairs"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r.as_str().unwrap().contains("WAL")));
    assert_eq!(fixed["ok"], true, "{:?}", problems(&fixed));
}

// -- DispatchFreshnessTests -----------------------------------------------------------

fn write_state(dir: &Path, text: &str) {
    fs::write(dir.join("dispatch-state.json"), text).expect("write");
}

#[test]
fn test_no_state_file_says_never_not_zero() {
    let dir = tempfile::tempdir().expect("tempdir");
    let result = dispatch_freshness(dir.path(), now(), &Thresholds::default());
    assert_eq!(result["status"], "absent");
    assert_eq!(result["age_s"], Value::Null);
    assert!(result["reason"].as_str().unwrap().contains("ever run"));
}

#[test]
fn test_a_recent_tick_is_ok_with_its_age() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_state(dir.path(), r#"{"tick_at": 1000.0}"#);
    let result = dispatch_freshness(dir.path(), 1120.0, &Thresholds::default());
    assert_eq!(result["status"], "ok");
    assert_eq!(result["age_s"], 120);
}

#[test]
fn test_a_stopped_tick_is_stale_and_names_the_threshold() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_state(dir.path(), r#"{"tick_at": 1000.0}"#);
    let t = Thresholds::default();
    let result = dispatch_freshness(dir.path(), 1000.0 + t.dispatch_stale_s as f64 + 1.0, &t);
    assert_eq!(result["status"], "stale");
    assert_eq!(result["age_s"], t.dispatch_stale_s + 1);
}

#[test]
fn test_an_unreadable_state_is_absent_not_ok() {
    let dir = tempfile::tempdir().expect("tempdir");
    write_state(dir.path(), "{not json");
    assert_eq!(
        dispatch_freshness(dir.path(), 1000.0, &Thresholds::default())["status"],
        "absent"
    );
}

// -- beyond the Python tests ----------------------------------------------------------

#[test]
fn test_a_stale_dispatcher_is_a_problem_with_the_exact_text() {
    let dir = tempfile::tempdir().expect("tempdir");
    let env = StoreEnv::new(dir.path());
    write_state(dir.path(), r#"{"tick_at": 1000.0}"#);
    let result = run_health(
        &env,
        false,
        false,
        dir.path(),
        1000.0 + 1030.0,
        &Thresholds::default(),
    )
    .expect("ok");
    assert_eq!(
        problems(&result),
        ["dispatcher: last successful tick 1030s ago (stale after 960s) - the feeder is not running"]
    );
}

#[test]
fn test_a_dead_registered_lane_is_listed_but_a_live_one_is_a_problem() {
    struct Gap(StoreEnv);
    impl HealthEnv for Gap {
        fn memory_enabled(&self) -> bool {
            false
        }
        fn memory_stats(&self) -> Result<Value, String> {
            self.0.memory_stats()
        }
        fn memory_status(&self) -> Result<Value, String> {
            self.0.memory_status()
        }
        fn recover_leases(&self) -> Result<(), DunkError> {
            Ok(())
        }
        fn requeue_partial(&self) -> Result<(), DunkError> {
            Ok(())
        }
        fn checkpoint(&self) -> Result<(), DunkError> {
            Ok(())
        }
        fn task_totals(&self) -> Result<Value, String> {
            Err("boom".into())
        }
        fn registry_pane_gap(&self) -> Option<Result<Vec<Value>, String>> {
            Some(Ok(vec![
                json!({"pane": "%1", "label": null, "role": "worker", "pid": 7, "alive": true}),
                json!({"pane": "%2", "label": "x", "role": "worker", "pid": 8, "alive": false}),
            ]))
        }
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let env = Gap(StoreEnv::new(dir.path()));
    let result = run_health(
        &env,
        false,
        false,
        dir.path(),
        now(),
        &Thresholds::default(),
    )
    .expect("ok");
    let all = problems(&result);
    assert_eq!(all[0], "memory service disabled");
    assert_eq!(all[1], "task stats failed: boom");
    assert!(all[2].starts_with("backend pane list is missing 1 registered lane(s) whose process is alive: %1 (worker, pid 7) - "));
    assert_eq!(
        result["checks"]["registry_pane_gap"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn test_write_report_is_sorted_two_space_json_without_trailing_newline() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("sub").join("health.json");
    write_report(&json!({"b": 1, "a": [1, 2]}), &path).expect("write");
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        "{\n  \"a\": [\n    1,\n    2\n  ],\n  \"b\": 1\n}"
    );
}
