use super::*;
use serde_json::json;
#[test]
fn registry_reloads_ownership_and_never_infers_children_from_parents() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("registry.json");
    let first = Registry::new(&path);
    let second = Registry::new(&path);
    first
        .remember("workspace", "$1", json!({"extra":"retained"}))
        .unwrap();
    first.remember("tab", "@1", json!({})).unwrap();
    let panes = vec![Pane {
        pane_id: "%1".into(),
        window_id: "@1".into(),
        session_id: "$1".into(),
        ..Default::default()
    }];
    assert!(!second.managed("pane", "%1", &panes).unwrap());
    assert!(!second.managed("tab", "@1", &panes).unwrap());
    assert!(!second.managed("workspace", "$1", &panes).unwrap());
    first
        .remember("pane", "%1", json!({"name":"worker","unknown":7}))
        .unwrap();
    assert!(second.managed("pane", "%1", &panes).unwrap());
    assert!(second.managed("tab", "@1", &panes).unwrap());
    assert!(second.managed("workspace", "$1", &panes).unwrap());
    second.forget("pane", "%1").unwrap();
    assert!(!first.managed("pane", "%1", &panes).unwrap());
    assert!(!first.managed("tab", "@1", &panes).unwrap());
}
#[test]
fn registry_corrupt_missing_structural_errors_and_atomic_sorted_private_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("child/registry.json");
    let registry = Registry::new(&path);
    assert!(!registry.managed("pane", "%1", &[]).unwrap());
    registry
        .remember("pane", "%1", json!({"name":"é"}))
        .unwrap();
    let mut data = registry.load().unwrap();
    data.insert("unknown".into(), json!({"z":1,"a":2}));
    registry.save(&data).unwrap();
    registry.remember("tab", "@1", json!({})).unwrap();
    assert_eq!(registry.load().unwrap()["unknown"], json!({"z":1,"a":2}));
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("\\u00e9"));
    assert!(text.find("\"a\"") < text.find("\"z\""));
    assert!(!dir.path().join("child/registry.json.tmp").exists());
    std::fs::write(&path, "broken").unwrap();
    assert!(!registry.managed("pane", "%1", &[]).unwrap());
    std::fs::write(&path, "[]").unwrap();
    assert!(registry.load().is_err());
    std::fs::write(&path, r#"{"panes":null}"#).unwrap();
    assert!(registry.managed("pane", "%1", &[]).is_err());
}
#[test]
fn registry_path_precedence_is_lazy_and_empty_home_is_not_missing() {
    let env = Env::from_pairs(&[
        ("DUNKINGSHEEP_TMUX_REGISTRY", "/registry"),
        ("DUNKINGSHEEP_DIR", "/config"),
        ("HOME", "/home/test"),
    ]);
    assert_eq!(
        Registry::configured(Some(Path::new("/explicit")), &env)
            .path()
            .unwrap(),
        Path::new("/explicit")
    );
    assert_eq!(
        Registry::configured(None, &env).path().unwrap(),
        Path::new("/registry")
    );
    let env = Env::from_pairs(&[
        ("DUNKINGSHEEP_TMUX_REGISTRY", ""),
        ("DUNKINGSHEEP_DIR", "/config"),
        ("HOME", ""),
    ]);
    assert_eq!(
        Registry::configured(None, &env).path().unwrap(),
        Path::new("/config/tmux-panes.json")
    );
    let env = Env::from_pairs(&[("HOME", "")]);
    assert_eq!(
        Registry::configured(None, &env).path().unwrap(),
        Path::new("/.config/dunkingsheep/tmux-panes.json")
    );
}
#[test]
fn registry_write_failure_propagates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file");
    std::fs::write(&path, "x").unwrap();
    assert!(Registry::new(path.join("registry"))
        .remember("pane", "%1", json!({}))
        .is_err());
}

