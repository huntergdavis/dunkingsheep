use super::*;
use crate::inbox_store::{Outgoing, ThreadKind, ThreadRoute, ThreadSender, ThreadWrite};

fn fixture() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("inbox.db"), dir.path().join("blobs")).unwrap();
    (dir, store)
}
fn write(store: &Store, parent: Option<&str>, human: bool, recipient: ThreadRoute) -> String {
    let mut thread = ThreadWrite::root(
        if human {
            ThreadKind::Human
        } else {
            ThreadKind::Vp
        },
        if human {
            ThreadSender::Human
        } else {
            ThreadSender::Agent
        },
    );
    thread.parent_id = parent.map(str::to_owned);
    thread.sender_route = if human {
        ThreadRoute::Human
    } else {
        ThreadRoute::Lane("fixture".into())
    };
    thread.recipient_route = recipient;
    store
        .send_snapshot(&Outgoing {
            mailbox: "owner",
            body: Some(&"🦊".repeat(400)),
            subject: Some("same subject"),
            thread: Some(thread),
            ..Default::default()
        })
        .unwrap()
        .summary
        .message_id
}
fn options(mode: ThreadPageMode, filter: ConversationFilter, limit: i64) -> ThreadPageOptions {
    ThreadPageOptions {
        mode,
        filter,
        limit,
        root_id: None,
        cursor: None,
    }
}
fn heads(store: &Store, filter: ConversationFilter) -> Value {
    store
        .conversation_thread_page(&options(ThreadPageMode::Heads, filter, 100))
        .unwrap()
}
fn detail(
    store: &Store,
    filter: ConversationFilter,
    root: &str,
    limit: i64,
    cursor: Option<String>,
) -> Result<Value, DunkError> {
    store.conversation_thread_page(&ThreadPageOptions {
        root_id: Some(root.into()),
        cursor,
        ..options(ThreadPageMode::Messages, filter, limit)
    })
}

#[test]
fn thread_heads_cover_whole_threads_counts_preview_and_chronological_paged_detail() {
    let (_dir, store) = fixture();
    let first = write(&store, None, true, ThreadRoute::Vp);
    let mut first_ids = vec![first.clone()];
    // More than the old 200-message window: grouping/counts must precede list LIMIT.
    for _ in 0..220 {
        first_ids.push(write(&store, Some(&first), false, ThreadRoute::Human));
    }
    let second = write(&store, None, true, ThreadRoute::Vp);
    for _ in 0..6 {
        write(&store, Some(&second), false, ThreadRoute::Human);
    }
    store
        .mark_read_with_receipt("owner", &first_ids[..2], Some(42.0))
        .unwrap();
    let before = store
        .conversation_thread_cursor(201, None, ConversationFilter::HumanThreads)
        .unwrap();
    let page = heads(&store, ConversationFilter::HumanThreads);
    let rows = page["threads"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["thread_id"], second);
    assert_eq!(rows[0]["message_count"], 7);
    assert_eq!(rows[1]["message_count"], 221);
    assert_eq!(rows[1]["unread_count"], 219);
    assert_eq!(
        rows[0]["latest"]["preview"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        240
    );
    assert!(rows[0]["latest"].get("body").is_none());
    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = detail(&store, ConversationFilter::HumanThreads, &first, 37, cursor).unwrap();
        let messages = page["messages"].as_array().unwrap();
        assert!(messages.len() <= 37);
        let ordering: Vec<(f64, i64)> = messages
            .iter()
            .map(|r| {
                (
                    r["created_at"].as_f64().unwrap(),
                    number(r["id"].as_str().unwrap()).unwrap(),
                )
            })
            .collect();
        assert!(ordering.windows(2).all(|pair| pair[0] < pair[1]));
        seen.extend(
            messages
                .iter()
                .map(|r| r["id"].as_str().unwrap().to_owned()),
        );
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        assert!(
            seen.len() <= first_ids.len(),
            "duplicate or nonterminating detail page"
        );
        if cursor.is_none() {
            break;
        }
    }
    seen.sort_by_key(|id| number(id).unwrap());
    assert_eq!(seen, first_ids);
    assert_eq!(
        store
            .conversation_thread_cursor(201, None, ConversationFilter::HumanThreads)
            .unwrap(),
        before
    );
}

