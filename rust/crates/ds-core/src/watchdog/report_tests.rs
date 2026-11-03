use super::{digest, evidence, line, recoveries, render_lines};
use serde_json::{json, Value};

#[test]
fn recovery_summary_selects_prior_alerts_and_confirmed_pages_in_sorted_order() {
    // Mirrors `test_recovery_line_goes_to_vp` and the recovery half of
    // `test_resolved_owner_line_only_for_a_confirmed_page`.
    let state = json!({
        "alerts": {
            "rule|z": {"last_alerted": 10.0},
            "rule|a": {"last_alerted": 5.0},
            "rule|not-sent": {"last_alerted": null},
            "rule|still-current": {"last_alerted": 12.0}
        },
        "owner_pages": {
            "rule|z-owner": {"confirmed_at": 30.0},
            "rule|a-owner": {"confirmed_at": 20.0},
            "rule|pending-owner": {"confirmed_at": null},
            "rule|still-current": {"confirmed_at": 40.0}
        }
    });
    let findings = vec![json!({"rule": "rule", "key": "still-current"})];

    assert_eq!(
        recoveries(&state, &findings),
        json!({
            "vp": ["rule|a", "rule|z"],
            "owner": ["rule|a-owner", "rule|z-owner"]
        })
    );
}

#[test]
fn a_finding_still_present_suppresses_both_recovery_lines() {
    let state = json!({
        "alerts": {"host.disk_full|/tmp": {"last_alerted": 1.0}},
        "owner_pages": {"host.disk_full|/tmp": {"confirmed_at": 2.0}}
    });
    let findings = vec![json!({"rule": "host.disk_full", "key": "/tmp"})];

    assert_eq!(
        recoveries(&state, &findings),
        json!({"vp": [], "owner": []})
    );
}

#[test]
fn missing_recovery_state_is_reported_as_empty_lists() {
    let recovered = recoveries(&Value::Null, &[]);
    assert_eq!(recovered, json!({"vp": [], "owner": []}));
}

#[test]
fn finding_line_matches_python_format_and_ascii_sorted_evidence_json() {
    let finding = json!({
        "rule": "inbox.undeliverable",
        "key": "dead",
        "class": "ops",
        "severity": "warning",
        "source": "inbox",
        "threshold": "1 msgs",
        "measured": "1 msgs",
        "window": "current backlog",
        "evidence": {"z": 1, "alpha": "café"}
    });
    assert_eq!(evidence(&finding), r#"{"alpha":"caf\u00e9","z":1}"#);
    assert_eq!(
        line("WOULD-NOTIFY VP", &finding, &json!("no"), None, None),
        r#"WOULD-NOTIFY VP rule=inbox.undeliverable class=ops severity=warning source=inbox threshold=1 msgs measured=1 msgs window=current backlog dedupe=inbox.undeliverable|dead suppressed=no evidence={"alpha":"caf\u00e9","z":1}"#
    );
}

#[test]
fn evidence_floats_match_python_json_notation_boundaries() {
    let finding = json!({
        "evidence": {
            "tiny": 1e-6,
            "small": 1e-5,
            "fixed_lower_edge": 1e-4,
            "fixed_upper_edge": 1e15,
            "scientific_upper_edge": 1e16,
            "integral_float": 1.0,
            "negative_zero": -0.0
        }
    });

    assert_eq!(
        evidence(&finding),
        r#"{"fixed_lower_edge":0.0001,"fixed_upper_edge":1000000000000000.0,"integral_float":1.0,"negative_zero":-0.0,"scientific_upper_edge":1e+16,"small":1e-05,"tiny":1e-06}"#
    );
}

#[test]
fn render_lines_and_digest_preserve_python_order_and_text() {
    let finding = json!({
        "rule": "host.disk_full", "key": "/tmp", "class": "ops", "severity": "critical",
        "source": "disk", "threshold": "90%", "measured": "95%", "window": "now",
        "evidence": {"free": 5}
    });
    let config = json!({"rules": {"host.disk_full": {"must_hold_runs": 3}}});
    let decisions = vec![json!({
        "finding": finding, "owner": "hold", "vp": "no", "streak": 2
    })];
    let skipped = vec![json!({"rule": "money.spend_projection", "error": "no metering source"})];
    let recovered = json!({
        "vp": ["host.disk_full|/old"], "owner": ["host.disk_full|/older"]
    });

    let lines = render_lines(&config, &decisions, &skipped, &recovered, "dry-run");
    assert_eq!(
        lines,
        vec![
            r#"WOULD-PAGE OWNER rule=host.disk_full class=ops severity=critical source=disk threshold=90% measured=95% window=now; held 2/3 runs dedupe=host.disk_full|/tmp suppressed=hold evidence={"free":5}"#,
            r#"WOULD-NOTIFY VP rule=host.disk_full class=ops severity=critical source=disk threshold=90% measured=95% window=now dedupe=host.disk_full|/tmp suppressed=no evidence={"free":5}"#,
            "SKIPPED rule=money.spend_projection error=no metering source",
            "RESOLVED VP dedupe=host.disk_full|/old",
            "RESOLVED OWNER dedupe=host.disk_full|/older"
        ]
    );
    assert_eq!(
        digest("daily", &lines),
        format!(
            "Watchdog daily: deterministic rules; no model involved.\n{}",
            lines.join("\n")
        )
    );
}
