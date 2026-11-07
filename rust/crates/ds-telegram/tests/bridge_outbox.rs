//! Ports of `tests/test_telegram_mailbox_pages.py` (t665) and `tests/test_telegram_page_watermark.py`
//! (t667, t678, t685): the outbox loop, the page budget and the legacy watermark, end to end through
//! the public `send_owner_outbox`, with fakes for Telegram, the daemon and the desk store.

mod common;

use common::{outbox_row, Fixture, NOW};
use serde_json::{json, Value};

/// The desk store of the watermark suite: pages 11, 12 and 14, an ordinary row at 13.
fn watermark_rows(fixture: &Fixture) {
    fixture.desk_rows(&[
        (11, "cos", "agent", "PAGE OWNER outage: web desk down"),
        (12, "cos", "agent", "PAGE OWNER money: provider cap hit"),
        (13, "cos", "agent", "an ordinary agent row, not a page"),
        (14, "cos", "agent", "PAGE OWNER security: token exposed"),
    ]);
}

/// Append rows to an existing desk store, for the test that adds pages mid-scenario.
fn append_rows(fixture: &Fixture, rows: &[(i64, &str, &str, &str)]) {
    let conn = rusqlite::Connection::open(fixture.paths.desk_db()).expect("desk db");
    for (id, channel, role, body) in rows {
        conn.execute(
            "INSERT INTO messages (id, channel, role, body) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![id, channel, role, body],
        )
        .expect("row");
    }
}

/// A paired config with the cursor scoped to `cos` (so no route reset happens), plus `extra`.
fn config_with(extra: Value) -> ds_telegram::Config {
    let mut config = common::paired_config(42);
    if let (Value::Object(base), Value::Object(more)) = (&mut config, extra) {
        for (key, value) in more {
            base.insert(key, value);
        }
    }
    match config {
        Value::Object(map) => map,
        _ => unreachable!(),
    }
}

// ---- owner mailbox pages and replies (test_telegram_mailbox_pages.py) ----

#[test]
fn a_mailbox_page_is_sent_once_and_receipted_by_its_message_id() {
    let fixture = Fixture::new();
    fixture.daemon.0.borrow_mut().outbox = vec![outbox_row(
        "i90",
        Some("owner-page"),
        "PAGE OWNER security: token exposed",
        None,
    )];
    let mut bridge = fixture.bridge();
    let mut config = config_with(json!({"last_page_id": 5}));
    bridge.send_owner_outbox(&mut config).unwrap();
    assert_eq!(
        fixture.bot.texts(),
        vec!["PAGE OWNER security: token exposed".to_string()]
    );
    assert_eq!(fixture.daemon.receipts(), vec!["i90".to_string()]);
}

#[test]
fn a_reply_is_still_sent_as_a_reply_and_receipted() {
    let fixture = Fixture::new();
    fixture.daemon.0.borrow_mut().outbox = vec![outbox_row(
        "i91",
        None,
        "Answer to your question.",
        Some("q1"),
    )];
    let mut bridge = fixture.bridge();
    let mut config = config_with(json!({"last_page_id": 5}));
    let sent = bridge.send_owner_outbox(&mut config).unwrap();
    assert_eq!(
        fixture.bot.texts(),
        vec!["Answer to your question.".to_string()]
    );
    assert_eq!(fixture.daemon.receipts(), vec!["i91".to_string()]);
    assert_eq!(sent[0].kind, "reply");
}

#[test]
fn mailbox_pages_share_the_hourly_budget_and_the_held_one_gets_no_receipt() {
    let fixture = Fixture::new();
    fixture.daemon.0.borrow_mut().outbox = (1..=4)
        .map(|n| {
            outbox_row(
                &format!("i{n}"),
                Some("owner-page"),
                &format!("PAGE OWNER outage: incident {n}"),
                None,
            )
        })
        .collect();
    let mut bridge = fixture.bridge();
    let mut config = config_with(json!({"last_page_id": 5}));
    bridge.send_owner_outbox(&mut config).unwrap();
    assert_eq!(
        fixture.bot.sent_count(),
        3,
        "three pages per hour, the fourth is held"
    );
    assert_eq!(fixture.daemon.receipts(), vec!["i1", "i2", "i3"]);
    assert_eq!(
        config["page_sent_at"].as_array().unwrap().len(),
        3,
        "each delivered page spends the budget"
    );
}

