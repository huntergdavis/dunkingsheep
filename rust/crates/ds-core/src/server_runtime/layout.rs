//! Concrete terminal-backed sources for the read-only layout adapter helpers.

use serde_json::{json, Value};

use crate::flock::mailbox::MailboxEnv;
use crate::flock::panes::PaneInfoEnv;
use crate::server_runtime::RequestRuntimeAdapter;
use crate::terminal::{Pane, Tab, Terminal, Workspace};

fn workspace_value(workspace: Workspace, tabs: &[Tab]) -> Value {
    let active_tab_id = tabs
        .iter()
        .find(|tab| tab.workspace_id == workspace.workspace_id && tab.active)
        .map(|tab| tab.tab_id.clone());
    json!({
        "workspace_id": workspace.workspace_id,
        "label": workspace.label,
        "number": workspace.index,
        "active_tab_id": active_tab_id,
    })
}

fn tab_value(tab: Tab) -> Value {
    json!({
        "tab_id": tab.tab_id,
        "workspace_id": tab.workspace_id,
        "label": tab.label,
        "number": tab.index,
        "active": tab.active,
    })
}

fn pane_value(terminal: &dyn Terminal, pane: &Pane) -> Value {
    let status = terminal.agent_status(&pane.pane_id);
    crate::flock::lanes::pane_to_json(pane, status.as_deref())
}

