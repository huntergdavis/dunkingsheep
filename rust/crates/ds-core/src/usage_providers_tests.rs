//! Ported from tests/test_usage.py::ProviderParserTests, plus codex/pi/opencode readers.
use super::*;

fn write(home: &Path, rel: &str, body: &str) {
    let p = home.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, body).unwrap();
}

#[test]
fn test_nested_rate_limits_is_found() {
    let event = json!({"timestamp": "2026-10-21T00:00:00Z",
        "payload": {"type": "token_count", "rate_limits": {"primary": {"used_percent": 15.0}}}});
    assert_eq!(
        find_rate_limits(&event).unwrap()["primary"]["used_percent"],
        15.0
    );
}

#[test]
fn test_stat_parses_k_suffix() {
    let text = "Total Cost $1.25\nInput 36.9K\nOutput 11";
    assert_eq!(stat(text, "Total Cost"), Some(1.25));
    assert_eq!(stat(text, "Input"), Some(36900.0));
    assert_eq!(stat(text, "Output"), Some(11.0));
}

#[test]
fn codex_fresh_is_ok_and_stale_is_unknown() {
    let h = tempfile::tempdir().unwrap();
    write(
        h.path(),
        ".codex/sessions/2026/a.jsonl",
        r#"{"timestamp":"2026-10-31T00:00:00Z","payload":{"rate_limits":{"primary":{"used_percent":40.0,"window_minutes":10080}}}}"#,
    );
    let t = 1_793_404_800.0; // 2026-10-31T00:00:00Z
    let ok = codex(h.path(), 3600.0, t + 60.0);
    assert_eq!(ok["status"], "ok");
    assert_eq!(ok["windows"][0]["name"], "weekly");
    assert_eq!(ok["windows"][0]["percent_remaining"], 60.0);
    let stale = codex(h.path(), 3600.0, t + 7200.0);
    assert_eq!(stale["status"], "unknown");
    assert_eq!(stale["last_snapshot"]["percent_used"], 40.0);
    assert!(stale["reason"].as_str().unwrap().contains("120 min old"));
}

#[test]
fn codex_without_snapshot_is_a_named_error() {
    let h = tempfile::tempdir().unwrap();
    let r = codex(h.path(), 3600.0, 0.0);
    assert_eq!(r["status"], "error");
    assert!(r["error"]
        .as_str()
        .unwrap()
        .contains("no Codex rate_limits"));
}

#[test]
fn pi_sums_usage_and_goes_unknown_when_stale() {
    let h = tempfile::tempdir().unwrap();
    write(
        h.path(),
        ".pi/agent/sessions/s.jsonl",
        concat!(
            r#"{"timestamp":"2026-10-31T00:00:00Z","message":{"usage":{"input":10,"output":5,"totalTokens":15,"cost":{"total":0.5}}}}"#,
            "\n",
            r#"{"timestamp":"2026-10-31T00:01:00Z","message":{"usage":{"input":20,"output":5,"totalTokens":40,"cost":{"total":0.25}}}}"#,
            "\n"
        ),
    );
    let t = 1_793_404_800.0;
    let r = pi(h.path(), 3600.0, t + 120.0);
    assert_eq!(r["status"], "ok");
    assert_eq!(r["session_messages_with_usage"], 2);
    assert_eq!(r["session_cost_usd"], 0.75);
    assert_eq!(r["input_tokens"], 30);
    assert_eq!(r["last_context_tokens"], 40);
    let stale = pi(h.path(), 3600.0, t + 100_000.0);
    assert_eq!(stale["status"], "unknown");
    assert!(stale["reason"].is_string());
}

#[test]
fn opencode_is_unknown_with_local_context_and_ansi_stripped() {
    let r = opencode_from_stats(
        "\x1b[1mSessions 3\x1b[0m\nTotal Cost $2.50\nInput 1.5M",
        7.0,
    );
    assert_eq!(r["status"], "unknown");
    assert_eq!(r["local"]["sessions"], 3.0);
    assert_eq!(r["local"]["total_cost_usd"], 2.5);
    assert_eq!(r["local"]["input_tokens"], 1_500_000.0);
}

