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

// -- reads through db::ReadPool ------------------------------------------------

/// A real database: the schema applied, then rows inserted directly (test setup, not
/// the store's write API - that half waits for `db::SyncWriter`, #187).
fn seeded() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("inbox.db");
    let connection = crate::db::open_writer(&path).expect("writer");
    for statement in SCHEMA {
        connection.execute_batch(statement).expect("schema");
    }
    let insert = "INSERT INTO messages (id, mailbox, mailbox_label, pane_id, sender, \
                  subject, body, state, created_at) VALUES (?,?,?,?,?,?,?,?,?)";
    // (id, mailbox, label, pane, sender, subject, body, state, created_at)
    let rows: [(
        &str,
        &str,
        &str,
        Option<&str>,
        &str,
        Option<&str>,
        &str,
        &str,
        f64,
    ); 5] = [
        (
            "i1",
            "alice",
            "alice",
            Some("%1"),
            "w1:p1",
            Some("first"),
            "hello",
            "queued",
            1_000.0,
        ),
        (
            "i2",
            "alice (w9:pH)",
            "alice (w9:pH)",
            None,
            "w1:p1",
            None,
            "second",
            "queued",
            1_100.0,
        ),
        (
            "i3", "alice", "alice", None, "w2:p2", None, "third", "read", 900.0,
        ),
        (
            "i4",
            "alice",
            "alice",
            None,
            "w2:p2",
            None,
            "gone",
            "cancelled",
            800.0,
        ),
        (
            "i5",
            "bob",
            "bob",
            None,
            "w1:p1",
            None,
            "other box",
            "queued",
            1_200.0,
        ),
    ];
    for row in rows {
        connection
            .execute(
                insert,
                rusqlite::params![row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8],
            )
            .expect("insert");
    }
    connection
        .execute(
            "INSERT INTO attachments (id, message_id, filename, media_type, size, sha256, created_at) \
             VALUES ('i2-1','i2','shot.png','image/png',10,'abc',1100.0)",
            [],
        )
        .expect("attachment");
    (dir, path)
}

#[test]
fn count_treats_a_pane_suffixed_mailbox_as_the_same_mailbox() {
    let (_dir, path) = seeded();
    let pool = crate::db::ReadPool::open(&path, 2).expect("pool");
    let counts = count(&pool, "alice", &[], 2_000.0).expect("count");
    assert_eq!(
        2, counts.unread,
        "the suffixed row must count as unread for 'alice'"
    );
    assert_eq!(
        3, counts.total,
        "cancelled mail is excluded from every number"
    );
    assert_eq!(0, counts.archived);
    assert_eq!(0, counts.undeliverable);
    assert_eq!(
        Some(1_000),
        counts.oldest_unread_s,
        "oldest unread is i1: 2000 - 1000"
    );
    // The suffixed spelling sees the same mailbox.
    let suffixed = count(&pool, "alice (w9:pH)", &[], 2_000.0).expect("count");
    assert_eq!(counts.unread, suffixed.unread);
}

#[test]
fn list_is_oldest_first_excludes_cancelled_and_can_filter_unread() {
    let (_dir, path) = seeded();
    let pool = crate::db::ReadPool::open(&path, 2).expect("pool");
    let all = list(&pool, "alice", false, 50, 2_000.0).expect("list");
    assert_eq!(
        vec!["i3".to_string(), "i1".to_string(), "i2".to_string()],
        all.iter()
            .map(|row| row.message_id.clone())
            .collect::<Vec<_>>()
    );
    let unread = list(&pool, "alice", true, 50, 2_000.0).expect("list");
    assert_eq!(2, unread.len());
    assert_eq!(Some(1_000), unread[0].waiting_s, "i1 waited 2000 - 1000");
    assert_eq!(None, all[0].waiting_s, "a read message is not waiting");
    // The limit is applied by SQL, and the attachment count comes with the row.
    let limited = list(&pool, "alice", true, 1, 2_000.0).expect("list");
    assert_eq!(1, limited.len());
    let with_attachment = list(&pool, "alice (w9:pH)", false, 50, 2_000.0).expect("list");
    assert_eq!(
        1,
        with_attachment
            .iter()
            .find(|row| row.message_id == "i2")
            .expect("i2")
            .attachments
    );
    // A different mailbox is a different inbox.
    assert_eq!(
        1,
        list(&pool, "bob", true, 50, 2_000.0).expect("list").len()
    );
}

