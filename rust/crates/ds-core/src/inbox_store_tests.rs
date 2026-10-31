//! Store-half tests for the inbox write path (T15b), ported from tests/test_dunk_inbox.py:
//! ids, attachments shared and duplicated, round trip, refusals, the 0600 mode, and the
//! #187 acceptance - a write through the store is visible to a fresh `ReadPool`.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use super::*;
use crate::db::ReadPool;
use crate::inbox;

struct Fixture {
    dir: tempfile::TempDir,
    store: Store,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let store =
            Store::open(dir.path().join("inbox.db"), dir.path().join("blobs")).expect("open");
        Fixture { dir, store }
    }

    fn pool(&self) -> ReadPool {
        ReadPool::open(self.store.path(), 1).expect("fresh pool")
    }

    fn file(&self, name: &str, text: &str) -> PathBuf {
        let path = self.dir.path().join(name);
        std::fs::write(&path, text).expect("write file");
        path
    }

    fn send(&self, mailbox: &str, body: &str) -> String {
        self.store
            .send(&Outgoing {
                mailbox,
                body: Some(body),
                sender: Some("pm"),
                ..Default::default()
            })
            .expect("send")
    }
}

#[test]
fn a_write_through_the_store_is_visible_to_a_fresh_read_pool() {
    let fixture = Fixture::new();
    let id = fixture.send("writer", "durable");
    let (summary, body) = inbox::get(&fixture.pool(), &id, now_s()).expect("get");
    assert_eq!("queued", summary.state);
    assert_eq!("durable", body);
    assert_eq!(Some("pm".to_string()), summary.sender);
    let counts = inbox::count(&fixture.pool(), "writer", &[], now_s()).expect("count");
    assert_eq!(1, counts.unread);
}

#[test]
fn ids_count_up_from_one_and_survive_a_restart() {
    let fixture = Fixture::new();
    assert_eq!("i1", fixture.send("writer", "one"));
    assert_eq!("i2", fixture.send("writer", "two"));
    let path = fixture.store.path().to_path_buf();
    let blobs = fixture.dir.path().join("blobs");
    fixture.store.shutdown();
    let reopened = Store::open(&path, &blobs).expect("reopen");
    let next = reopened
        .send(&Outgoing {
            mailbox: "writer",
            body: Some("next"),
            ..Default::default()
        })
        .expect("send");
    assert_eq!(
        "i3", next,
        "the counter lives in the database, not the process"
    );
    reopened.shutdown();
}

#[test]
fn the_same_attachment_can_be_used_by_two_messages() {
    let fixture = Fixture::new();
    let path = fixture.file("same.txt", "same payload\n");
    let attachments = [path];
    let send = |mailbox: &'static str| {
        fixture
            .store
            .send(&Outgoing {
                mailbox,
                body: Some("x"),
                attachments: &attachments,
                ..Default::default()
            })
            .expect("send")
    };
    let (first, second) = (send("a"), send("b"));
    let pool = fixture.pool();
    let rows: Vec<(String, String, String)> = pool
        .with(|c| {
            let mut statement = c
                .prepare("SELECT id, message_id, sha256 FROM attachments ORDER BY created_at, id")
                .map_err(map_sqlite)?;
            let rows = statement
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .map_err(map_sqlite)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(map_sqlite)?;
            Ok(rows)
        })
        .expect("rows");
    assert_eq!(2, rows.len());
    assert_ne!(rows[0].0, rows[1].0, "row ids must be unique");
    assert_eq!(rows[0].2, rows[1].2, "the blob is shared");
    let messages: std::collections::BTreeSet<_> = rows.iter().map(|r| r.1.clone()).collect();
    assert_eq!(
        [first, second]
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>(),
        messages
    );
    let blobs = std::fs::read_dir(fixture.dir.path().join("blobs"))
        .expect("dir")
        .count();
    assert_eq!(1, blobs, "one content-addressed copy, not one per message");
}

