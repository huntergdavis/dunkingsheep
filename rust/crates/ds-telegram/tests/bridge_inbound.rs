//! The inbound side of `telegram_bridge.py`: pairing, the private-chat gate, text questions into the
//! owner mailbox, the fixed replies, the poll offset, and the routing refusals. Fakes only.

mod common;

use common::{paired_config, text_update, Fixture};
use ds_telegram::{BridgeError, Config};
use serde_json::{json, Value};

fn as_config(value: Value) -> Config {
    match value {
        Value::Object(map) => map,
        _ => panic!("config must be an object"),
    }
}

fn unpaired() -> Config {
    let mut config = paired_config(0);
    config["chat_id"] = Value::Null;
    as_config(config)
}

const CONNECTED: &str = "Chief of Staff connected. Send text, a photo, or a file. Replies will appear here. /status checks your last send.";
const HELP: &str = "Send a message, photo, or file directly to Chief of Staff. /status checks your last send. Files over 20 MB use the web desk.";
const OTHER: &str =
    "I can receive text, photos, and files. Other Telegram items need the web desk.";

#[test]
fn a_pairing_code_from_a_private_chat_connects_the_chat() {
    let fixture = Fixture::new();
    let mut bridge = fixture.bridge();
    let mut config = unpaired();
    bridge
        .process_update(&text_update(1, 42, "/pair PAIRCODE"), &mut config)
        .unwrap();
    assert_eq!(config["chat_id"], json!(42));
    assert_eq!(config["brief_channel"], json!("cos"));
    assert_eq!(fixture.bot.texts(), vec![CONNECTED.to_string()]);
    assert_eq!(
        fixture.read_config()["chat_id"],
        json!(42),
        "the pairing is saved, not only held in memory"
    );
}

#[test]
fn a_wrong_pairing_code_connects_nothing_and_says_nothing() {
    let fixture = Fixture::new();
    let mut bridge = fixture.bridge();
    let mut config = unpaired();
    bridge
        .process_update(&text_update(1, 42, "/pair WRONGCODE"), &mut config)
        .unwrap();
    assert!(config["chat_id"].is_null());
    assert_eq!(fixture.bot.sent_count(), 0);
}

#[test]
fn a_text_question_is_one_owner_mailbox_row_with_its_telegram_id() {
    let fixture = Fixture::new();
    fixture.daemon.0.borrow_mut().ingest =
        json!({"message": {"message_id": "i7"}, "duplicate": false});
    let mut bridge = fixture.bridge();
    let mut config = as_config(paired_config(42));
    bridge
        .process_update(&text_update(7, 42, "is the desk up?"), &mut config)
        .unwrap();
    let ingest = fixture.daemon.args_of("owner_ingest");
    assert_eq!(ingest.len(), 1);
    assert_eq!(ingest[0]["body"], json!("is the desk up?"));
    assert_eq!(ingest[0]["transport"], json!("telegram"));
    assert_eq!(ingest[0]["external_id"], json!("telegram:7"));
    assert!(ingest[0]["subject"].is_null());
    assert_eq!(
        fixture.bot.texts(),
        vec!["Chief of Staff message #i7. I'll reply here when ready.".to_string()]
    );
}

#[test]
fn a_redelivered_update_says_it_is_already_recorded() {
    let fixture = Fixture::new();
    fixture.daemon.0.borrow_mut().ingest =
        json!({"message": {"message_id": "i7"}, "duplicate": true});
    let mut bridge = fixture.bridge();
    let mut config = as_config(paired_config(42));
    bridge
        .process_update(&text_update(7, 42, "again"), &mut config)
        .unwrap();
    assert_eq!(
        fixture.bot.texts(),
        vec![
            "Chief of Staff message #i7 (already recorded). I'll reply here when ready."
                .to_string()
        ]
    );
}

#[test]
fn the_desk_name_comes_from_the_routing_channel_and_the_env_override() {
    let fixture = Fixture::new();
    fixture.daemon.0.borrow_mut().ingest =
        json!({"message": {"message_id": "i8"}, "duplicate": false});
    let mut bridge = fixture.bridge();
    bridge.env_channel = Some("vp".into());
    let mut config = as_config(paired_config(42));
    bridge
        .process_update(&text_update(8, 42, "hello"), &mut config)
        .unwrap();
    assert_eq!(
        fixture.bot.texts(),
        vec!["VP message #i8. I'll reply here when ready.".to_string()]
    );
}

#[test]
fn a_group_chat_and_another_chat_are_ignored() {
    let fixture = Fixture::new();
    let mut bridge = fixture.bridge();
    let mut config = as_config(paired_config(42));
    let group =
        json!({"update_id": 1, "message": {"chat": {"id": 42, "type": "group"}, "text": "hi"}});
    bridge.process_update(&group, &mut config).unwrap();
    bridge
        .process_update(&text_update(2, 99, "hi"), &mut config)
        .unwrap();
    assert_eq!(fixture.bot.sent_count(), 0);
    assert!(fixture.daemon.args_of("owner_ingest").is_empty());
}

