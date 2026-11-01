//! Ported 1:1 from tests/test_dunk_policy.py, same order and names.

use super::*;
use std::process::Command;

fn sample(overrides: Value) -> Object {
    let mut base = json!({"name": "lane", "pid": 4242, "starttime": 99, "cpu_pct": 340.0,
        "rss_mb": 2100.0, "wall_minutes": 95, "cwd": "/home/hunter/workspace/x"})
    .as_object()
    .cloned()
    .expect("object");
    for (key, value) in overrides.as_object().cloned().unwrap_or_default() {
        base.insert(key, value);
    }
    base
}

fn obj(value: Value) -> Object {
    value.as_object().cloned().expect("object")
}

const REGISTERED: &[(i64, Option<i64>)] = &[(4242, Some(99))];

fn no_signal(
    calls: &mut Vec<(i64, Signal)>,
) -> impl FnMut(i64, Signal) -> Result<(), SignalError> + '_ {
    move |pid, signal| {
        calls.push((pid, signal));
        Ok(())
    }
}

// -- EvaluateTests ---------------------------------------------------------------

#[test]
fn test_cpu_threshold_trips_with_its_window() {
    let findings = evaluate(
        &sample(json!({})),
        &obj(json!({"cpu_pct_over": 300, "cpu_window_s": 600})),
    )
    .expect("ok");
    assert_eq!(findings[0]["rule"], "cpu");
    assert_eq!(findings[0]["measured"], json!(340.0));
    assert_eq!(findings[0]["window"], json!(600.0));
    assert_eq!(findings[0]["threshold"], json!(300.0));
}

#[test]
fn test_rss_threshold_trips() {
    let findings = evaluate(
        &sample(json!({})),
        &obj(json!({"rss_mb_over": 1500, "rss_window_s": 300})),
    )
    .expect("ok");
    assert_eq!(findings[0]["rule"], "rss");
}

#[test]
fn test_wall_threshold_trips() {
    let findings =
        evaluate(&sample(json!({})), &obj(json!({"wall_minutes_over": 90}))).expect("ok");
    assert_eq!(findings[0]["rule"], "wall");
    assert_eq!(findings[0]["window"], Value::Null);
}

#[test]
fn test_wall_exempt_skips_the_wall_rule() {
    let policy = obj(json!({"wall_minutes_over": 90, "wall_exempt": true}));
    assert!(evaluate(&sample(json!({"wall_minutes": 300})), &policy)
        .expect("ok")
        .is_empty());
}

#[test]
fn test_below_threshold_does_not_trip() {
    let policy = obj(json!({"cpu_pct_over": 300, "cpu_window_s": 60}));
    assert!(evaluate(&sample(json!({"cpu_pct": 10})), &policy)
        .expect("ok")
        .is_empty());
}

#[test]
fn test_a_threshold_without_a_window_is_a_policy_error() {
    let error =
        evaluate(&sample(json!({})), &obj(json!({"cpu_pct_over": 300}))).expect_err("refused");
    assert_eq!(error.to_string(), "cpu_pct_over needs cpu_window_s");
    let error = evaluate(
        &sample(json!({})),
        &obj(json!({"cpu_pct_over": -1, "cpu_window_s": 1})),
    )
    .expect_err("refused");
    assert_eq!(
        error.to_string(),
        "policy.cpu_pct_over must be a positive number"
    );
}

// -- KillTests --------------------------------------------------------------------

#[test]
fn test_dry_run_logs_and_signals_nothing() {
    let mut calls = Vec::new();
    let record = kill_decision(
        &sample(json!({})),
        &obj(json!({"kill": true, "cpu_pct_over": 300, "cpu_window_s": 600})),
        &KillOptions {
            registered: REGISTERED,
            dry_run: true,
            ..KillOptions::default()
        },
        &mut no_signal(&mut calls),
        &mut |_| {},
    )
    .expect("ok");
    assert_eq!(record["decision"], "would-kill");
    assert!(calls.is_empty());
}

#[test]
fn test_default_is_off() {
    let record = kill_decision(
        &sample(json!({})),
        &obj(json!({"cpu_pct_over": 300, "cpu_window_s": 60})),
        &KillOptions {
            registered: REGISTERED,
            ..KillOptions::default()
        },
        &mut |_, _| Ok(()),
        &mut |_| {},
    )
    .expect("ok");
    assert_eq!(record["decision"], "alert-only");
}

#[test]
fn test_an_unregistered_pid_is_refused() {
    let record = kill_decision(
        &sample(json!({})),
        &obj(json!({"kill": true, "cpu_pct_over": 1, "cpu_window_s": 1})),
        &KillOptions {
            dry_run: false,
            ..KillOptions::default()
        },
        &mut |_, _| panic!("must not signal"),
        &mut |_| {},
    )
    .expect("ok");
    assert_eq!(record["decision"], "refused");
    assert!(record["reason"]
        .as_str()
        .expect("reason")
        .contains("unattributed"));
}

