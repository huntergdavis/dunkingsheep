//! `inbox` slice tests (T15): the schema against the fixture, and the pure
//! predicates the SQL is built from. The store half (send/list/count through
//! `db::SyncWriter`) is deliberately NOT here - it needs #187 merged.

use super::*;

/// The fixture T00 snapshots from the live database.
const SCHEMA_FIXTURE: &str = include_str!("../../../fixtures/state/schema.json");

#[test]
fn the_schema_is_verbatim_from_the_fixture() {
    // The port must not drift from the schema the daemon actually writes: every
    // statement here has to appear in the snapshot, in the same order, byte for byte.
    let fixture: serde_json::Value = serde_json::from_str(SCHEMA_FIXTURE).expect("fixture json");
    let inbox = fixture["inbox"].as_array().expect("inbox schema list");
    let fixture_statements: Vec<&str> = inbox.iter().filter_map(|value| value.as_str()).collect();
    assert_eq!(
        fixture_statements,
        SCHEMA.to_vec(),
        "the schema differs from rust/fixtures/state/schema.json"
    );
}

#[test]
fn the_schema_covers_the_five_tables_and_their_indexes() {
    let joined = SCHEMA.join("\n");
    for table in ["messages", "attachments", "counters", "deliveries"] {
        assert!(
            joined.contains(&format!("CREATE TABLE {table}")),
            "missing table {table}"
        );
    }
    assert!(
        joined.contains("messages_by_mailbox"),
        "missing mailbox index"
    );
    assert!(joined.contains("messages_by_state"), "missing state index");
}

#[test]
fn a_pane_suffixed_mailbox_is_the_same_mailbox() {
    // The incident: mailbox 'X (w9:pH)', caller 'X', and the VP's own mail was
    // refused because the guard compared raw strings.
    assert_eq!("vp of agentry", normalise_mailbox("vp of agentry (w9:pH)"));
    assert_eq!("vp of agentry", normalise_mailbox("vp of agentry"));
    assert_eq!(
        "vp of agentry",
        normalise_mailbox("  vp of agentry (w9:pH)  ")
    );
    assert_eq!("alice", normalise_mailbox("alice (%4)"));
    assert_eq!("alice", normalise_mailbox("alice (%4) "));
}

#[test]
fn a_label_that_merely_contains_brackets_stays_intact() {
    // Only a TRAILING PANE ID is stripped - not any parenthetical.
    assert_eq!(
        "par-d (second shift)",
        normalise_mailbox("par-d (second shift)")
    );
    assert_eq!("pool (b-side)", normalise_mailbox("pool (b-side)"));
    assert_eq!(
        "x (w9:p)",
        normalise_mailbox("x (w9:p)"),
        "no digits after w"
    );
    assert_eq!("x (p4)", normalise_mailbox("x (p4)"), "no window prefix");
}

#[test]
fn the_where_clause_matches_both_spellings_and_escapes_the_like() {
    let (sql, params) = mailbox_where("alice", "mailbox");
    assert_eq!("(mailbox = ? OR mailbox LIKE ? ESCAPE '\\')", sql);
    assert_eq!(vec!["alice".to_string(), "alice (%".to_string()], params);

    // The suffixed and the plain spelling produce the SAME predicate.
    let (_, suffixed) = mailbox_where("alice (w9:pH)", "mailbox");
    assert_eq!(params, suffixed);

    // A mailbox whose name contains LIKE wildcards cannot widen its own match.
    let (_, escaped) = mailbox_where("a%b_c", "mailbox");
    assert_eq!(
        vec!["a%b_c".to_string(), "a\\%b\\_c (%".to_string()],
        escaped
    );
}

#[test]
fn the_where_clause_takes_the_column_it_is_given() {
    let (sql, _) = mailbox_where("alice", "mailbox_label");
    assert!(sql.contains("mailbox_label = ?"), "{sql}");
}

#[test]
fn unread_mail_is_never_purged_by_age() {
    // The rule lives in the SQL, so it is asserted on the SQL: `state != 'queued'`
    // is what keeps a month-old unread message alive.
    assert!(
        PURGE_SELECTION.contains("state != 'queued'"),
        "{PURGE_SELECTION}"
    );
    assert_eq!("queued", UNREAD);
    assert!(
        PURGE_SELECTION
            .contains("COALESCE(acked_at, cancelled_at, read_at, archived_at, created_at)"),
        "the age must be decided in that order: {PURGE_SELECTION}"
    );
}

#[test]
fn the_retention_cutoff_is_now_minus_seven_days() {
    assert_eq!(604_800.0, RETENTION_S);
    assert_eq!(
        1_000.0 - RETENTION_S,
        retention_cutoff(1_000.0, RETENTION_S)
    );
    // An explicit override is the Python's `older_than_s` argument.
    assert_eq!(900.0, retention_cutoff(1_000.0, 100.0));
}

#[test]
fn message_numbers_are_stable_and_start_at_one() {
    assert_eq!(1, next_message_number(None));
    assert_eq!(2, next_message_number(Some(1)));
    assert_eq!(4_001, next_message_number(Some(4_000)));
    // The upsert is what makes the counter survive a restart, beside the messages.
    assert!(
        COUNTER_UPSERT.contains("ON CONFLICT(name) DO UPDATE"),
        "{COUNTER_UPSERT}"
    );
    assert!(COUNTER_UPSERT.contains("'message'"), "{COUNTER_UPSERT}");
}

#[test]
fn the_size_ceilings_are_the_pythons() {
    assert_eq!(64 * 1024 * 1024, MAX_ATTACHMENT_BYTES);
    assert_eq!(2 * 1024 * 1024 * 1024, MAX_BLOB_BYTES);
    assert_eq!(8 * 1024 * 1024, MAX_BODY_BYTES);
    assert_eq!(
        vec![
            "queued",
            "read",
            "acked",
            "cancelled",
            "archived",
            "undeliverable"
        ],
        STATES.to_vec()
    );
}