#[test]
fn start_and_help_get_the_help_text_and_record_nothing() {
    let fixture = Fixture::new();
    let mut bridge = fixture.bridge();
    let mut config = as_config(paired_config(42));
    bridge
        .process_update(&text_update(3, 42, "/start"), &mut config)
        .unwrap();
    bridge
        .process_update(&text_update(4, 42, " /help "), &mut config)
        .unwrap();
    assert_eq!(
        fixture.bot.texts(),
        vec![HELP.to_string(), HELP.to_string()]
    );
    assert!(fixture.daemon.args_of("owner_ingest").is_empty());
}

#[test]
fn a_sticker_with_no_text_gets_the_other_items_reply() {
    let fixture = Fixture::new();
    let mut bridge = fixture.bridge();
    let mut config = as_config(paired_config(42));
    let sticker =
        json!({"update_id": 5, "message": {"chat": {"id": 42, "type": "private"}, "sticker": {}}});
    bridge.process_update(&sticker, &mut config).unwrap();
    assert_eq!(fixture.bot.texts(), vec![OTHER.to_string()]);
    assert!(fixture.daemon.args_of("owner_ingest").is_empty());
}

#[test]
fn a_file_is_not_recorded_by_this_build_and_says_so() {
    // Attachments are a named gap of this build (see the PR body): a file gets a clear reply and no
    // mailbox row, never a silent drop.
    let fixture = Fixture::new();
    let mut bridge = fixture.bridge();
    let mut config = as_config(paired_config(42));
    let photo = json!({
        "update_id": 6,
        "message": {"chat": {"id": 42, "type": "private"}, "caption": "look", "photo": [{"file_id": "f", "file_size": 9}]}
    });
    bridge.process_update(&photo, &mut config).unwrap();
    assert_eq!(
        fixture.bot.texts(),
        vec!["Telegram attachments are not in the Rust bridge yet. Send the text, or use the web desk.".to_string()]
    );
    assert!(fixture.daemon.args_of("owner_ingest").is_empty());
}

#[test]
fn status_is_named_as_not_in_this_build() {
    let fixture = Fixture::new();
    let mut bridge = fixture.bridge();
    let mut config = as_config(paired_config(42));
    bridge
        .process_update(&text_update(9, 42, "/status"), &mut config)
        .unwrap();
    assert_eq!(
        fixture.bot.texts(),
        vec![
            "/status is not in the Rust bridge yet. Check the web desk for your last send."
                .to_string()
        ]
    );
}

#[test]
fn a_config_that_still_names_a_channel_is_refused_with_the_move_message() {
    let fixture = Fixture::new();
    let mut bridge = fixture.bridge();
    let mut config = as_config(paired_config(42));
    config.insert("channel".into(), json!("vp"));
    let error = bridge
        .process_update(&text_update(1, 42, "hi"), &mut config)
        .unwrap_err();
    assert_eq!(
        error,
        BridgeError::Value(
            "Move Telegram channel from credential config to telegram-routing.json".into()
        )
    );
}

#[test]
fn a_routing_file_with_an_extra_key_is_refused() {
    let fixture = Fixture::new();
    fixture.write_routing(r#"{"channel":"cos","extra":1}"#);
    let mut bridge = fixture.bridge();
    let mut config = as_config(paired_config(42));
    let error = bridge
        .process_update(&text_update(1, 42, "hi"), &mut config)
        .unwrap_err();
    assert_eq!(
        error,
        BridgeError::Value("Telegram routing config must contain only channel".into())
    );
}

#[test]
fn a_routing_channel_missing_from_the_registry_is_refused() {
    let fixture = Fixture::new();
    fixture.write_routing(r#"{"channel":"nope"}"#);
    let mut bridge = fixture.bridge();
    let mut config = as_config(paired_config(42));
    let error = bridge
        .process_update(&text_update(1, 42, "hi"), &mut config)
        .unwrap_err();
    assert_eq!(
        error,
        BridgeError::Value("Telegram channel must name a configured desk channel".into())
    );
}

#[test]
fn a_tick_polls_with_the_saved_offset_and_advances_it_past_each_update() {
    let fixture = Fixture::new();
    fixture.daemon.0.borrow_mut().ingest =
        json!({"message": {"message_id": "i1"}, "duplicate": false});
    fixture.bot.0.borrow_mut().updates = vec![text_update(7, 42, "one"), text_update(8, 42, "two")];
    fixture.write_config(&paired_config(42));
    let mut bridge = fixture.bridge();
    let mut config = as_config(paired_config(42));
    config.insert("update_offset".into(), json!(7));
    bridge.tick(&mut config).unwrap();
    let polls = fixture.bot.0.borrow().poll_payloads.clone();
    assert_eq!(polls[0]["offset"], json!(7));
    assert_eq!(polls[0]["timeout"], json!(10));
    assert_eq!(polls[0]["allowed_updates"], json!(["message"]));
    assert_eq!(
        config["update_offset"],
        json!(9),
        "the offset moves past the last update"
    );
    assert_eq!(
        fixture.read_config()["update_offset"],
        json!(9),
        "and is saved after each one"
    );
    assert_eq!(fixture.daemon.args_of("owner_ingest").len(), 2);
}

#[test]
fn a_first_poll_sends_no_offset_at_all() {
    let fixture = Fixture::new();
    let mut bridge = fixture.bridge();
    let mut config = as_config(paired_config(42));
    config.insert("update_offset".into(), Value::Null);
    bridge.tick(&mut config).unwrap();
    let polls = fixture.bot.0.borrow().poll_payloads.clone();
    assert!(!polls[0].contains_key("offset"));
}
