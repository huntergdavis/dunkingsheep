//! Tests for the owner-mailbox escalation channel (t3624 / t3625): one inbox_send to the owner as
//! the watchdog, confirmed on post, never a telegram transport; the desk brief keeps failing closed;
//! and the enum accepts `owner-mailbox` only.

use super::owner_mailbox::{OwnerMailbox, OWNER_MAILBOX, OWNER_MAILBOX_SUBJECT};
use super::run::{owner_page_entry, OwnerTransport};
use crate::client::{FlockClient, Transport};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::time::Duration;

/// A daemon that records each request line. `fail` simulates a write that times out.
struct FakeDaemon {
    calls: RefCell<Vec<Value>>,
    fail: bool,
}

impl Transport for &FakeDaemon {
    fn exchange(&self, line: &str, _: Duration) -> std::io::Result<Option<String>> {
        self.calls
            .borrow_mut()
            .push(serde_json::from_str(line).unwrap());
        if self.fail {
            Err(std::io::Error::from(std::io::ErrorKind::TimedOut))
        } else {
            Ok(Some(
                json!({"ok": true, "result": {"id": "i1"}}).to_string(),
            ))
        }
    }
}

/// A channel over a daemon that lives for the whole test (the connector must be `'static`).
fn channel_over(daemon: &'static FakeDaemon) -> OwnerMailbox<&'static FakeDaemon> {
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

#[test]
fn an_owner_escalation_is_one_inbox_send_to_the_owner_as_the_watchdog() {
    let daemon: &'static FakeDaemon = Box::leak(Box::new(FakeDaemon {
        calls: RefCell::new(vec![]),
        fail: false,
    }));
    let channel = channel_over(daemon);
    assert_eq!(channel.desk_channel().unwrap(), "owner-mailbox");
    assert!(channel.confirms_on_post());
    assert_eq!(channel.post("ESCALATION body").unwrap(), 0);

    let calls = daemon.calls.borrow();
    assert_eq!(calls.len(), 1, "exactly one daemon write");
    assert_eq!(calls[0]["cmd"], "inbox_send");
    assert_eq!(
        calls[0]["args"],
        json!({
            "to": OWNER_MAILBOX,
            "subject": OWNER_MAILBOX_SUBJECT,
            "body": "ESCALATION body",
            "system_sender": "watchdog"
        })
    );
    // Never a telegram transport or a page verb: the phone is not reachable from here.
    assert!(calls[0]["args"].get("transport").is_none());
    assert_ne!(calls[0]["cmd"], "owner_page");
}

#[test]
fn a_refused_owner_write_is_one_named_send_error_and_is_not_retried() {
    let daemon: &'static FakeDaemon = Box::leak(Box::new(FakeDaemon {
        calls: RefCell::new(vec![]),
        fail: true,
    }));
    let error = channel_over(daemon).post("body").unwrap_err();
    assert_eq!(error.kind, "ClientError");
    assert_eq!(
        daemon.calls.borrow().len(),
        1,
        "a failed write is never retried"
    );
}

#[test]
fn a_confirm_on_post_page_is_confirmed_at_once_and_a_desk_page_waits() {
    let confirmed = owner_page_entry(0, 100.0, "owner-mailbox", true);
    assert_eq!(confirmed["confirmed_at"], json!(100.0));
    assert_eq!(confirmed["desk_channel"], "owner-mailbox");

    let waiting = owner_page_entry(1065, 100.0, "cos", false);
    assert!(
        waiting["confirmed_at"].is_null(),
        "the desk brief waits for its forwarder"
    );
    assert_eq!(waiting["brief_id"], json!(1065));
}

#[test]
fn the_channel_enum_accepts_owner_mailbox_and_refuses_anything_else() {
    let mut config: Value = serde_json::from_str(include_str!(
        "../../../../../packaging/watchdog.example.json"
    ))
    .unwrap();
    config["escalation"]["owner"]["channel"] = json!("owner-mailbox");
    assert!(super::config::validate_config(&mut config).is_ok());
    config["escalation"]["owner"]["channel"] = json!("telegram");
    let error = super::config::validate_config(&mut config).unwrap_err();
    assert!(
        error.to_string().contains("escalation.owner.channel"),
        "{error}"
    );
}
