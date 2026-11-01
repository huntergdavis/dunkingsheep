//! Ports of the inbox-rule tests of `tests/test_dunk_watchdog.py` (t1400 slice C). The Python tests
//! run them through `evaluate` (a later slice), so each calls the rule on the same fixture instead;
//! the expectations are the Python's. The store is a real read-only `inbox.db` with the real schema.

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde_json::{json, Value};

use super::*;

const NOW: f64 = 1_800_000_000.0;
const MIN: f64 = 60.0;
const HOUR: f64 = 3600.0;
const PATTERN: &str = "^w[0-9A-Za-z]+:p[0-9A-Za-z]+$";

struct Fleet {
    dir: tempfile::TempDir,
    db: PathBuf,
    seq: std::cell::Cell<u32>,
}

impl Fleet {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("inbox.db");
        Connection::open(&db)
            .unwrap()
            .execute(
                "CREATE TABLE messages (id TEXT PRIMARY KEY, mailbox TEXT NOT NULL, mailbox_label TEXT, \
                 pane_id TEXT, sender TEXT, subject TEXT, body TEXT NOT NULL, state TEXT NOT NULL, \
                 created_at REAL NOT NULL, read_at REAL, acked_at REAL, cancelled_at REAL, reason TEXT, \
                 notify_state TEXT, notify_reason TEXT, archived_at REAL)",
                [],
            )
            .unwrap();
        Fleet {
            dir,
            db,
            seq: std::cell::Cell::new(0),
        }
    }

    fn message(
        &self,
        mailbox: &str,
        state: &str,
        created_at: f64,
        pane_id: Option<&str>,
        sender: Option<&str>,
        reason: Option<&str>,
    ) -> String {
        self.seq.set(self.seq.get() + 1);
        let id = format!("i{}", self.seq.get());
        Connection::open(&self.db)
            .unwrap()
            .execute(
                "INSERT INTO messages (id, mailbox, pane_id, sender, body, state, created_at, reason, \
                 read_at) VALUES (?,?,?,?,?,?,?,?,?)",
                rusqlite::params![
                    id, mailbox, pane_id, sender, "BODY-NEVER-REPORTED", state, created_at, reason,
                    Option::<f64>::None
                ],
            )
            .unwrap();
        id
    }

    fn inbox(&self) -> InboxDb<'_> {
        InboxDb { path: &self.db }
    }

    fn retired(&self) -> PathBuf {
        self.dir.path().join("retired-panes.json")
    }
}

