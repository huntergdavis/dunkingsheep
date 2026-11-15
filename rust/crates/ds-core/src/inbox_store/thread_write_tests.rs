use super::*;
use crate::inbox_store::{Outgoing, Store};
use serde_json::json;

fn fixture() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("inbox.db"), dir.path().join("blobs")).unwrap();
    (dir, store)
}
fn human() -> ThreadWrite {
    let mut context = ThreadWrite::root(ThreadKind::Human, ThreadSender::Human);
    context.sender_route = ThreadRoute::Human;
    context.recipient_route = ThreadRoute::Vp;
    context.route_reply = true;
    context.dedupe_external_id = true;
    context
}
fn write(store: &Store, mailbox: &str, context: ThreadWrite) -> crate::inbox_store::Message {
    store
        .send_snapshot(&Outgoing {
            mailbox,
            body: Some("recorded body"),
            transport: Some("web"),
            thread: Some(context),
            ..Default::default()
        })
        .unwrap()
}
fn reference(id: &str, parent: Option<&str>) -> ThreadTransportRef {
    ThreadTransportRef {
        transport: "telegram".into(),
        channel: "42".into(),
        external_id: id.into(),
        parent_external_id: parent.map(str::to_owned),
    }
}

#[test]
fn human_root_reply_and_reply_to_reply_keep_exact_edges() {
    let (_dir, store) = fixture();
    let root = write(&store, "owner", human()).summary.message_id;
    let reply = store
        .answer_owner_question(&root, "answer", Some("%1"))
        .unwrap()
        .summary
        .message_id;
    let mut context = human();
    context.parent_id = Some(reply.clone());
    let follow = write(&store, "owner", context).summary.message_id;
    let last = store
        .answer_owner_question(&follow, "clarification", Some("%1"))
        .unwrap()
        .summary
        .message_id;
    for (id, parent, sender) in [
        (&root, None, "human"),
        (&reply, Some(&root), "agent"),
        (&follow, Some(&reply), "human"),
        (&last, Some(&follow), "agent"),
    ] {
        let info = store.thread_info(id).unwrap().unwrap();
        assert_eq!(info["parent_id"], json!(parent));
        assert_eq!(info["root_id"], root);
        assert_eq!(info["sender_kind"], sender);
    }
    assert!(
        store
            .get_message(&follow)
            .unwrap()
            .summary
            .reply_to
            .is_none(),
        "an old outbox must not echo incoming human replies"
    );
}