#[test]
fn claude_is_a_named_error_not_a_guess() {
    let r = claude(1.0);
    assert_eq!(r["status"], "error");
    assert!(r["error"].as_str().unwrap().contains("not ported"));
}

// ---- t1235: fixes from the t1213 review ----

#[test]
fn a_hung_opencode_times_out_instead_of_blocking_the_caller() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("opencode");
    fs::write(&script, "#!/bin/sh\nsleep 5\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    let started = Instant::now();
    let r = opencode_with(script.to_str().unwrap(), 0.3, 5.0);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "returned at the deadline"
    );
    assert_eq!(r["status"], "error");
    let text = r["error"].as_str().unwrap();
    assert!(
        text.starts_with("opencode stats failed: Command '["),
        "{text}"
    );
    assert!(
        text.ends_with("'stats', '--days', '1']' timed out after 0.3 seconds"),
        "{text}"
    );
}

#[test]
fn a_missing_opencode_and_a_nonzero_exit_are_named_errors() {
    let missing = opencode_with("/nonexistent/opencode-xyz", 5.0, 1.0);
    assert!(missing["error"]
        .as_str()
        .unwrap()
        .starts_with("opencode stats failed: "));
    let failed = opencode_with("false", 5.0, 1.0);
    assert_eq!(failed["error"], "opencode stats exited 1: ");
}

#[test]
fn a_signal_exit_is_minus_the_signal_like_python() {
    let status = Command::new("sh")
        .args(["-c", "kill -9 $$"])
        .status()
        .unwrap();
    assert_eq!(return_code(&status), -9);
}

#[test]
fn codex_window_names_follow_python() {
    assert_eq!(window_name(&json!(10080)), "weekly");
    assert_eq!(
        window_name(&json!(10080.0)),
        "weekly",
        "numeric equality, as Python =="
    );
    assert_eq!(window_name(&Value::Null), "Nonemin");
    assert_eq!(window_name(&json!(300)), "300min");
    assert_eq!(window_name(&json!(300.5)), "300.5min");
    assert_eq!(window_name(&json!(300.0)), "300.0min");
}

#[test]
fn hidden_files_and_directories_are_not_searched() {
    let h = tempfile::tempdir().unwrap();
    write(h.path(), "s/visible.jsonl", "{}");
    write(h.path(), "s/.hidden.jsonl", "{}");
    write(h.path(), "s/.cache/inner.jsonl", "{}");
    let names: Vec<String> = newest_jsonl(h.path(), 10)
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["visible.jsonl".to_string()]);
}

#[test]
fn epoch_handles_offsets_naive_times_and_zero() {
    assert_eq!(epoch(&json!("2026-10-31T00:00:00Z")), Some(1_793_404_800.0));
    assert_eq!(
        epoch(&json!("2026-10-31T01:00:00+01:00")),
        Some(1_793_404_800.0)
    );
    assert_eq!(
        epoch(&json!("2026-10-31 00:00:00+00:00")),
        Some(1_793_404_800.0)
    );
    // naive: local zone, so only assert it parses and is within a day of the UTC reading
    let naive = epoch(&json!("2026-10-31T00:00:00")).expect("naive parses");
    assert!((naive - 1_793_404_800.0).abs() <= 14.0 * 3600.0);
    assert_eq!(epoch(&json!("not a date")), None);
    assert_eq!(epoch(&json!(null)), None);
}

#[test]
fn a_zero_codex_timestamp_falls_back_to_the_file_mtime_like_python_or() {
    let h = tempfile::tempdir().unwrap();
    write(
        h.path(),
        ".codex/sessions/a.jsonl",
        r#"{"timestamp":0,"payload":{"rate_limits":{"primary":{"used_percent":1.0,"window_minutes":10080}}}}"#,
    );
    let now = mtime(&h.path().join(".codex/sessions/a.jsonl")) + 10.0;
    let r = codex(h.path(), 3600.0, now);
    assert_eq!(r["status"], "ok", "{r}");
    assert!((r["snapshot_age_s"].as_f64().unwrap() - 10.0).abs() < 1.0);
}
