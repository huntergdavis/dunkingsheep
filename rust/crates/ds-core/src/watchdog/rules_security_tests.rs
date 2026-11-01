//! Port of the secret-scan test of `tests/test_dunk_watchdog.py` (t1400 slice F1), against real
//! `inbox.db` / `tasks.db` / `dunks.json` fixtures and the shipped example's real patterns, plus the
//! edge cases a secret scanner must get right (line splitting, the second read, lookback, hashes
//! only). The Python runs it through `evaluate` (a later slice), so the rule is called directly.

use std::cell::Cell;
use std::path::PathBuf;

use rusqlite::Connection;
use serde_json::{json, Value};

use super::*;

const NOW: f64 = 1_800_000_000.0;
const HOUR: f64 = 3600.0;
/// Synthetic: matches the pattern, is no credential (the Python suite's `FAKE_TOKEN`).
const FAKE_TOKEN: &str = "ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8";
const EXAMPLE: &str = include_str!("../../../../../packaging/watchdog.example.json");

struct Fleet {
    dir: tempfile::TempDir,
}

impl Fleet {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let run = |db: &str, sql: &str| {
            Connection::open(dir.path().join(db))
                .unwrap()
                .execute(sql, [])
                .unwrap();
        };
        run(
            "tasks.db",
            "CREATE TABLE tasks (id TEXT PRIMARY KEY, scope TEXT NOT NULL, title TEXT NOT NULL, \
            body TEXT NOT NULL DEFAULT '', status TEXT NOT NULL DEFAULT 'open', \
            priority TEXT NOT NULL DEFAULT 'medium', parent_id TEXT, \
            multi_assignee INTEGER NOT NULL DEFAULT 0, attempts INTEGER NOT NULL DEFAULT 0, \
            assignees TEXT NOT NULL DEFAULT '[]', depends_on TEXT NOT NULL DEFAULT '[]', \
            created_at REAL NOT NULL, updated_at REAL NOT NULL, created_by TEXT, \
            completed_by TEXT, completed_at REAL, cancelled_at REAL, block_reason TEXT)",
        );
        run("tasks.db", "CREATE TABLE task_leases (task_id TEXT NOT NULL, pane_id TEXT NOT NULL, \
            claimed_at REAL NOT NULL, heartbeat_at REAL NOT NULL, expires_at REAL NOT NULL, \
            caller_pid INTEGER, caller_start TEXT, caller_name TEXT, PRIMARY KEY (task_id, pane_id))");
        run(
            "tasks.db",
            "CREATE TABLE task_events (id INTEGER PRIMARY KEY AUTOINCREMENT, \
            task_id TEXT NOT NULL, event TEXT NOT NULL, at REAL NOT NULL, actor TEXT, scope TEXT, \
            priority TEXT, status_from TEXT, status_to TEXT, duration_ms INTEGER, meta TEXT)",
        );
        run("inbox.db", "CREATE TABLE messages (id TEXT PRIMARY KEY, mailbox TEXT NOT NULL, \
            mailbox_label TEXT, pane_id TEXT, sender TEXT, subject TEXT, body TEXT NOT NULL, \
            state TEXT NOT NULL, created_at REAL NOT NULL, read_at REAL, acked_at REAL, \
            cancelled_at REAL, reason TEXT, notify_state TEXT, notify_reason TEXT, archived_at REAL)");
        std::fs::write(
            dir.path().join("dunks.json"),
            r#"{"version":1,"next_id":1,"dunkers":[]}"#,
        )
        .unwrap();
        Fleet { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn message(&self, id: &str, created_at: f64, body: &str) {
        Connection::open(self.path("inbox.db"))
            .unwrap()
            .execute(
                "INSERT INTO messages (id, mailbox, body, state, created_at) VALUES (?,?,?,?,?)",
                rusqlite::params![id, "x", body, "read", created_at],
            )
            .unwrap();
    }

    fn task(&self, id: &str, title: &str, body: &str) {
        Connection::open(self.path("tasks.db"))
            .unwrap()
            .execute(
                "INSERT INTO tasks (id, scope, title, body, status, created_at, updated_at) \
                 VALUES (?, 'global', ?, ?, 'open', ?, ?)",
                rusqlite::params![id, title, body, NOW, NOW],
            )
            .unwrap();
    }

    fn dunks(&self, dunkers: Value) {
        std::fs::write(
            self.path("dunks.json"),
            json!({"dunkers": dunkers}).to_string(),
        )
        .unwrap();
    }

    fn records(&self, since: f64) -> Result<Vec<Record>, DunkError> {
        text_records(
            &self.path("inbox.db"),
            &self.path("tasks.db"),
            &self.path("dunks.json"),
            since,
            NOW,
        )
    }

    fn scan_result(&self, rule: &Value) -> Result<Vec<Value>, DunkError> {
        let read = |since: f64| self.records(since);
        rule_security_secret_text(&read, rule, NOW)
    }

    fn scan(&self, rule: &Value) -> Vec<Value> {
        self.scan_result(rule).unwrap()
    }
}

/// The shipped example's secret rule: its real patterns, class `security`.
fn rule() -> Value {
    let config: Value = serde_json::from_str(EXAMPLE).unwrap();
    let mut rule = config["rules"]["security.secret_text"].clone();
    rule["lookback_hours"] = json!(24);
    rule["allowlist_sha256"] = json!([]);
    rule
}

fn sha(line: &str) -> String {
    format!("{:x}", Sha256::digest(line.as_bytes()))
}

#[test]
fn test_secret_scan_reports_hashes_only_and_honours_the_allowlist() {
    let fleet = Fleet::new();
    fleet.message(
        "m1",
        NOW - HOUR,
        &format!("here it is\n{FAKE_TOKEN}\nthanks"),
    );
    fleet.message("m2", NOW - 25.0 * HOUR, FAKE_TOKEN); // outside lookback
    fleet.task("t1", "a task", &format!("token: {FAKE_TOKEN}"));
    let mut rule = rule();
    let found = fleet.scan(&rule);
    let kinds: Vec<&str> = found
        .iter()
        .map(|f| f["evidence"]["kind"].as_str().unwrap())
        .collect();
    assert_eq!(vec!["inbox", "task"], kinds);
    assert!(
        !serde_json::to_string(&found).unwrap().contains(FAKE_TOKEN),
        "evidence is (kind, id, sha256), never the match"
    );
    assert_eq!("security", found[0]["class"]);
    // the digest is of the matching LINE, not of the whole body
    assert_eq!(json!(sha(FAKE_TOKEN)), found[0]["evidence"]["sha256"]);
    assert_eq!(
        json!(sha(&format!("token: {FAKE_TOKEN}"))),
        found[1]["evidence"]["sha256"]
    );
    assert_eq!("a credential-shaped line in inbox m1", found[0]["summary"]);
    assert_eq!("24h lookback (inbox)", found[0]["window"]);
    assert_eq!(
        format!("inbox:m1:{}", &sha(FAKE_TOKEN)[..16]),
        found[0]["key"].as_str().unwrap()
    );
    // the allowlist (sorted unique digests of what was found) silences both
    let mut allow: Vec<String> = found
        .iter()
        .map(|f| f["evidence"]["sha256"].as_str().unwrap().to_string())
        .collect();
    allow.sort();
    allow.dedup();
    rule["allowlist_sha256"] = json!(allow);
    assert!(fleet.scan(&rule).is_empty());
}

// -- extra: what a secret scanner must get right -------------------------------------------------

#[test]
fn extra_python_splitlines_semantics() {
    assert_eq!(vec!["a", "b"], py_splitlines("a\nb"));
    assert_eq!(vec!["a", "b"], py_splitlines("a\r\nb\n"));
    assert_eq!(vec!["a", "", "b"], py_splitlines("a\n\nb"));
    assert_eq!(vec!["a", "b", "c"], py_splitlines("a\x0cb\x0bc"));
    assert_eq!(
        vec!["a", "b", "c", "d"],
        py_splitlines("a\u{85}b\u{2028}c\u{2029}d")
    );
    assert_eq!(vec!["a", "b"], py_splitlines("a\rb"));
    assert_eq!(vec!["a", "", "b"], py_splitlines("a\r\r\nb"));
    assert!(py_splitlines("").is_empty());
}

#[test]
fn extra_a_credential_behind_a_form_feed_or_bare_cr_is_still_found() {
    // Rust's `lines()` would treat `x\x0c<token>` as ONE line, whose hash differs from the line
    // Python hashes: the finding (and any allowlist entry) must be for the LINE after the break.
    let fleet = Fleet::new();
    fleet.message("m1", NOW - HOUR, &format!("noise\x0c{FAKE_TOKEN}\rmore"));
    let found = fleet.scan(&rule());
    assert_eq!(1, found.len());
    assert_eq!(json!(sha(FAKE_TOKEN)), found[0]["evidence"]["sha256"]);
}

#[test]
fn extra_nonempty_string_dunkers_is_an_error_instead_of_an_empty_security_scan() {
    let fleet = Fleet::new();
    fleet.dunks(json!("not-a-list"));
    assert!(text_records(
        &fleet.path("inbox.db"),
        &fleet.path("tasks.db"),
        &fleet.path("dunks.json"),
        NOW - 24.0 * HOUR,
        NOW,
    )
    .is_err());
}

#[test]
fn extra_scalar_dunk_record_is_an_error_instead_of_an_empty_security_record() {
    let fleet = Fleet::new();
    fleet.dunks(json!([17]));
    assert!(fleet
        .scan_result(&rule())
        .unwrap_err()
        .to_string()
        .contains("dunk record 0 is not an object"));
}

#[test]
fn extra_dunk_without_an_id_is_an_error() {
    let fleet = Fleet::new();
    fleet.dunks(json!([{"text": FAKE_TOKEN}]));
    assert!(fleet
        .scan_result(&rule())
        .unwrap_err()
        .to_string()
        .contains("dunk record 0 has no id"));
}

#[test]
fn extra_nonstring_dunk_id_is_rejected_instead_of_coerced_to_empty() {
    let fleet = Fleet::new();
    fleet.dunks(json!([{"id": 7, "text": FAKE_TOKEN}]));
    assert!(fleet
        .scan_result(&rule())
        .unwrap_err()
        .to_string()
        .contains("dunk record 0 id is not a string"));
}

#[test]
fn extra_truthy_nonstring_dunk_text_is_an_error_instead_of_a_clean_scan() {
    let fleet = Fleet::new();
    fleet.dunks(json!([{"id": "d1", "text": 123}]));
    assert!(fleet
        .scan_result(&rule())
        .unwrap_err()
        .to_string()
        .contains("dunk record 0 text is not a string"));
}

#[test]
fn extra_falsey_dunk_text_keeps_python_empty_default_and_string_id_order() {
    let fleet = Fleet::new();
    fleet.dunks(json!([
        {"id": "d2", "text": null},
        {"id": "d10", "text": false},
        {"id": "d4", "text": 0},
        {"id": "d3", "text": 0.0},
        {"id": "d1", "text": ""},
        {"id": "d5", "text": []},
        {"id": "d6", "text": {}},
        {"id": "d0"}
    ]));
    let records = fleet.records(NOW - 24.0 * HOUR).unwrap();
    let dunks: Vec<(&str, &str)> = records
        .iter()
        .filter(|(kind, _, _)| kind == "dunk")
        .map(|(_, id, text)| (id.as_str(), text.as_str()))
        .collect();
    assert_eq!(
        vec![
            ("d0", ""),
            ("d1", ""),
            ("d10", ""),
            ("d2", ""),
            ("d3", ""),
            ("d4", ""),
            ("d5", ""),
            ("d6", "")
        ],
        dunks
    );
}

#[test]
fn extra_dunk_texts_and_task_titles_are_scanned_and_clean_content_is_quiet() {
    let fleet = Fleet::new();
    fleet.dunks(json!([{"id": "d2", "text": format!("run it with {FAKE_TOKEN}")}, {"id": "d1", "text": "all fine"}]));
    fleet.task("t9", &format!("title has {FAKE_TOKEN}"), "");
    fleet.message("m1", NOW - HOUR, "nothing to see");
    let found = fleet.scan(&rule());
    let who: Vec<(&str, &str)> = found
        .iter()
        .map(|f| {
            (
                f["evidence"]["kind"].as_str().unwrap(),
                f["evidence"]["id"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(vec![("dunk", "d2"), ("task", "t9")], who);
}

#[test]
fn extra_a_match_that_does_not_survive_the_second_read_is_not_reported() {
    // Re-verify on a second, independent read before anything can page (spec section 2).
    let calls = Cell::new(0);
    let read = |_since: f64| {
        calls.set(calls.get() + 1);
        let body = if calls.get() == 1 {
            FAKE_TOKEN
        } else {
            "scrubbed"
        };
        Ok(vec![(
            "inbox".to_string(),
            "m1".to_string(),
            body.to_string(),
        )])
    };
    assert!(rule_security_secret_text(&read, &rule(), NOW)
        .unwrap()
        .is_empty());
    assert_eq!(2, calls.get(), "a first hit is always re-read");
    // and a clean first read never pays for a second
    let clean_calls = Cell::new(0);
    let clean = |_since: f64| {
        clean_calls.set(clean_calls.get() + 1);
        Ok(vec![(
            "inbox".to_string(),
            "m1".to_string(),
            "nothing".to_string(),
        )])
    };
    assert!(rule_security_secret_text(&clean, &rule(), NOW)
        .unwrap()
        .is_empty());
    assert_eq!(1, clean_calls.get());
}

#[test]
fn extra_an_unreadable_store_is_an_error_not_a_pass() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope");
    assert!(text_records(&missing, &missing, &missing, 0.0, NOW).is_err());
    let read = |_since: f64| Err(DunkError::Refused("store gone".into()));
    assert_eq!(
        "store gone",
        rule_security_secret_text(&read, &rule(), NOW)
            .unwrap_err()
            .to_string()
    );
}

#[test]
fn extra_every_shipped_pattern_matches_its_own_example_and_nothing_innocent() {
    let patterns: Vec<Regex> = rule()["patterns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| Regex::new(p.as_str().unwrap()).unwrap())
        .collect();
    let hit = |text: &str| patterns.iter().any(|p| p.is_match(text));
    assert!(hit(FAKE_TOKEN));
    assert!(hit(&format!("sk-ant-{}", "a".repeat(24))));
    assert!(hit("AKIA0123456789ABCDEF"));
    assert!(hit(&format!("sk-{}", "A1".repeat(20))));
    assert!(!hit("a perfectly ordinary sentence about sk learning"));
    assert!(!hit("ghp_tooshort"));
}