#[test]
fn human_addressed_vp_notice_is_visible_alone_until_real_human_promotion() {
    let (_dir, store) = fixture();
    let root = write(&store, None, false, ThreadRoute::Vp);
    let addressed = write(&store, Some(&root), false, ThreadRoute::Human);
    write(&store, Some(&root), false, ThreadRoute::Vp);
    let mut system = ThreadWrite::root(ThreadKind::Vp, ThreadSender::System);
    system.parent_id = Some(root.clone());
    system.recipient_route = ThreadRoute::Human;
    store
        .send_snapshot(&Outgoing {
            mailbox: "owner",
            body: Some("system notice"),
            thread: Some(system),
            ..Default::default()
        })
        .unwrap();
    let human = heads(&store, ConversationFilter::HumanThreads);
    assert_eq!(human["threads"][0]["thread_id"], root);
    assert_eq!(human["threads"][0]["latest"]["id"], addressed);
    assert_eq!(human["threads"][0]["message_count"], 1);
    assert_eq!(
        detail(&store, ConversationFilter::HumanThreads, &root, 25, None).unwrap()["messages"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        heads(&store, ConversationFilter::VpThreads)["threads"][0]["message_count"],
        4
    );
    let promoted = write(&store, Some(&addressed), true, ThreadRoute::Vp);
    let human = heads(&store, ConversationFilter::HumanThreads);
    assert_eq!(human["threads"][0]["message_count"], 5);
    assert_eq!(human["threads"][0]["latest"]["id"], promoted);
    assert!(heads(&store, ConversationFilter::VpThreads)["threads"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(detail(&store, ConversationFilter::VpThreads, &root, 25, None).is_err());
}

#[test]
fn legacy_rows_remain_singletons_and_preserve_senderless_reply_ambiguity() {
    let (_dir, store) = fixture();
    let root = store
        .send(&Outgoing {
            mailbox: "owner",
            body: Some("same body"),
            subject: Some("same subject"),
            transport: Some("web"),
            ..Default::default()
        })
        .unwrap();
    let reply = store
        .send(&Outgoing {
            mailbox: "owner",
            body: Some("same body"),
            subject: Some("same subject"),
            transport: Some("web"),
            reply_to: Some(&root),
            ..Default::default()
        })
        .unwrap();
    let rows = heads(&store, ConversationFilter::Historical);
    assert_eq!(rows["threads"].as_array().unwrap().len(), 2);
    for row in rows["threads"].as_array().unwrap() {
        assert_eq!(row["message_count"], 1);
        assert!(row["latest"]["root_id"].is_null());
        assert_eq!(row["latest"]["historical_reason"], "prior_version");
    }
    let historical = detail(&store, ConversationFilter::Historical, &reply, 25, None).unwrap();
    assert_eq!(historical["messages"][0]["ambiguous_owner_reply"], true);
    assert!(detail(&store, ConversationFilter::HumanThreads, &root, 25, None).is_err());
}

#[test]
fn fixed_id_fence_preserves_older_pages_after_a_reply_and_cursor_scope_is_bound() {
    let (_dir, store) = fixture();
    let first = write(&store, None, true, ThreadRoute::Vp);
    let second = write(&store, None, true, ThreadRoute::Vp);
    let opts = options(ThreadPageMode::Heads, ConversationFilter::HumanThreads, 1);
    let page = store.conversation_thread_page(&opts).unwrap();
    assert_eq!(page["threads"][0]["thread_id"], second);
    let cursor = page["next_cursor"].as_str().unwrap().to_owned();
    write(&store, Some(&first), false, ThreadRoute::Human);
    let next = store
        .conversation_thread_page(&ThreadPageOptions {
            cursor: Some(cursor.clone()),
            ..opts.clone()
        })
        .unwrap();
    assert_eq!(next["threads"][0]["thread_id"], first);
    assert_eq!(next["threads"][0]["message_count"], 1);
    assert_eq!(
        heads(&store, ConversationFilter::HumanThreads)["threads"][0]["message_count"],
        2
    );
    for changed in [
        ThreadPageOptions {
            limit: 2,
            cursor: Some(cursor.clone()),
            ..opts.clone()
        },
        ThreadPageOptions {
            filter: ConversationFilter::VpThreads,
            cursor: Some(cursor.clone()),
            ..opts.clone()
        },
        ThreadPageOptions {
            mode: ThreadPageMode::Messages,
            root_id: Some(first),
            cursor: Some(cursor),
            ..opts
        },
    ] {
        assert!(store.conversation_thread_page(&changed).is_err());
    }
}

#[test]
fn cancellation_deletion_timestamp_change_and_promotion_invalidate_cursors_but_plain_read_does_not()
{
    for mutation in ["cancel", "delete", "timestamp", "promote"] {
        let (_dir, store) = fixture();
        let root = write(&store, None, false, ThreadRoute::Vp);
        let last = write(&store, None, false, ThreadRoute::Vp);
        let opts = options(ThreadPageMode::Heads, ConversationFilter::VpThreads, 1);
        let page = store.conversation_thread_page(&opts).unwrap();
        let cursor = page["next_cursor"].as_str().unwrap().to_owned();
        store
            .mark_read_with_receipt("owner", std::slice::from_ref(&root), Some(42.0))
            .unwrap();
        assert!(store
            .conversation_thread_page(&ThreadPageOptions {
                cursor: Some(cursor.clone()),
                ..opts.clone()
            })
            .is_ok());
        match mutation {
            "cancel" => {
                store.cancel(std::slice::from_ref(&last)).unwrap();
            }
            "delete" => {
                store.delete(std::slice::from_ref(&last)).unwrap();
            }
            "timestamp" => {
                store
                    .writer
                    .call(move |c| {
                        c.execute(
                            "UPDATE messages SET created_at=created_at-100 WHERE id=?",
                            [last],
                        )
                        .map_err(map_sqlite)?;
                        Ok(())
                    })
                    .unwrap();
            }
            "promote" => {
                write(&store, Some(&root), true, ThreadRoute::Vp);
            }
            _ => unreachable!(),
        }
        let changed = store
            .conversation_thread_page(&ThreadPageOptions {
                cursor: Some(cursor),
                ..opts
            })
            .unwrap();
        assert_eq!(changed["reload_from_top"], true, "{mutation}: {changed}");
        assert!(changed["threads"].as_array().unwrap().is_empty());
        assert!(changed["next_cursor"].is_null());
    }
}

#[test]
fn numeric_ties_empty_views_bounds_and_additive_reopen_keep_legacy_wire_stable() {
    let (dir, store) = fixture();
    assert!(heads(&store, ConversationFilter::HumanThreads)["threads"]
        .as_array()
        .unwrap()
        .is_empty());
    for _ in 0..12 {
        write(&store, None, true, ThreadRoute::Vp);
    }
    store
        .writer
        .call(|c| {
            c.execute("UPDATE messages SET created_at=100", [])
                .map_err(map_sqlite)?;
            Ok(())
        })
        .unwrap();
    let old = store
        .conversation_thread_cursor(100, None, ConversationFilter::HumanThreads)
        .unwrap();
    let page = heads(&store, ConversationFilter::HumanThreads);
    let ids: Vec<i64> = page["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| number(r["latest"]["id"].as_str().unwrap()).unwrap())
        .collect();
    assert_eq!(ids, (1..=12).rev().collect::<Vec<_>>());
    for limit in [0, 101] {
        assert!(store
            .conversation_thread_page(&options(
                ThreadPageMode::Heads,
                ConversationFilter::HumanThreads,
                limit
            ))
            .is_err());
    }
    for raw in ["{}".to_owned(), "not json".to_owned(), "x".repeat(4097)] {
        assert!(store
            .conversation_thread_page(&ThreadPageOptions {
                cursor: Some(raw),
                ..options(ThreadPageMode::Heads, ConversationFilter::HumanThreads, 1)
            })
            .is_err());
    }
    assert!(store
        .conversation_thread_page(&options(ThreadPageMode::Heads, ConversationFilter::All, 1))
        .is_err());
    drop(store);
    let store = Store::open(dir.path().join("inbox.db"), dir.path().join("blobs")).unwrap();
    assert_eq!(
        store
            .conversation_thread_cursor(100, None, ConversationFilter::HumanThreads)
            .unwrap(),
        old
    );
    assert_eq!(heads(&store, ConversationFilter::HumanThreads), page);
}

#[test]
fn cursor_bootstrap_then_digest_returns_clean_reload_and_installs_no_write_hooks() {
    let (_dir, store) = fixture();
    for _ in 0..5 {
        write(&store, None, true, ThreadRoute::Vp);
    }
    let opts = options(ThreadPageMode::Heads, ConversationFilter::HumanThreads, 1);
    let first = store.conversation_thread_page(&opts).unwrap();
    let initial: Cursor = serde_json::from_str(first["next_cursor"].as_str().unwrap()).unwrap();
    assert!(initial.digest.is_none());
    assert_eq!(initial.anchors.len(), 1);
    let second = store
        .conversation_thread_page(&ThreadPageOptions {
            cursor: first["next_cursor"].as_str().map(str::to_owned),
            ..opts.clone()
        })
        .unwrap();
    let established: Cursor =
        serde_json::from_str(second["next_cursor"].as_str().unwrap()).unwrap();
    assert_eq!(established.digest.as_ref().unwrap().len(), 64);
    assert!(established.anchors.is_empty());
    // Change a previously served head after bootstrap; no duplicate/partial result.
    let seen = first["threads"][0]["latest"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    store.delete(&[seen]).unwrap();
    let changed = store
        .conversation_thread_page(&ThreadPageOptions {
            cursor: second["next_cursor"].as_str().map(str::to_owned),
            ..opts.clone()
        })
        .unwrap();
    assert_eq!(changed["reload_from_top"], true);
    assert_eq!(changed["message"], "List changed, reload from top.");
    assert_eq!(changed["threads"], json!([]));
    assert!(changed["next_cursor"].is_null());
    let polled = store.conversation_thread_page(&opts).unwrap();
    let poll_cursor: Cursor =
        serde_json::from_str(polled["next_cursor"].as_str().unwrap()).unwrap();
    assert!(poll_cursor.digest.is_none());
    store.readers.with(|c| {
        let hooks:i64=c.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name LIKE 'thread_pages_%'",[],|r|r.get(0)).map_err(map_sqlite)?;
        let counters:i64=c.query_row("SELECT COUNT(*) FROM counters WHERE name='thread_page_revision'",[],|r|r.get(0)).map_err(map_sqlite)?;
        assert_eq!((hooks,counters),(0,0));
        Ok(())
    }).unwrap();
}

#[test]
fn exact_fractional_sqlite_boundaries_page_every_head_and_detail_once() {
    let (_dir, store) = fixture();
    let root = write(&store, None, true, ThreadRoute::Vp);
    let mut ids = vec![root.clone()];
    for _ in 0..11 {
        ids.push(write(&store, Some(&root), false, ThreadRoute::Human));
    }
    for _ in 0..11 {
        write(&store, None, true, ThreadRoute::Vp);
    }
    store
        .writer
        .call(|c| {
            for n in 1..=23 {
                let at = f64::from_bits(1_794_771_000.1234567_f64.to_bits() + n / 2);
                c.execute(
                    "UPDATE messages SET created_at=? WHERE id=?",
                    params![at, format!("i{n}")],
                )
                .map_err(map_sqlite)?;
            }
            Ok(())
        })
        .unwrap();
    for mode in [ThreadPageMode::Heads, ThreadPageMode::Messages] {
        let mut opts = options(mode, ConversationFilter::HumanThreads, 2);
        if mode == ThreadPageMode::Messages {
            opts.root_id = Some(root.clone());
        }
        let mut seen = Vec::new();
        let mut finished = false;
        for _ in 0..20 {
            let page = store.conversation_thread_page(&opts).unwrap();
            assert_eq!(page["reload_from_top"], false);
            let rows = page[if mode == ThreadPageMode::Heads {
                "threads"
            } else {
                "messages"
            }]
            .as_array()
            .unwrap();
            for row in rows {
                let id = if mode == ThreadPageMode::Heads {
                    &row["thread_id"]
                } else {
                    &row["id"]
                }
                .as_str()
                .unwrap()
                .to_owned();
                assert!(!seen.contains(&id), "duplicate {id}");
                seen.push(id);
            }
            opts.cursor = page["next_cursor"].as_str().map(str::to_owned);
            if opts.cursor.is_none() {
                finished = true;
                break;
            }
        }
        assert!(finished, "pagination did not terminate");
        if mode == ThreadPageMode::Heads {
            assert_eq!(seen.len(), 12);
        } else {
            seen.sort();
            ids.sort();
            assert_eq!(seen, ids);
        }
    }
}
