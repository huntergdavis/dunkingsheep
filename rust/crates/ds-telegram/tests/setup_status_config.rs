//! `setup`, `status`, the 0600 state file, the channel registry and the bot token's secrecy.
//! The token used here is a fake string; nothing is sent anywhere.

mod common;

use common::{paired_config, Fixture};
use ds_telegram::bridge::{setup, status_line, token_urlsafe};
use ds_telegram::config::{load_config, save_config, Paths};
use ds_telegram::{BridgeError, Config};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;

const FAKE_TOKEN: &str = "123456:FAKE-TOKEN-FOR-TESTS";

#[test]
fn setup_refuses_when_the_bridge_is_already_configured() {
    let fixture = Fixture::new();
    fixture.write_config(&paired_config(42));
    let result = setup(
        &fixture.paths,
        || panic!("the token must not be read when already configured"),
        |_| Ok(common::FakeBot::default()),
    );
    assert_eq!(
        result.unwrap_err(),
        BridgeError::Runtime(
            "Telegram is already configured; inspect ./telegram-bridge status".into()
        )
    );
}

#[test]
fn setup_refuses_a_token_with_whitespace_inside_it() {
    let fixture = Fixture::new();
    let result = setup(
        &fixture.paths,
        || Ok("12 34:bad".to_string()),
        |_| Ok(common::FakeBot::default()),
    );
    assert_eq!(
        result.unwrap_err(),
        BridgeError::Runtime("Token must be a single nonempty value".into())
    );
}

#[test]
fn setup_checks_the_token_and_saves_a_pairing_code_without_printing_the_token() {
    let fixture = Fixture::new();
    let bot = common::FakeBot::default();
    bot.0.borrow_mut().get_me = json!({"username": "hunter_test_bot"});
    let bot_for_factory = bot.clone();
    let output = setup(
        &fixture.paths,
        || Ok(format!("  {FAKE_TOKEN}\n")),
        move |token| {
            assert_eq!(token, FAKE_TOKEN, "the stripped token reaches the client");
            Ok(bot_for_factory.clone())
        },
    )
    .unwrap();
    assert!(output.starts_with("Bot @hunter_test_bot saved in private local data.\n"));
    assert!(output.contains("then DM the bot: /pair "));
    assert!(
        !output.contains(FAKE_TOKEN),
        "the bot token must never be printed"
    );
    let config = fixture.read_config();
    assert_eq!(config["bot_token"], json!(FAKE_TOKEN));
    assert!(config["chat_id"].is_null());
    let code = config["pair_code"].as_str().unwrap();
    assert_eq!(code.len(), 24, "token_urlsafe(18) is 24 characters");
    assert!(output.ends_with(code));
    assert_eq!(bot.0.borrow().calls, vec!["getMe".to_string()]);
}

#[test]
fn the_state_file_is_private_to_its_owner() {
    let fixture = Fixture::new();
    let config = common::paired_config(42);
    fixture.write_config(&config);
    let as_map: Config = match config {
        Value::Object(map) => map,
        _ => unreachable!(),
    };
    save_config(&fixture.paths, &as_map).unwrap();
    let mode = std::fs::metadata(fixture.paths.config())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "telegram.json holds the bot token");
}

#[test]
fn a_missing_config_is_the_setup_message() {
    let fixture = Fixture::new();
    assert_eq!(
        load_config(&fixture.paths).unwrap_err(),
        BridgeError::Runtime("Run ./telegram-bridge setup first".into())
    );
}

#[test]
fn status_of_a_fresh_config_shows_the_defaults_the_python_shows() {
    let fixture = Fixture::new();
    fixture.write_config(&json!({"bot_token": FAKE_TOKEN, "pair_code": "x", "chat_id": null}));
    let line = status_line(&fixture.paths, &fixture.registry, None).unwrap();
    assert_eq!(
        line,
        "channel=cos; brief_channel=vp; paired=no; last_brief_id=None; update_offset=None"
    );
    assert!(!line.contains(FAKE_TOKEN), "status never prints the token");
}

#[test]
fn status_reports_the_routing_channel_and_the_pairing() {
    let fixture = Fixture::new();
    fixture.write_routing(r#"{"channel":"vp"}"#);
    let mut config = paired_config(42);
    config["last_brief_id"] = json!(13);
    config["update_offset"] = json!(900);
    fixture.write_config(&config);
    assert_eq!(
        status_line(&fixture.paths, &fixture.registry, None).unwrap(),
        "channel=vp; brief_channel=cos; paired=yes; last_brief_id=13; update_offset=900"
    );
    assert_eq!(
        status_line(&fixture.paths, &fixture.registry, Some("vp")).unwrap(),
        "channel=vp; brief_channel=cos; paired=yes; last_brief_id=13; update_offset=900"
    );
}

#[test]
fn an_environment_channel_overrides_the_routing_file() {
    let fixture = Fixture::new();
    fixture.write_routing(r#"{"channel":"cos"}"#);
    fixture.write_config(&paired_config(42));
    let line = status_line(&fixture.paths, &fixture.registry, Some("vp")).unwrap();
    assert!(line.starts_with("channel=vp;"), "{line}");
}

#[test]
fn an_unknown_environment_channel_is_refused() {
    let fixture = Fixture::new();
    fixture.write_config(&paired_config(42));
    assert_eq!(
        status_line(&fixture.paths, &fixture.registry, Some("nope")).unwrap_err(),
        BridgeError::Value("Telegram channel must name a configured desk channel".into())
    );
}

#[test]
fn a_registry_without_a_valid_id_is_refused() {
    let fixture = Fixture::new();
    std::fs::write(&fixture.registry, r#"[{"name":"no id"}]"#).unwrap();
    fixture.write_config(&paired_config(42));
    assert_eq!(
        status_line(&fixture.paths, &fixture.registry, None).unwrap_err(),
        BridgeError::Value("Telegram desk channel registry is invalid".into())
    );
}

#[test]
fn token_urlsafe_is_base64url_without_padding() {
    let value = token_urlsafe(18).unwrap();
    assert_eq!(value.len(), 24);
    assert!(value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    assert_ne!(
        value,
        token_urlsafe(18).unwrap(),
        "two codes are not the same"
    );
}

#[test]
fn paths_resolve_the_data_directory_next_to_the_config_dir() {
    let paths = Paths::new("data-dir-is-only-a-path-here");
    assert_eq!(paths.config().file_name().unwrap(), "telegram.json");
    assert_eq!(paths.lock().file_name().unwrap(), "telegram.lock");
    assert_eq!(paths.status().file_name().unwrap(), "telegram-status.json");
    assert_eq!(paths.desk_db().file_name().unwrap(), "messages.sqlite3");
}
