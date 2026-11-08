//! t3700: the owner-mailbox channel's routing of owner pages. A critical emergency page is recorded
//! with the daemon's `owner_page` verb (the Telegram outbox and the owner mailbox), and is confirmed
//! only by the outbox receipt. A routine escalation is one inbox write, confirmed on post.

use super::owner_mailbox::OwnerMailbox;
use super::run::OwnerTransport;
use crate::client::{FlockClient, Transport};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::time::Duration;

/// A daemon that records each request and answers every request with `reply`.
struct PageDaemon {
    calls: RefCell<Vec<Value>>,
    reply: Value,
}

impl Transport for &PageDaemon {
    fn exchange(&self, line: &str, _: Duration) -> std::io::Result<Option<String>> {
        self.calls
            .borrow_mut()
            .push(serde_json::from_str(line).unwrap());
        Ok(Some(json!({"ok": true, "result": self.reply}).to_string()))
    }
}

fn channel_over(daemon: &'static PageDaemon) -> OwnerMailbox<&'static PageDaemon> {
    OwnerMailbox::new(move || {
        Ok(FlockClient::new(
            daemon,
            Some("fake-pane".into()),
            String::new(),
            Duration::from_secs(1),
            None,
        ))
    })
}

fn daemon(reply: Value) -> &'static PageDaemon {
    Box::leak(Box::new(PageDaemon {
        calls: RefCell::new(vec![]),
        reply,
    }))
}

#[test]
fn an_emergency_page_is_an_owner_page_with_its_class_and_its_number_is_the_page_id() {
    let daemon = daemon(json!({"message": {"message_id": "i42"}}));
    let number = channel_over(daemon)
        .post_page(true, "security", "PAGE OWNER security: token exposed")
        .unwrap();
    assert_eq!(number, 42);
    let calls = daemon.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["cmd"], "owner_page");
    assert_eq!(calls[0]["args"]["class"], "security");
    assert_eq!(
        calls[0]["args"]["body"],
        "PAGE OWNER security: token exposed"
    );
}

#[test]
fn a_routine_escalation_is_one_inbox_write_and_never_an_owner_page() {
    let daemon = daemon(json!({"message_id": "i7"}));
    let number = channel_over(daemon)
        .post_page(false, "security", "ESCALATION routine")
        .unwrap();
    assert_eq!(number, 0);
    let calls = daemon.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["cmd"], "inbox_send");
    assert!(calls.iter().all(|call| call["cmd"] != "owner_page"));
}

#[test]
fn only_an_emergency_page_waits_for_the_outbox_receipt() {
    let channel = channel_over(daemon(json!({})));
    assert!(
        channel.confirms_on_post_for(false),
        "routine: confirmed on post"
    );
    assert!(
        !channel.confirms_on_post_for(true),
        "emergency: waits for the receipt"
    );
}

#[test]
fn the_confirmed_through_number_comes_from_the_outbox_receipt() {
    let daemon = daemon(json!({"through": 42}));
    let through = channel_over(daemon).confirmed_through().unwrap();
    assert_eq!(through, Some(json!(42)));
    assert_eq!(daemon.calls.borrow()[0]["cmd"], "owner_page_confirmed");
}

#[test]
fn no_receipt_means_no_confirmed_page_yet() {
    let through = channel_over(daemon(json!({"through": null})))
        .confirmed_through()
        .unwrap();
    assert_eq!(through, None);
}