#[test]
fn get_returns_the_body_and_a_named_refusal_for_an_unknown_id() {
    let (_dir, path) = seeded();
    let pool = crate::db::ReadPool::open(&path, 2).expect("pool");
    let (summary, body) = get(&pool, "i2", 2_000.0).expect("get");
    assert_eq!("second", body);
    assert_eq!(6, summary.body_bytes);
    assert_eq!("alice (w9:pH)", summary.destination);
    let error = get(&pool, "nope", 2_000.0).expect_err("unknown id");
    assert!(error.to_string().contains("no message with id"), "{error}");
}

// -- review t995: four blocking findings, pinned ---------------------------------

#[test]
fn mixed_case_window_ids_are_normalised_like_the_python() {
    // The live mailbox table spells panes `claude poolrad (wR:p1)`, `PM for
    // Poolrad-decomp (wN:p1)` and `PM (par-a) (wW:p1)`. A `w<digits>` rule left all
    // three UNNORMALISED, so a read addressed to the suffixed key missed the rows filed
    // under the plain name - the two-spelling rule failing in its own docstring.
    assert_eq!(
        "claude poolrad",
        normalise_mailbox("claude poolrad (wR:p1)")
    );
    assert_eq!(
        "PM for Poolrad-decomp",
        normalise_mailbox("PM for Poolrad-decomp (wN:p1)")
    );
    assert_eq!("PM (par-a)", normalise_mailbox("PM (par-a) (wW:p1)"));
    // the digit spelling keeps working, and a non-pane parenthetical is left alone
    assert_eq!(
        "Chief of Staff",
        normalise_mailbox("Chief of Staff (w9:pS)")
    );
    assert_eq!("vp of agentry", normalise_mailbox("vp of agentry"));
    assert_eq!(
        "a note (not a pane)",
        normalise_mailbox("a note (not a pane)")
    );
}

#[test]
fn count_reports_the_lane_and_attachment_numbers() {
    let (_dir, path) = seeded();
    let pool = crate::db::ReadPool::open(&path, 2).expect("pool");
    let lanes = vec!["w1:p1".to_string()];
    let counts = count(&pool, "alice", &lanes, 2_000.0).expect("count");
    // from_lanes counts QUEUED mail whose sender is a lane pane: i1 is queued from
    // w1:p1, so the fleet's "mail that bypassed the PM" number is visible at all.
    // The fixture has TWO queued rows in `alice` sent by w1:p1 (i1 and i2), and one in
    // `bob`. I asserted 1 first and the store said 2 - the store was right and my
    // arithmetic about the fixture was not, which is why the number is named here.
    assert_eq!(
        2, counts.from_lanes,
        "both lane-sent queued rows must be counted"
    );
    // i2 carries `shot.png` and i2 is queued, so the fixture has exactly ONE unread
    // attachment. I asserted 0 first: the store was right and I had not checked the
    // fixture, which is twice in this one test - hence the row is named here.
    assert_eq!(
        1, counts.attachments_unread,
        "i2 (queued) carries one attachment: shot.png"
    );
    // and with no lane list it reports 0, exactly as the Python does with lane_panes=None
    let bare = count(&pool, "alice", &[], 2_000.0).expect("count");
    assert_eq!(0, bare.from_lanes);
}

