//! Port of `tests/test_telegram_transport.py` (t594): what Telegram may carry, and how it is sent.
//! Pure: no network, no token, no desk.

use ds_telegram::policy::{
    chunks, deliver, is_page, outbox_items, page_class, pages_allowed, Item, Row, Sent,
    KIND_MAILBOX_PAGE, KIND_PAGE, KIND_REPLY, PAGE_MAX_PER_HOUR, PAGE_WINDOW_S,
};
use ds_telegram::BridgeError;

const NOW: f64 = 1000.0;

fn row(message_id: &str, body: &str, kind: &str) -> Row {
    Row {
        message_id: message_id.into(),
        body: body.into(),
        kind: kind.into(),
        id: None,
    }
}

fn items_for(replies: &[Row], pages: &[Row], sent_at: &[f64], now: f64) -> Vec<Item> {
    outbox_items(
        replies,
        pages,
        sent_at,
        now,
        PAGE_MAX_PER_HOUR,
        PAGE_WINDOW_S,
    )
}

/// What `run` returns: the delivery result, the bodies sent, and the `(message_id, kind)` receipts.
type Outcome = (
    Result<Vec<Sent>, BridgeError>,
    Vec<String>,
    Vec<(String, String)>,
);

/// Deliver with a sender that records bodies and a receipt log.
fn run(items: &[Item], fail_first: bool) -> Outcome {
    let mut bodies = Vec::new();
    let mut receipts = Vec::new();
    let mut failed = fail_first;
    let result = deliver(
        items,
        |body| {
            if failed {
                failed = false;
                return Err(BridgeError::Runtime("Telegram sendMessage: 502".into()));
            }
            bodies.push(body.to_string());
            Ok(())
        },
        |id, kind| {
            receipts.push((id.to_string(), kind.to_string()));
            Ok(())
        },
    );
    (result, bodies, receipts)
}

#[test]
fn a_web_question_produces_nothing_for_telegram() {
    let items = items_for(&[], &[], &[], NOW);
    assert!(items.is_empty());
    let (result, bodies, receipts) = run(&items, false);
    assert_eq!(result.unwrap(), vec![]);
    assert!(bodies.is_empty());
    assert!(receipts.is_empty());
}

#[test]
fn a_telegram_reply_is_sent_and_receipted() {
    let replies = [row("m9", "the answer", KIND_REPLY)];
    let items = items_for(&replies, &[], &[], NOW);
    let (result, bodies, receipts) = run(&items, false);
    let sent = result.unwrap();
    assert_eq!(bodies, vec!["the answer".to_string()]);
    assert_eq!(receipts, vec![("m9".to_string(), KIND_REPLY.to_string())]);
    assert_eq!(sent[0].kind, KIND_REPLY);
}

#[test]
fn a_brief_or_digest_is_never_selected() {
    for body in ["BRIEF #42 ...", "DIGEST ...", "web conversation echo"] {
        assert!(!is_page(body), "{body}");
    }
    // Nothing reads a brief channel: the only inputs are the outbox and the page rows.
    let items = items_for(&[], &[], &[], NOW);
    let (_, bodies, _) = run(&items, false);
    assert!(bodies.is_empty());
}

#[test]
fn pages_are_sent_and_classed() {
    let pages = [row(
        "m10",
        "PAGE OWNER outage: web desk down",
        KIND_MAILBOX_PAGE,
    )];
    let items = items_for(&[], &pages, &[], NOW);
    assert_eq!(items[0].page_class.as_deref(), Some("outage"));
    let (result, bodies, receipts) = run(&items, false);
    result.unwrap();
    assert_eq!(bodies.len(), 1);
    assert_eq!(
        receipts,
        vec![("m10".to_string(), KIND_MAILBOX_PAGE.to_string())]
    );
}

#[test]
fn the_fourth_page_in_an_hour_is_held_not_dropped() {
    let now = 10_000.0;
    let already = [now - 60.0, now - 120.0, now - 180.0];
    let items = items_for(
        &[],
        &[row("m11", "PAGE OWNER security: x", KIND_PAGE)],
        &already,
        now,
    );
    assert!(items[0].hold);
    assert!(
        items[0].reason.contains("3 pages per hour"),
        "{}",
        items[0].reason
    );
    let (result, bodies, _) = run(&items, false);
    result.unwrap();
    assert!(bodies.is_empty(), "a held page is not sent");
}