#[test]
fn recorded_lane_reply_address_survives_without_a_live_pane() {
    let (_dir, store) = fixture();
    let mut context = human();
    context.route_reply = false;
    context.recipient_route = ThreadRoute::Lane("writer-stable".into());
    let root = write(&store, "writer-stable", context).summary.message_id;
    let mut agent = ThreadWrite::root(ThreadKind::Human, ThreadSender::Agent);
    agent.parent_id = Some(root.clone());
    agent.allowed_parent_mailboxes = Some(vec!["writer-stable".into()]);
    agent.sender_route = ThreadRoute::Lane("writer-stable".into());
    agent.recipient_route = ThreadRoute::Human;
    let reply = write(&store, "owner", agent).summary.message_id;
    let mut follow = human();
    follow.parent_id = Some(reply.clone());
    let message = write(&store, "owner", follow);
    assert_eq!(message.summary.mailbox, "writer-stable");
    let pane: Option<String> = crate::db::open_writer(store.path())
        .unwrap()
        .query_row(
            "SELECT pane_id FROM messages WHERE id=?",
            [&message.summary.message_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(pane.is_none());
    let info = store
        .thread_info(&message.summary.message_id)
        .unwrap()
        .unwrap();
    assert_eq!(info["parent_id"], reply);
    assert_eq!(info["root_id"], root);
    assert_eq!(info["recipient_route_mailbox"], "writer-stable");
    let rows = store
        .conversation_thread_cursor(100, None, crate::owner::ConversationFilter::HumanThreads)
        .unwrap();
    assert_eq!(
        rows.len(),
        3,
        "human thread includes its physically delivered lane rows"
    );
}

#[test]
fn human_participation_moves_the_whole_vp_thread_and_advances_revision() {
    let (_dir, store) = fixture();
    let mut context = ThreadWrite::root(ThreadKind::Vp, ThreadSender::Agent);
    context.sender_route = ThreadRoute::Vp;
    context.recipient_route = ThreadRoute::Lane("worker".into());
    let root = write(&store, "worker", context).summary.message_id;
    let mut context = human();
    context.parent_id = Some(root.clone());
    write(&store, "owner", context);
    assert_eq!(store.thread_view_revision().unwrap(), 1);
    assert_eq!(
        store.thread_info(&root).unwrap().unwrap()["conversation_kind"],
        "human"
    );
    assert!(store
        .conversation_thread_cursor(100, None, crate::owner::ConversationFilter::VpThreads)
        .unwrap()
        .is_empty());
    let other = ThreadWrite::root(ThreadKind::Other, ThreadSender::Agent);
    write(&store, "unrelated", other);
    assert_eq!(
        store
            .conversation_thread_cursor(100, None, crate::owner::ConversationFilter::HumanThreads)
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn foreign_and_historical_parents_are_refused_without_allocating_a_message() {
    let (_dir, store) = fixture();
    let foreign = write(&store, "foreign", human()).summary.message_id;
    let historical = store
        .send(&Outgoing {
            mailbox: "own",
            body: Some("prior version"),
            ..Default::default()
        })
        .unwrap();
    for parent in [&foreign, &historical] {
        let mut context = ThreadWrite::root(ThreadKind::Vp, ThreadSender::Agent);
        context.parent_id = Some(parent.clone());
        context.allowed_parent_mailboxes = Some(vec!["own".into()]);
        assert!(store
            .send(&Outgoing {
                mailbox: "owner",
                body: Some("refused"),
                thread: Some(context),
                ..Default::default()
            })
            .is_err());
    }
    assert!(store.thread_info(&historical).unwrap().is_none());
    assert_eq!(
        store
            .send(&Outgoing {
                mailbox: "own",
                body: Some("next"),
                ..Default::default()
            })
            .unwrap(),
        "i3"
    );
}

#[test]
fn sidecar_failure_rolls_back_message_id_promotion_and_revision() {
    let (_dir, store) = fixture();
    let mut context = ThreadWrite::root(ThreadKind::Vp, ThreadSender::Agent);
    context.sender_route = ThreadRoute::Vp;
    let root = write(&store, "owner", context).summary.message_id;
    let db = crate::db::open_writer(store.path()).unwrap();
    db.execute_batch("CREATE TRIGGER refuse_thread BEFORE INSERT ON message_threads BEGIN SELECT RAISE(ABORT,'fixture sidecar failure'); END;").unwrap();
    let mut context = human();
    context.parent_id = Some(root.clone());
    assert!(store
        .send(&Outgoing {
            mailbox: "owner",
            body: Some("must roll back"),
            thread: Some(context),
            ..Default::default()
        })
        .is_err());
    assert_eq!(
        store.thread_info(&root).unwrap().unwrap()["conversation_kind"],
        "vp"
    );
    assert_eq!(store.thread_view_revision().unwrap(), 0);
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    db.execute_batch("DROP TRIGGER refuse_thread").unwrap();
    assert_eq!(write(&store, "owner", human()).summary.message_id, "i2");
}

#[test]
fn telegram_mapping_dedupes_and_resolves_only_the_recorded_chat_parent() {
    let (_dir, store) = fixture();
    let mut context = human();
    context.transport_ref = Some(reference("10", None));
    let outgoing = Outgoing {
        mailbox: "owner",
        body: Some("question"),
        transport: Some("telegram"),
        thread: Some(context),
        ..Default::default()
    };
    let (question, duplicate) = store
        .send_thread_snapshot_wire(&outgoing, None, None, false)
        .unwrap();
    assert!(!duplicate);
    let (again, duplicate) = store
        .send_thread_snapshot_wire(&outgoing, None, None, false)
        .unwrap();
    assert!(duplicate);
    assert_eq!(again.summary.message_id, question.summary.message_id);
    let answer = store
        .answer_owner_question(&question.summary.message_id, "answer", None)
        .unwrap();
    store
        .mark_delivered_with_refs(
            &answer.summary.message_id,
            Some("telegram"),
            None,
            vec![reference("20", None), reference("21", None)],
        )
        .unwrap();
    let mut context = human();
    context.transport_ref = Some(reference("30", Some("20")));
    let follow = store
        .send_snapshot(&Outgoing {
            mailbox: "owner",
            body: Some("follow up"),
            transport: Some("telegram"),
            thread: Some(context),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        store
            .thread_info(&follow.summary.message_id)
            .unwrap()
            .unwrap()["parent_id"],
        answer.summary.message_id
    );
    let mut context = human();
    let mut r = reference("40", Some("20"));
    r.channel = "other-chat".into();
    context.transport_ref = Some(r);
    let unlinked = store
        .send_snapshot(&Outgoing {
            mailbox: "owner",
            body: Some("new root"),
            transport: Some("telegram"),
            thread: Some(context),
            ..Default::default()
        })
        .unwrap();
    let info = store
        .thread_info(&unlinked.summary.message_id)
        .unwrap()
        .unwrap();
    assert_eq!(info["root_id"], unlinked.summary.message_id);
    assert_eq!(info["parent_id"], json!(null));
    assert_eq!(info["unresolved_parent"], true);
    let second = store
        .answer_owner_question(&follow.summary.message_id, "second answer", None)
        .unwrap();
    assert!(store
        .mark_delivered_with_refs(
            &second.summary.message_id,
            Some("telegram"),
            None,
            vec![reference("new-id", None), reference("20", None)]
        )
        .is_err());
    assert!(store
        .get_message(&second.summary.message_id)
        .unwrap()
        .summary
        .delivered_at
        .is_none());
    let count: i64 = crate::db::open_writer(store.path())
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM message_thread_refs WHERE external_id='new-id'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}

#[test]
fn redelivered_legacy_transport_update_keeps_the_original_historical_row() {
    let (_dir, store) = fixture();
    let legacy = store
        .send(&Outgoing {
            mailbox: "owner",
            body: Some("old recorded question"),
            transport: Some("telegram"),
            external_id: Some("telegram:7"),
            ..Default::default()
        })
        .unwrap();
    let mut context = human();
    context.transport_ref = Some(reference("17", None));
    let (again, duplicate) = store
        .send_thread_snapshot_wire(
            &Outgoing {
                mailbox: "owner",
                body: Some("redelivery"),
                transport: Some("telegram"),
                external_id: Some("telegram:7"),
                thread: Some(context),
                ..Default::default()
            },
            None,
            None,
            false,
        )
        .unwrap();
    assert!(duplicate);
    assert_eq!(again.summary.message_id, legacy);
    assert_eq!(again.body, "old recorded question");
    assert!(store.thread_info(&legacy).unwrap().is_none());
    let db = crate::db::open_writer(store.path()).unwrap();
    let count: i64 = db
        .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn normal_delete_cleans_all_thread_sidecars_and_preserves_the_other_conversation() {
    let (_dir, store) = fixture();
    let mut context = human();
    context.transport_ref = Some(reference("10", None));
    let removed = store
        .send(&Outgoing {
            mailbox: "owner",
            body: Some("delete this conversation"),
            transport: Some("telegram"),
            thread: Some(context),
            ..Default::default()
        })
        .unwrap();
    let retained = write(&store, "owner", human()).summary.message_id;
    store.delete(std::slice::from_ref(&removed)).unwrap();
    assert!(store.thread_info(&removed).unwrap().is_none());
    assert!(store.thread_info(&retained).unwrap().is_some());
    let db = crate::db::open_writer(store.path()).unwrap();
    for table in [
        "message_threads",
        "message_thread_routes",
        "message_thread_refs",
    ] {
        let count: i64 = db
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE message_id=?"),
                [&removed],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }
}

#[test]
fn agent_addressed_to_human_is_visible_alone_until_human_participates() {
    let (_dir, store) = fixture();
    let mut context = ThreadWrite::root(ThreadKind::Vp, ThreadSender::Agent);
    context.sender_route = ThreadRoute::Vp;
    context.recipient_route = ThreadRoute::Lane("worker".into());
    let root = write(&store, "worker", context).summary.message_id;
    // Even a caller's proposed Human scope cannot promote on an agent write.
    let mut agent = ThreadWrite::root(ThreadKind::Human, ThreadSender::Agent);
    agent.parent_id = Some(root.clone());
    agent.sender_route = ThreadRoute::Lane("worker".into());
    agent.recipient_route = ThreadRoute::Human;
    let addressed = write(&store, "owner", agent).summary.message_id;
    let read = |filter| store.conversation_thread_cursor(100, None, filter).unwrap();
    let ids = |rows: Vec<serde_json::Value>| {
        rows.into_iter()
            .map(|row| row["id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(store.thread_view_revision().unwrap(), 0);
    assert_eq!(
        store.thread_info(&root).unwrap().unwrap()["conversation_kind"],
        "vp"
    );
    assert_eq!(
        store.thread_info(&addressed).unwrap().unwrap()["root_id"],
        root
    );
    assert_eq!(
        store.thread_info(&addressed).unwrap().unwrap()["conversation_kind"],
        "vp"
    );
    assert_eq!(
        ids(read(crate::owner::ConversationFilter::HumanThreads)),
        vec![addressed.clone()]
    );
    let mut vp = ids(read(crate::owner::ConversationFilter::VpThreads));
    vp.sort();
    let mut expected = vec![root.clone(), addressed.clone()];
    expected.sort();
    assert_eq!(vp, expected);
    let mut follow = human();
    follow.parent_id = Some(addressed);
    write(&store, "owner", follow);
    assert_eq!(store.thread_view_revision().unwrap(), 1);
    assert!(read(crate::owner::ConversationFilter::VpThreads).is_empty());
    assert_eq!(
        read(crate::owner::ConversationFilter::HumanThreads).len(),
        3
    );
}
