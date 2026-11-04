use super::{tmux::TmuxBackend, AgentSpec, Terminal};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;

fn fixture() -> (tempfile::TempDir, TmuxBackend) {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("tmux");
    std::fs::write(&script,r#"#!/bin/sh
cd "$(dirname "$0")" || exit 1
printf '%s\n' "$@" >> calls
if [ -f refuse ] && [ "$(cat refuse)" = "$1" ]; then exit 1; fi
case "$1" in
new-window|split-window) printf '%%2\n' ;;
list-panes)
 printf '%%1\037$1\037@1\037/work\037bash\037original\0371\037work\037tab\0370\0370\0371\03780\03724\n'
 printf '%%2\037$1\037@2\037/work\037codex\037%s\0371\037work\037tab\0370\0370\0371\03780\03724\n' "$(cat title 2>/dev/null)" ;;
list-windows) printf '@1\037$1\037original\0370\0370\n@2\037$1\037new\0371\0371\n' ;;
select-pane) if [ "$4" = '-T' ]; then printf '%s' "$5" > title; fi ;;
esac
"#).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let backend = TmuxBackend::with_registry(script, dir.path().join("registry.json"));
    (dir, backend)
}
fn spec(tab: bool) -> AgentSpec {
    AgentSpec {
        name: "probe".into(),
        argv: vec!["codex".into(), "--model".into(), "test model".into()],
        workspace_id: Some("$1".into()),
        tab_id: tab.then(|| "@1".into()),
        ..Default::default()
    }
}
#[test]
fn raw_agent_terminal_launch_registry_and_focus_match_python() {
    for (tab, focus) in [(false, false), (false, true), (true, false), (true, true)] {
        let (dir, mut backend) = fixture();
        let pane = backend.start_agent_raw(&spec(tab), focus).unwrap().unwrap();
        assert_eq!(pane.pane_id, "%2");
        assert_eq!(pane.label, "probe");
        assert_eq!(pane.command, "codex");
        let registry: Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("registry.json")).unwrap())
                .unwrap();
        assert_eq!(registry["panes"]["%2"]["name"], "probe");
        assert_eq!(registry["panes"]["%2"]["agent_kind"], "codex");
        if tab {
            assert!(registry["tabs"].as_object().unwrap().is_empty());
        } else {
            assert_eq!(registry["tabs"]["@2"]["label"], "probe");
        }
        let calls = std::fs::read_to_string(dir.path().join("calls")).unwrap();
        assert_eq!(
            calls.lines().filter(|line| *line == "-d").count(),
            usize::from(!tab || !focus)
        );
        assert!(calls.contains("--model"));
        assert_eq!(
            calls.lines().filter(|line| *line == "select-pane").count(),
            1
        );
        assert!(!calls.contains("switch-client"));
        assert!(!calls.contains("select-window"));
    }
}
#[test]
fn raw_agent_registry_errors_preserve_creation_before_label_and_partial_state() {
    for tab in [false, true] {
        let (dir, mut backend) = fixture();
        let registry = dir.path().join("registry.json");
        std::fs::write(
            &registry,
            json!({"panes":{},"tabs":null,"workspaces":{}}).to_string(),
        )
        .unwrap();
        if tab {
            // Split saves a pane before applying the name. A real save error stops there.
            std::fs::create_dir(dir.path().join("registry.json.tmp")).unwrap();
        }
        let error = backend.start_agent_raw(&spec(tab), false).unwrap_err();
        assert_eq!(
            error.category(),
            if tab {
                "IsADirectoryError"
            } else {
                "TypeError"
            }
        );
        let calls = std::fs::read_to_string(dir.path().join("calls")).unwrap();
        assert!(calls.contains(if tab { "split-window" } else { "new-window" }));
        assert!(!calls.contains("select-pane"));
        let saved: Value = serde_json::from_slice(&std::fs::read(registry).unwrap()).unwrap();
        assert_eq!(saved, json!({"panes":{},"tabs":null,"workspaces":{}}));
    }
}
#[test]
fn raw_rename_registry_failure_and_terminal_refusals_remain_distinct() {
    let (dir, mut backend) = fixture();
    std::fs::create_dir(dir.path().join("registry.json.tmp")).unwrap();
    let error = backend.rename_raw("pane", "%2", "new name").unwrap_err();
    assert_eq!(error.category(), "IsADirectoryError");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("title")).unwrap(),
        "new name"
    );
    std::fs::write(dir.path().join("refuse"), "select-pane").unwrap();
    assert_eq!(
        backend
            .rename_raw("pane", "%2", "other")
            .unwrap()
            .unwrap_err()
            .message,
        "tmux rename pane failed"
    );
    assert_eq!(
        backend.focus_raw("pane", "%2").unwrap_err().message,
        "tmux focus pane failed"
    );
    std::fs::write(dir.path().join("refuse"), "new-window").unwrap();
    assert_eq!(
        backend
            .start_agent_raw(&spec(false), false)
            .unwrap()
            .unwrap_err()
            .message,
        "tmux new-window failed"
    );
}

#[test]
fn raw_rename_captures_label_title_and_managed_before_registry_write() {
    for previously_managed in [false, true] {
        let (dir, mut backend) = fixture();
        let path = dir.path().join("registry.json");
        let initial = if previously_managed {
            json!({"panes":{"%2":{"name":"old-agent"}},"tabs":{},"workspaces":{}})
        } else {
            json!({"panes":{},"tabs":{},"workspaces":{}})
        };
        std::fs::write(&path, initial.to_string()).unwrap();
        let snapshot = backend
            .rename_raw("pane", "%2", "new-label")
            .unwrap()
            .unwrap()
            .unwrap();
        let super::RenamedItem::Pane {
            pane,
            managed,
            agent_status,
        } = snapshot
        else {
            panic!("pane snapshot required")
        };
        assert_eq!(
            pane.label,
            if previously_managed {
                "old-agent"
            } else {
                "new-label"
            }
        );
        assert_eq!(pane.title, "new-label");
        assert_eq!(managed, Some(previously_managed));
        assert_eq!(agent_status.as_deref(), Some("unknown"));
        let saved: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["panes"]["%2"]["label"], "new-label");
        assert!(saved["panes"]["%2"].get("name").is_none());
        let after = backend.get_pane("%2").unwrap();
        assert_eq!(after.label, "new-label");
        assert_eq!(after.title, "new-label");
        assert!(backend.managed("pane", "%2", &[]));
    }
}
#[test]
fn raw_rename_save_failure_leaves_old_registry_and_new_title() {
    let (dir, mut backend) = fixture();
    let path = dir.path().join("registry.json");
    let initial = json!({"panes":{"%2":{"name":"old-agent"}},"tabs":{},"workspaces":{}});
    std::fs::write(&path, initial.to_string()).unwrap();
    std::fs::create_dir(dir.path().join("registry.json.tmp")).unwrap();
    let error = backend.rename_raw("pane", "%2", "new-label").unwrap_err();
    assert_eq!(error.category(), "IsADirectoryError");
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(path).unwrap()).unwrap(),
        initial
    );
    let after = backend.get_pane("%2").unwrap();
    assert_eq!(after.label, "old-agent");
    assert_eq!(after.title, "new-label");
}
