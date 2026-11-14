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

#[test]
fn agent_only_rows_stay_in_inbox_without_crowding_out_human_or_legacy_mail() {
    let (_dir, flock, store) = fixture("agent-only");
    let send = |mailbox: &str, subject: &str, body: &str, pane: Option<&str>| {
        store
            .send(&Outgoing {
                mailbox,
                subject: Some(subject),
                body: Some(body),
                pane_id: pane,
                sender: Some(WORKER),
                ..Outgoing::default()
            })
            .unwrap()
    };
    let human = send(
        OWNER_MAILBOX,
        "[agent-coordination] literal human subject",
        "human",
        None,
    );
    let legacy = send(
        OWNER_LANE_MAILBOX,
        "unmarked legacy decision",
        "legacy",
        Some(OWNER),
    );
    let decision = send(
        OWNER_LANE_MAILBOX,
        "[owner-decision] choose",
        "decide",
        Some(OWNER),
    );
    let literal = send(
        OWNER_LANE_MAILBOX,
        "[agent-coordination] literal",
        "legacy literal",
        Some(OWNER),
    );
    let mut hidden = Vec::new();
    for _ in 0..6 {
        hidden.push(
            store
                .send_snapshot_wire_agent_only(
                    &Outgoing {
                        mailbox: OWNER_LANE_MAILBOX,
                        subject: Some("ordinary subject"),
                        body: Some("private"),
                        pane_id: Some(OWNER),
                        sender: Some(WORKER),
                        ..Outgoing::default()
                    },
                    Some(&serde_json::json!("ordinary subject")),
                    None,
                    true,
                )
                .unwrap()
                .summary
                .message_id,
        );
    }
    let result = OwnerConversations::new(&flock, &NoPanes)
        .conversation(Some(4), None, None)
        .unwrap();
    let ids: Vec<_> = result
        .messages
        .iter()
        .map(|row| row["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 4);
    assert_eq!(result.count, 4);
    assert_eq!(result.waiting, vec![human.clone()]);
    for id in [&human, &legacy, &decision, &literal] {
        assert!(ids.contains(&id.as_str()));
    }
    for id in hidden {
        assert!(!ids.contains(&id.as_str()));
        assert_eq!(store.get_message(&id).unwrap().summary.message_id, id);
    }
}

#[test]
fn deleting_and_purging_mail_cleans_only_its_audience() {
    let (_dir, _flock, store) = fixture("agent-only-retention");
    let send = || {
        store
            .send_snapshot_wire_agent_only(
                &Outgoing {
                    mailbox: OWNER_LANE_MAILBOX,
                    body: Some("private"),
                    ..Outgoing::default()
                },
                None,
                None,
                true,
            )
            .unwrap()
            .summary
            .message_id
    };
    let deleted = send();
    let purged = send();
    let retained = send();
    store.delete(std::slice::from_ref(&deleted)).unwrap();
    store.ack(std::slice::from_ref(&purged), None).unwrap();
    store.purge(Some(-1.0)).unwrap();
    let db = rusqlite::Connection::open(store.path()).unwrap();
    let mut stmt = db
        .prepare("SELECT message_id FROM message_audiences")
        .unwrap();
    let ids: Vec<String> = stmt
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(ids, vec![retained]);
}

#[test]
fn failed_audience_write_rolls_back_the_message() {
    let (_dir, _flock, store) = fixture("agent-only-rollback");
    let db = rusqlite::Connection::open(store.path()).unwrap();
    db.execute_batch("CREATE TRIGGER fail_audience BEFORE INSERT ON message_audiences BEGIN SELECT RAISE(ABORT, 'test'); END;").unwrap();
    assert!(store
        .send_snapshot_wire_agent_only(
            &Outgoing {
                mailbox: OWNER_LANE_MAILBOX,
                body: Some("private"),
                ..Outgoing::default()
            },
            None,
            None,
            true,
        )
        .is_err());
    let count: i64 = db
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn opening_an_older_inbox_keeps_unclassified_messages_visible() {
    let (dir, _flock, store) = fixture("agent-only-migration");
    let id = store
        .send(&Outgoing {
            mailbox: OWNER_LANE_MAILBOX,
            body: Some("legacy"),
            subject: Some("[agent-coordination] literal"),
            ..Outgoing::default()
        })
        .unwrap();
    let db = rusqlite::Connection::open(store.path()).unwrap();
    db.execute_batch("DROP TABLE message_audiences").unwrap();
    drop(db);
    let reopened = Store::open(store.path(), dir.path().join("reopened-blobs")).unwrap();
    let rows = reopened
        .conversation_across_visible(&[OWNER_LANE_MAILBOX.into()], Some(10), None)
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["id"], id);
}

#[test]
fn inbox_source_filters_use_metadata_scope_and_opt_in_without_consuming() {
    use crate::owner::ConversationFilter::{All, AllSources, Human, OwnerLane};
    let human = Human {
        include_internal: false,
    };
    let (dir, flock, store) = fixture("source-filters");
    flock
        .with_roles(|roles, _| {
            roles.register("w1:p4", "desk", None, None, None, None, None, None)?;
            Ok(())
        })
        .unwrap();
    let send = |mailbox: &str, pane, transport, subject: &str, agent| {
        let outgoing = Outgoing {
            mailbox,
            pane_id: pane,
            transport,
            subject: Some(subject),
            body: Some("human codex-rust-owner agent-only: these words do not classify mail"),
            ..Default::default()
        };
        if agent {
            store
                .send_snapshot_wire_agent_only(
                    &outgoing,
                    Some(&serde_json::json!(subject)),
                    None,
                    true,
                )
                .unwrap()
                .summary
                .message_id
        } else {
            store.send(&outgoing).unwrap()
        }
    };
    let human_legacy = send(
        "owner",
        None,
        None,
        "[review-note] still human by mailbox",
        false,
    );
    let human_web = send("owner", None, Some("web"), "question", false);
    let root_reply = store
        .send(&Outgoing {
            mailbox: "owner",
            transport: Some("web"),
            reply_to: Some(&human_web),
            body: Some("sender-less answer remains unattributed"),
            ..Default::default()
        })
        .unwrap();
    let root_agent = send("owner", None, None, "ordinary", true);
    let lane_legacy = send(
        OWNER_LANE_MAILBOX,
        Some(OWNER),
        None,
        "[review-note] internal routine",
        false,
    );
    let lane_agent = send(
        OWNER_LANE_MAILBOX,
        Some(OWNER),
        Some("web"),
        "ordinary",
        true,
    );
    let lane_alias = send("owner-alias", Some(OWNER), None, "legacy", false);
    let foreign = send(
        "codex-rust-owner-lookalike",
        Some(WORKER),
        None,
        "ordinary",
        false,
    );
    let db = rusqlite::Connection::open(dir.path().join("source-filters-inbox.db")).unwrap();
    db.execute("UPDATE messages SET created_at=100", [])
        .unwrap();
    let snapshot = || {
        let mut statement = db.prepare("SELECT * FROM messages ORDER BY id").unwrap();
        let columns = statement.column_count();
        let rows = statement
            .query_map([], |row| {
                (0..columns)
                    .map(|column| row.get::<_, rusqlite::types::Value>(column))
                    .collect::<Result<Vec<_>, _>>()
            })
            .unwrap();
        rows.collect::<Result<Vec<_>, _>>().unwrap()
    };
    let before = snapshot();
    let service = OwnerConversations::new(&flock, &NoPanes);
    let ids = |filter| {
        service
            .conversation_filtered_cursor(50, None, Some(OWNER), filter)
            .unwrap()
            .messages
            .into_iter()
            .map(|row| row["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(human),
        vec![human_legacy.clone(), human_web.clone(), root_reply.clone()]
    );
    assert_eq!(
        ids(OwnerLane {
            include_internal: false
        }),
        vec![lane_legacy.clone(), lane_alias.clone()]
    );
    assert_eq!(
        ids(OwnerLane {
            include_internal: true
        }),
        vec![lane_legacy.clone(), lane_agent.clone(), lane_alias.clone()]
    );
    assert_eq!(
        service.conversation_cursor(50, None, Some(OWNER)).unwrap(),
        service
            .conversation_filtered_cursor(50, None, Some(OWNER), All)
            .unwrap()
    );
    let human_rows = service
        .conversation_filtered_cursor(50, None, Some(OWNER), human)
        .unwrap();
    assert_eq!(
        human_rows
            .messages
            .iter()
            .find(|row| row["id"] == root_reply)
            .unwrap()["ambiguous_owner_reply"],
        true
    );
    assert_eq!(
        ids(Human {
            include_internal: true
        }),
        vec![
            human_legacy.clone(),
            human_web.clone(),
            root_reply.clone(),
            root_agent.clone()
        ]
    );
    let combined = ids(AllSources {
        include_internal: false,
    });
    assert!(combined.contains(&human_legacy) && combined.contains(&lane_legacy));
    assert!(!combined.contains(&root_agent) && !combined.contains(&lane_agent));
    let combined = ids(AllSources {
        include_internal: true,
    });
    assert!(combined.contains(&root_agent) && combined.contains(&lane_agent));
    assert!(!combined.contains(&foreign));
    assert!(ids(All).contains(&root_agent)); // preserve the existing root-mailbox audience exception.
    assert!(!ids(All).contains(&foreign));
    let page = service
        .conversation_filtered_cursor(
            1,
            None,
            Some(OWNER),
            OwnerLane {
                include_internal: true,
            },
        )
        .unwrap();
    assert_eq!(page.messages[0]["id"], lane_alias);
    let page = service
        .conversation_filtered_cursor(
            1,
            Some(&lane_alias),
            Some(OWNER),
            OwnerLane {
                include_internal: true,
            },
        )
        .unwrap();
    assert_eq!(page.messages[0]["id"], lane_agent);
    assert!(service
        .conversation_filtered_cursor(
            1,
            Some(&lane_agent),
            Some(OWNER),
            OwnerLane {
                include_internal: false
            }
        )
        .is_err());
    assert!(service
        .conversation_filtered_cursor(1, Some(&lane_alias), Some(OWNER), human)
        .is_err());
    assert!(service
        .conversation_filtered_cursor(
            1,
            Some(&human_web),
            Some(OWNER),
            OwnerLane {
                include_internal: true
            }
        )
        .is_err());
    for pane in [Some(WORKER), Some("w1:p4"), None] {
        assert!(service
            .conversation_filtered_cursor(50, None, pane, human)
            .is_err());
        assert!(service
            .conversation_filtered_cursor(
                50,
                None,
                pane,
                OwnerLane {
                    include_internal: true
                }
            )
            .is_err());
    }
    assert!(service
        .conversation_filtered_cursor(1, Some(&root_agent), Some(OWNER), human)
        .is_err());
    assert_eq!(before, snapshot());
}
