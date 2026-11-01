//! t1619 ports of the rule-level cases in `tests/test_watchdog_ambiguous_queued.py`.

use std::path::PathBuf;

use rusqlite::Connection;
use serde_json::{json, Value};

use super::*;

const NOW: f64 = 5_000_000.0;
const PANE: &str = "%79";
const MESSAGE: &str = "m9180";
const SECRET: &str = "the private message body that must never appear in an alert";
const AMBIGUOUS: &str = "ambiguous composer/transcript: prompt row 18 is separated from footer row 23 by 2 blank rows; visible prompt content may be pending input or transcript, so no keys are authorised";
const TURN_WAIT: &str = "codex does not queue text typed during a turn (measured: notices are lost), so this waits for the turn to end";

struct Fixture {
    _dir: tempfile::TempDir,
    metrics: PathBuf,
    outbox: Vec<Value>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let metrics = dir.path().join("metrics.db");
        Connection::open(&metrics)
            .unwrap()
            .execute_batch(
                "CREATE TABLE events (id INTEGER PRIMARY KEY, at REAL, kind TEXT, \
                 pane_id TEXT, reason TEXT, attempts INTEGER);",
            )
            .unwrap();
        let outbox = vec![json!({
            "id": MESSAGE,
            "pane_id": PANE,
            "target_label": "t372-wiring",
            "text": SECRET,
            "source": "send_text",
            "kind": "text",
            "hold_while_typing": true,
            "is_notice": false,
            "notify_for": [],
            "holds": 13,
            "next_attempt_at": NOW + 300.0,
            "hold_signature": ["gone", ""],
            "created_at": NOW - 40.0 * 60.0,
        })];
        let fixture = Self {
            _dir: dir,
            metrics,
            outbox,
        };
        fixture.insert(&[(AMBIGUOUS, NOW - 60.0)]);
        fixture
    }

    fn insert(&self, rows: &[(&str, f64)]) {
        let conn = Connection::open(&self.metrics).unwrap();
        conn.execute("DELETE FROM events", []).unwrap();
        for (reason, at) in rows {
            conn.execute(
                "INSERT INTO events (at, kind, pane_id, reason, attempts) \
                 VALUES (?, 'message.held', ?, ?, 13)",
                rusqlite::params![at, PANE, reason],
            )
            .unwrap();
        }
    }

    fn events(&self) -> MetricsDb<'_> {
        MetricsDb {
            path: &self.metrics,
        }
    }

    fn rule(&self) -> Value {
        json!({
            "enabled": true,
            "severity": "warning",
            "class": "ops",
            "cooldown_minutes": 240,
            "must_hold_runs": 1,
            "min_holds": 10,
            "min_age_minutes": 20,
        })
    }

    fn findings(&self) -> Vec<Value> {
        rule_delivery_ambiguous_queued(&self.outbox, &self.events(), &self.rule(), NOW)
    }
}

#[test]
fn the_held_message_surfaces_with_structure_not_content() {
    let fixture = Fixture::new();
    let found = fixture.findings();
    assert_eq!(1, found.len(), "{found:?}");
    let item = &found[0];
    assert_eq!("delivery.ambiguous_queued", item["rule"]);
    assert_eq!(MESSAGE, item["key"]);
    assert_eq!(13, item["evidence"]["holds"]);
    assert_eq!(40.0, item["evidence"]["age_minutes"]);
    assert_eq!(PANE, item["evidence"]["pane"]);
    assert_eq!("ambiguous_reader", item["evidence"]["reason_class"]);
    assert_eq!(
        json!({"prompt_row": 18, "footer_row": 23, "blank_rows": 2}),
        item["evidence"]["shape"]
    );
    assert_eq!(60, item["evidence"]["last_refusal_age_s"]);
    assert_eq!(300, item["evidence"]["next_attempt_in_s"]);
    let blob = item.to_string();
    assert!(!blob.contains(SECRET));
    assert!(!blob.contains("t372-wiring"));
    for claim in [
        "\"is dead\"",
        "dead session",
        "recipient is dead",
        "agent is dead",
    ] {
        assert!(
            !blob.to_lowercase().contains(claim),
            "unexpected death claim: {claim}"
        );
    }
    assert!(item["summary"].as_str().unwrap().contains("not terminal"));
    assert!(item["summary"]
        .as_str()
        .unwrap()
        .contains("not assumed dead"));
}