#[test]
fn test_a_protected_pane_is_refused_by_policy_not_by_luck() {
    let mut calls = Vec::new();
    let record = kill_decision(
        &sample(json!({})),
        &obj(json!({"kill": true, "cpu_pct_over": 1, "cpu_window_s": 1})),
        &KillOptions {
            registered: REGISTERED,
            deny: Deny {
                protected_panes: &[4242],
                ..Deny::default()
            },
            dry_run: false,
            ..KillOptions::default()
        },
        &mut no_signal(&mut calls),
        &mut |_| {},
    )
    .expect("ok");
    assert_eq!(record["decision"], "refused");
    assert!(record["reason"]
        .as_str()
        .expect("reason")
        .contains("protected"));
    assert!(calls.is_empty());
}

#[test]
fn test_a_cwd_outside_a_workspace_is_refused() {
    let roots = vec!["/home/hunter/workspace".to_string()];
    let record = kill_decision(
        &sample(json!({"cwd": "/tmp/elsewhere"})),
        &obj(json!({"kill": true, "cpu_pct_over": 1, "cpu_window_s": 1})),
        &KillOptions {
            registered: REGISTERED,
            deny: Deny {
                workspace_roots: &roots,
                ..Deny::default()
            },
            dry_run: false,
            ..KillOptions::default()
        },
        &mut |_, _| panic!("must not signal"),
        &mut |_| {},
    )
    .expect("ok");
    assert_eq!(record["decision"], "refused");
    assert_eq!(record["reason"], "cwd /tmp/elsewhere is not a workspace");
}

#[test]
fn test_a_real_kill_of_a_process_we_started() {
    let mut child = Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn sleep");
    let pid = i64::from(child.id());
    let start: i64 = crate::proc::stat(pid)
        .expect("stat")
        .start
        .parse()
        .expect("starttime");
    let registered = [(pid, Some(start))];
    let record = kill_decision(
        &obj(
            json!({"name": "child", "pid": pid, "starttime": start, "cpu_pct": 999.0,
            "rss_mb": 1.0, "wall_minutes": 1}),
        ),
        &obj(json!({"kill": true, "cpu_pct_over": 1, "cpu_window_s": 1})),
        &KillOptions {
            registered: &registered,
            dry_run: false,
            grace_s: 0.2,
            ..KillOptions::default()
        },
        &mut os_kill,
        &mut |seconds| std::thread::sleep(std::time::Duration::from_secs_f64(seconds)),
    )
    .expect("ok");
    assert_eq!(record["decision"], "kill");
    assert!(record["signals"]
        .as_array()
        .expect("signals")
        .contains(&json!("TERM")));
    child.wait().expect("child reaped");
}

// -- beyond the Python tests: the record shape the harness diffs ---------------------

#[test]
fn test_a_vanished_pid_after_term_is_reported_as_exited() {
    let mut calls = Vec::new();
    // pid i32::MAX is never a live process, so the liveness probe says ESRCH.
    let pid = i64::from(i32::MAX);
    let registered = [(pid, Some(1))];
    let record = kill_decision(
        &obj(json!({"name": "ghost", "pid": pid, "starttime": 1, "cpu_pct": 9.0, "rss_mb": 1.0})),
        &obj(json!({"kill": true, "cpu_pct_over": 1, "cpu_window_s": 1})),
        &KillOptions {
            registered: &registered,
            dry_run: false,
            grace_s: 0.0,
            ..KillOptions::default()
        },
        &mut no_signal(&mut calls),
        &mut |_| {},
    )
    .expect("ok");
    assert_eq!(record["outcome"], "exited after TERM");
    assert_eq!(record["signals"], json!(["TERM"]));
    assert_eq!(calls, vec![(pid, Signal::SIGTERM)]);
}

#[test]
fn test_record_keys_keep_the_python_order() {
    let record = kill_decision(
        &sample(json!({})),
        &obj(json!({"kill": true, "cpu_pct_over": 300, "cpu_window_s": 600})),
        &KillOptions {
            registered: REGISTERED,
            ..KillOptions::default()
        },
        &mut |_, _| Ok(()),
        &mut |_| {},
    )
    .expect("ok");
    let keys: Vec<&str> = record
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["decision", "agent", "pid", "findings", "sample"]);
    let finding: Vec<&str> = record["findings"][0]
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        finding,
        ["rule", "agent", "measured", "threshold", "window", "pid"]
    );
}

#[test]
fn test_a_cwd_inside_a_workspace_is_permitted() {
    let roots = vec!["/home/hunter/workspace".to_string()];
    let deny = Deny {
        workspace_roots: &roots,
        cwd: Some("/home/hunter/workspace/x"),
        ..Deny::default()
    };
    assert_eq!(denied(Some(1), &deny), None);
    let deny = Deny {
        workspace_roots: &roots,
        cwd: Some("/home/hunter/workspace"),
        ..Deny::default()
    };
    assert_eq!(denied(Some(1), &deny), None);
    let deny = Deny {
        workspace_roots: &roots,
        cwd: Some("/home/hunter/workspace-evil"),
        ..Deny::default()
    };
    assert!(denied(Some(1), &deny).is_some());
}
