//! Real inbox rows, the existing queue, and a terminal spy. No live panes or credentials.
use super::*;
use crate::flock::{commands_turns::TurnState, panes::Stores};
use crate::inbox_store::Outgoing;
use serde_json::json;
use std::sync::{atomic::AtomicBool, Arc};

struct Fixture {
    _dir: tempfile::TempDir,
    flock: Flock,
    inbox: Arc<Store>,
    ids: Vec<String>,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let flock = Flock::new(dir.path().join("launches"), dir.path(), false);
        let inbox =
            Arc::new(Store::open(dir.path().join("inbox.db"), dir.path().join("blobs")).unwrap());
        flock.attach_stores(Stores {
            inbox: Some(inbox.clone()),
            ..Stores::default()
        });
        let ids: Vec<_> = ["first", "second"]
            .into_iter()
            .map(|body| {
                inbox
                    .send(&Outgoing {
                        mailbox: P,
                        body: Some(body),
                        ..Default::default()
                    })
                    .unwrap()
            })
            .collect();
        let mut notice = Message::new(
            "m38500-private",
            P,
            "private lane",
            "[inbox] private notice",
            "inbox",
        );
        notice.is_notice = true;
        notice.notify_for = ids.clone();
        notice.created_at = 100.0;
        flock.outbox.lock().unwrap().insert(P.into(), vec![notice]);
        let f = Self {
            _dir: dir,
            flock,
            inbox,
            ids,
        };
        f.turn("turn1", None);
        f
    }
    fn turn(&self, id: &str, end: Option<f64>) {
        self.flock.turn_states.lock().unwrap().insert(
            P.into(),
            TurnState {
                started_at: 100.0,
                expires_at: if end.is_some() { 0.0 } else { 1000.0 },
                ended_at: end,
                ttl_s: 900.0,
                turn_id: id.into(),
                source: Some("codex-hook-mail-v1".into()),
                caller: None,
                handoff_id: None,
            },
        );
    }
    fn offer(&self, turn: &str, edge: &str, now: f64) -> Option<serde_json::Value> {
        self.flock.offer_mail_notice(P, P, turn, edge, now).unwrap()
    }
}

#[test]
fn hook_offer_has_no_terminal_io_no_consumption_and_survives_snapshot() {
    let f = Fixture::new();
    let mut terminal = Pane::default();
    let step = outbox_step(
        &f.flock,
        &mut terminal,
        P,
        105.0,
        &AtomicBool::new(false),
        &|_| false,
        &mut || None,
    );
    assert_eq!(step, OutboxStep::HookWaiting);
    assert!(
        terminal.reads.borrow().is_empty() && terminal.typed.is_empty() && terminal.keys.is_empty()
    );
    let offer = f.offer("turn1", "start", 105.0).unwrap();
    assert_eq!(offer["unread"], 2);
    assert_eq!(offer["action"], "offer");
    assert!(f.offer("turn1", "start", 106.0).is_none());
    for id in &f.ids {
        assert_eq!(f.inbox.get_message(id).unwrap().summary.state, "queued");
    }
    assert!(crate::inbox::pending_delivery(f.inbox.read_pool(), P)
        .unwrap()
        .is_none());
    let row = f.flock.outbox_snapshot()[P][0].clone();
    let restored = message_from_snapshot(&message_snapshot(&row)).unwrap();
    assert_eq!(restored.notice_offer, row.notice_offer);
    f.flock
        .outbox
        .lock()
        .unwrap()
        .insert(P.into(), vec![restored]);
    assert!(f.offer("turn1", "start", 106.0).is_none());
    assert_eq!(f.offer("turn1", "end", 110.0).unwrap()["offers"], 2);
    assert_eq!(
        f.offer("turn2", "start", 111.0).unwrap()["action"],
        "fallback"
    );
    assert_eq!(
        f.flock.outbox_snapshot()[P].len(),
        1,
        "one logical notice per batch"
    );
}

