//! T15a: the pure half of the message store, ported from tests/test_dunk_inbox.py.

use ds_core::inbox::{self, MAX_ATTACHMENT_BYTES, MAX_BODY_BYTES, RETENTION_S, STATES, TRANSPORTS};

const NOW: f64 = 1_700_000_000.0;

#[test]
fn the_limits_and_vocabulary_are_the_pythons() {
    assert_eq!(7.0 * 24.0 * 3600.0, RETENTION_S);
    assert_eq!(64 * 1024 * 1024, MAX_ATTACHMENT_BYTES);
    assert_eq!(8 * 1024 * 1024, MAX_BODY_BYTES);
    assert_eq!(["web", "telegram", "report"], TRANSPORTS);
    assert_eq!(
        [
            "queued",
            "read",
            "acked",
            "cancelled",
            "archived",
            "undeliverable"
        ],
        STATES
    );
    assert!(inbox::SCHEMA.contains("CREATE TABLE IF NOT EXISTS messages"));
    for column in [
        "mailbox",
        "transport",
        "reply_to",
        "external_id",
        "delivered_at",
        "delivery_transport",
    ] {
        assert!(
            inbox::SCHEMA.contains(column),
            "the messages table lost {column}"
        );
    }
}

#[test]
fn a_trailing_pane_id_parenthetical_is_the_same_mailbox() {
    // 'vp of agentry (w9:pH)' and 'vp of agentry' are ONE mailbox: the table carries
    // both spellings, and comparing raw strings made the ack guard refuse the VP's mail.
    assert_eq!(
        Some("vp of agentry".to_string()),
        inbox::normalise_mailbox(Some("vp of agentry (w9:pH)"))
    );
    assert_eq!(
        Some("vp of agentry".to_string()),
        inbox::normalise_mailbox(Some("vp of agentry"))
    );
    assert_eq!(
        Some("worker-4".to_string()),
        inbox::normalise_mailbox(Some("worker-4 (%109)"))
    );
    assert_eq!(
        Some("worker-4".to_string()),
        inbox::normalise_mailbox(Some("worker-4(%109)"))
    );
}

#[test]
fn only_a_pane_id_parenthetical_is_stripped_never_a_label_with_brackets() {
    // A label that merely contains brackets stays intact.
    assert_eq!(
        Some("the desk (pm)".to_string()),
        inbox::normalise_mailbox(Some("the desk (pm)"))
    );
    assert_eq!(
        Some("a (b) c".to_string()),
        inbox::normalise_mailbox(Some("a (b) c"))
    );
    // A pane id NOT at the end is not a suffix.
    assert_eq!(
        Some("x (%1) y".to_string()),
        inbox::normalise_mailbox(Some("x (%1) y"))
    );
    assert_eq!(None, inbox::normalise_mailbox(None));
}

#[test]
fn the_mailbox_predicate_matches_both_spellings_and_escapes_wildcards() {
    let (sql, params) = inbox::mailbox_where("vp of agentry (w9:pH)", "mailbox");
    assert_eq!("(mailbox = ? OR mailbox LIKE ? ESCAPE '\\')", sql);
    assert_eq!(
        "vp of agentry", params[0],
        "the plain spelling is compared exactly"
    );
    assert_eq!(
        "vp of agentry (%", params[1],
        "the suffixed spelling is the LIKE arm"
    );
    // A mailbox with wildcards must not match more than itself: the LIKE arm escapes
    // them, while the exact-equality arm compares the raw name (SQL `=` has no
    // wildcards, which is why the Python escapes only the second parameter).
    let (_sql, params) = inbox::mailbox_where("a_b%c", "mailbox");
    assert_eq!("a_b%c", params[0], "the exact arm needs no escaping");
    assert_eq!("a\\_b\\%c (%", params[1], "the LIKE arm escapes _ and %");
}

#[test]
fn pane_ids_are_recognised_in_both_backends_spellings() {
    // The two predicates differ ON PURPOSE (the Python has two regexes): a SUFFIX may
    // carry letters after `:p` (a tab label can be `w9:pH`), a mailbox KEY may not.
    for good in ["w9:pH", "w1:p1", "%109", "%4"] {
        assert!(inbox::is_pane_token(good), "{good} is a pane token");
    }
    for good in ["w1:p1", "%109", "%4"] {
        assert!(inbox::is_pane_id(good), "{good} is a pane-id mailbox key");
    }
    assert!(
        !inbox::is_pane_id("w9:pH"),
        "a letter after :p is not a mailbox key"
    );
    assert!(
        inbox::is_pane_token("w:p1"),
        "the permissive suffix pattern allows it"
    );
    assert!(
        !inbox::is_pane_id("w:p1"),
        "but it is not a pane-id mailbox key"
    );
    for bad in ["", "%", "w1:p", "w1p1", "%1x", "worker-4", "vp of agentry"] {
        assert!(!inbox::is_pane_token(bad), "{bad:?} is not a pane token");
        assert!(!inbox::is_pane_id(bad), "{bad:?} is not a pane id");
    }
    assert!(inbox::is_pane_mailbox("%109"));
    assert!(
        !inbox::is_pane_mailbox(" %109 "),
        "the Python's strict regex does not tolerate padding: a padded key is a caller mistake"
    );
    assert!(!inbox::is_pane_mailbox("worker-4"));
}