#[test]
fn created_workspace_persists_parent_and_root_across_backend_instances() {
    use crate::terminal::{tmux::TmuxBackend, Terminal};
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("tmux");
    let root = crate::terminal::tmux::shell_join(&[dir.path().to_str().unwrap()]);
    let script = format!(
        r#"#!/bin/sh
cd {root} || exit 1
printf '%s\n' "$*" >> calls
case "$1" in
new-window) if test "$5" = '#{{pane_id}}'; then printf '%%1\n'; else printf '@1\037%%1\n'; fi ;;
list-sessions) printf '$1\037fleet\0370\n' ;;
list-windows) printf '@1\037$1\037tab\0370\0371\n' ;;
list-panes) printf '%%1\037$1\037@1\037/tmp\037sh\037root\0371\037fleet\037tab\0370\0370\037123\03780\03724\n'; test ! -f extra || cat extra ;;
esac
exit 0
"#
    );
    std::fs::write(&binary, script).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = dir.path().join("registry.json");
    let mut first = TmuxBackend::with_registry(&binary, &path);
    first.create_workspace(Some("fleet"), None, false).unwrap();
    first
        .create_tab(Some("$1"), Some("tab"), None, false)
        .unwrap();
    first
        .start_agent(&crate::terminal::AgentSpec {
            name: "worker".into(),
            argv: vec!["claude".into()],
            workspace_id: Some("$1".into()),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        first.registry().load().unwrap()["panes"]["%1"]["agent_kind"],
        "claude"
    );
    let mut second = TmuxBackend::with_registry(&binary, &path);
    let panes = second.list_panes();
    first
        .registry()
        .remember("pane", "%1", json!({"name":"  "}))
        .unwrap();
    assert_eq!(second.get_pane("%1").unwrap().label, "  ");
    assert!(second.managed("workspace", "$1", &panes));
    assert!(second.managed("tab", "@1", &panes));
    assert!(second.managed("pane", "%1", &panes));
    std::fs::write(
        dir.path().join("extra"),
        "%2\x1f$1\x1f@1\x1f/tmp\x1fsh\x1fhuman\x1f0\x1ffleet\x1ftab\x1f0\x1f0\x1f456\n",
    )
    .unwrap();
    assert!(second.close("workspace", "$1").is_err());
    assert!(!std::fs::read_to_string(dir.path().join("calls"))
        .unwrap()
        .contains("kill-session"));
    second.close("pane", "%1").unwrap();
    assert!(!first.managed("pane", "%1", &[]));
}

#[test]
fn home_expansion_handles_root_and_named_real_account() {
    assert_eq!(
        expand_home("~", &Env::from_pairs(&[("HOME", "")])).unwrap(),
        Path::new("/")
    );
    assert_eq!(
        expand_home("~/a", &Env::from_pairs(&[("HOME", "/")])).unwrap(),
        Path::new("/a")
    );
    if let Some(user) = nix::unistd::User::from_uid(nix::unistd::Uid::current()).unwrap() {
        assert_eq!(
            expand_home(
                &format!("~{}/a", user.name),
                &Env::from_pairs(&[("HOME", "/unrelated")])
            )
            .unwrap(),
            user.dir.join("a")
        );
    }
}

#[test]
fn account_lookup_failure_propagates_but_missing_account_preserves_tilde() {
    let fail = |_: Option<&str>| Err(TerminalError::refused("passwd read failed"));
    assert_eq!(
        expand_home_with("~/x", &Env::from_pairs(&[("HOME", "/owned")]), &fail).unwrap(),
        Path::new("/owned/x")
    );
    assert!(expand_home_with("~/x", &Env::default(), &fail).is_err());
    assert_eq!(
        expand_home_with("~/x", &Env::default(), &|_| Ok(None)).unwrap(),
        Path::new("~/x")
    );
    let registry = Registry {
        path: Err(TerminalError::refused("passwd read failed")),
    };
    assert!(registry.load().is_err());
    assert!(registry.remember("pane", "%1", json!({})).is_err());
}