#[test]
fn idle_fallback_reuses_the_guarded_queue_once_and_real_read_retires_offer() {
    let f = Fixture::new();
    f.offer("turn1", "start", 100.0);
    f.offer("turn1", "end", 110.0);
    f.turn("turn1", Some(110.0));
    let mut terminal = Pane {
        receipt_ok: true,
        ..Pane::default()
    };
    terminal.statuses.insert(P.into(), "idle".into());
    terminal.panes = vec![json!({"pane_id":P,"pane_pid":std::process::id()})];
    terminal.screens.insert(P.into(), hint_screen());
    // A no-Enter fixture keeps submit verification out of this timing/queue test.
    f.flock.outbox.lock().unwrap().get_mut(P).unwrap()[0].press_enter = false;
    assert_eq!(
        outbox_step(
            &f.flock,
            &mut terminal,
            P,
            229.0,
            &AtomicBool::new(false),
            &|_| false,
            &mut || None
        ),
        OutboxStep::HookWaiting
    );
    assert_eq!(
        outbox_step(
            &f.flock,
            &mut terminal,
            P,
            230.0,
            &AtomicBool::new(false),
            &|_| false,
            &mut || None
        ),
        OutboxStep::Delivered
    );
    assert_eq!(terminal.typed.len(), 1);
    assert_eq!(
        outbox_step(
            &f.flock,
            &mut terminal,
            P,
            231.0,
            &AtomicBool::new(false),
            &|_| false,
            &mut || None
        ),
        OutboxStep::Empty
    );
    assert_eq!(terminal.typed.len(), 1);
    let f = Fixture::new();
    f.offer("turn1", "start", 100.0);
    f.inbox
        .read_with_receipt(P, &f.ids, true, Some(105.0))
        .unwrap();
    assert!(f.offer("turn1", "end", 110.0).is_none());
    assert!(
        f.flock.outbox_snapshot()[P].is_empty(),
        "consumed mail must not keep next-job blocked by its obsolete wake"
    );
}

#[test]
fn foreign_refs_and_legacy_adapters_do_not_gain_hook_delivery() {
    let f = Fixture::new();
    assert!(f
        .flock
        .offer_mail_notice(P, "other", "turn1", "start", 100.0)
        .unwrap()
        .is_none());
    f.flock
        .turn_states
        .lock()
        .unwrap()
        .get_mut(P)
        .unwrap()
        .source = Some("codex-hook".into());
    assert!(!f
        .flock
        .notice_waits_for_hook(&f.flock.outbox_snapshot()[P][0], 105.0));
}

#[test]
fn final_guard_also_blocks_a_notice_selected_before_the_new_turn() {
    let f = Fixture::new();
    let mut terminal = Pane::default();
    assert_eq!(
        f.flock
            .deliver_message("m38500-private", 105.0, &mut terminal, || None),
        DeliveryOutcome::Held
    );
    assert!(
        terminal.typed.is_empty() && terminal.keys.is_empty() && terminal.reads.borrow().is_empty()
    );
}

#[test]
fn fallback_idle_bound_is_measured_from_end_not_the_previous_offer() {
    let f = Fixture::new();
    f.offer("turn1", "start", 100.0);
    f.offer("turn1", "end", 110.0);
    f.turn("turn2", None);
    assert_eq!(
        f.offer("turn2", "end", 400.0).unwrap()["action"],
        "fallback"
    );
    f.turn("turn2", Some(400.0));
    assert!(f
        .flock
        .turn_expire(Some(519.0), &crate::flock::commands_turns::NoHooks)
        .is_empty());
    let row = f.flock.outbox_snapshot()[P][0].clone();
    assert!(f.flock.notice_waits_for_hook(&row, 519.0));
    assert_eq!(
        f.flock
            .turn_expire(Some(520.0), &crate::flock::commands_turns::NoHooks)
            .len(),
        1
    );
    assert!(!f.flock.notice_waits_for_hook(&row, 520.0));
    // Losing volatile turn state on restart still retains the last end edge on the notice.
    let restored = message_from_snapshot(&message_snapshot(&row)).unwrap();
    assert!(f.flock.notice_waits_for_hook(&restored, 519.0));
}

#[test]
fn proactive_read_before_the_first_offer_also_retires_the_stale_wake() {
    let f = Fixture::new();
    f.inbox
        .read_with_receipt(P, &f.ids, true, Some(105.0))
        .unwrap();
    assert!(f.offer("turn1", "end", 110.0).is_none());
    assert!(f.flock.outbox_snapshot()[P].is_empty());
    let recovered = f.inbox.recover_delivery(P).unwrap();
    assert_eq!(
        recovered.len(),
        2,
        "retiring a wake must preserve lost-response recovery bodies"
    );
}
