//! Behaviour tests for the T34 slice-1 commands, from the docstrings and the incidents named
//! in `dunk_core._mailbox_or_self`/`_own_mailbox` and `test_dunk_roles::
//! test_another_lanes_mailbox_is_not_readable`, `test_dunk_inbox::
//! test_reading_and_acking_a_dead_name_still_works` (the count/read half). NOT a 1:1 port of
//! the Python harness classes: those drive a whole Flock and belong to the later slices.

use super::commands_inbox::InboxEnv;
use super::mailbox::MailboxEnv;
use super::Flock;
use crate::db::ReadPool;
use crate::inbox_store::{Outgoing, Store};
use serde_json::{json, Value};

struct Env {
    metrics: std::sync::Mutex<Vec<(String, Option<String>, Option<i64>)>>,
    panes: Vec<Value>,
    role: &'static str,
    override_name: Option<String>,
    pool: ReadPool,
    store: Store,
    _dir: tempfile::TempDir,
}

impl MailboxEnv for Env {
    fn list_panes(&self) -> Vec<Value> {
        self.panes.clone()
    }
    fn list_panes_grouped(&self) -> Vec<Value> {
        self.panes.clone()
    }
    fn registered_pane(&self, _: &str) -> (bool, bool, Option<String>) {
        (false, false, None)
    }
}

impl InboxEnv for Env {
    fn caller_role(&self, _: &str) -> String {
        self.role.to_string()
    }
    fn mailbox_override(&self) -> Option<String> {
        self.override_name.clone()
    }
    fn backend_name(&self) -> String {
        "fake".to_string()
    }
    fn lane_panes(&self) -> Vec<String> {
        vec![]
    }
    fn pool(&self) -> &ReadPool {
        &self.pool
    }
    fn store(&self) -> &Store {
        &self.store
    }
    fn now(&self) -> f64 {
        crate::clock::now_s()
    }
    fn record_metric(
        &self,
        kind: &str,
        pane: Option<&str>,
        _: &str,
        _: Option<&str>,
        ms: Option<i64>,
    ) {
        self.metrics
            .lock()
            .unwrap()
            .push((kind.to_string(), pane.map(str::to_string), ms));
    }
}

fn pane(id: &str, tab: &str) -> Value {
    json!({"pane_id": id, "terminal_id": format!("term_{}", id.replace([':', '%'], "")),
        "label": tab, "tab_label": tab, "workspace_label": "ws", "agent": "claude"})
}

fn env(role: &'static str) -> Env {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::open(dir.path().join("inbox.db"), dir.path().join("blobs")).expect("open");
    let pool = ReadPool::open(store.path(), 1).expect("pool");
    Env {
        metrics: Default::default(),
        panes: vec![pane("w1:p1", "Mine"), pane("w1:p2", "Other")],
        role,
        override_name: None,
        pool,
        store,
        _dir: dir,
    }
}

fn flock(dir: &tempfile::TempDir) -> Flock {
    Flock::new(
        dir.path().join("l.json"),
        dir.path().join("lanes.json"),
        false,
    )
}

fn put(e: &Env, mailbox: &str, body: &str) -> String {
    e.store
        .send(&Outgoing {
            mailbox,
            body: Some(body),
            sender: Some("pm"),
            ..Default::default()
        })
        .expect("send")
}

