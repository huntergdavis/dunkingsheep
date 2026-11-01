//! Behaviour tests for the T34 slice-1 commands, from the docstrings and the incidents named
//! in `dunk_core._mailbox_or_self`/`_own_mailbox` and `test_dunk_roles::
//! test_another_lanes_mailbox_is_not_readable`, `test_dunk_inbox::
//! test_reading_and_acking_a_dead_name_still_works` (the count/read half). NOT a 1:1 port of
//! the Python harness classes: those drive a whole Flock and belong to the later slices.

use super::commands_inbox::{notice_text, InboxEnv, Nudge, RouteEntry, SendArgs};
use super::mailbox::MailboxEnv;
use super::Flock;
use crate::db::ReadPool;
use crate::inbox_store::{Outgoing, Store};
use serde_json::{json, Value};

struct Env {
    routing: Vec<RouteEntry>,
    waiting_notice: Option<String>,
    nudges: std::sync::Mutex<Vec<String>>,
    fail_nudge: bool,
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
    fn pm_routing(&self) -> Vec<RouteEntry> {
        self.routing.clone()
    }
    fn fold_into_queued_notice(&self, _: &str, _: &str, _: &str) -> Option<String> {
        self.waiting_notice.clone()
    }
    fn send_now(&self, _: &str, text: &str, _: &str) -> Result<Nudge, crate::error::DunkError> {
        if self.fail_nudge {
            return Err(crate::error::DunkError::Refused("pane gone".to_string()));
        }
        self.nudges.lock().unwrap().push(text.to_string());
        Ok(Nudge {
            queued: false,
            message: Some("typed".to_string()),
        })
    }
    fn record_sent(&self, _: Option<&str>, _: &str, _: Option<&str>, _: usize, _: usize) {}
    fn tasks_for_pane(&self, pane: &str) -> Value {
        json!([{"pane": pane}])
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
        routing: vec![],
        waiting_notice: None,
        nudges: Default::default(),
        fail_nudge: false,
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

fn send<'a>(to: &'a str, from: Option<&'a str>) -> SendArgs<'a> {
    SendArgs {
        to,
        body: "hello",
        notify: true,
        self_pane_id: from,
        ..Default::default()
    }
}

#[test]
fn the_notice_is_one_short_line_naming_inbox_sender_and_subject() {
    assert_eq!(
        "[inbox] new message for Mine. Read it with: dunkingsheep inbox read",
        notice_text("Mine", 1, None, None)
    );
    assert_eq!(
        "[inbox] new message for Mine (3 unread) from %1: PR 12. Read it with: dunkingsheep inbox read",
        notice_text("Mine", 3, Some("%1"), Some("  PR 12 "))
    );
    let long = "x".repeat(100);
    assert!(notice_text("m", 1, None, Some(&long)).contains(&format!(": {}. Read", "x".repeat(60))));
}

#[test]
fn a_send_stores_the_mail_from_the_raw_pane_id_and_nudges_once() {
    let (e, d) = (env("worker"), tempfile::tempdir().unwrap());
    let f = flock(&d);
    let sent = f.inbox_send(&e, &send("w1:p2", Some("w1:p1"))).unwrap();
    assert_eq!(
        Some("w1:p1".to_string()),
        sent.sender,
        "the sender is the pane id, never a label"
    );
    assert_eq!("Other", sent.mailbox);
    assert!(sent.notified);
    assert_eq!(1, e.nudges.lock().unwrap().len());
    assert_eq!(
        1,
        f.inbox_count(&e, Some("Other"), Some("w1:p1"))
            .unwrap()
            .unread
    );
}

#[test]
fn a_waiting_notice_is_folded_not_stacked() {
    let (mut e, d) = (env("worker"), tempfile::tempdir().unwrap());
    e.waiting_notice = Some("n7".to_string());
    let f = flock(&d);
    let sent = f.inbox_send(&e, &send("w1:p2", Some("w1:p1"))).unwrap();
    assert!(sent.notified);
    assert!(
        e.nudges.lock().unwrap().is_empty(),
        "no second notice typed"
    );
    let status = f.inbox_status(&e, &sent.message_id).unwrap();
    assert_eq!(Some("queued".to_string()), status.notify_state);
    assert_eq!(Some("folded into n7".to_string()), status.notify_reason);
}

#[test]
fn a_failed_nudge_is_recorded_and_the_mail_is_still_stored() {
    let (mut e, d) = (env("worker"), tempfile::tempdir().unwrap());
    e.fail_nudge = true;
    let f = flock(&d);
    let sent = f.inbox_send(&e, &send("w1:p2", Some("w1:p1"))).unwrap();
    assert!(!sent.notified);
    let status = f.inbox_status(&e, &sent.message_id).unwrap();
    assert_eq!(Some("failed".to_string()), status.notify_state);
    assert_eq!(Some("pane gone".to_string()), status.notify_reason);
}

#[test]
fn notify_false_types_nothing() {
    let (e, d) = (env("worker"), tempfile::tempdir().unwrap());
    let f = flock(&d);
    let mut args = send("w1:p2", Some("w1:p1"));
    args.notify = false;
    assert!(!f.inbox_send(&e, &args).unwrap().notified);
    assert!(e.nudges.lock().unwrap().is_empty());
}

#[test]
fn an_agent_gets_the_hard_refusal_for_a_dead_pane() {
    let (e, d) = (env("worker"), tempfile::tempdir().unwrap());
    let f = flock(&d);
    let err = f
        .inbox_send(&e, &send("w9:pH", Some("w1:p1")))
        .unwrap_err()
        .to_string();
    assert!(err.contains("dead pane id is not a mailbox"), "{err}");
}

#[test]
fn a_system_sender_is_stored_undeliverable_instead_of_dying_on_a_refusal() {
    let (e, d) = (env("worker"), tempfile::tempdir().unwrap());
    let f = flock(&d);
    let mut args = send("w9:pH", None);
    args.system_sender = Some("watchdog");
    let sent = f.inbox_send(&e, &args).unwrap();
    assert_eq!(Some("undeliverable".to_string()), sent.outcome);
    assert!(sent
        .reason
        .unwrap()
        .contains("dead pane id is not a mailbox"));
    assert_eq!("w9:pH", sent.mailbox);
    assert_eq!(Some("watchdog".to_string()), sent.sender);
    assert!(!sent.notified);
}

#[test]
fn only_known_system_senders_and_never_from_a_pane() {
    let (e, d) = (env("worker"), tempfile::tempdir().unwrap());
    let f = flock(&d);
    let mut args = send("w1:p2", None);
    args.system_sender = Some("mallory");
    assert_eq!(
        "unknown system sender 'mallory'",
        f.inbox_send(&e, &args).unwrap_err().to_string()
    );
    args.system_sender = Some("watchdog");
    args.self_pane_id = Some("w1:p1");
    assert_eq!(
        "a system sender cannot be claimed from a pane",
        f.inbox_send(&e, &args).unwrap_err().to_string()
    );
}

#[test]
fn over_the_socket_a_system_sender_needs_the_system_token() {
    let (e, d) = (env("worker"), tempfile::tempdir().unwrap());
    let f = flock(&d);
    f.set_socket_peer(Some(true));
    let mut args = send("w1:p2", None);
    args.system_sender = Some("watchdog");
    let err = f.inbox_send(&e, &args).unwrap_err().to_string();
    assert!(err.contains("requires the system token"), "{err}");
}

fn vp_routing() -> Vec<RouteEntry> {
    vec![RouteEntry {
        project: "poolrad".to_string(),
        pm: "w1:p2".to_string(),
        members: vec!["w1:p1".to_string(), "w1:p2".to_string()],
        mode: "redirect".to_string(),
        vp_pane: "w1:p3".to_string(),
    }]
}

fn env_with_vp(mode: &str) -> Env {
    let mut e = env("worker");
    e.panes.push(pane("w1:p3", "VP"));
    e.routing = vp_routing();
    e.routing[0].mode = mode.to_string();
    e
}

#[test]
fn a_lane_mailing_the_vp_is_redirected_to_its_pm() {
    let (e, d) = (env_with_vp("redirect"), tempfile::tempdir().unwrap());
    let f = flock(&d);
    let sent = f.inbox_send(&e, &send("w1:p3", Some("w1:p1"))).unwrap();
    assert_eq!("Other", sent.mailbox, "the PM's mailbox, not the VP's");
    assert_eq!(
        Some(json!({"from": "vp", "to": "w1:p2", "project": "poolrad"})),
        sent.redirected
    );
    let (_, messages) = f
        .inbox_list(&e, Some("Other"), true, 10, Some("w1:p2"))
        .unwrap();
    assert_eq!(1, messages.len());
}

#[test]
fn flag_mode_keeps_the_vp_as_recipient_and_marks_the_subject() {
    let (e, d) = (env_with_vp("flag"), tempfile::tempdir().unwrap());
    let f = flock(&d);
    let mut args = send("w1:p3", Some("w1:p1"));
    args.subject = Some("PR 12");
    let sent = f.inbox_send(&e, &args).unwrap();
    assert_eq!("VP", sent.mailbox);
    assert_eq!(Some(json!({"flag": "poolrad"})), sent.redirected);
    let (_, rows) = f
        .inbox_list(&e, Some("VP"), true, 10, Some("w1:p3"))
        .unwrap();
    assert_eq!(
        Some("[lane mail to VP: poolrad] PR 12".to_string()),
        rows[0].subject
    );
}

#[test]
fn the_pm_is_never_redirected_to_itself() {
    let (e, d) = (env_with_vp("redirect"), tempfile::tempdir().unwrap());
    let f = flock(&d);
    let sent = f.inbox_send(&e, &send("w1:p3", Some("w1:p2"))).unwrap();
    assert_eq!("VP", sent.mailbox);
    assert_eq!(None, sent.redirected);
}

#[test]
fn reading_marks_the_batch_read_and_carries_the_task_queue() {
    let (e, d) = (env("worker"), tempfile::tempdir().unwrap());
    let f = flock(&d);
    put(&e, "Mine", "one");
    put(&e, "Mine", "two");
    let read = f.inbox_read(&e, None, 10, false, Some("w1:p1")).unwrap();
    assert_eq!(2, read.messages.len());
    assert_eq!(Some(2), read.marked_read);
    assert_eq!(Some(json!([{"pane": "w1:p1"}])), read.tasks);
    assert!(read
        .messages
        .iter()
        .all(|m| m.summary.state == "read" && m.read_at.is_some()));
    assert_eq!(0, f.inbox_count(&e, None, Some("w1:p1")).unwrap().unread);
    let again = f.inbox_read(&e, None, 10, false, Some("w1:p1")).unwrap();
    assert!(again.messages.is_empty());
    assert_eq!(None, again.marked_read);
}

#[test]
fn peek_does_not_consume_and_a_read_can_be_recovered() {
    let (e, d) = (env("worker"), tempfile::tempdir().unwrap());
    let f = flock(&d);
    put(&e, "Mine", "one");
    assert_eq!(
        1,
        f.inbox_read(&e, None, 10, true, Some("w1:p1"))
            .unwrap()
            .messages
            .len()
    );
    assert_eq!(1, f.inbox_count(&e, None, Some("w1:p1")).unwrap().unread);
    f.inbox_read(&e, None, 10, false, Some("w1:p1")).unwrap();
    let (_, replay) = f.inbox_recover(&e, None, Some("w1:p1")).unwrap();
    assert_eq!(
        1,
        replay.len(),
        "t128: the delivered batch replays after a lost response"
    );
}

#[test]
fn reading_another_lanes_mail_is_refused_before_anything_is_marked() {
    let (e, d) = (env("worker"), tempfile::tempdir().unwrap());
    let f = flock(&d);
    put(&e, "Other", "private");
    assert!(f
        .inbox_read(&e, Some("w1:p2"), 10, false, Some("w1:p1"))
        .is_err());
    assert_eq!(
        1,
        f.inbox_count(&e, Some("Other"), Some("w1:p1"))
            .unwrap()
            .unread
    );
}