#[test]
fn duplicate_attachments_in_one_message_get_distinct_ids() {
    let fixture = Fixture::new();
    let path = fixture.file("dup.txt", "dup\n");
    let attachments = [path.clone(), path];
    let id = fixture
        .store
        .send(&Outgoing {
            mailbox: "a",
            body: Some("dup"),
            attachments: &attachments,
            ..Default::default()
        })
        .expect("send");
    assert_eq!("i1", id);
    let ids: Vec<String> = fixture
        .pool()
        .with(|c| {
            let mut statement = c
                .prepare("SELECT id FROM attachments WHERE message_id = ? ORDER BY rowid")
                .map_err(map_sqlite)?;
            let rows = statement
                .query_map([&id], |r| r.get(0))
                .map_err(map_sqlite)?
                .collect::<Result<Vec<String>, _>>()
                .map_err(map_sqlite)?;
            Ok(rows)
        })
        .expect("ids");
    assert_eq!(vec!["i1-1".to_string(), "i1-2".to_string()], ids);
}

#[test]
fn attachments_round_trip_with_size_name_hash_and_a_private_blob() {
    let fixture = Fixture::new();
    let source = fixture.file("patch.diff", "--- a\n+++ b\n");
    let attachments = [source];
    fixture
        .store
        .send(&Outgoing {
            mailbox: "writer",
            body: Some("see attached"),
            attachments: &attachments,
            ..Default::default()
        })
        .expect("send");
    let (filename, size, sha): (String, i64, String) = fixture
        .pool()
        .with(|c| {
            c.query_row("SELECT filename, size, sha256 FROM attachments", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .map_err(map_sqlite)
        })
        .expect("row");
    assert_eq!("patch.diff", filename);
    assert_eq!(12, size);
    let blob = fixture.dir.path().join("blobs").join(&sha);
    assert_eq!(
        "--- a\n+++ b\n",
        std::fs::read_to_string(&blob).expect("blob")
    );
    let mode = std::fs::metadata(&blob).expect("meta").permissions().mode() & 0o777;
    assert_eq!(0o600, mode, "t739: a blob must never be world-readable");
    let dir_mode = std::fs::metadata(fixture.dir.path().join("blobs"))
        .expect("meta")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(0o700, dir_mode);
    assert_eq!(
        1,
        inbox::count(&fixture.pool(), "writer", &[], now_s())
            .expect("count")
            .attachments_unread
    );
    assert_eq!(64, sha.len(), "a sha256 hex digest");
}

#[test]
fn attaching_a_missing_file_is_refused_and_stores_nothing() {
    let fixture = Fixture::new();
    let attachments = [PathBuf::from("/no/such/file")];
    let error = fixture
        .store
        .send(&Outgoing {
            mailbox: "writer",
            body: Some("body"),
            attachments: &attachments,
            ..Default::default()
        })
        .expect_err("refused");
    assert!(error.to_string().contains("no such file"), "{error}");
    let counts = inbox::count(&fixture.pool(), "writer", &[], now_s()).expect("count");
    assert_eq!(0, counts.total, "a refused attachment leaves no message");
}

#[test]
fn an_empty_message_a_missing_mailbox_and_a_bad_transport_are_refused() {
    let fixture = Fixture::new();
    let text = |outgoing: Outgoing| {
        fixture
            .store
            .send(&outgoing)
            .expect_err("refused")
            .to_string()
    };
    assert!(text(Outgoing {
        mailbox: "writer",
        body: Some("   "),
        ..Default::default()
    })
    .contains("body or an attachment"));
    assert_eq!(
        "a mailbox is required",
        text(Outgoing {
            mailbox: "  ",
            body: Some("x"),
            ..Default::default()
        })
    );
    assert!(text(Outgoing {
        mailbox: "writer",
        body: Some("x"),
        transport: Some("carrier-pigeon"),
        ..Default::default()
    })
    .starts_with("transport must be one of"));
}

#[test]
fn an_attachment_alone_is_a_message_and_the_label_defaults_to_the_mailbox() {
    let fixture = Fixture::new();
    let attachments = [fixture.file("a.png", "x")];
    let id = fixture
        .store
        .send(&Outgoing {
            mailbox: "  writer ",
            body: None,
            attachments: &attachments,
            ..Default::default()
        })
        .expect("send");
    let (summary, body) = inbox::get(&fixture.pool(), &id, now_s()).expect("get");
    assert_eq!("writer", summary.mailbox, "the mailbox is stripped");
    assert_eq!("", body);
    let media: String = fixture
        .pool()
        .with(|c| {
            c.query_row("SELECT media_type FROM attachments", [], |r| r.get(0))
                .map_err(map_sqlite)
        })
        .expect("media");
    assert_eq!("image/png", media);
}

#[test]
fn owner_mailbox_fields_are_stored_and_an_empty_external_id_is_null() {
    let fixture = Fixture::new();
    let id = fixture
        .store
        .send(&Outgoing {
            mailbox: "owner",
            body: Some("question"),
            transport: Some(" Web "),
            reply_to: Some("i0"),
            external_id: Some("upd-9"),
            ..Default::default()
        })
        .expect("send");
    let (summary, _) = inbox::get(&fixture.pool(), &id, now_s()).expect("get");
    assert_eq!(Some("web".to_string()), summary.transport);
    assert_eq!(Some("i0".to_string()), summary.reply_to);
    assert_eq!(Some("upd-9".to_string()), summary.external_id);
    let blank = fixture
        .store
        .send(&Outgoing {
            mailbox: "owner",
            body: Some("b"),
            external_id: Some(""),
            ..Default::default()
        })
        .expect("a blank external id must not collide");
    let (summary, _) = inbox::get(&fixture.pool(), &blank, now_s()).expect("get");
    assert_eq!(None, summary.external_id);
}

#[test]
fn a_duplicate_external_id_in_one_mailbox_is_refused() {
    let fixture = Fixture::new();
    let send = |body: &'static str| {
        fixture.store.send(&Outgoing {
            mailbox: "owner",
            body: Some(body),
            external_id: Some("upd-1"),
            ..Default::default()
        })
    };
    send("first").expect("first");
    assert!(
        send("retry").is_err(),
        "the transport's own retry must dedupe, not double-store"
    );
}

