//! t1620 ports of `resource.would_kill` behavior from `tests/test_dunk_watchdog.py`.

use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;

use serde_json::{json, Value};

use super::*;

const NOW: f64 = 1_800_000_000.0;

struct Fixture {
    dir: tempfile::TempDir,
    policies: PathBuf,
    state: PathBuf,
    data: TaskData,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let policies = dir.path().join("policies.json");
        let state = dir.path().join("load-state.json");
        fs::write(
            &policies,
            r#"{"w8:p8":{"rss_mb_over":0,"rss_window_s":60,"dry_run":true},"w9:pM":{"rss_mb_over":0,"rss_window_s":60,"dry_run":true}}"#,
        )
        .unwrap();
        let data = TaskData {
            tasks: vec![
                json!({"id":"t6","status":"in_progress","title":"advance strict C toward the ceiling"}),
                json!({"id":"t9","status":"in_progress","title":"parked work"}),
            ],
            leases: vec![
                json!({"task_id":"t6","pane_id":"w8:p8","caller_pid":4000000,"caller_start":"1","caller_name":"w8:p8"}),
                json!({"task_id":"t9","pane_id":"w9:pM","caller_pid":4000001,"caller_start":"2","caller_name":"w9:pM"}),
            ],
            events: Vec::new(),
        };
        Self {
            dir,
            policies,
            state,
            data,
        }
    }

    fn rule(&self, require_task: bool) -> Value {
        json!({
            "severity": "warning",
            "class": "ops",
            "policies_path": self.policies,
            "state_path": self.state,
            "require_task": require_task,
        })
    }
}

fn canned() -> Value {
    json!({
        "would_kill": [
            {"agent":"w8:p8","pid":4000000,"findings":[
                {"rule":"cpu_pct_over","measured":"96.0%","threshold":"80%","window":"60s"}
            ],"kill":false,"dry_run":true},
            {"agent":"w9:pM","pid":4000001,"findings":[
                {"rule":"cpu_pct_over","measured":"99.0%","threshold":"80%","window":"60s"}
            ],"kill":false,"dry_run":true}
        ]
    })
}

#[test]
fn would_kill_names_the_lane_and_the_task_it_was_doing() {
    let fixture = Fixture::new();
    let collected: RefCell<Option<(Vec<Caller>, Map<String, Value>, PathBuf)>> = RefCell::new(None);
    let result = rule_resource_would_kill_with(
        &fixture.data,
        &fixture.rule(true),
        NOW,
        &|callers, _now, policies, state_path| {
            *collected.borrow_mut() =
                Some((callers.to_vec(), policies.clone(), state_path.to_path_buf()));
            Ok(canned())
        },
    )
    .unwrap();

    let (callers, policies, state_path) = collected.into_inner().unwrap();
    assert_eq!(
        vec![
            ("w8:p8".into(), 4_000_000, 1),
            ("w9:pM".into(), 4_000_001, 2)
        ],
        callers
    );
    assert_eq!(json!(0), policies["w8:p8"]["rss_mb_over"]);
    assert_eq!(fixture.state, state_path);
    assert_eq!(2, result.len());
    assert_eq!("resource.would_kill", result[0]["rule"]);
    assert_eq!("info", result[0]["severity"]);
    assert_eq!("t6", result[0]["evidence"]["task"]);
    assert!(result[0]["summary"]
        .as_str()
        .unwrap()
        .contains("advance strict C toward the ceiling"));
    assert!(result[0]["measured"].as_str().unwrap().contains("96.0%"));
}

#[test]
fn require_task_omits_idle_lanes() {
    let fixture = Fixture::new();
    let mut data = fixture.data.clone();
    data.tasks[1]["status"] = json!("done");
    let result =
        rule_resource_would_kill_with(&data, &fixture.rule(true), NOW, &|_, _, _, _| Ok(canned()))
            .unwrap();
    assert_eq!(1, result.len());
    assert_eq!("w8:p8", result[0]["evidence"]["agent"]);
}

