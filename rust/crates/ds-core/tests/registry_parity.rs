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
        "inbox_get",
        "inbox_send_human",
        "owner_ingest_thread",
        "owner_delivered_thread",
        "pr_wait",
        "pr_author_correct",
        "task_owner_answer",
        "task_owner_question",
        "owner_page",
        "task_batch_claim",
        "owner_page_confirmed",
        "owner_handoff",
        // t4648: the read-only backlog query, CLI and API; Python had no project mapping.
        "task_backlog",
        // t5014: the read-only mailbox retirement dry run.
        "inbox_retire",
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
                // t4473 slice D: switching the model by a guarded respawn is Rust-only.
                assert_eq!(actions.pop(), Some(serde_json::json!("switch_model")));
                // t4473 slice C: the record-only model declaration is Rust-only.
                assert_eq!(actions.pop(), Some(serde_json::json!("model")));
                assert_eq!(actions.pop(), Some(serde_json::json!("project")));
                properties.get_mut("action").unwrap()["description"] =
                    want["properties"]["action"]["description"].clone();
                for key in ["model", "effort", "provider"] {
                    let param = properties
                        .remove(key)
                        .unwrap_or_else(|| panic!("lanes advertises the Rust-only {key}"));
                    assert_eq!(param["type"], "string", "{key}");
                }
                // `pane` is the same string; its description also names the model action.
                assert_eq!(properties["pane"]["type"], "string");
                properties.get_mut("pane").unwrap()["description"] =
                    want["properties"]["pane"]["description"].clone();
            }
        }
        if name == "owner_conversation" {
            // Rust-only opt-in ID paging; the retired Python timestamp API stays frozen.
            for key in ["before_id", "source_filter", "include_internal"] {
                assert!(got["properties"]
                    .as_object_mut()
                    .unwrap()
                    .remove(key)
                    .is_some());
            }
        }
        if matches!(name.as_str(), "inbox_send" | "inbox_send_agent_only") {
            assert!(got["properties"]
                .as_object_mut()
                .unwrap()
                .remove("parent_id")
                .is_some());
        }
        if name == "inbox_read" {
            assert_eq!(
                got["properties"]
                    .as_object_mut()
                    .unwrap()
                    .remove("ack")
                    .unwrap()["type"],
                "boolean"
            );
        }
        if name == "inbox_list" {
            // t4684 summary paging is opt-in and Rust-only; preserve the frozen legacy schema.
            let properties = got["properties"].as_object_mut().unwrap();
            for key in ["newest_first", "before", "literal_mailbox", "include_agent"] {
                assert!(properties.remove(key).is_some());
            }
        }
        if name == "task_add" {
            // t4701: a new task takes its creator lane's project; the two overrides are
            // Rust-only (the Python task store had no project mapping).
            let properties = got["properties"].as_object_mut().unwrap();
            for key in ["project", "no_project"] {
                assert!(properties.remove(key).is_some());
            }
        }
        if name == "task_complete" {
            let properties = got["properties"].as_object_mut().unwrap();
            for key in ["note", "next"] {
                assert!(properties.remove(key).is_some());
            }
        }
        if name == "task_block" {
            // t4704: the typed wait is Rust-only.
            assert!(got["properties"]
                .as_object_mut()
                .unwrap()
                .remove("waiting_on")
                .is_some());
        }
        if name == "task_assign" || name == "task_unblock" {
            // t4700: audit reasons extend only the Rust surface; the Python oracle is frozen.
            let reason = got["properties"]
                .as_object_mut()
                .unwrap()
                .remove("reason")
                .expect("Rust task transitions advertise an audit reason");
            assert_eq!(reason["type"], "string");
            assert!(
                !got["required"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|field| field == "reason"),
                "audit reason remains optional"
            );
        }
        if name == "task_read" || name == "task_note" {
            let properties = got["properties"].as_object_mut().unwrap();
            assert!(properties.remove("note_kind").is_some());
            if name == "task_read" {
                assert!(properties.remove("summary").is_some());
                assert!(properties.remove("notes").is_some());
                assert!(properties.remove("before_note").is_some());
            }
        }
        if name == "roster" {
            assert!(got["properties"]
                .as_object_mut()
                .unwrap()
                .remove("project")
                .is_some());
        }
        if name == "memory_backfill" {
            let properties = got["properties"].as_object_mut().unwrap();
            for key in ["queue_id", "path", "reason"] {
                assert!(properties.remove(key).is_some());
            }
            let actions = properties.get_mut("action").unwrap()["enum"]
                .as_array_mut()
                .unwrap();
            assert_eq!(actions.pop(), Some(serde_json::json!("restore")));
            assert_eq!(actions.pop(), Some(serde_json::json!("quarantine")));
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
        let extension_count = usize::from(name == "inbox");
        assert_eq!(
            actions.len() + extension_count,
            advertised.len(),
            "{name}: action count"
        );
        if name == "inbox" {
            assert!(advertised.contains(&"handoff"));
        }
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

#[test]
fn task_history_mcp_fields_do_not_collide_with_task_type() {
    let schema = registry::grouped_input_schema("task").unwrap();
    assert_eq!(schema["properties"]["summary"]["type"], "boolean");
    assert_eq!(schema["properties"]["notes"]["type"], "string");
    assert_eq!(schema["properties"]["before_note"]["type"], "string");
    let task_kinds = schema["properties"]["kind"]["enum"].as_array().unwrap();
    assert!(task_kinds.contains(&serde_json::json!("work")));
    let note_kinds = schema["properties"]["note_kind"]["enum"]
        .as_array()
        .unwrap();
    for kind in [
        "evidence",
        "decision",
        "handoff",
        "progress",
        "correction",
        "note",
    ] {
        assert!(note_kinds.contains(&serde_json::json!(kind)));
    }
}

#[test]
fn atomic_completion_fields_are_available_to_socket_and_mcp() {
    for schema in [
        registry::input_schema("task_complete").unwrap(),
        registry::grouped_input_schema("task").unwrap(),
    ] {
        assert_eq!(schema["properties"]["note"]["type"], "string");
        assert_eq!(schema["properties"]["next"]["type"], "boolean");
    }
}

#[test]
fn private_human_producers_are_not_mcp_actions_or_lane_grants() {
    for command in [
        "inbox_send_human",
        "owner_ingest_thread",
        "owner_delivered_thread",
    ] {
        assert!(!registry::command(command).unwrap().mcp);
        for role in [
            "owner",
            "worker",
            "reviewer",
            "planner",
            "desk",
            "anonymous",
        ] {
            assert!(
                !ds_core::roles::allowed(role, command),
                "{role} cannot claim human transport provenance through {command}"
            );
        }
    }
    assert!(ds_core::roles::allowed("web", "inbox_send_human"));
    assert!(ds_core::roles::allowed("web", "owner_ingest_thread"));
    assert!(ds_core::roles::allowed("system", "owner_ingest_thread"));
    assert!(ds_core::roles::allowed("system", "owner_delivered_thread"));
}

#[test]
fn task_paging_socket_arguments_are_explicit_and_absent_from_public_schema() {
    assert_eq!(
        registry::socket_only_params("task_list"),
        &["paged", "page_size", "cursor", "priority", "page_group"]
    );
    for command in registry::COMMANDS {
        if command.name != "task_list" {
            assert!(registry::socket_only_params(command.name).is_empty());
        }
    }
    let schema = registry::input_schema("task_list").unwrap();
    for key in registry::socket_only_params("task_list") {
        assert!(schema["properties"].get(key).is_none());
    }
    let grouped = registry::grouped_input_schema("task").unwrap();
    for key in ["paged", "page_size", "cursor", "page_group"] {
        assert!(grouped["properties"].get(key).is_none());
    }
}

#[test]
fn pr_author_correction_stays_planner_only_and_socket_only() {
    let command = registry::command("pr_author_correct").unwrap();
    assert!(
        !command.mcp,
        "rare coordinator mutation must not expand MCP schemas"
    );
    assert_eq!(command.roles, &["planner"]);
    assert!(!registry::GROUPS
        .iter()
        .any(|g| g.actions.iter().any(|(_, cmd)| *cmd == "pr_author_correct")));
}