#[test]
fn the_body_ceiling_is_a_named_refusal() {
    let fixture = Fixture::new();
    let big = "x".repeat(crate::inbox::MAX_BODY_BYTES + 1);
    let error = fixture
        .store
        .send(&Outgoing {
            mailbox: "w",
            body: Some(&big),
            ..Default::default()
        })
        .expect_err("refused");
    assert!(
        error.to_string().starts_with("body is larger than"),
        "{error}"
    );
}

#[test]
fn an_empty_mailbox_label_falls_back_to_the_mailbox_like_the_python() {
    let fixture = Fixture::new();
    let label = |label: Option<&str>| -> String {
        let id = fixture
            .store
            .send(&Outgoing {
                mailbox: "w",
                body: Some("x"),
                mailbox_label: label,
                ..Default::default()
            })
            .expect("send");
        fixture
            .pool()
            .with(|c| {
                c.query_row(
                    "SELECT mailbox_label FROM messages WHERE id = ?",
                    [&id],
                    |r| r.get(0),
                )
                .map_err(map_sqlite)
            })
            .expect("label")
    };
    assert_eq!("w", label(Some("")), "Python: mailbox_label or mailbox");
    assert_eq!("w", label(None));
    assert_eq!("desk", label(Some("desk")));
}

#[test]
fn media_types_match_python_mimetypes_for_the_common_attachments() {
    // Each expected value is what Python 3 `mimetypes.guess_type` returns for a.<ext>
    // (checked 2026-10-30): `.log` and bare `.gz` are None -> octet-stream, `.tar.gz` is tar.
    for (name, expected) in [
        ("a.png", "image/png"),
        ("a.txt", "text/plain"),
        ("a.md", "text/markdown"),
        ("a.json", "application/json"),
        ("a.tar", "application/x-tar"),
        ("a.tar.gz", "application/x-tar"),
        ("a.tgz", "application/x-tar"),
        ("a.log", "application/octet-stream"),
        ("a.gz", "application/octet-stream"),
        ("noextension", "application/octet-stream"),
    ] {
        assert_eq!(expected, media_type(Path::new(name)), "{name}");
    }
}