#[test]
fn young_or_lightly_retried_messages_are_not_reported() {
    let mut fixture = Fixture::new();
    fixture.outbox[0]["created_at"] = json!(NOW - 5.0 * 60.0);
    assert!(fixture.findings().is_empty());
    fixture.outbox[0]["created_at"] = json!(NOW - 40.0 * 60.0);
    fixture.outbox[0]["holds"] = json!(3);
    assert!(fixture.findings().is_empty());
}

#[test]
fn a_turn_wait_is_not_this_class() {
    let fixture = Fixture::new();
    fixture.insert(&[(TURN_WAIT, NOW - 60.0)]);
    assert!(fixture.findings().is_empty());
}

#[test]
fn a_cleared_or_decided_message_is_no_longer_reported() {
    let mut fixture = Fixture::new();
    fixture.outbox.clear();
    assert!(fixture.findings().is_empty());
}

#[test]
fn falsey_identity_and_creation_values_keep_python_defaults() {
    let mut fixture = Fixture::new();
    fixture.outbox[0]["id"] = Value::Null;
    assert!(fixture.findings().is_empty());
    fixture.outbox[0]["id"] = json!(MESSAGE);
    fixture.outbox[0]["pane_id"] = json!("");
    assert!(fixture.findings().is_empty());
    fixture.outbox[0]["pane_id"] = json!(PANE);
    fixture.outbox[0]["created_at"] = json!(0);
    assert!(fixture.findings().is_empty());
}

#[test]
fn the_rule_is_read_only_and_leaves_the_queue_payload_unchanged() {
    let fixture = Fixture::new();
    let before = fixture.outbox.clone();
    let event_count: i64 = Connection::open(&fixture.metrics)
        .unwrap()
        .query_row("SELECT count(*) FROM events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(1, fixture.findings().len());
    assert_eq!(before, fixture.outbox);
    let after_count: i64 = Connection::open(&fixture.metrics)
        .unwrap()
        .query_row("SELECT count(*) FROM events", [], |row| row.get(0))
        .unwrap();
    assert_eq!(event_count, after_count);
    assert_eq!(SECRET, fixture.outbox[0]["text"]);
    assert_eq!(13, fixture.outbox[0]["holds"]);
    assert_eq!(NOW + 300.0, fixture.outbox[0]["next_attempt_at"]);
}

#[test]
fn future_ambiguous_rows_cannot_override_a_current_turn_wait() {
    let fixture = Fixture::new();
    fixture.insert(&[(TURN_WAIT, NOW - 60.0), (AMBIGUOUS, NOW + 3600.0)]);
    assert!(fixture.findings().is_empty());
}

#[test]
fn a_future_only_ambiguous_row_is_not_reported() {
    let fixture = Fixture::new();
    fixture.insert(&[(AMBIGUOUS, NOW + 3600.0)]);
    assert!(fixture.findings().is_empty());
}

#[test]
fn current_ambiguous_observation_reports_its_freshness() {
    let fixture = Fixture::new();
    fixture.insert(&[(AMBIGUOUS, NOW - 30.0)]);
    let found = fixture.findings();
    assert_eq!(1, found.len());
    assert_eq!(30, found[0]["evidence"]["last_refusal_age_s"]);
}

#[test]
fn arbitrary_reason_suffix_never_reaches_the_finding() {
    let fixture = Fixture::new();
    let sentinel = "PRIVATE_SENTINEL_CONTENT_9f2b";
    fixture.insert(&[(&format!("{AMBIGUOUS} {sentinel}"), NOW - 60.0)]);
    let found = fixture.findings();
    assert_eq!(1, found.len());
    assert!(!serde_json::to_string(&found).unwrap().contains(sentinel));
}

#[test]
fn an_unrecognised_structural_sentence_reports_the_class_only() {
    let fixture = Fixture::new();
    fixture.insert(&[(
        "ambiguous composer/transcript: PRIVATE_SENTINEL_CONTENT_9f2b",
        NOW - 60.0,
    )]);
    let found = fixture.findings();
    assert_eq!(1, found.len());
    assert_eq!(json!({}), found[0]["evidence"]["shape"]);
    assert!(!serde_json::to_string(&found)
        .unwrap()
        .contains("PRIVATE_SENTINEL_CONTENT_9f2b"));
}

#[test]
fn metrics_errors_are_a_no_finding_result() {
    let fixture = Fixture::new();
    let absent = MetricsDb {
        path: &fixture._dir.path().join("missing.db"),
    };
    assert!(
        rule_delivery_ambiguous_queued(&fixture.outbox, &absent, &fixture.rule(), NOW).is_empty()
    );
}

#[test]
fn marker_matches_the_documented_ambiguous_composer_class() {
    assert_eq!("ambiguous composer/transcript", AMBIGUOUS_READER_MARKER);
}
