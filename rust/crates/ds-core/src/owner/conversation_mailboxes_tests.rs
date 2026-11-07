//! t3535: a lane's send to the owner lane's mailbox name (`codex-rust-owner`) must reach the
//! owner conversation (`/api/owner-thread`) as an agent message, with its sender and time, on
//! the next read. The owner pane is the only pane whose mailboxes the conversation adds.

use std::sync::Arc;

use serde_json::Value;

use super::*;
use crate::config::Env;
use crate::flock::panes::{PaneInfoEnv, Stores};
use crate::flock::Flock;
use crate::inbox_store::{Outgoing, Store};
use crate::roles::Roles;

const OWNER: &str = "w1:p2";
const WORKER: &str = "w1:p3";
const OWNER_LANE_MAILBOX: &str = "codex-rust-owner";

struct NoPanes;

impl PaneInfoEnv for NoPanes {
    fn list_panes_grouped(&self) -> Vec<Value> {
        Vec::new()
    }

    fn get_pane(&self, _: &str) -> Option<Value> {
        None
    }
}

/// A fake owner mailbox: an owner-role lane, a worker, and a store attached to the flock.
fn fixture(tag: &str) -> (tempfile::TempDir, Flock, Arc<Store>) {
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(
        dir.path().join(format!("{tag}-launches.json")),
        dir.path().join(format!("{tag}-lanes.json")),
        false,
    );
    let mut roles = Roles::new(
        &Env::default(),
        Some(dir.path().join(format!("{tag}-roles.json"))),
        Some(dir.path().join(format!("{tag}-tokens.json"))),
        None,
    )
    .unwrap();
    roles
        .register(OWNER, "owner", None, None, None, None, None, None)
        .unwrap();
    roles
        .register(WORKER, "worker", None, None, None, None, None, None)
        .unwrap();
    flock.attach_roles(roles, None);
    let store = Arc::new(
        Store::open(
            dir.path().join(format!("{tag}-inbox.db")),
            dir.path().join(format!("{tag}-blobs")),
        )
        .unwrap(),
    );
    flock.attach_stores(Stores {
        inbox: Some(store.clone()),
        ..Stores::default()
    });
    (dir, flock, store)
}

#[test]
fn a_lane_send_to_the_owner_lane_mailbox_shows_in_the_owner_conversation() {
    let (_dir, flock, store) = fixture("owner-lane-mailbox");
    // The coordinator's send to the Codex owner's lane name: recorded under that name, with the
    // owner pane's id, exactly as `resolve_mailbox` files it.
    let lane_id = store
        .send(&Outgoing {
            mailbox: OWNER_LANE_MAILBOX,
            body: Some("coordinator says hi"),
            mailbox_label: Some(OWNER_LANE_MAILBOX),
            pane_id: Some(OWNER),
            sender: Some(WORKER),
            ..Outgoing::default()
        })
        .unwrap();
    // An ordinary owner question: unread, unanswered, so it IS waiting for the desk.
    let question_id = store
        .send(&Outgoing {
            mailbox: OWNER_MAILBOX,
            body: Some("owner question"),
            mailbox_label: Some(OWNER_MAILBOX),
            ..Outgoing::default()
        })
        .unwrap();
    // A worker's own mailbox is not the owner's, so it must not appear in the conversation.
    store
        .send(&Outgoing {
            mailbox: "rust-worker-box",
            body: Some("private to a worker"),
            mailbox_label: Some("rust-worker-box"),
            pane_id: Some(WORKER),
            sender: Some(OWNER),
            ..Outgoing::default()
        })
        .unwrap();

    let conversation = OwnerConversations::new(&flock, &NoPanes)
        .conversation(Some(50), None, None)
        .unwrap();
    let lane = conversation
        .messages
        .iter()
        .find(|row| row.get("body").and_then(Value::as_str) == Some("coordinator says hi"))
        .expect("the lane message reaches the owner conversation");
    assert_eq!(Some(WORKER), lane.get("sender").and_then(Value::as_str));
    assert!(lane.get("created_at").and_then(Value::as_f64).is_some());
    assert!(conversation
        .messages
        .iter()
        .all(|row| row.get("body").and_then(Value::as_str) != Some("private to a worker")));
    // The desk's waiting list is owner questions only: a lane's unread mail is in the window
    // but `reply()` could never answer it, so it must not be listed (t3535 review).
    assert!(
        !conversation.waiting.contains(&lane_id),
        "a lane message is not a question waiting for the desk"
    );
    assert!(
        conversation.waiting.contains(&question_id),
        "an ordinary owner question still waits for the desk"
    );
}
