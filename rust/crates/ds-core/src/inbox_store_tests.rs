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
