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
    // These commands are Rust-only additions; Python remains frozen. owner_page (t665) is
    // recorded by the Rust daemon for the Telegram outbox and has no Python dunk_api entry.
    // task_batch_claim (t3583) is the guarded batch claim, API only: the owner keeps it out of
    // the public CLI help, which must match Python.
    let rust_only = [
        "task_owner_answer",
        "task_owner_question",
        "owner_page",
        "task_batch_claim",
        "owner_page_confirmed",
    ];
    let extra: Vec<_> = rust
        .iter()
        .copied()
        .filter(|name| !python.contains(name))
        .collect();
    assert_eq!(extra.len(), rust_only.len());
    for name in rust_only {
        assert!(extra.contains(&name), "missing Rust-only command {name}");
    }
    assert_eq!(rust.len(), python.len() + rust_only.len());
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
        let mut got =
            registry::input_schema(name).unwrap_or_else(|| panic!("{name} has no schema"));
        // Explicit project membership is a Rust-only extension; Python is retired.
        if name == "start_agent" || name == "lanes" {
            let properties = got["properties"].as_object_mut().unwrap();
            assert!(properties.remove("project").is_some());
            assert!(properties.remove("global").is_some());
            if name == "lanes" {
                assert!(properties.remove("operation").is_some());
                assert!(properties.remove("task").is_some());
                assert!(properties.remove("repo").is_some());
                let actions = properties.get_mut("action").unwrap()["enum"]
                    .as_array_mut()
                    .unwrap();
                assert_eq!(actions.pop(), Some(serde_json::json!("planner")));
                assert_eq!(actions.pop(), Some(serde_json::json!("project")));
                properties.get_mut("action").unwrap()["description"] =
                    want["properties"]["action"]["description"].clone();
            }
        }
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