#[test]
fn body_bytes_counts_bytes_not_characters() {
    // MAX_BODY_BYTES is a BYTE ceiling, so a character count is the wrong unit in the
    // module that enforces bytes: "πππ abc" is 7 characters and 8 bytes.
    let (_dir, path) = seeded();
    let pool = crate::db::ReadPool::open(&path, 2).expect("pool");
    let (summary, body) = get(&pool, "i2", 2_000.0).expect("get");
    assert_eq!("second", body);
    assert_eq!(6, summary.body_bytes);
}

#[test]
fn get_reports_the_same_wait_as_list_for_a_queued_message() {
    // Two readers of one row disagreed: list() said 1000 s, get() said 0.
    let (_dir, path) = seeded();
    let pool = crate::db::ReadPool::open(&path, 2).expect("pool");
    let listed = list(&pool, "alice", true, 10, 2_000.0).expect("list");
    let (got, _body) = get(&pool, "i1", 2_000.0).expect("get");
    let from_list = listed
        .iter()
        .find(|row| row.message_id == "i1")
        .and_then(|row| row.waiting_s);
    assert_eq!(
        from_list, got.waiting_s,
        "get and list must agree on the wait"
    );
    assert_eq!(Some(1_000), got.waiting_s);
}

#[test]
fn the_read_half_carries_the_owner_mailbox_fields() {
    // transport / reply_to / external_id / delivered_at / delivery_transport are what
    // FLAT_FLEET 7.2 relies on; digest_id is the Python's own summary field. Dropping
    // them silently made the caller believe the column was empty.
    let (_dir, path) = seeded();
    let pool = crate::db::ReadPool::open(&path, 2).expect("pool");
    let rows = list(&pool, "alice", false, 10, 2_000.0).expect("list");
    let row = rows.first().expect("at least one row");
    // present (None in this fixture) is the point: the FIELD exists on the struct
    assert_eq!(None, row.digest_id);
}

// ---- t1113: tests for the helpers added from PR #199 (ported from its inbox_pure.rs) ----

const NOW: f64 = 1_700_000_000.0;

#[test]
fn pane_ids_are_recognised_in_both_backends_spellings() {
    // The two predicates differ ON PURPOSE (the Python has two regexes): a SUFFIX may
    // carry letters after `:p` (a tab label can be `w9:pH`), a mailbox KEY may not.
    for good in ["w9:pH", "w1:p1", "%109", "%4"] {
        assert!(is_pane_token(good), "{good} is a pane token");
    }
    for good in ["w1:p1", "%109", "%4"] {
        assert!(is_pane_id(good), "{good} is a pane-id mailbox key");
    }
    assert!(
        !is_pane_id("w9:pH"),
        "a letter after :p is not a mailbox key"
    );
    assert!(
        is_pane_token("w:p1"),
        "the permissive suffix pattern allows it"
    );
    assert!(!is_pane_id("w:p1"), "but it is not a pane-id mailbox key");
    for bad in ["", "%", "w1:p", "w1p1", "%1x", "worker-4", "vp of agentry"] {
        assert!(!is_pane_token(bad), "{bad:?} is not a pane token");
        assert!(!is_pane_id(bad), "{bad:?} is not a pane id");
    }
    assert!(is_pane_mailbox("%109"));
    assert!(
        !is_pane_mailbox(" %109 "),
        "the Python's strict regex does not tolerate padding: a padded key is a caller mistake"
    );
    assert!(!is_pane_mailbox("worker-4"));
}

#[test]
fn a_transport_is_none_web_telegram_or_report_and_nothing_else() {
    assert_eq!(Ok(None), validate_transport(None));
    assert_eq!(Ok(None), validate_transport(Some("")));
    // Python: only None and exactly '' are a lane message; whitespace is refused.
    for blank in ["  ", "\t"] {
        assert_eq!(
            Err("transport must be one of web, telegram, report; got ''".to_string()),
            validate_transport(Some(blank))
        );
    }
    assert_eq!(Ok(Some("web".to_string())), validate_transport(Some("WEB")));
    assert_eq!(
        Ok(Some("telegram".to_string())),
        validate_transport(Some(" Telegram "))
    );
    let error = validate_transport(Some("signal")).unwrap_err();
    assert!(
        error.contains("transport must be one of web, telegram, report"),
        "{error}"
    );
}