#[test]
fn absent_policy_file_means_no_findings_without_loading_callers() {
    let fixture = Fixture::new();
    let rule = json!({
        "severity":"warning", "class":"ops",
        "policies_path":fixture.dir.path().join("absent.json"),
        "state_path":fixture.state, "require_task":true,
    });
    let result = rule_resource_would_kill_with(&fixture.data, &rule, NOW, &|_, _, _, _| {
        panic!("missing policies must skip load::collect")
    })
    .unwrap();
    assert!(result.is_empty());
}

#[test]
fn malformed_or_missing_caller_identity_fails_closed() {
    let fixture = Fixture::new();
    let mut data = fixture.data.clone();
    data.leases = vec![
        json!({"task_id":"t6","pane_id":"w8:p8","caller_pid":4000000}),
        json!({"task_id":"t6","pane_id":"w8:p8","caller_pid":4000000,"caller_start":"not-a-starttime"}),
        json!({"task_id":"t6","pane_id":"w8:p8","caller_pid":4000000,"caller_start":"0001"}),
        json!({"task_id":"t6","pane_id":"w8:p8","caller_pid":"bad","caller_start":"1"}),
        json!({"task_id":"t6","pane_id":"w8:p8","caller_start":"1"}),
    ];
    let called = std::cell::Cell::new(false);
    let result = rule_resource_would_kill_with(&data, &fixture.rule(true), NOW, &|_, _, _, _| {
        called.set(true);
        Ok(canned())
    })
    .unwrap();
    assert!(
        !called.get(),
        "an invalid identity must never reach the sampler"
    );
    assert!(result.is_empty());
}

#[test]
fn public_rule_uses_load_collect_with_text_starttime_identity() {
    let fixture = Fixture::new();
    let pid = i64::from(std::process::id());
    let start = crate::load::stat(pid).expect("test process stat").starttime;
    fs::write(
        &fixture.policies,
        r#"{"codex-rust-writer":{"rss_mb_over":0,"rss_window_s":60,"dry_run":true}}"#,
    )
    .unwrap();
    let mut data = fixture.data.clone();
    data.tasks = vec![json!({"id":"t6","status":"in_progress","title":"test sampler"})];
    data.leases = vec![json!({
        "task_id":"t6", "pane_id":"codex-rust-writer", "caller_pid":pid,
        "caller_start":start.to_string(), "caller_name":"codex-rust-writer"
    })];
    let found = rule_resource_would_kill(&data, &fixture.rule(true), NOW).unwrap();
    let state: Value = serde_json::from_slice(&fs::read(&fixture.state).unwrap()).unwrap();
    assert!(state["callers"].get(&format!("{pid}:{start}")).is_some());
    assert_eq!(1, found.len());
    assert_eq!("resource.would_kill", found[0]["rule"]);
    assert_eq!(pid, found[0]["evidence"]["pid"]);
}

#[test]
fn bad_policy_json_is_reported_with_resource_rule_context() {
    let fixture = Fixture::new();
    fs::write(&fixture.policies, "{").unwrap();
    let error =
        rule_resource_would_kill_with(&fixture.data, &fixture.rule(true), NOW, &|_, _, _, _| {
            panic!("invalid policy JSON must not call load::collect")
        })
        .unwrap_err();
    assert!(error
        .to_string()
        .starts_with("resource.would_kill: cannot read "));
}

#[test]
fn the_collector_receives_the_policy_and_expanded_state_path() {
    let fixture = Fixture::new();
    let rule = json!({
        "severity":"warning", "class":"ops",
        "policies_path":fixture.policies, "state_path":fixture.state,
        "require_task":false,
    });
    let called = std::cell::Cell::new(false);
    let result = rule_resource_would_kill_with(
        &fixture.data,
        &rule,
        NOW,
        &|callers, at, policies, state| {
            assert_eq!(NOW, at);
            assert_eq!(2, callers.len());
            assert!(policies.contains_key("w8:p8"));
            assert_eq!(fixture.state, state);
            called.set(true);
            Ok(json!({"would_kill": []}))
        },
    )
    .unwrap();
    assert!(called.get());
    assert!(result.is_empty());
}
