//! Ported 1:1 from tests/test_dunk_load.py, same order and names.

use super::*;

fn me() -> (i64, i64) {
    let pid = i64::from(std::process::id());
    (pid, stat(pid).expect("own stat").starttime)
}

fn no_policy(_: &Object, _: &Object) -> Result<Vec<Value>, String> {
    Err("no evaluator wired".into())
}

/// Stand-in for `dunk_policy.evaluate` covering only the rss rule these tests use; the real
/// one is policy.rs (separate PR).
fn rss_rule(sample: &Object, policy: &Object) -> Result<Vec<Value>, String> {
    let limit = policy.get("rss_mb_over").and_then(Value::as_f64);
    Ok(match limit {
        Some(limit) if sample["rss_mb"].as_f64().unwrap_or(0.0) >= limit => vec![json!({
            "rule": "rss", "agent": sample["name"], "measured": sample["rss_mb"],
            "threshold": limit, "window": policy["rss_window_s"], "pid": sample["pid"]})],
        _ => vec![],
    })
}

#[test]
fn test_a_recycled_pid_produces_no_sample() {
    let (pid, start) = me();
    let rows = sample(&[("lane".into(), pid, start + 1)], 0.0);
    assert_eq!(rows["callers"], json!([]));
    assert_eq!(
        rows["skipped"][0]["reason"],
        "pid reused (starttime differs)"
    );
}

#[test]
fn test_a_matching_starttime_is_sampled() {
    let (pid, start) = me();
    let rows = sample(&[("lane".into(), pid, start)], 0.0);
    assert_eq!(rows["callers"].as_array().expect("rows").len(), 1);
    assert_eq!(rows["callers"][0]["pid"], pid);
}

#[test]
fn test_a_vanished_pid_is_skipped_not_an_error() {
    let rows = sample(&[("gone".into(), i64::from(i32::MAX), 1)], 0.0);
    assert_eq!(rows["callers"], json!([]));
    assert_eq!(rows["skipped"][0]["reason"], "process gone");
}

#[test]
fn test_the_tree_includes_descendants() {
    let (me, _) = me();
    let ppids = BTreeMap::from([(me, 1), (me + 1, me), (me + 2, me + 1), (999_999, 1)]);
    let tree = tree_of(me, &ppids);
    assert!(tree.contains(&me));
    assert!(tree.contains(&(me + 1))); // child
    assert!(tree.contains(&(me + 2))); // grandchild
    assert!(!tree.contains(&999_999));
}

#[test]
fn test_gpu_is_null_with_a_reason() {
    let gpu = gpu();
    assert_eq!(gpu["value"], Value::Null); // the Python skips when a host reports a counter; none can here
    assert!(!gpu["reason"].as_str().expect("reason").is_empty());
}

#[test]
fn test_a_policy_produces_a_would_kill_line_without_killing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("load-state.json");
    let (pid, start) = me();
    let callers = [("lane".to_string(), pid, start)];
    collect(&callers, 1000.0, None, &no_policy, &path).expect("first");
    let policies = json!({"lane": {"rss_mb_over": 0.001, "rss_window_s": 1}});
    let second = collect(&callers, 1030.0, policies.as_object(), &rss_rule, &path).expect("second");
    let would_kill = second["would_kill"].as_array().expect("would_kill");
    assert!(!would_kill.is_empty());
    assert_eq!(would_kill[0]["kill"], false);
    assert_eq!(would_kill[0]["dry_run"], true);
    assert!(would_kill[0]["line"]
        .as_str()
        .expect("line")
        .contains("would-kill"));
}

#[test]
fn test_unattributed_is_reported_not_dropped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("load-state.json");
    let (pid, start) = me();
    let callers = [("lane".to_string(), pid, start)];
    collect(&callers, 1000.0, None, &no_policy, &path).expect("first");
    let result = collect(&callers, 1030.0, None, &no_policy, &path).expect("second");
    assert!(!result["unattributed_cores"].is_null());
    assert!(result.get("attributed_cores").is_some());
}

// -- beyond the Python tests ------------------------------------------------------------

#[test]
fn test_cpu_pct_stays_null_because_the_python_never_writes_last() {
    // Ported defect (module note): the previous sample is read from state["last"], never written.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("load-state.json");
    let (pid, start) = me();
    let callers = [("lane".to_string(), pid, start)];
    collect(&callers, 1000.0, None, &no_policy, &path).expect("first");
    let second = collect(&callers, 1030.0, None, &no_policy, &path).expect("second");
    assert_eq!(second["callers"][0]["cpu_pct"], Value::Null);
    assert_eq!(second["callers"][0]["peak_cpu_pct"], json!(0.0));
    assert_eq!(second["elapsed_s"], json!(30.0));
}

#[test]
fn test_registered_callers_dedupes_and_names() {
    let tasks = [
        json!({"caller_pid": 5, "caller_start": 9, "caller_name": "a"}),
        json!({"caller_pid": 5, "caller_start": 9}),
        json!({"caller_pid": 6, "caller_start": 1}),
        json!({"caller_pid": 7}),
        json!({"caller_start": 3}),
    ];
    assert_eq!(
        registered_callers(&tasks),
        vec![("pid 5".to_string(), 5, 9), ("pid 6".to_string(), 6, 1)]
    );
}

#[test]
fn test_a_malformed_state_file_is_an_empty_state() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("load-state.json");
    fs::write(&path, "{not json").expect("write");
    let (pid, start) = me();
    let result = collect(
        &[("lane".to_string(), pid, start)],
        5.0,
        None,
        &no_policy,
        &path,
    )
    .expect("ok");
    assert_eq!(result["elapsed_s"], Value::Null);
}
