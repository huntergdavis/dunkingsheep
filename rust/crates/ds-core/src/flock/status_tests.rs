//! Behaviour tests for T38b, from the docstrings of `Flock.status/poll_status/view` and
//! `h_status` and `test_dunk_server::test_status_and_crud_over_the_socket` (the dunker count
//! and the `self_pane_id` echo). The socket test itself belongs to the daemon.

use std::cell::{Cell, RefCell};

use serde_json::{json, Map, Value};

use super::dunks::{AddArgs, DunkList};
use super::status::*;
use crate::error::DunkError;

struct Env {
    dunks: DunkList,
    now: Cell<f64>,
    probes: Cell<u32>,
    unread_scans: Cell<u32>,
    available: bool,
    inbox_fails: bool,
    message: RefCell<String>,
}

impl Env {
    fn new() -> Env {
        Env {
            dunks: DunkList::new(),
            now: Cell::new(1100.0),
            probes: Cell::new(0),
            unread_scans: Cell::new(0),
            available: true,
            inbox_fails: false,
            message: RefCell::new("idle".to_string()),
        }
    }
}

impl StatusEnv for Env {
    fn version(&self) -> String {
        "2.99.0".to_string()
    }
    fn federated_input(&self) -> Value {
        json!(false)
    }
    fn code_status(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("code_hash".into(), json!("abc"));
        m.insert("code_stale".into(), json!(false));
        m
    }
    fn dunks(&self) -> &DunkList {
        &self.dunks
    }
    fn started_at(&self) -> f64 {
        1000.0
    }
    fn pid(&self) -> u32 {
        42
    }
    fn now(&self) -> f64 {
        self.now.get()
    }
    fn queued_messages(&self) -> usize {
        3
    }
    fn inbox_unread(&self) -> Result<i64, DunkError> {
        self.unread_scans.set(self.unread_scans.get() + 1);
        Ok(7)
    }
    fn task_totals(&self) -> Result<Value, DunkError> {
        Ok(json!({"open": 2}))
    }
    fn task_followers(&self) -> usize {
        1
    }
    fn quarantined_mailboxes(&self) -> Option<Value> {
        None
    }
    fn last_message(&self) -> String {
        self.message.borrow().clone()
    }
    fn herdr_available(&self) -> bool {
        self.probes.set(self.probes.get() + 1);
        self.available
    }
    fn herdr_hint(&self) -> String {
        "start herdr".to_string()
    }
    fn state_file(&self) -> Option<String> {
        Some("/tmp/dunks.json".to_string())
    }
    fn inbox_count(&self, pane: &str) -> Result<Value, String> {
        if self.inbox_fails {
            Err("inbox down".to_string())
        } else {
            Ok(json!({"mailbox": pane, "unread": 1}))
        }
    }
}

fn keys(map: &Map<String, Value>) -> Vec<&str> {
    map.keys().map(String::as_str).collect()
}

#[test]
fn status_has_the_pythons_keys_in_the_pythons_order_with_code_status_spliced_after_federated_input()
{
    let env = Env::new();
    let status = status(&env, true).unwrap();
    assert_eq!(
        vec![
            "version",
            "federated_input",
            "code_hash",
            "code_stale",
            "started_at",
            "pid",
            "dunkers",
            "running",
            "herdr_available",
            "herdr_hint",
            "state_file",
            "uptime_s",
            "queued_messages",
            "inbox_unread",
            "tasks",
            "task_followers",
            "quarantined_mailboxes",
            "message"
        ],
        keys(&status)
    );
    assert_eq!(json!(100), status["uptime_s"]);
    assert_eq!(json!(7), status["inbox_unread"]);
    assert_eq!(json!({"open": 2}), status["tasks"]);
    assert_eq!(Value::Null, status["quarantined_mailboxes"], "`or None`");
}

#[test]
fn herdr_availability_is_null_not_down_unless_checked_and_the_hint_only_when_down() {
    let env = Env::new();
    let unchecked = status(&env, false).unwrap();
    assert_eq!(Value::Null, unchecked["herdr_available"]);
    assert_eq!(Value::Null, unchecked["herdr_hint"]);
    assert_eq!(0, env.probes.get(), "the hot path spawns nothing");
    let mut down = Env::new();
    down.available = false;
    let s = status(&down, true).unwrap();
    assert_eq!(
        (json!(false), json!("start herdr")),
        (s["herdr_available"].clone(), s["herdr_hint"].clone())
    );
    assert_eq!(
        Value::Null,
        status(&env, true).unwrap()["herdr_hint"],
        "up: no hint"
    );
}