#[test]
fn another_lanes_mailbox_is_not_readable() {
    let (e, d) = (env("worker"), tempfile::tempdir().unwrap());
    let f = flock(&d);
    let err = f
        .inbox_list(&e, Some("w1:p2"), true, 10, Some("w1:p1"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("not yours"), "{err}");
    assert!(err.contains("needs the owner role"), "{err}");
}

#[test]
fn counting_another_lanes_mailbox_needs_no_owner_role() {
    let (e, d) = (env("worker"), tempfile::tempdir().unwrap());
    let f = flock(&d);
    put(&e, "Other", "hello");
    assert_eq!(
        1,
        f.inbox_count(&e, Some("w1:p2"), Some("w1:p1"))
            .unwrap()
            .unread
    );
}

#[test]
fn the_owner_role_may_read_any_mailbox() {
    let (e, d) = (env("owner"), tempfile::tempdir().unwrap());
    let f = flock(&d);
    put(&e, "Other", "hello");
    let (key, rows) = f
        .inbox_list(&e, Some("w1:p2"), true, 10, Some("w1:p1"))
        .unwrap();
    assert_eq!("Other", key);
    assert_eq!(1, rows.len());
}

#[test]
fn no_mailbox_and_no_caller_pane_is_a_refusal() {
    let (e, d) = (env("worker"), tempfile::tempdir().unwrap());
    let f = flock(&d);
    let err = f.inbox_count(&e, None, None).unwrap_err().to_string();
    assert_eq!("no mailbox given and no caller pane to default to", err);
}

#[test]
fn a_lane_defaults_to_its_own_mailbox() {
    let (e, d) = (env("worker"), tempfile::tempdir().unwrap());
    let f = flock(&d);
    put(&e, "Mine", "for me");
    assert_eq!(1, f.inbox_count(&e, None, Some("w1:p1")).unwrap().unread);
}

#[test]
fn a_dead_pane_falls_back_to_the_named_mailbox_never_a_guess() {
    let (mut e, d) = (env("worker"), tempfile::tempdir().unwrap());
    let f = flock(&d);
    let err = f.own_mailbox(&e, "w9:p9").unwrap_err().to_string();
    assert!(err.contains("cannot resolve this lane's mailbox"), "{err}");
    assert!(err.contains("(fake)"), "{err}");
    e.override_name = Some("lane-x".to_string());
    assert_eq!("lane-x", f.own_mailbox(&e, "w9:p9").unwrap());
    e.override_name = None;
    put(&e, "w9:p9", "stranded");
    assert_eq!("w9:p9", f.own_mailbox(&e, "w9:p9").unwrap());
}

#[test]
fn recover_with_no_receipt_is_empty_not_an_error() {
    let (e, d) = (env("worker"), tempfile::tempdir().unwrap());
    let f = flock(&d);
    let (key, rows) = f.inbox_recover(&e, None, Some("w1:p1")).unwrap();
    assert_eq!("Mine", key);
    assert!(rows.is_empty());
}

fn ids(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn an_ack_applies_only_to_the_callers_own_mailbox() {
    let (e, d) = (env("worker"), tempfile::tempdir().unwrap());
    let f = flock(&d);
    let other = put(&e, "Other", "not yours");
    let err = f
        .inbox_ack(&e, &ids(&[&other]), None, Some("w1:p1"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("is not in mailbox 'Mine'"), "{err}");
    assert_eq!(
        1,
        f.inbox_count(&e, Some("Other"), Some("w1:p1"))
            .unwrap()
            .unread
    );
}

#[test]
fn acking_own_mail_reports_the_mailbox_pending_ids_and_a_metric() {
    let (e, d) = (env("worker"), tempfile::tempdir().unwrap());
    let f = flock(&d);
    let id = put(&e, "Mine", "mine");
    let result = f.inbox_ack(&e, &ids(&[&id]), None, Some("w1:p1")).unwrap();
    assert_eq!(vec![id], result.transition.changed);
    assert_eq!(Some("Mine".to_string()), result.acked_in);
    assert_eq!(Some(vec![]), result.still_pending);
    let metrics = e.metrics.lock().unwrap();
    assert_eq!(1, metrics.len());
    assert_eq!("inbox.acked", metrics[0].0);
}

#[test]
fn an_unscoped_in_process_ack_keeps_the_old_behaviour() {
    let (e, d) = (env("worker"), tempfile::tempdir().unwrap());
    let f = flock(&d);
    let id = put(&e, "Other", "x");
    let result = f.inbox_ack(&e, &ids(&[&id]), None, None).unwrap();
    assert_eq!(None, result.acked_in);
    assert_eq!(None, result.still_pending);
    assert_eq!(1, result.transition.changed.len());
}

#[test]
fn archive_cancel_delete_pass_through_the_store() {
    let (e, d) = (env("worker"), tempfile::tempdir().unwrap());
    let f = flock(&d);
    let a = put(&e, "Mine", "a");
    let b = put(&e, "Mine", "b");
    assert_eq!(1, f.inbox_cancel(&e, &ids(&[&a])).unwrap().changed.len());
    assert_eq!(1, f.inbox_archive(&e, "Mine", None, None).unwrap().archived);
    assert_eq!(vec![b.clone()], f.inbox_delete(&e, &ids(&[&b])).unwrap());
    assert!(f.inbox_status(&e, &b).is_err());
}

#[test]
fn fetch_of_an_unknown_attachment_is_a_plain_refusal() {
    let (e, d) = (env("worker"), tempfile::tempdir().unwrap());
    let f = flock(&d);
    let err = f
        .inbox_fetch(&e, "nope", Some(d.path()))
        .unwrap_err()
        .to_string();
    assert_eq!("no attachment with id 'nope'", err);
}
