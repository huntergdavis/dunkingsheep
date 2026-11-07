//! t665: an owner page is an owner-mailbox row the Telegram outbox carries, and nothing else.
//! A page has no reply_to, so the outbox must select it by its sender, not by a reply link.

use super::Fixture;
use crate::inbox_store::Outgoing;

/// The row `OwnerConversations::page` writes: owner mailbox, telegram transport, owner-page
/// sender, no reply link. The dedupe key is the producer's external id.
fn page(fixture: &Fixture, body: &str, external_id: Option<&str>) -> String {
    fixture.send_outgoing(&Outgoing {
        mailbox: "owner",
        body: Some(body),
        subject: Some("PAGE OWNER"),
        sender: Some("owner-page"),
        transport: Some("telegram"),
        external_id,
        ..Default::default()
    })
}

fn telegram_outbox(fixture: &Fixture) -> Vec<String> {
    fixture
        .store
        .owner_outbox(Some("telegram"), Some(50))
        .expect("telegram outbox")
        .iter()
        .map(|message| message.summary.message_id.clone())
        .collect()
}

#[test]
fn a_page_waits_in_the_telegram_outbox_until_its_receipt_then_leaves_it() {
    let fixture = Fixture::new();
    let id = page(
        &fixture,
        "PAGE OWNER security: a credential-shaped line",
        Some("page:key-1"),
    );
    assert_eq!(vec![id.clone()], telegram_outbox(&fixture));

    fixture
        .store
        .mark_delivered(&id, Some("telegram"), None)
        .expect("the telegram receipt for the page");
    assert!(
        telegram_outbox(&fixture).is_empty(),
        "a delivered page must not be sent to the phone twice"
    );
}

#[test]
fn only_pages_and_replies_reach_telegram_never_a_human_question_or_another_transports_page() {
    let fixture = Fixture::new();
    let page_id = page(&fixture, "PAGE OWNER outage: the desk is down", None);
    // A question the owner typed on Telegram: no sender, no reply link. It must not be
    // handed back to the phone as if it were an answer.
    fixture.send_outgoing(&Outgoing {
        mailbox: "owner",
        body: Some("human question"),
        transport: Some("telegram"),
        ..Default::default()
    });
    // The same sender on the web transport is a web page, not a telegram one.
    fixture.send_outgoing(&Outgoing {
        mailbox: "owner",
        body: Some("web page"),
        sender: Some("owner-page"),
        transport: Some("web"),
        ..Default::default()
    });
    assert_eq!(vec![page_id], telegram_outbox(&fixture));
}

#[test]
fn a_producers_repeated_external_id_finds_the_page_it_already_wrote() {
    let fixture = Fixture::new();
    let first = page(
        &fixture,
        "PAGE OWNER money: spend over budget",
        Some("page:key-2"),
    );
    let found = fixture
        .store
        .find_by_external_id("owner", Some("page:key-2"))
        .expect("the lookup by dedupe key")
        .expect("the page written under that key");
    assert_eq!(first, found.summary.message_id);
}
