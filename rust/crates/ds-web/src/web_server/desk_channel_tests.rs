use super::*;
use serde_json::json;

#[test]
fn owner_delivery_uses_configured_front_desk_channel() {
    let fleet = OwnerFleetConfig::new("messages.sqlite3", "ops.sqlite3");
    let configured = configure_desk_channel(
        fleet,
        &[
            json!({"id": "other", "pane": "%133"}),
            json!({"id": "cos", "match": "desk", "pane": "%259"}),
        ],
    );
    assert_eq!(configured.desk_channel.unwrap()["pane"], "%259");
}