fn panes(list: &[(&str, &str)]) -> Panes {
    list.iter()
        .map(|(p, s)| (p.to_string(), s.to_string()))
        .collect()
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

fn pair(rule: &str, key: &str) -> (String, String) {
    (rule.to_string(), key.to_string())
}

fn undeliverable_rule(min_messages: i64, min_age_hours: f64) -> Value {
    json!({"severity": "warning", "class": "ops", "min_messages": min_messages,
           "min_age_hours": min_age_hours})
}

fn live_rule(age: i64, min: i64, idle: &[&str]) -> Value {
    json!({"severity": "warning", "class": "ops", "age_minutes": age, "min_messages": min,
           "idle_statuses": idle})
}

fn dead_rule(min: i64, age: i64) -> Value {
    json!({"severity": "warning", "class": "ops", "min_messages": min, "min_age_minutes": age})
}

fn write_retired(path: &Path, body: Value) {
    std::fs::write(path, body.to_string()).unwrap();
}

#[test]
fn test_inbox_undeliverable_backlog_has_no_age_limit() {
    let fleet = Fleet::new();
    for _ in 0..2 {
        fleet.message(
            "w9:p8",
            "undeliverable",
            NOW - 30.0 * 24.0 * HOUR,
            None,
            None,
            Some("target pane no longer exists"),
        );
    }
    fleet.message(
        "w9:p7",
        "undeliverable",
        NOW - 30.0 * HOUR,
        None,
        None,
        None,
    );
    let found =
        rule_inbox_undeliverable(&fleet.inbox(), &undeliverable_rule(2, 0.01), NOW).unwrap();
    assert_eq!(vec![pair("inbox.undeliverable", "w9:p8")], keys(&found));
    assert!(!serde_json::to_string(&found)
        .unwrap()
        .contains("BODY-NEVER-REPORTED"));
    // the texts a dry-run line prints
    assert_eq!("2 msgs older than 0.01h", found[0]["threshold"]);
    assert_eq!("2 msgs, oldest 720.0h", found[0]["measured"]);
    assert_eq!(
        json!({"target pane no longer exists": 2}),
        found[0]["evidence"]["reasons"]
    );
}

#[test]
fn test_inbox_undeliverable_pages_only_above_the_auto_archive_age() {
    // The auto-archive clears undeliverable mail at 24h, so a page inside that window describes
    // mail that is about to disappear on its own. The floor sits above it.
    let fleet = Fleet::new();
    let rule = undeliverable_rule(2, 26.0);
    for _ in 0..3 {
        fleet.message(
            "young",
            "undeliverable",
            NOW - 3.0 * HOUR,
            None,
            None,
            Some("target pane no longer exists"),
        );
    }
    assert!(rule_inbox_undeliverable(&fleet.inbox(), &rule, NOW)
        .unwrap()
        .is_empty());
    for _ in 0..3 {
        fleet.message(
            "stuck",
            "undeliverable",
            NOW - 30.0 * HOUR,
            None,
            None,
            Some("target pane no longer exists"),
        );
    }
    let found = rule_inbox_undeliverable(&fleet.inbox(), &rule, NOW).unwrap();
    assert_eq!(vec![pair("inbox.undeliverable", "stuck")], keys(&found));
}

#[test]
fn test_a_retired_pane_is_reported_not_paged() {
    let fleet = Fleet::new();
    let rule = live_rule(60, 1, &["idle", "done"]);
    let p = panes(&[("w1:p1", "idle")]);
    fleet.message(
        "alive",
        "queued",
        NOW - 120.0 * MIN,
        Some("w1:p1"),
        None,
        None,
    );
    fleet.message(
        "retired",
        "queued",
        NOW - 120.0 * MIN,
        Some("w9:p9"),
        None,
        None,
    );
    // `retired` is queued for w9:p9, which is not in the idle live set: it is excluded by the pane
    // filter. The retired record makes the mailbox NAME exclusion explicit as well.
    write_retired(
        &fleet.retired(),
        json!({"w9:p9": {"pane_id": "w9:p9",
        "reason": "superseded-by:w9:p10", "labels": [], "retired_at": NOW}}),
    );
    let found =
        rule_inbox_unread_live(&fleet.inbox(), Some(&p), &fleet.retired(), &rule, NOW).unwrap();
    let k = keys(&found);
    assert!(k.contains(&pair("inbox.unread_live", "alive")));
    assert!(
        !k.contains(&pair("inbox.unread_live", "retired")),
        "a retired pane must not page"
    );
}

#[test]
fn test_a_retired_mailbox_name_is_skipped_too() {
    // The name-vs-id instance: the row's mailbox is 'crackdown-codex', the retired pane is 'wS:p2'.
    // Keying on the pane id alone kept alerting.
    let fleet = Fleet::new();
    let rule = live_rule(60, 1, &["idle", "done"]);
    let p = panes(&[("w1:p1", "idle")]);
    fleet.message(
        "crackdown-codex",
        "queued",
        NOW - 42.0 * HOUR,
        Some("w1:p1"),
        None,
        None,
    );
    fleet.message(
        "alive",
        "queued",
        NOW - 42.0 * HOUR,
        Some("w1:p1"),
        None,
        None,
    );
    write_retired(
        &fleet.retired(),
        json!({"wS:p2": {"pane_id": "wS:p2", "reason": "shelved",
        "labels": [], "mailboxes": ["crackdown-codex"], "retired_at": NOW}}),
    );
    let found =
        rule_inbox_unread_live(&fleet.inbox(), Some(&p), &fleet.retired(), &rule, NOW).unwrap();
    let k = keys(&found);
    assert!(k.contains(&pair("inbox.unread_live", "alive")));
    assert!(
        !k.contains(&pair("inbox.unread_live", "crackdown-codex")),
        "a retired mailbox NAME must not page"
    );
}

#[test]
fn test_a_busy_lane_with_old_unread_mail_is_a_queue_not_a_miss() {
    // par-c authoring a body for four hours is throughput, not a missed lane.
    let fleet = Fleet::new();
    let rule = live_rule(60, 1, &["idle", "done"]);
    let p = panes(&[("w1:p1", "working"), ("w1:p2", "idle")]);
    fleet.message(
        "busy",
        "queued",
        NOW - 4.0 * 3600.0,
        Some("w1:p1"),
        None,
        None,
    );
    fleet.message(
        "idle",
        "queued",
        NOW - 4.0 * 3600.0,
        Some("w1:p2"),
        None,
        None,
    );
    let found =
        rule_inbox_unread_live(&fleet.inbox(), Some(&p), &fleet.retired(), &rule, NOW).unwrap();
    let k = keys(&found);
    assert!(k.contains(&pair("inbox.unread_live", "idle")));
    assert!(
        !k.contains(&pair("inbox.unread_live", "busy")),
        "a working lane's unread mail is a queue, not a miss"
    );
}

#[test]
fn test_an_unknown_state_does_not_page_as_a_miss() {
    let fleet = Fleet::new();
    let rule = live_rule(60, 1, &["idle", "done"]);
    let p = panes(&[("w1:p1", "unknown")]);
    fleet.message(
        "mystery",
        "queued",
        NOW - 4.0 * 3600.0,
        Some("w1:p1"),
        None,
        None,
    );
    let found =
        rule_inbox_unread_live(&fleet.inbox(), Some(&p), &fleet.retired(), &rule, NOW).unwrap();
    assert!(found.is_empty());
}

#[test]
fn test_inbox_unread_live_and_dead() {
    let fleet = Fleet::new();
    let live = live_rule(60, 1, &["idle", "done"]);
    let dead = dead_rule(1, 10);
    let p = panes(&[("w1:p1", "idle")]);
    fleet.message(
        "alive",
        "queued",
        NOW - 60.0 * MIN,
        Some("w1:p1"),
        None,
        None,
    );
    fleet.message(
        "alive2",
        "queued",
        NOW - 60.0 * MIN + 1.0,
        Some("w1:p1"),
        None,
        None,
    );
    fleet.message(
        "gone",
        "queued",
        NOW - 11.0 * MIN,
        Some("w9:p9"),
        None,
        None,
    );
    let mut found =
        rule_inbox_unread_dead(&fleet.inbox(), Some(&p), &fleet.retired(), &dead, NOW).unwrap();
    found.extend(
        rule_inbox_unread_live(&fleet.inbox(), Some(&p), &fleet.retired(), &live, NOW).unwrap(),
    );
    assert_eq!(
        vec![
            pair("inbox.unread_dead", "gone"),
            pair("inbox.unread_live", "alive")
        ],
        keys(&found)
    );
}

#[test]
fn test_unread_dead_needs_a_minimum_age() {
    // Measured 2026-10-24: the rule fired on the vp mailbox with a message 0.0h old, because a lane
    // being handed over looks exactly like a dead one for the first minutes. Tested in BOTH
    // directions - a threshold nobody tested on the quiet side is just a constant.
    let fleet = Fleet::new();
    let rule = dead_rule(1, 10);
    let none = panes(&[]);
    fleet.message("fresh", "queued", NOW - 30.0, Some("w9:p9"), None, None);
    let found =
        rule_inbox_unread_dead(&fleet.inbox(), Some(&none), &fleet.retired(), &rule, NOW).unwrap();
    assert!(
        found.is_empty(),
        "30 seconds old is not evidence the lane is dead"
    );
    fleet.message(
        "stale",
        "queued",
        NOW - 11.0 * MIN,
        Some("w9:p8"),
        None,
        None,
    );
    let k = keys(
        &rule_inbox_unread_dead(&fleet.inbox(), Some(&none), &fleet.retired(), &rule, NOW).unwrap(),
    );
    assert!(k.contains(&pair("inbox.unread_dead", "stale")));
    assert!(!k.contains(&pair("inbox.unread_dead", "fresh")));
}

#[test]
fn test_the_unread_dead_age_is_configurable() {
    let fleet = Fleet::new();
    let rule = dead_rule(1, 30);
    let none = panes(&[]);
    fleet.message(
        "eleven",
        "queued",
        NOW - 11.0 * MIN,
        Some("w9:p9"),
        None,
        None,
    );
    let found =
        rule_inbox_unread_dead(&fleet.inbox(), Some(&none), &fleet.retired(), &rule, NOW).unwrap();
    assert!(
        found.is_empty(),
        "11 minutes is under a 30-minute threshold"
    );
    fleet.message(
        "long",
        "queued",
        NOW - 31.0 * MIN,
        Some("w9:p8"),
        None,
        None,
    );
    let k = keys(
        &rule_inbox_unread_dead(&fleet.inbox(), Some(&none), &fleet.retired(), &rule, NOW).unwrap(),
    );
    assert!(k.contains(&pair("inbox.unread_dead", "long")));
}

fn ambiguous_rule(lookback: i64, samples: i64, live: bool) -> Value {
    json!({"severity": "warning", "class": "ops", "lookback_hours": lookback, "sample_ids": samples,
           "require_live_pane": live})
}

#[test]
fn test_inbox_ambiguous_sender_names_only_pane_resolving_labels() {
    let fleet = Fleet::new();
    let p = panes(&[("wR:p4", "idle")]);
    fleet.message(
        "codex poolrad",
        "read",
        NOW - 48.0 * HOUR,
        Some("wR:p4"),
        None,
        None,
    );
    fleet.message(
        "dead lane",
        "read",
        NOW - 48.0 * HOUR,
        Some("w9:p9"),
        None,
        None,
    );
    let ids: Vec<String> = (0..3)
        .map(|_| fleet.message("x", "read", NOW - HOUR, None, Some("codex poolrad"), None))
        .collect();
    fleet.message("x", "read", NOW - HOUR, None, Some("dead lane"), None);
    fleet.message("x", "read", NOW - HOUR, None, Some("legacy-name"), None);
    fleet.message("x", "read", NOW - HOUR, None, Some("wR:p4"), None);
    let found = rule_inbox_ambiguous_sender(
        &fleet.inbox(),
        PATTERN,
        Some(&p),
        &ambiguous_rule(24, 2, true),
        NOW,
    )
    .unwrap();
    assert_eq!(
        vec![pair("inbox.ambiguous_sender", "codex poolrad")],
        keys(&found)
    );
    assert_eq!(json!(ids[..2]), found[0]["evidence"]["sample_ids"]);
    assert_eq!(
        "sender 'codex poolrad' is a mailbox name resolving to pane wR:p4, not a pane id",
        found[0]["summary"]
    );
    assert_eq!("3 msgs", found[0]["measured"]);
    assert_eq!("24h lookback", found[0]["window"]);
    assert_eq!("live", found[0]["evidence"]["pane_liveness"]);
}

// -- extra: the branches the Python suite does not name ----------------------------------------

#[test]
fn extra_without_require_live_pane_a_dead_lane_name_is_reported_unchecked() {
    let fleet = Fleet::new();
    fleet.message(
        "dead lane",
        "read",
        NOW - 48.0 * HOUR,
        Some("w9:p9"),
        None,
        None,
    );
    fleet.message("x", "read", NOW - HOUR, None, Some("dead lane"), None);
    let found = rule_inbox_ambiguous_sender(
        &fleet.inbox(),
        PATTERN,
        Some(&panes(&[])),
        &ambiguous_rule(24, 2, false),
        NOW,
    )
    .unwrap();
    assert_eq!(
        vec![pair("inbox.ambiguous_sender", "dead lane")],
        keys(&found)
    );
    assert_eq!("unchecked", found[0]["evidence"]["pane_liveness"]);
}

#[test]
fn extra_with_the_pane_source_disabled_unread_live_still_pages_and_says_so() {
    let fleet = Fleet::new();
    fleet.message(
        "anyone",
        "queued",
        NOW - 4.0 * HOUR,
        Some("w1:p1"),
        None,
        None,
    );
    let found = rule_inbox_unread_live(
        &fleet.inbox(),
        None,
        &fleet.retired(),
        &live_rule(60, 1, &["idle"]),
        NOW,
    )
    .unwrap();
    assert_eq!(1, found.len());
    assert_eq!(
        "unknown (pane source disabled)",
        found[0]["evidence"]["pane_liveness"]
    );
    assert_eq!(
        "unknown (pane source disabled)",
        found[0]["evidence"]["pane_state"]
    );
}

#[test]
fn extra_with_the_pane_source_disabled_unread_dead_is_an_error_like_the_pythons_type_error() {
    let fleet = Fleet::new();
    fleet.message("x", "queued", NOW - HOUR, Some("w1:p1"), None, None);
    let error = rule_inbox_unread_dead(
        &fleet.inbox(),
        None,
        &fleet.retired(),
        &dead_rule(1, 10),
        NOW,
    )
    .unwrap_err();
    assert_eq!(
        "argument of type 'NoneType' is not iterable",
        error.to_string()
    );
    // ... but with nothing to judge it is quiet, as in Python (the loop body never runs).
    let empty = Fleet::new();
    assert!(rule_inbox_unread_dead(
        &empty.inbox(),
        None,
        &empty.retired(),
        &dead_rule(1, 10),
        NOW
    )
    .unwrap()
    .is_empty());
}

#[test]
fn extra_idle_statuses_are_compared_case_insensitively_and_each_listed_status_pages() {
    let fleet = Fleet::new();
    let p = panes(&[
        ("w1:p1", "IDLE"),
        ("w1:p2", "Done"),
        ("w1:p3", "blocked"),
        ("w1:p4", "unknown"),
    ]);
    for (n, pane) in ["w1:p1", "w1:p2", "w1:p3", "w1:p4"].iter().enumerate() {
        fleet.message(
            &format!("m{n}"),
            "queued",
            NOW - 4.0 * HOUR,
            Some(pane),
            None,
            None,
        );
    }
    for (statuses, expect) in [
        (vec!["idle"], vec!["m0"]),
        (vec!["idle", "done"], vec!["m0", "m1"]),
        (vec!["blocked"], vec!["m2"]),
        (vec!["unknown"], vec!["m3"]),
    ] {
        let found = rule_inbox_unread_live(
            &fleet.inbox(),
            Some(&p),
            &fleet.retired(),
            &live_rule(60, 1, &statuses),
            NOW,
        )
        .unwrap();
        let got: Vec<&str> = found.iter().map(|f| f["key"].as_str().unwrap()).collect();
        assert_eq!(expect, got, "{statuses:?}");
    }
}

#[test]
fn extra_the_inbox_store_must_exist() {
    let db = InboxDb {
        path: Path::new("/nonexistent/inbox.db"),
    };
    assert!(rule_inbox_undeliverable(&db, &undeliverable_rule(1, 1.0), NOW).is_err());
    assert!(db.queued(NOW).is_err());
    assert!(db.name_to_pane(NOW).is_err());
}

#[test]
fn extra_match_is_anchored_at_the_start_only_like_re_match() {
    let pattern = Regex::new("w[0-9]+:p[0-9]+").unwrap(); // no ^ and no $
    assert!(matches_at_start(&pattern, "w1:p2 trailing"));
    assert!(!matches_at_start(&pattern, "lead w1:p2"));
}
