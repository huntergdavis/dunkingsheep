//! t665 piece-2 gate: the `owner_page` verb records owner pages the Telegram outbox carries.
//!
//! These run through the same dispatcher, role table and temporary stores as the owner
//! delivery tests in `tests.rs`, so the role gate and the argument checks are the real ones.
use super::*;

#[test]
fn owner_page_records_a_telegram_page_and_repeats_idempotently() {
    let (_dir, flock, env) = owner_api_fixture("owner-page");
    let dispatcher = owner_view_dispatcher();
    let page_args = || {
        args(json!({
            "body": "PAGE OWNER security: token exposed",
            "external_id": "page:key-1"
        }))
    };

    let first = dispatch_owner_api(
        &dispatcher,
        &flock,
        &env,
        "owner_page",
        page_args(),
        Some("w1:p1"),
    )
    .expect("owner page via dispatcher");
    assert_eq!(first["duplicate"], json!(false));
    assert_eq!(first["transport"], json!("telegram"));
    assert_eq!(first["message"]["sender"], json!("owner-page"));
    assert_eq!(first["message"]["subject"], json!("PAGE OWNER"));
    assert_eq!(first["message"]["transport"], json!("telegram"));
    assert_eq!(first["message"]["external_id"], json!("page:key-1"));
    assert!(
        first["message"]["reply_to"].is_null(),
        "a page is not a reply"
    );
    let message_id = first["message"]["message_id"].clone();

    let again = dispatch_owner_api(
        &dispatcher,
        &flock,
        &env,
        "owner_page",
        page_args(),
        Some("w1:p1"),
    )
    .expect("the repeated producer key");
    assert_eq!(again["duplicate"], json!(true));
    assert_eq!(again["message"]["message_id"], message_id);

    let outbox = dispatch_owner_api(
        &dispatcher,
        &flock,
        &env,
        "owner_outbox",
        args(json!({"transport": "telegram"})),
        Some("w1:p1"),
    )
    .expect("telegram outbox");
    let waiting: Vec<Value> = outbox["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .map(|message| message["message_id"].clone())
        .collect();
    assert_eq!(
        waiting,
        vec![message_id.clone()],
        "the page waits for its receipt"
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
    let after = dispatch_owner_api(
        &dispatcher,
        &flock,
        &env,
        "owner_outbox",
        args(json!({"transport": "telegram"})),
        Some("w1:p1"),
    )
    .expect("telegram outbox after the receipt");
    assert!(
        after["messages"].as_array().expect("messages").is_empty(),
        "a delivered page is not sent twice"
    );
}

#[test]
fn owner_page_is_refused_for_a_worker_and_for_an_empty_body() {
    let (_dir, flock, env) = owner_api_fixture("owner-page-refuse");
    let dispatcher = owner_view_dispatcher();

    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_page",
            args(json!({"body": "PAGE OWNER money: spend cap"})),
            Some("w1:p2"),
        ),
        Err(DunkError::Refused(message)) if message.contains("not permitted for role worker")
    ));

    assert!(matches!(
        dispatch_owner_api(
            &dispatcher,
            &flock,
            &env,
            "owner_page",
            args(json!({"body": "   "})),
            Some("w1:p1"),
        ),
        Err(DunkError::Refused(message)) if message.contains("needs a body")
    ));
}