#[test]
fn a_mailbox_page_does_not_move_the_legacy_page_watermark() {
    let fixture = Fixture::new();
    fixture.daemon.0.borrow_mut().outbox = vec![outbox_row(
        "i90",
        Some("owner-page"),
        "PAGE OWNER money: spend cap",
        None,
    )];
    let mut bridge = fixture.bridge();
    let mut config = config_with(json!({"last_page_id": 5}));
    bridge.send_owner_outbox(&mut config).unwrap();
    assert_eq!(config["last_page_id"], json!(5));
}

// ---- the legacy watermark (test_telegram_page_watermark.py) ----

#[test]
fn a_held_page_is_selected_again_on_the_next_tick() {
    let fixture = Fixture::new();
    watermark_rows(&fixture);
    let mut bridge = fixture.bridge();
    // Three pages in the window: the budget is spent, so every page read now is held.
    let mut config = config_with(json!({"last_page_id": 10, "page_sent_at": [NOW, NOW, NOW]}));
    bridge.send_owner_outbox(&mut config).unwrap();
    assert_eq!(fixture.bot.sent_count(), 0, "a held page must not be sent");
    assert_eq!(
        config["last_page_id"],
        json!(10),
        "a held page advanced the watermark"
    );
    // The window clears; the same pages are offered again.
    config.insert("page_sent_at".into(), json!([]));
    bridge.send_owner_outbox(&mut config).unwrap();
    assert_eq!(
        fixture.bot.sent_count(),
        3,
        "the held pages were not offered again"
    );
}

#[test]
fn a_delivered_page_moves_the_watermark_and_the_next_one_follows() {
    let fixture = Fixture::new();
    watermark_rows(&fixture);
    let mut bridge = fixture.bridge();
    let mut config = config_with(json!({"last_page_id": 10}));
    bridge.send_owner_outbox(&mut config).unwrap();
    assert_eq!(fixture.bot.sent_count(), 3);
    assert_eq!(config["last_page_id"], json!(14));
    bridge.send_owner_outbox(&mut config).unwrap();
    assert_eq!(fixture.bot.sent_count(), 3, "delivered pages came back");
}

#[test]
fn a_failed_send_does_not_advance_past_the_page_that_failed() {
    let fixture = Fixture::new();
    watermark_rows(&fixture);
    fixture.bot.0.borrow_mut().fail_on = Some("money".into());
    let mut bridge = fixture.bridge();
    let mut config = config_with(json!({"last_page_id": 10}));
    assert!(bridge.send_owner_outbox(&mut config).is_err());
    assert_eq!(
        config["last_page_id"],
        json!(11),
        "the batch stopped at 12, so 12 must stay unread"
    );
    // Once the transport recovers, the failed page and the one after it are the pending ones.
    fixture.bot.0.borrow_mut().fail_on = None;
    bridge.send_owner_outbox(&mut config).unwrap();
    assert_eq!(fixture.bot.sent_count(), 1 + 2, "page 11, then 12 and 14");
}

#[test]
fn pages_delivered_in_one_tick_each_spend_the_hourly_budget() {
    let fixture = Fixture::new();
    watermark_rows(&fixture);
    let mut bridge = fixture.bridge();
    let mut config = config_with(json!({"last_page_id": 10}));
    bridge.send_owner_outbox(&mut config).unwrap();
    assert_eq!(fixture.bot.sent_count(), 3);
    assert_eq!(config["page_sent_at"].as_array().unwrap().len(), 3);
    append_rows(
        &fixture,
        &[
            (15, "cos", "agent", "PAGE OWNER outage: a fourth page"),
            (16, "cos", "agent", "PAGE OWNER money: a fifth page"),
        ],
    );
    bridge.send_owner_outbox(&mut config).unwrap();
    assert_eq!(
        fixture.bot.sent_count(),
        3,
        "the budget was spent in tick 1: tick 2 must hold"
    );
    assert_eq!(
        config["last_page_id"],
        json!(14),
        "held pages must stay unread"
    );
}

