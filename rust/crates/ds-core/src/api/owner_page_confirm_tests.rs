//! t665 piece 2: the watchdog's confirmation read (`owner_page_confirmed`) and the page number
//! the watchdog compares. Through the same dispatcher and role table as the owner page gate.
use super::*;
use crate::owner::conversations::owner_page_number;

/// The page verb plus the outbox and receipt handlers these tests read back.
fn confirm_dispatcher() -> Dispatcher {
    let mut dispatcher = owner_view_dispatcher();
    dispatcher.register_owner_reply_ingest_outbox();
    dispatcher
}

#[test]
fn a_page_number_is_the_digits_of_its_mailbox_id() {
    assert_eq!(owner_page_number("i47710"), Some(47710));
    assert_eq!(
        owner_page_number("47710"),
        None,
        "a row id without the mailbox prefix"
    );
    assert_eq!(owner_page_number("ix"), None);
    assert_eq!(owner_page_number(""), None);
}

#[test]
fn the_watchdog_is_confirmed_only_by_a_delivered_page() {
    let (_dir, flock, env) = owner_api_fixture("owner-page-confirm");
    let dispatcher = confirm_dispatcher();
    let confirmed = |dispatcher: &Dispatcher| {
        dispatch_owner_api(
            dispatcher,
            &flock,
            &env,
            "owner_page_confirmed",
            args(json!({})),
            Some("w1:p1"),
        )
        .expect("the confirmation read")
    };

    assert!(
        confirmed(&dispatcher)["through"].is_null(),
        "nothing delivered yet"
    );

    let page = dispatch_owner_api(
        &dispatcher,
        &flock,
        &env,
        "owner_page",
        args(json!({"body": "PAGE OWNER outage: web desk down"})),
        Some("w1:p1"),
    )
    .expect("owner page");
    let message_id = page["message"]["message_id"].clone();
    let number = owner_page_number(message_id.as_str().expect("message id")).expect("page number");
    assert!(
        confirmed(&dispatcher)["through"].is_null(),
        "a page waiting in the outbox never confirms"
    );

    dispatch_owner_api(
        &dispatcher,
        &flock,
        &env,
        "owner_delivered",
        args(json!({"message_id": message_id, "transport": "telegram"})),
        Some("w1:p1"),
    )
    .expect("the telegram receipt for the page");
    let after = confirmed(&dispatcher);
    assert_eq!(after["through"], json!(number));
    assert_eq!(after["transport"], json!("telegram"));
}

#[test]
fn a_worker_cannot_read_the_confirmation() {
    let (_dir, flock, env) = owner_api_fixture("owner-page-confirm-refuse");
    let dispatcher = confirm_dispatcher();
    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_page_confirmed",
            args(json!({})),
            Some("w1:p2"),
        ),
        Err(DunkError::Refused(message)) if message.contains("not permitted for role worker")
    ));
}
