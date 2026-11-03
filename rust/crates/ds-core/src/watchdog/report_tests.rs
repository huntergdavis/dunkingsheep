use super::recoveries;
use serde_json::{json, Value};

#[test]
fn recovery_summary_selects_prior_alerts_and_confirmed_pages_in_sorted_order() {
    // Mirrors `test_recovery_line_goes_to_vp` and the recovery half of
    // `test_resolved_owner_line_only_for_a_confirmed_page`.
    let state = json!({
        "alerts": {
            "z|rule": {"last_alerted": 10.0},
            "a|rule": {"last_alerted": 5.0},
            "not-sent|rule": {"last_alerted": null},
            "still-current|rule": {"last_alerted": 12.0}
        },
        "owner_pages": {
            "z-owner|rule": {"confirmed_at": 30.0},
            "a-owner|rule": {"confirmed_at": 20.0},
            "pending-owner|rule": {"confirmed_at": null},
            "still-current|rule": {"confirmed_at": 40.0}
        }
    });
    let findings = vec![json!({"rule": "rule", "key": "still-current"})];

    assert_eq!(
        recoveries(&state, &findings),
        json!({
            "vp": ["a|rule", "z|rule"],
            "owner": ["a-owner|rule", "z-owner|rule"]
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