#[test]
fn a_leftover_partial_blob_is_not_counted_as_stored() {
    let fixture = Fixture::new();
    std::fs::write(
        fixture.dir.path().join("blobs").join(".abc.partial"),
        "12345",
    )
    .expect("partial");
    assert_eq!(0, fixture.store.blob_bytes());
}

// ---- T15c: reads, deliveries, status, stats, retention -----------------------------------

fn bodies(messages: &[Message]) -> Vec<&str> {
    messages.iter().map(|m| m.body.as_str()).collect()
}

fn unread(fixture: &Fixture, mailbox: &str) -> i64 {
    inbox::count(&fixture.pool(), mailbox, &[], now_s())
        .expect("count")
        .unread
}

#[test]
fn test_send_read_and_count_in_queue_order() {
    let fixture = Fixture::new();
    for n in 0..3 {
        fixture.send("writer", &format!("message {n}"));
    }
    let counts = inbox::count(&fixture.pool(), "writer", &[], now_s()).expect("count");
    assert_eq!((3, 3), (counts.unread, counts.total));
    let first = fixture.store.read("writer", 2, false, true).expect("read");
    assert_eq!(vec!["message 0", "message 1"], bodies(&first));
    assert!(first
        .iter()
        .all(|m| m.summary.state == "read" && m.read_at.is_some()));
    assert_eq!(1, unread(&fixture, "writer"));
    let rest = fixture.store.read("writer", 10, false, true).expect("read");
    assert_eq!(vec!["message 2"], bodies(&rest));
    assert_eq!(0, unread(&fixture, "writer"));
}

#[test]
fn test_peek_does_not_consume() {
    let fixture = Fixture::new();
    fixture.send("writer", "still here");
    let peeked = fixture.store.read("writer", 10, true, true).expect("peek");
    assert_eq!(vec!["still here"], bodies(&peeked));
    assert_eq!("queued", peeked[0].summary.state);
    assert_eq!(1, unread(&fixture, "writer"));
}

#[test]
fn test_mailboxes_are_separate() {
    let fixture = Fixture::new();
    fixture.send("a", "for a");
    fixture.send("b", "for b");
    assert_eq!(
        vec!["for a"],
        bodies(&fixture.store.read("a", 10, false, true).unwrap())
    );
    assert_eq!(1, unread(&fixture, "b"));
}

#[test]
fn mark_false_defers_the_read_until_mark_read_commits_it() {
    let fixture = Fixture::new();
    let id = fixture.send("writer", "render me first");
    let shown = fixture
        .store
        .read("writer", 10, false, false)
        .expect("read");
    assert_eq!(1, shown.len());
    assert_eq!(
        1,
        unread(&fixture, "writer"),
        "nothing consumed before the commit"
    );
    let ids = vec![id.clone()];
    assert_eq!(1, fixture.store.mark_read(&ids, Some(5.0)).unwrap());
    assert_eq!(
        0,
        fixture.store.mark_read(&ids, Some(9.0)).unwrap(),
        "idempotent"
    );
    let (summary, _) = inbox::get(&fixture.pool(), &id, now_s()).unwrap();
    assert_eq!("read", summary.state);
    assert_eq!(0, fixture.store.mark_read(&[], None).unwrap());
}

#[test]
fn a_read_message_carries_its_attachment_rows() {
    let fixture = Fixture::new();
    let file = fixture.file("note.txt", "hello");
    fixture
        .store
        .send(&Outgoing {
            mailbox: "writer",
            body: Some("with file"),
            attachments: &[file],
            ..Default::default()
        })
        .unwrap();
    let read = fixture.store.read("writer", 10, true, true).unwrap();
    assert_eq!(1, read[0].summary.attachments);
    assert_eq!("note.txt", read[0].attachments[0].filename);
    assert_eq!(5, read[0].attachments[0].size);
}