impl RequestRuntimeAdapter<'_> {
    #[allow(dead_code)] // The complete LayoutBackend assembly belongs to its dependent leaf.
    pub(crate) fn helper_layoutbackend_list_panes(&self) -> Vec<Value> {
        self.terminal
            .with_env(|env| MailboxEnv::list_panes_grouped(env))
    }

    #[allow(dead_code)] // The complete LayoutBackend assembly belongs to its dependent leaf.
    pub(crate) fn helper_layoutbackend_list_tabs(&self) -> Vec<Value> {
        self.terminal.with_terminal(|terminal| {
            terminal
                .list_tabs()
                .into_iter()
                .map(|tab| {
                    json!({
                        "tab_id": tab.tab_id,
                        "workspace_id": tab.workspace_id,
                        "label": tab.label,
                        "number": tab.index,
                        "active": tab.active,
                    })
                })
                .collect()
        })
    }

    #[allow(dead_code)] // The complete LayoutBackend assembly belongs to its dependent leaf.
    pub(crate) fn helper_layoutbackend_list_workspaces(&self) -> Vec<Value> {
        self.terminal.with_terminal(|terminal| {
            let tabs = terminal.list_tabs();
            terminal
                .list_workspaces()
                .into_iter()
                .map(|workspace| {
                    let active_tab_id = tabs
                        .iter()
                        .find(|tab| tab.workspace_id == workspace.workspace_id && tab.active)
                        .map(|tab| tab.tab_id.clone());
                    json!({
                        "workspace_id": workspace.workspace_id,
                        "label": workspace.label,
                        "number": workspace.index,
                        "active_tab_id": active_tab_id,
                    })
                })
                .collect()
        })
    }

    #[allow(dead_code)] // The complete LayoutBackend assembly belongs to its dependent leaf.
    pub(crate) fn helper_layoutbackend_get_pane(&self, pane_id: &str) -> Option<Value> {
        self.terminal
            .with_env(|env| PaneInfoEnv::get_pane(env, pane_id))
    }

    #[allow(dead_code)] // The complete LayoutBackend assembly belongs to its dependent leaf.
    pub(crate) fn helper_layoutbackend_server_error_hint(&self) -> String {
        self.terminal
            .with_terminal(|terminal| terminal.server_error_hint())
    }

    #[allow(dead_code)] // The complete LayoutBackend assembly belongs to its dependent leaf.
    pub(crate) fn helper_layoutbackend_create_workspace(
        &mut self,
        label: Option<&str>,
        cwd: Option<&str>,
        focus: bool,
    ) -> Result<Value, String> {
        self.terminal.with_terminal_mut(|terminal| {
            let workspace = terminal
                .create_workspace(label, cwd, focus)
                .map_err(|error| error.message)?;
            let tabs = terminal.list_tabs();
            let tab = tabs
                .iter()
                .find(|tab| tab.workspace_id == workspace.workspace_id)
                .cloned()
                .map(tab_value);
            let pane = terminal
                .list_panes()
                .into_iter()
                .find(|pane| pane.session_id == workspace.workspace_id)
                .map(|pane| pane_value(terminal, &pane));
            Ok(json!({
                "workspace": workspace_value(workspace, &tabs),
                "tab": tab,
                "root_pane": pane,
            }))
        })
    }

    #[allow(dead_code)] // The complete LayoutBackend assembly belongs to its dependent leaf.
    pub(crate) fn helper_layoutbackend_create_tab(
        &mut self,
        workspace_id: Option<&str>,
        label: Option<&str>,
        cwd: Option<&str>,
        focus: bool,
    ) -> Result<Value, String> {
        self.terminal.with_terminal_mut(|terminal| {
            let tab = terminal
                .create_tab(workspace_id, label, cwd, focus)
                .map_err(|error| error.message)?;
            let pane = terminal
                .list_panes()
                .into_iter()
                .find(|pane| pane.window_id == tab.tab_id)
                .map(|pane| pane_value(terminal, &pane));
            let Some(pane) = pane else {
                return Err("tmux created the window but it is not visible".into());
            };
            Ok(json!({
                "tab": tab_value(tab),
                "root_pane": pane,
            }))
        })
    }

    #[allow(dead_code)] // The complete LayoutBackend assembly belongs to its dependent leaf.
    pub(crate) fn helper_layoutbackend_split_pane(
        &mut self,
        pane_id: &str,
        direction: &str,
        cwd: Option<&str>,
        focus: bool,
    ) -> Result<Value, String> {
        self.terminal.with_terminal_mut(|terminal| {
            let pane = terminal
                .split_pane(pane_id, direction, cwd)
                .map_err(|error| error.message)?;
            if !focus {
                terminal
                    .focus("pane", pane_id)
                    .map_err(|error| error.message)?;
            }
            let pane = pane_value(terminal, &pane);
            Ok(json!({ "root_pane": pane }))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::flock::Flock;
    use crate::server::RequestCtx;
    use crate::terminal::{fakes::FakeTerminal, Terminal};

    fn with_adapter(terminal: FakeTerminal, run: impl FnOnce(&mut RequestRuntimeAdapter<'_>)) {
        let dir = tempfile::tempdir().unwrap();
        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        );
        let request = RequestCtx::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(terminal));
        let mut adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);
        run(&mut adapter);
    }

    #[test]
    fn production_env_layout_list_uses_concrete_services() {
        with_adapter(FakeTerminal::new(), |adapter| {
            let panes = adapter.helper_layoutbackend_list_panes();
            assert_eq!(3, panes.len());
            let pane = panes
                .iter()
                .find(|pane| pane["pane_id"] == "w1:p1")
                .unwrap();
            assert_eq!("1", pane["tab_label"]);
            assert_eq!("work", pane["workspace_label"]);

            let tabs = adapter.helper_layoutbackend_list_tabs();
            assert_eq!(3, tabs.len());
            assert_eq!(json!("w1"), tabs[0]["tab_id"]);
            assert_eq!(json!(1), tabs[0]["number"]);
            assert_eq!(json!(true), tabs[0]["active"]);

            let workspaces = adapter.helper_layoutbackend_list_workspaces();
            assert_eq!(2, workspaces.len());
            assert_eq!(json!("w1"), workspaces[0]["workspace_id"]);
            assert_eq!(json!(1), workspaces[0]["number"]);
            assert_eq!(json!("w1"), workspaces[0]["active_tab_id"]);
        });
    }

    #[test]
    fn production_env_layout_pane_uses_concrete_services() {
        with_adapter(FakeTerminal::new(), |adapter| {
            let pane = adapter.helper_layoutbackend_get_pane("w1:p1").unwrap();
            assert_eq!("w1:p1", pane["pane_id"]);
            assert_eq!("work", pane["workspace_label"]);
            assert!(adapter.helper_layoutbackend_get_pane("missing").is_none());
            assert_eq!(
                "tmux server not running - start it with `tmux new -d`",
                adapter.helper_layoutbackend_server_error_hint()
            );
        });
    }

    #[test]
    fn production_env_layout_create_uses_concrete_services() {
        let mut visible_pane_terminal = FakeTerminal::new();
        visible_pane_terminal.show_created_tab_pane = true;
        with_adapter(visible_pane_terminal, |adapter| {
            let workspace = adapter
                .helper_layoutbackend_create_workspace(Some("demo"), None, false)
                .unwrap();
            assert_eq!("demo", workspace["workspace"]["label"]);
            assert_eq!("w101", workspace["workspace"]["workspace_id"]);

            let tab = adapter
                .helper_layoutbackend_create_tab(Some("w1"), Some("notes"), None, true)
                .unwrap();
            assert_eq!("notes", tab["tab"]["label"]);
            assert_eq!("w1", tab["tab"]["workspace_id"]);
            assert_eq!(
                format!("{}:p1", tab["tab"]["tab_id"].as_str().unwrap()),
                tab["root_pane"]["pane_id"]
            );

            let pane = adapter
                .helper_layoutbackend_split_pane("w1:p1", "right", None, false)
                .unwrap();
            assert!(pane["root_pane"]["pane_id"]
                .as_str()
                .unwrap()
                .starts_with("w9:p"));
        });

        let mut failed_workspace = FakeTerminal::new();
        failed_workspace.fail_layout = Some("workspace refused".into());
        with_adapter(failed_workspace, |adapter| {
            assert_eq!(
                Err("workspace refused".to_string()),
                adapter.helper_layoutbackend_create_workspace(None, None, false)
            );
        });

        let mut failed_tab = FakeTerminal::new();
        failed_tab.fail_layout = Some("tab refused".into());
        with_adapter(failed_tab, |adapter| {
            assert_eq!(
                Err("tab refused".to_string()),
                adapter.helper_layoutbackend_create_tab(None, None, None, false)
            );
        });

        let mut invisible_tab = FakeTerminal::new();
        with_adapter(invisible_tab, |adapter| {
            assert_eq!(
                Err("tmux created the window but it is not visible".to_string()),
                adapter.helper_layoutbackend_create_tab(None, None, None, false)
            );
        });

        let mut failed_split = FakeTerminal::new();
        failed_split.fail_layout = Some("split refused".into());
        with_adapter(failed_split, |adapter| {
            assert_eq!(
                Err("split refused".to_string()),
                adapter.helper_layoutbackend_split_pane("w1:p1", "right", None, true)
            );
        });
    }
}
