use super::{tmux::TmuxBackend, Terminal};
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
 printf '%%1__SEP__$1__SEP__@1__SEP__/work__SEP__bash__SEP__original__SEP__1__SEP__work__SEP__tab__SEP__0__SEP__0__SEP__1__SEP__80__SEP__24\n'
 printf '%%2__SEP__$1__SEP__@2__SEP__/work__SEP__codex__SEP__%s__SEP__1__SEP__work__SEP__tab__SEP__0__SEP__0__SEP__1__SEP__80__SEP__24\n' "$(cat title 2>/dev/null)" ;;
list-windows) printf '@1__SEP__$1__SEP__original__SEP__0__SEP__0\n@2__SEP__$1__SEP__new__SEP__1__SEP__1\n' ;;
select-pane) if [ "$4" = '-T' ]; then printf '%s' "$5" > title; fi ;;
esac
"#.replace("__SEP__", crate::terminal::parse::pane_separator())).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let backend = TmuxBackend::with_registry(script, dir.path().join("registry.json"));
    (dir, backend)
}
#[test]
fn raw_close_preserves_managed_parent_and_unowned_child_guards() {
    for (kind, id) in [("pane", "%2"), ("tab", "@2"), ("workspace", "$1")] {
        let (dir, mut backend) = fixture();
        assert!(backend.close_raw(kind, id).unwrap().is_err());
        let path = dir.path().join("registry.json");
        std::fs::write(
            &path,
            json!({"panes":{"%2":{}},"tabs":{"@2":{}},"workspaces":{"$1":{}}}).to_string(),
        )
        .unwrap();
        if kind == "workspace" {
            assert!(backend.close_raw(kind, id).unwrap().is_err());
            assert!(!std::fs::read_to_string(dir.path().join("calls"))
                .unwrap()
                .contains("kill-session"));
            std::fs::write(
                &path,
                json!({"panes":{"%1":{},"%2":{}},"tabs":{"@2":{}},"workspaces":{"$1":{}}})
                    .to_string(),
            )
            .unwrap();
        }
        backend.close_raw(kind, id).unwrap().unwrap();
        let saved: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        let key = match kind {
            "pane" => "panes",
            "tab" => "tabs",
            _ => "workspaces",
        };
        assert!(saved[key].get(id).is_none());
    }
}
#[test]
fn raw_close_save_exception_occurs_after_kill_and_keeps_registry() {
    let (dir, mut backend) = fixture();
    let path = dir.path().join("registry.json");
    let value = json!({"panes":{"%2":{}},"tabs":{},"workspaces":{}});
    std::fs::write(&path, value.to_string()).unwrap();
    std::fs::create_dir(dir.path().join("registry.json.tmp")).unwrap();
    let error = backend.close_raw("pane", "%2").unwrap_err();
    assert_eq!(error.category(), "IsADirectoryError");
    assert!(std::fs::read_to_string(dir.path().join("calls"))
        .unwrap()
        .contains("kill-pane"));
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(&path).unwrap()).unwrap(),
        value
    );
    std::fs::write(dir.path().join("refuse"), "kill-pane").unwrap();
    assert_eq!(
        backend
            .close_raw("pane", "%2")
            .unwrap()
            .unwrap_err()
            .message,
        "tmux kill-pane failed"
    );
}
#[test]
fn raw_close_registry_shape_is_not_a_backend_refusal() {
    let (dir, mut backend) = fixture();
    std::fs::write(dir.path().join("registry.json"), "[]").unwrap();
    assert_eq!(
        backend.close_raw("pane", "%2").unwrap_err().category(),
        "AttributeError"
    );
    assert!(!std::fs::read_to_string(dir.path().join("calls"))
        .unwrap()
        .contains("kill-pane"));
}
#[test]
fn raw_close_malformed_ownership_never_grants_authority() {
    let (dir, mut backend) = fixture();
    std::fs::write(
        dir.path().join("registry.json"),
        json!({"panes":["%2"]}).to_string(),
    )
    .unwrap();
    assert!(backend.close_raw("pane", "%2").is_err());
    assert!(!std::fs::read_to_string(dir.path().join("calls"))
        .unwrap()
        .contains("kill-pane"));
}
