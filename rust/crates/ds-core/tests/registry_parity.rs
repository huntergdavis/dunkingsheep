//! T04 acceptance: the Rust registry equals the Python one, command by command.

use ds_core::registry;

const EXPECTED: &str = include_str!("registry_expected.json");

fn expected() -> serde_json::Value {
    serde_json::from_str(EXPECTED).expect("the fixture is valid JSON")
}

#[test]
fn every_python_command_is_present_and_no_extra_ones_are() {
    let fixture = expected();
    let commands = fixture["commands"].as_object().expect("commands");
    let rust: Vec<&str> = registry::COMMANDS.iter().map(|c| c.name).collect();
    let python: Vec<&str> = commands.keys().map(String::as_str).collect();
    assert_eq!(
        python.len(),
        rust.len(),
        "the registry has {} commands, Python has {}",
        rust.len(),
        python.len()
    );
    for name in &python {
        assert!(
            registry::command(name).is_some(),
            "{name} is missing from registry.rs"
        );
    }
}

#[test]
fn every_input_schema_equals_the_pythons() {
    let fixture = expected();
    for (name, want) in fixture["commands"].as_object().expect("commands") {
        let got = registry::input_schema(name).unwrap_or_else(|| panic!("{name} has no schema"));
        assert_eq!(want, &got, "{name}: input_schema() differs from the Python");
    }
}

#[test]
fn dispatch_and_mcp_required_metadata_match_python_for_every_command() {
    let fixture = expected();
    for (name, want) in fixture["required_metadata"].as_object().expect("metadata") {
        let command = registry::command(name).unwrap_or_else(|| panic!("{name} missing"));
        assert_eq!(
            serde_json::json!(command.dispatch_required),
            want["dispatch_required"],
            "{name}: dispatch required metadata differs from Python"
        );
        assert_eq!(
            serde_json::json!(command.required),
            want["mcp_required"],
            "{name}: MCP required metadata differs from Python"
        );
    }

    let add_dunk = registry::command("add_dunk").expect("add_dunk");
    assert!(add_dunk.dispatch_required.is_empty());
    assert_eq!(&["target", "text", "interval_minutes"], add_dunk.required);

    // create_tab has MCP-required `label` but intentionally no dispatcher requirement.
    let create_tab = registry::command("create_tab").expect("create_tab");
    assert!(create_tab.dispatch_required.is_empty());
    assert_eq!(&["label"], create_tab.required);
}

#[test]
fn the_grouped_schemas_advertise_the_action_enum() {
    let fixture = expected();
    for (name, want) in fixture["groups"].as_object().expect("groups") {
        let schema = registry::grouped_input_schema(name)
            .unwrap_or_else(|| panic!("{name} has no grouped schema"));
        let actions = want["actions"].as_object().expect("actions");
        let advertised: Vec<&str> = schema["properties"]["action"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(actions.len(), advertised.len(), "{name}: action count");
        for action in actions.keys() {
            assert!(
                advertised.contains(&action.as_str()),
                "{name} lost action {action}"
            );
        }
        assert_eq!(serde_json::json!(["action"]), schema["required"]);
        assert_eq!(serde_json::json!(false), schema["additionalProperties"]);
    }
}

#[test]
fn the_owner_only_column_matches_superuser_only() {
    // A sample that pins the split: the destructive verbs are owner-only, a read is not.
    for name in [
        "close",
        "shutdown",
        "add_dunk",
        "start_agent",
        "token_issue",
    ] {
        assert!(registry::command(name).expect(name).owner_only, "{name}");
    }
    for name in ["status", "task_list", "whoami", "memory_recall"] {
        assert!(!registry::command(name).expect(name).owner_only, "{name}");
    }
}

#[test]
fn roles_are_the_python_allowlist() {
    let status = registry::command("status").expect("status");
    assert!(status.roles.contains(&"owner"));
    assert!(status.roles.contains(&"anonymous"));
    let close = registry::command("close").expect("close");
    assert_eq!(&["owner"], close.roles);
}