#[test]
fn a_delivery_receipt_is_one_batch_per_mailbox_and_an_empty_batch_keeps_it() {
    let fixture = Fixture::new();
    let (a, b) = (fixture.send("w", "a"), fixture.send("w", "b"));
    let store = &fixture.store;
    assert_eq!(0, store.record_delivery("w", &[], None).unwrap());
    assert!(inbox::pending_delivery(&fixture.pool(), "w")
        .unwrap()
        .is_none());
    assert_eq!(
        2,
        store
            .record_delivery("w", &[a.clone(), "  ".into(), b.clone()], Some(1.0))
            .unwrap()
    );
    store.record_delivery("w", &[b.clone()], Some(2.0)).unwrap();
    assert_eq!(0, store.record_delivery("w", &[], Some(3.0)).unwrap());
    let receipt = inbox::pending_delivery(&fixture.pool(), "w")
        .unwrap()
        .expect("receipt");
    assert_eq!(vec![b], receipt.message_ids);
    assert_eq!(2.0, receipt.delivered_at);
}

#[test]
fn the_receipt_column_is_json_dumps_text_like_the_python() {
    let fixture = Fixture::new();
    let ids = vec!["i1".to_string(), "i2".to_string()];
    fixture.store.record_delivery("w", &ids, Some(1.0)).unwrap();
    let raw: String = rusqlite::Connection::open(fixture.store.path())
        .unwrap()
        .query_row(
            "SELECT message_ids FROM deliveries WHERE mailbox = 'w'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(r#"["i1", "i2"]"#, raw);
}

#[test]
fn recover_replays_the_batch_whatever_its_read_state_and_none_is_empty() {
    let fixture = Fixture::new();
    assert!(fixture.store.recover_delivery("w").unwrap().is_empty());
    let (a, b) = (fixture.send("w", "a"), fixture.send("w", "b"));
    let read = fixture.store.read("w", 10, false, true).unwrap();
    let ids: Vec<String> = read.iter().map(|m| m.summary.message_id.clone()).collect();
    fixture.store.record_delivery("w", &ids, None).unwrap();
    let again = fixture.store.recover_delivery("w").unwrap();
    assert_eq!(vec!["a", "b"], bodies(&again));
    assert!(again.iter().all(|m| m.summary.state == "read"));
    // an explicit delete of `b` shrinks the receipt instead of haunting it
    let writer = rusqlite::Connection::open(fixture.store.path()).unwrap();
    writer
        .execute("DELETE FROM messages WHERE id = ?", [&b])
        .unwrap();
    let shrunk = fixture.store.recover_delivery("w").unwrap();
    assert_eq!(vec!["a"], bodies(&shrunk));
    assert_eq!(
        vec![a.clone()],
        inbox::pending_delivery(&fixture.pool(), "w")
            .unwrap()
            .unwrap()
            .message_ids
    );
    writer
        .execute("DELETE FROM messages WHERE id = ?", [&a])
        .unwrap();
    assert!(fixture.store.recover_delivery("w").unwrap().is_empty());
    assert!(
        inbox::pending_delivery(&fixture.pool(), "w")
            .unwrap()
            .is_none(),
        "dropped"
    );
}

#[test]
fn note_acked_keeps_only_the_ids_not_yet_acked() {
    let fixture = Fixture::new();
    let (a, b) = (fixture.send("w", "a"), fixture.send("w", "b"));
    assert!(
        fixture.store.note_acked("w").unwrap().is_empty(),
        "no receipt"
    );
    fixture
        .store
        .record_delivery("w", &[a.clone(), b.clone()], None)
        .unwrap();
    let conn = rusqlite::Connection::open(fixture.store.path()).unwrap();
    conn.execute("UPDATE messages SET state = 'acked' WHERE id = ?", [&a])
        .unwrap();
    assert_eq!(vec![b.clone()], fixture.store.note_acked("w").unwrap());
    conn.execute("UPDATE messages SET state = 'acked' WHERE id = ?", [&b])
        .unwrap();
    assert!(fixture.store.note_acked("w").unwrap().is_empty());
    assert!(inbox::pending_delivery(&fixture.pool(), "w")
        .unwrap()
        .is_none());
}

#[test]
fn status_reports_state_and_sizes_and_never_the_text() {
    let fixture = Fixture::new();
    let file = fixture.file("a.bin", "12345");
    let id = fixture
        .store
        .send(&Outgoing {
            mailbox: "w",
            body: Some("secret body"),
            subject: Some("secret subject"),
            sender: Some("pm"),
            attachments: &[file],
            ..Default::default()
        })
        .unwrap();
    let status = inbox::status(&fixture.pool(), &id, now_s() + 30.0).unwrap();
    assert_eq!(
        ("queued", "w", "w"),
        (
            status.state.as_str(),
            status.mailbox.as_str(),
            status.destination.as_str()
        )
    );
    assert_eq!(11, status.body_bytes);
    assert_eq!((1, 5), (status.attachments, status.attachment_bytes));
    assert!(status.waiting_s.expect("queued waits") >= 29);
    assert!(format!("{status:?}").find("secret").is_none());
    fixture.store.read("w", 10, false, true).unwrap();
    assert_eq!(
        None,
        inbox::status(&fixture.pool(), &id, now_s())
            .unwrap()
            .waiting_s
    );
    let err = inbox::status(&fixture.pool(), "i999", now_s()).unwrap_err();
    assert!(
        err.to_string().contains("no message with id \"i999\""),
        "{err}"
    );
}

#[test]
fn test_stats_across_mailboxes() {
    let fixture = Fixture::new();
    fixture.send("one", "one");
    fixture.send("two", "two");
    fixture.send("two", "three");
    fixture.send("done", "x");
    fixture.store.read("done", 10, false, true).unwrap();
    let stats = inbox::stats(&fixture.pool(), now_s()).unwrap();
    let rows: Vec<(&str, i64, i64)> = stats
        .iter()
        .map(|s| (s.mailbox.as_str(), s.unread, s.total))
        .collect();
    assert_eq!(vec![("two", 2, 2), ("one", 1, 1), ("done", 0, 1)], rows);
    assert!(stats[2].oldest_unread_s.is_none());
    assert!(stats[0].oldest_unread_s.is_some());
}

#[test]
fn test_purge_keeps_unread_mail_and_drops_old_read_mail() {
    let fixture = Fixture::new();
    let old = fixture.send("writer", "old news");
    fixture.store.read("writer", 10, false, true).unwrap();
    fixture.send("writer", "still waiting");
    assert_eq!(1, fixture.store.purge(Some(-1.0)).unwrap());
    assert_eq!(1, unread(&fixture, "writer"));
    assert!(inbox::status(&fixture.pool(), &old, now_s()).is_err());
}

#[test]
fn purge_by_the_default_retention_keeps_recent_mail_and_collects_orphan_blobs() {
    let fixture = Fixture::new();
    let file = fixture.file("x.txt", "payload");
    fixture
        .store
        .send(&Outgoing {
            mailbox: "w",
            body: Some("b"),
            attachments: &[file],
            ..Default::default()
        })
        .unwrap();
    fixture.store.read("w", 10, false, true).unwrap();
    assert_eq!(
        0,
        fixture.store.purge(None).unwrap(),
        "read a moment ago is inside 7 days"
    );
    assert_eq!(
        1,
        std::fs::read_dir(fixture.dir.path().join("blobs"))
            .unwrap()
            .count()
    );
    assert_eq!(1, fixture.store.purge(Some(-1.0)).unwrap());
    assert_eq!(
        0,
        std::fs::read_dir(fixture.dir.path().join("blobs"))
            .unwrap()
            .count()
    );
}
