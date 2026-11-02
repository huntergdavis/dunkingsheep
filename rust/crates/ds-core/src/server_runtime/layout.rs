//! Concrete terminal-backed sources for the read-only layout adapter helpers.

use serde_json::{json, Value};

use crate::flock::mailbox::MailboxEnv;
use crate::flock::panes::PaneInfoEnv;
use crate::server_runtime::RequestRuntimeAdapter;

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
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::flock::Flock;
    use crate::server::RequestCtx;
    use crate::terminal::{fakes::FakeTerminal, Terminal};

    fn with_adapter(run: impl FnOnce(&RequestRuntimeAdapter<'_>)) {
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
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);
        run(&adapter);
    }

    #[test]
    fn production_env_layout_list_uses_concrete_services() {
        with_adapter(|adapter| {
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
        with_adapter(|adapter| {
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
}