#[test]
fn status_counts_dunkers_and_running_ones() {
    let env = Env::new();
    assert_eq!(json!(0), status(&env, false).unwrap()["dunkers"]);
    let pane = json!({"pane_id": "w1:p1", "label": "one"});
    let pane = pane.as_object().unwrap();
    env.dunks
        .add(
            Some(pane),
            &AddArgs {
                start: true,
                ..Default::default()
            },
        )
        .unwrap();
    env.dunks.add(Some(pane), &AddArgs::default()).unwrap();
    let s = status(&env, false).unwrap();
    assert_eq!(
        (json!(2), json!(1)),
        (s["dunkers"].clone(), s["running"].clone())
    );
}

#[test]
fn poll_status_is_counts_only_and_scans_no_store() {
    let env = Env::new();
    let poll = poll_status(&env);
    assert_eq!(
        vec![
            "version",
            "federated_input",
            "code_hash",
            "code_stale",
            "started_at",
            "pid",
            "dunkers",
            "running",
            "queued_messages",
            "message",
            "herdr_available",
            "herdr_hint",
            "uptime_s"
        ],
        keys(&poll)
    );
    assert_eq!(
        0,
        env.unread_scans.get(),
        "the sidebar poll must not scan the inbox"
    );
    assert_eq!(Value::Null, poll["herdr_available"]);
}

#[test]
fn the_view_is_cached_until_invalidated_fresh_or_the_ttl_or_another_pane() {
    let env = Env::new();
    let cache = ViewCache::new();
    env.message.replace("first".into());
    let one = cache.view(&env, Some("w1:p1"), false, VIEW_CACHE_S);
    env.message.replace("second".into());
    assert_eq!(
        one,
        cache.view(&env, Some("w1:p1"), false, VIEW_CACHE_S),
        "cached"
    );
    assert_eq!(
        json!("second"),
        cache.view(&env, Some("w1:p1"), true, VIEW_CACHE_S)["status"]["message"],
        "fresh"
    );
    env.message.replace("third".into());
    assert_eq!(
        json!("second"),
        cache.view(&env, Some("w1:p1"), false, VIEW_CACHE_S)["status"]["message"]
    );
    assert_eq!(
        json!("third"),
        cache.view(&env, Some("w1:p2"), false, VIEW_CACHE_S)["status"]["message"],
        "another pane"
    );
    env.message.replace("fourth".into());
    cache.invalidate();
    assert_eq!(
        json!("fourth"),
        cache.view(&env, Some("w1:p2"), false, VIEW_CACHE_S)["status"]["message"],
        "invalidated"
    );
    env.message.replace("fifth".into());
    env.now.set(env.now.get() + VIEW_CACHE_S + 1.0);
    assert_eq!(
        json!("fifth"),
        cache.view(&env, Some("w1:p2"), false, VIEW_CACHE_S)["status"]["message"],
        "ttl"
    );
}

#[test]
fn flock_owns_one_shared_view_cache_across_request_borrows() {
    let dir = tempfile::tempdir().unwrap();
    let flock = super::Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        false,
    );
    let other_flock = super::Flock::new(
        dir.path().join("other-launches.json"),
        dir.path().join("other-lanes.json"),
        false,
    );
    let cache = flock.view_cache();
    assert!(std::ptr::eq(cache, flock.view_cache()));
    assert!(!std::ptr::eq(cache, other_flock.view_cache()));

    let env = Env::new();
    env.message.replace("before".into());
    let before = flock
        .view_cache()
        .view(&env, Some("w1:p1"), false, VIEW_CACHE_S);
    env.message.replace("after".into());
    assert_eq!(
        before,
        flock
            .view_cache()
            .view(&env, Some("w1:p1"), false, VIEW_CACHE_S),
        "a borrowed handle sees the shared cached view"
    );

    flock.invalidate_view();
    assert_eq!(
        json!("after"),
        flock
            .view_cache()
            .view(&env, Some("w1:p1"), false, VIEW_CACHE_S)["status"]["message"]
    );
}

#[test]
fn a_view_carries_dunks_the_pane_and_an_inbox_count_or_its_error() {
    let mut env = Env::new();
    env.dunks.add(None, &AddArgs::default()).unwrap();
    let view = ViewCache::new().view(&env, Some("w1:p1"), true, VIEW_CACHE_S);
    assert_eq!(json!("w1:p1"), view["status"]["self_pane_id"]);
    assert_eq!(1, view["dunks"].as_array().unwrap().len());
    assert_eq!(
        json!({"mailbox": "w1:p1", "unread": 1}),
        view["inbox_count"]
    );
    env.inbox_fails = true;
    let view = ViewCache::new().view(&env, Some("w1:p1"), true, VIEW_CACHE_S);
    assert_eq!(
        json!("inbox down"),
        view["inbox_error"],
        "a view never breaks"
    );
    assert!(view.get("inbox_count").is_none());
    let anonymous = ViewCache::new().view(&env, None, true, VIEW_CACHE_S);
    assert!(anonymous.get("inbox_count").is_none() && anonymous.get("inbox_error").is_none());
    assert_eq!(Value::Null, anonymous["status"]["self_pane_id"]);
}

