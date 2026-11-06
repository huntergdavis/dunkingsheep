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
    let sep = crate::terminal::parse::pane_separator();
    let script = format!(
        r#"#!/bin/sh
cd {root} || exit 1
printf '%s\n' "$*" >> calls
case "$1" in
new-window) if test "$5" = '#{{pane_id}}'; then printf '%%1\n'; else printf '@1{sep}%%1\n'; fi ;;
list-sessions) printf '$1{sep}fleet{sep}0\n' ;;
list-windows) printf '@1{sep}$1{sep}tab{sep}0{sep}1\n' ;;
list-panes) printf '%%1{sep}$1{sep}@1{sep}/tmp{sep}sh{sep}root{sep}1{sep}fleet{sep}tab{sep}0{sep}0{sep}123{sep}80{sep}24\n'; test ! -f extra || cat extra ;;
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
        "%2\x1f$1\x1f@1\x1f/tmp\x1fsh\x1fhuman\x1f0\x1ffleet\x1ftab\x1f0\x1f0\x1f456\n"
            .replace('\x1f', crate::terminal::parse::pane_separator()),
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

#[test]
fn parent_close_requires_successful_complete_nonempty_inventory() {
    use crate::terminal::{tmux::TmuxBackend, Terminal};
    let dir = tempfile::tempdir().unwrap();
    let binary = dir.path().join("tmux");
    let root = crate::terminal::tmux::shell_join(&[dir.path().to_str().unwrap()]);
    std::fs::write(
        &binary,
        format!(
            r#"#!/bin/sh
cd {root} || exit 1
printf '%s\n' "$*" >> calls
case "$1" in
list-panes) cat inventory; if test -f fail; then echo 'inventory failed' >&2; exit 1; fi ;;
esac
"#
        ),
    )
    .unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = dir.path().join("registry.json");
    let registry = Registry::new(&path);
    let owned =
        "%1\x1f$1\x1f@1\x1f/tmp\x1fsh\x1froot\x1f1\x1ffleet\x1ftab\x1f0\x1f0\x1f123\x1f80\x1f24\n"
            .replace('\x1f', crate::terminal::parse::pane_separator());
    for (kind, id, kill) in [
        ("tab", "@1", "kill-window"),
        ("workspace", "$1", "kill-session"),
    ] {
        registry.remember(kind, id, json!({})).unwrap();
        registry.remember("pane", "%1", json!({})).unwrap();
        let mut backend = TmuxBackend::with_registry(&binary, &path);
        for (inventory, fail) in [
            (owned.to_string(), true),
            (String::new(), false),
            (
                format!("{owned}%2\x1f$1\x1f@1\n")
                    .replace('\x1f', crate::terminal::parse::pane_separator()),
                false,
            ),
            (owned.replace("%1", "%2"), false),
            (owned.replace("$1", "$2").replace("@1", "@2"), false),
        ] {
            std::fs::write(dir.path().join("inventory"), inventory).unwrap();
            if fail {
                std::fs::write(dir.path().join("fail"), "").unwrap();
            }
            assert!(backend.close(kind, id).is_err());
            assert!(!std::fs::read_to_string(dir.path().join("calls"))
                .unwrap()
                .contains(kill));
            assert!(
                registry.load().unwrap()[if kind == "tab" { "tabs" } else { "workspaces" }]
                    .get(id)
                    .is_some()
            );
            if fail {
                std::fs::remove_file(dir.path().join("fail")).unwrap();
            }
        }
        std::fs::write(dir.path().join("inventory"), owned).unwrap();
        backend.close(kind, id).unwrap();
        assert!(std::fs::read_to_string(dir.path().join("calls"))
            .unwrap()
            .contains(kill));
        std::fs::write(dir.path().join("calls"), "").unwrap();
    }
}

#[test]
fn raw_registry_load_swallows_only_io_and_decoding_errors() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("registry.json");
    let registry = Registry::new(&path);
    assert_eq!(registry.load_raw().unwrap().len(), 3);
    for bytes in [b"broken".as_slice(), b"\xff".as_slice()] {
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(registry.load_raw().unwrap().len(), 3);
    }
    for (text, kind) in [
        ("[]", "list"),
        ("null", "NoneType"),
        ("false", "bool"),
        ("1", "int"),
        ("\"x\"", "str"),
    ] {
        std::fs::write(&path, text).unwrap();
        let error = registry.load_raw().unwrap_err();
        assert_eq!(error.category(), "AttributeError");
        assert_eq!(
            error.to_string(),
            format!("'{kind}' object has no attribute 'setdefault'")
        );
    }
    std::fs::write(&path, r#"{"panes":null}"#).unwrap();
    let error = registry.remember_raw("pane", "%1", json!({})).unwrap_err();
    assert_eq!(error.category(), "TypeError");
    assert_eq!(
        error.to_string(),
        "'NoneType' object does not support item assignment"
    );
}

#[test]
fn raw_registry_actual_write_and_rename_failures_retain_operation_errno_paths() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("registry.json");
    let temp = dir.path().join("registry.json.tmp");
    let registry = Registry::new(&path);
    std::fs::create_dir(&temp).unwrap();
    let error = registry
        .remember_raw("pane", "%1", json!({"label":"x"}))
        .unwrap_err();
    assert_eq!(error.category(), "IsADirectoryError");
    match &error {
        RegistryError::Io {
            operation,
            path,
            destination,
            error,
        } => {
            assert_eq!(*operation, RegistryOperation::Write);
            assert_eq!(path, &temp);
            assert!(destination.is_none());
            assert_eq!(error.raw_os_error(), Some(nix::libc::EISDIR));
        }
        other => panic!("expected real filesystem error: {other:?}"),
    }
    assert_eq!(
        error.to_string(),
        format!(
            "[Errno {}] Is a directory: {}",
            nix::libc::EISDIR,
            crate::api::python_value_repr(&json!(temp.to_string_lossy()))
        )
    );
    std::fs::remove_dir(&temp).unwrap();
    std::fs::create_dir(&path).unwrap();
    // The read error is swallowed; the later rename error is propagated with both paths.
    assert_eq!(registry.load_raw().unwrap().len(), 3);
    let error = registry
        .remember_raw("pane", "%1", json!({"label":"x"}))
        .unwrap_err();
    match &error {
        RegistryError::Io {
            operation,
            path: failed,
            destination,
            error,
        } => {
            assert_eq!(*operation, RegistryOperation::Rename);
            assert_eq!(failed, &temp);
            assert_eq!(destination.as_deref(), Some(path.as_path()));
            assert_eq!(error.raw_os_error(), Some(nix::libc::EISDIR));
        }
        other => panic!("expected real filesystem error: {other:?}"),
    }
    assert_eq!(
        error.to_string(),
        format!(
            "[Errno {}] Is a directory: {} -> {}",
            nix::libc::EISDIR,
            crate::api::python_value_repr(&json!(temp.to_string_lossy())),
            crate::api::python_value_repr(&json!(path.to_string_lossy()))
        )
    );
    assert!(
        temp.is_file(),
        "Python leaves the temp file after os.replace fails"
    );
    assert_eq!(
        std::fs::metadata(&temp).unwrap().permissions().mode() & 0o777,
        0o600
    );
}