#[test]
fn a_page_older_than_the_window_does_not_count_after_a_tick() {
    let fixture = Fixture::new();
    watermark_rows(&fixture);
    let old = NOW - 2.0 * 3600.0;
    let mut bridge = fixture.bridge();
    let mut config = config_with(json!({"last_page_id": 10, "page_sent_at": [old, old]}));
    bridge.send_owner_outbox(&mut config).unwrap();
    assert_eq!(
        fixture.bot.sent_count(),
        3,
        "stale entries must not spend the budget"
    );
    assert_eq!(
        config["page_sent_at"].as_array().unwrap().len(),
        3,
        "stale entries are pruned"
    );
}

#[test]
fn a_first_run_carries_the_old_watermark_over() {
    let fixture = Fixture::new();
    watermark_rows(&fixture);
    let mut bridge = fixture.bridge();
    let mut config = config_with(json!({"last_brief_id": 13}));
    bridge.send_owner_outbox(&mut config).unwrap();
    assert_eq!(
        fixture.bot.texts(),
        vec!["PAGE OWNER security: token exposed".to_string()]
    );
    assert_eq!(config["last_page_id"], json!(14));
}

#[test]
fn a_first_run_with_no_marker_starts_from_now_not_from_history() {
    let fixture = Fixture::new();
    watermark_rows(&fixture);
    let mut bridge = fixture.bridge();
    let mut config = config_with(json!({}));
    bridge.send_owner_outbox(&mut config).unwrap();
    assert_eq!(config["last_page_id"], json!(14));
    assert_eq!(
        fixture.bot.sent_count(),
        0,
        "the first tick paged history to the phone"
    );
}

#[test]
fn a_page_is_not_starved_by_ordinary_rows_ahead_of_it() {
    let fixture = Fixture::new();
    let mut rows: Vec<(i64, &str, &str, String)> = (0..25)
        .map(|n| {
            (
                100 + n,
                "cos",
                "agent",
                format!("an ordinary agent row {n}"),
            )
        })
        .collect();
    rows.push((
        200,
        "cos",
        "agent",
        "PAGE OWNER security: token exposed".into(),
    ));
    let owned: Vec<(i64, &str, &str, &str)> = rows
        .iter()
        .map(|(id, channel, role, body)| (*id, *channel, *role, body.as_str()))
        .collect();
    fixture.desk_rows(&owned);
    let mut bridge = fixture.bridge();
    let mut config = config_with(json!({"last_page_id": 10}));
    bridge.send_owner_outbox(&mut config).unwrap();
    assert_eq!(
        fixture.bot.texts(),
        vec!["PAGE OWNER security: token exposed".to_string()],
        "25 ordinary rows starved the page"
    );
    assert_eq!(config["last_page_id"], json!(200));
}

#[test]
fn ordinary_rows_never_move_the_watermark() {
    let fixture = Fixture::new();
    fixture.desk_rows(&[
        (30, "cos", "agent", "a report"),
        (31, "cos", "agent", "a web echo"),
        (32, "cos", "agent", "PAGE OWNER outage: desk down"),
    ]);
    let mut bridge = fixture.bridge();
    let mut config = config_with(json!({"last_page_id": 10}));
    bridge.send_owner_outbox(&mut config).unwrap();
    assert_eq!(config["last_page_id"], json!(32));
    assert_eq!(fixture.bot.sent_count(), 1);
}

#[test]
fn an_unpaired_bridge_sends_nothing_from_the_outbox() {
    let fixture = Fixture::new();
    fixture.daemon.0.borrow_mut().outbox = vec![outbox_row("i1", None, "answer", Some("q"))];
    let mut bridge = fixture.bridge();
    let mut config = config_with(json!({"chat_id": null}));
    let sent = bridge.send_owner_outbox(&mut config).unwrap();
    assert!(sent.is_empty());
    assert!(
        fixture.daemon.commands().is_empty(),
        "no daemon call before pairing"
    );
}

#[test]
fn the_daemon_is_asked_for_the_telegram_outbox_only() {
    let fixture = Fixture::new();
    let mut bridge = fixture.bridge();
    let mut config = config_with(json!({"last_page_id": 10}));
    bridge.send_owner_outbox(&mut config).unwrap();
    let asked = fixture.daemon.args_of("owner_outbox");
    assert_eq!(asked.len(), 1);
    assert_eq!(asked[0]["transport"], json!("telegram"));
}