#[test]
fn h_status_takes_the_cached_view_by_default_the_full_path_with_full_and_probes_on_request() {
    let env = Env::new();
    let cache = ViewCache::new();
    let fast = h_status(&env, &cache, false, false, false, Some("w1:p2")).unwrap();
    assert_eq!(json!("w1:p2"), fast["self_pane_id"]);
    assert!(
        fast.get("inbox_unread").is_none(),
        "the fast path is poll_status"
    );
    assert_eq!(Value::Null, fast["herdr_available"]);
    let probed = h_status(&env, &cache, false, false, true, Some("w1:p2")).unwrap();
    assert_eq!(json!(true), probed["herdr_available"]);
    assert_eq!(1, env.probes.get());
    let full = h_status(&env, &cache, true, false, false, Some("w1:p2")).unwrap();
    assert_eq!(json!(7), full["inbox_unread"]);
    assert_eq!(json!("w1:p2"), full["self_pane_id"]);
    assert_eq!(
        json!(0),
        h_status(&env, &cache, false, false, false, None).unwrap()["dunkers"]
    );
}

// ---- CodeIdentity (leader's decision on t1397) -------------------------------------------

mod code_identity {
    use super::super::status::CodeIdentity;
    use serde_json::json;

    fn fake_exe(dir: &tempfile::TempDir, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.path().join("dunkingsheep-daemon");
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn the_hash_is_twelve_hex_of_sha256_over_the_bytes_and_an_untouched_binary_is_not_stale() {
        let dir = tempfile::tempdir().unwrap();
        let identity = CodeIdentity::at_start(fake_exe(&dir, b"abc"));
        let status = identity.code_status();
        assert_eq!(json!("ba7816bf8f01"), status["code_hash"]);
        assert_eq!(json!(false), status["code_stale"]);
        assert_eq!(json!([]), status["code_changed_modules"]);
        assert_eq!(json!([]), status["code_identity_errors"]);
        assert!(status["code_mtime"].as_f64().unwrap() > 0.0);
        assert_eq!(
            vec![
                "code_hash",
                "code_mtime",
                "code_stale",
                "code_changed_modules",
                "code_identity_errors"
            ],
            status.keys().map(String::as_str).collect::<Vec<_>>(),
            "the Python's key order"
        );
    }

    #[test]
    fn replacing_the_file_under_a_running_daemon_is_stale_and_names_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = fake_exe(&dir, b"old build");
        let identity = CodeIdentity::at_start(&path);
        // an installer replaces the file (new inode), exactly what a deploy does
        let replacement = dir.path().join("new");
        std::fs::write(&replacement, b"new build, different bytes").unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        let status = identity.code_status();
        assert_eq!(json!(true), status["code_stale"]);
        assert_eq!(
            json!(["dunkingsheep-daemon"]),
            status["code_changed_modules"]
        );
        assert_eq!(json!([]), status["code_identity_errors"]);
        assert_eq!(
            identity.code_status()["code_hash"],
            status["code_hash"],
            "the start hash is frozen, never re-read"
        );
    }

    #[test]
    fn a_size_change_in_place_is_stale_too() {
        let dir = tempfile::tempdir().unwrap();
        let path = fake_exe(&dir, b"short");
        let identity = CodeIdentity::at_start(&path);
        std::fs::write(&path, b"a much longer rewrite in place").unwrap();
        assert_eq!(json!(true), identity.code_status()["code_stale"]);
    }

    #[test]
    fn an_unreadable_binary_has_no_hash_an_unknown_staleness_and_names_the_error() {
        let dir = tempfile::tempdir().unwrap();
        let identity = CodeIdentity::at_start(dir.path().join("missing"));
        let status = identity.code_status();
        assert_eq!(serde_json::Value::Null, status["code_hash"]);
        assert_eq!(
            serde_json::Value::Null,
            status["code_stale"],
            "unknown, not false"
        );
        assert_eq!(json!(["missing"]), status["code_identity_errors"]);
        assert_eq!(json!([]), status["code_changed_modules"]);
    }

    #[test]
    fn a_binary_deleted_after_start_reports_the_error_not_a_change() {
        let dir = tempfile::tempdir().unwrap();
        let path = fake_exe(&dir, b"abc");
        let identity = CodeIdentity::at_start(&path);
        std::fs::remove_file(&path).unwrap();
        let status = identity.code_status();
        assert_eq!(
            json!(["dunkingsheep-daemon"]),
            status["code_identity_errors"]
        );
        assert_eq!(serde_json::Value::Null, status["code_stale"]);
        assert_eq!(json!([]), status["code_changed_modules"]);
    }
}