#[test]
fn retention_purges_by_last_action_and_never_unread_mail() {
    let old = NOW - RETENTION_S - 1.0;
    let day = 24.0 * 3600.0;
    assert_eq!(NOW - RETENTION_S, retention_cutoff(NOW, RETENTION_S));
    let expired = |state, created, read, acked, archived, cancelled| {
        is_expired(
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
    assert!(check_sizes(10, "a.txt", 10, 10).is_ok());
    let body = check_sizes(MAX_BODY_BYTES + 1, "", 0, 0).unwrap_err();
    assert_eq!(format!("body is larger than {MAX_BODY_BYTES} bytes"), body);
    let attachment = check_sizes(0, "a.bin", MAX_ATTACHMENT_BYTES + 1, 0).unwrap_err();
    assert_eq!(
        format!(
            "a.bin is {} bytes, over the {MAX_ATTACHMENT_BYTES} byte attachment limit",
            MAX_ATTACHMENT_BYTES + 1
        ),
        attachment
    );
    let blob = check_sizes(0, "", 0, MAX_BLOB_BYTES + 1).unwrap_err();
    assert_eq!("attachment store is full; delete some mail first", blob);
}

#[test]
fn the_unknown_transport_text_quotes_like_the_python() {
    let error = validate_transport(Some("signal")).unwrap_err();
    assert_eq!(
        "transport must be one of web, telegram, report; got 'signal'",
        error
    );
}

// -- the sweep's unread gate (t1053; dunk_inbox.py::Inbox.gate_unread) -----------

/// A database holding only the given (id, sender, state, created_at) rows for `w`.
fn gate_db(rows: &[(&str, Option<&str>, &str, f64)]) -> (tempfile::TempDir, crate::db::ReadPool) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("inbox.db");
    let connection = crate::db::open_writer(&path).expect("writer");
    for statement in SCHEMA {
        connection.execute_batch(statement).expect("schema");
    }
    for (id, sender, state, created_at) in rows {
        connection
            .execute(
                "INSERT INTO messages (id, mailbox, mailbox_label, sender, body, state, created_at) \
                 VALUES (?, 'w', 'w', ?, 'x', ?, ?)",
                rusqlite::params![id, sender, state, created_at],
            )
            .expect("insert");
    }
    drop(connection);
    let pool = crate::db::ReadPool::open(&path, 1).expect("pool");
    (dir, pool)
}

#[test]
fn gate_ignores_daemon_announcements() {
    // NULL and empty-string senders are both "the daemon"; five of them gate nothing.
    let rows: Vec<(String, Option<&str>)> = (0..5)
        .map(|n| (format!("d{n}"), if n % 2 == 0 { None } else { Some("") }))
        .collect();
    let seeded: Vec<(&str, Option<&str>, &str, f64)> = rows
        .iter()
        .map(|(id, sender)| (id.as_str(), *sender, "queued", 1_000.0))
        .collect();
    let (_dir, pool) = gate_db(&seeded);
    let gate = gate_unread(&pool, "w", 600.0, None, 1_000.0).expect("gate");
    assert_eq!(
        Gate {
            fresh: 0,
            expired: 0
        },
        gate
    );
}

#[test]
fn gate_counts_fresh_lane_mail_then_lets_it_expire() {
    let (_dir, pool) = gate_db(&[("m1", Some("%5"), "queued", 1_000.0)]);
    assert_eq!(
        1,
        gate_unread(&pool, "w", 600.0, None, 1_000.0)
            .expect("gate")
            .fresh
    );
    // 601 s later the same mail is past the window: nothing fresh, one expired
    let gate = gate_unread(&pool, "w", 600.0, None, 1_601.0).expect("gate");
    assert_eq!(
        Gate {
            fresh: 0,
            expired: 1
        },
        gate
    );
    // the window is inclusive at exactly 600 s, as `t >= cutoff` is in the Python
    let edge = gate_unread(&pool, "w", 600.0, None, 1_600.0).expect("gate");
    assert_eq!(
        Gate {
            fresh: 1,
            expired: 0
        },
        edge
    );
}

#[test]
fn gate_ignores_mail_older_than_last_turn_end() {
    let (_dir, pool) = gate_db(&[("m1", Some("%5"), "queued", 1_000.0)]);
    let gate = gate_unread(&pool, "w", 600.0, Some(1_005.0), 1_000.0).expect("gate");
    assert_eq!(
        Gate {
            fresh: 0,
            expired: 0
        },
        gate
    );
    // strictly newer than `since`: mail created AT the turn end was already seen
    let at = gate_unread(&pool, "w", 600.0, Some(1_000.0), 1_000.0).expect("gate");
    assert_eq!(
        Gate {
            fresh: 0,
            expired: 0
        },
        at
    );
}

#[test]
fn gate_counts_only_queued_mail() {
    let (_dir, pool) = gate_db(&[
        ("m1", Some("%5"), "read", 1_000.0),
        ("m2", Some("%5"), "cancelled", 1_000.0),
        ("m3", Some("%5"), "queued", 1_000.0),
    ]);
    assert_eq!(
        Gate {
            fresh: 1,
            expired: 0
        },
        gate_unread(&pool, "w", 600.0, None, 1_000.0).expect("gate")
    );
}

#[test]
fn the_sweep_gate_feeds_a_lane_whose_only_mail_expired() {
    // daemon-only mail fed, fresh lane mail gates, expired mail fed (the task's three cases)
    let (_dir, daemon_only) = gate_db(&[("d1", None, "queued", 1_000.0)]);
    assert_eq!(
        (0, false),
        sweep_gate(&daemon_only, "w", None, 1_000.0).expect("gate")
    );

    let (_dir2, lane) = gate_db(&[("m1", Some("%5"), "queued", 1_000.0)]);
    assert_eq!(
        (1, false),
        sweep_gate(&lane, "w", None, 1_100.0).expect("gate")
    );
    assert_eq!(
        (0, true),
        sweep_gate(&lane, "w", None, 1_601.0).expect("gate")
    );

    // fresh mail beside expired mail still gates, and the pane is NOT reported expired
    let (_dir3, mixed) = gate_db(&[
        ("m1", Some("%5"), "queued", 1_000.0),
        ("m2", Some("%5"), "queued", 1_590.0),
    ]);
    assert_eq!(
        (1, false),
        sweep_gate(&mixed, "w", None, 1_601.0).expect("gate")
    );
}

#[test]
fn mailboxes_for_pane_lists_each_name_a_pane_owns_once() {
    let (_dir, path) = seeded();
    // i1 is the only seeded row filed under pane %1 (mailbox 'alice'); the rest carry no pane.
    let writer = crate::db::open_writer(&path).expect("writer");
    writer
        .execute(
            "INSERT INTO messages (id, mailbox, mailbox_label, pane_id, sender, body, state, created_at) \
             VALUES ('i9','crackdown-codex','crackdown-codex','%1','w1:p1','later','read',1200.0)",
            [],
        )
        .expect("a second mailbox for the same pane");
    writer
        .execute(
            "INSERT INTO messages (id, mailbox, mailbox_label, pane_id, sender, body, state, created_at) \
             VALUES ('i10','alice','alice','%1','w1:p1','again','read',1300.0)",
            [],
        )
        .expect("a repeat of an existing mailbox");
    drop(writer);
    let pool = crate::db::ReadPool::open(&path, 2).expect("pool");
    assert_eq!(
        mailboxes_for_pane(&pool, "%1").expect("query"),
        ["alice", "crackdown-codex"]
    );
    assert!(
        mailboxes_for_pane(&pool, "%404").expect("query").is_empty(),
        "an unknown pane owns nothing"
    );
}