#[test]
fn a_transport_is_none_web_telegram_or_report_and_nothing_else() {
    assert_eq!(Ok(None), inbox::validate_transport(None));
    assert_eq!(Ok(None), inbox::validate_transport(Some("  ")));
    assert_eq!(
        Ok(Some("web".to_string())),
        inbox::validate_transport(Some("WEB"))
    );
    assert_eq!(
        Ok(Some("telegram".to_string())),
        inbox::validate_transport(Some(" Telegram "))
    );
    let error = inbox::validate_transport(Some("signal")).unwrap_err();
    assert!(
        error.contains("transport must be one of web, telegram, report"),
        "{error}"
    );
}

#[test]
fn an_external_id_is_the_transport_and_the_id() {
    // The dedupe key for a redelivered update: same transport + same update id = one row.
    assert_eq!("telegram:4411", inbox::external_id("telegram", "4411"));
    assert_eq!("telegram:4411", inbox::external_id("telegram", " 4411 "));
}

#[test]
fn retention_purges_by_last_action_and_never_unread_mail() {
    let old = NOW - RETENTION_S - 1.0;
    let day = 24.0 * 3600.0;
    assert_eq!(NOW - RETENTION_S, inbox::retention_cutoff(NOW, RETENTION_S));
    let expired = |state, created, read, acked, archived, cancelled| {
        inbox::is_expired(
            state,
            created,
            read,
            acked,
            archived,
            cancelled,
            NOW,
            RETENTION_S,
        )
    };
    // an unread row 8 days old is NEVER purged by age
    assert!(!expired("queued", NOW - 8.0 * day, None, None, None, None));
    // read 8 days ago: expired; created 8 days ago but read yesterday: not
    assert!(expired("read", old, Some(old), None, None, None));
    assert!(!expired(
        "read",
        NOW - 8.0 * day,
        Some(NOW - day),
        None,
        None,
        None
    ));
    // COALESCE order is acked, cancelled, read, archived, created
    assert!(!expired(
        "acked",
        old,
        Some(old),
        Some(NOW - day),
        None,
        None
    ));
    assert!(expired("archived", old, None, None, Some(old), None));
    assert!(expired("undeliverable", old, None, None, None, None));
    // exactly at the cutoff is not yet expired
    assert!(!expired("read", NOW - RETENTION_S, None, None, None, None));
}

#[test]
fn a_size_over_a_limit_is_a_named_refusal_not_a_truncation() {
    assert!(inbox::check_sizes(10, "a.txt", 10, 10).is_ok());
    let body = inbox::check_sizes(MAX_BODY_BYTES + 1, "", 0, 0).unwrap_err();
    assert_eq!(format!("body is larger than {MAX_BODY_BYTES} bytes"), body);
    let attachment = inbox::check_sizes(0, "a.bin", MAX_ATTACHMENT_BYTES + 1, 0).unwrap_err();
    assert_eq!(
        format!(
            "a.bin is {} bytes, over the {MAX_ATTACHMENT_BYTES} byte attachment limit",
            MAX_ATTACHMENT_BYTES + 1
        ),
        attachment
    );
    let blob = inbox::check_sizes(0, "", 0, inbox::MAX_BLOB_BYTES + 1).unwrap_err();
    assert_eq!("attachment store is full; delete some mail first", blob);
}

#[test]
fn the_unknown_transport_text_quotes_like_the_python() {
    let error = inbox::validate_transport(Some("signal")).unwrap_err();
    assert_eq!(
        "transport must be one of web, telegram, report; got 'signal'",
        error
    );
}

/// The schema is compared to the live database's fixture by COLUMN NAME: the 23 in
/// SCHEMA plus the 3 `_migrate` adds must be exactly the 26 the fixture holds.
#[test]
fn the_schema_columns_are_the_fixtures_columns() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../../fixtures/state/schema.json"))
            .expect("schema.json");
    let table = fixture["inbox"]
        .as_array()
        .expect("inbox statements")
        .iter()
        .filter_map(|s| s.as_str())
        .find(|s| s.starts_with("CREATE TABLE messages"))
        .expect("messages table");
    let columns = |text: &str| -> std::collections::BTreeSet<String> {
        let body = &text[text.find('(').expect("(") + 1..text.rfind(')').expect(")")];
        body.split(',')
            .filter_map(|part| part.split_whitespace().next())
            .map(String::from)
            .collect()
    };
    let mut ours = columns(inbox::SCHEMA.trim_end_matches(';'));
    ours.remove("IF");
    for (name, _) in inbox::MIGRATED_COLUMNS {
        ours.insert(name.to_string());
    }
    let theirs = columns(table);
    assert_eq!(26, theirs.len());
    assert_eq!(theirs, ours);
}