#[test]
fn a_page_older_than_the_window_does_not_count_against_the_budget() {
    let now = 10_000.0;
    let already = [now - 4000.0, now - 5000.0, now - 6000.0];
    let items = items_for(
        &[],
        &[row("m12", "PAGE OWNER money: cap", KIND_PAGE)],
        &already,
        now,
    );
    assert!(!items[0].hold);
}

#[test]
fn a_failed_send_stops_the_batch_and_records_no_receipt() {
    let replies = [
        row("m13", "first", KIND_REPLY),
        row("m14", "second", KIND_REPLY),
    ];
    let items = items_for(&replies, &[], &[], NOW);
    let (result, bodies, receipts) = run(&items, true);
    assert!(result.is_err());
    assert!(
        receipts.is_empty(),
        "no receipt for a send that did not happen"
    );
    assert!(bodies.is_empty());
}

#[test]
fn the_budget_counts_only_timestamps_inside_the_window() {
    assert_eq!(pages_allowed(&[], NOW, 3, PAGE_WINDOW_S), 3);
    assert_eq!(
        pages_allowed(&[NOW - 10.0, NOW - 20.0], NOW, 3, PAGE_WINDOW_S),
        1
    );
    assert_eq!(pages_allowed(&[NOW - 7200.0; 5], NOW, 3, PAGE_WINDOW_S), 3);
    assert_eq!(pages_allowed(&[NOW; 9], NOW, 3, PAGE_WINDOW_S), 0);
}

#[test]
fn page_classes_follow_declaration_order() {
    assert_eq!(page_class("PAGE OWNER security outage"), Some("security"));
    assert_eq!(page_class("money and outage"), Some("outage"));
    assert_eq!(page_class("nothing named"), None);
}

#[test]
fn chunks_pass_a_short_body_through_and_drop_an_empty_one() {
    assert_eq!(chunks("hello", 3900), vec!["hello".to_string()]);
    assert!(chunks("", 3900).is_empty());
}

#[test]
fn chunks_cut_at_a_newline_in_the_second_half_of_the_window() {
    let body = format!("{}\n{}", "a".repeat(2000), "b".repeat(3000));
    let parts = chunks(&body, 3900);
    assert_eq!(parts, vec!["a".repeat(2000), "b".repeat(3000)]);
}

#[test]
fn chunks_ignore_a_newline_in_the_first_half_and_cut_at_the_limit() {
    let body = format!("{}\n{}", "a".repeat(100), "b".repeat(5000));
    let parts = chunks(&body, 3900);
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].chars().count(), 3900);
    // 5101 characters in all: the remainder after a cut at 3900 is 1201.
    assert_eq!(parts[1].chars().count(), 1201);
}

#[test]
fn chunks_count_characters_not_bytes() {
    let body = "é".repeat(4000);
    let parts = chunks(&body, 3900);
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0].chars().count(), 3900);
    assert_eq!(parts[1].chars().count(), 100);
}

#[test]
fn chunks_drop_the_leading_newlines_of_the_remainder() {
    // The cut is at the LAST newline in the window (index 2002), so the first part keeps the two
    // newlines before it, exactly as Python's body[:cut] does; the remainder loses its leading one.
    let body = format!("{}\n\n\n{}", "a".repeat(2000), "b".repeat(2500));
    let parts = chunks(&body, 3900);
    assert_eq!(
        parts,
        vec![format!("{}\n\n", "a".repeat(2000)), "b".repeat(2500)]
    );
}

#[test]
fn chunks_of_a_long_text_join_back_to_the_text_without_its_cut_newlines() {
    let body = "x".repeat(9000);
    let parts = chunks(&body, 3900);
    assert_eq!(
        parts.iter().map(String::len).collect::<Vec<_>>(),
        vec![3900, 3900, 1200]
    );
    assert_eq!(parts.concat(), body);
}
