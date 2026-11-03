//! Concrete terminal and role sources for retiring a pane.

use serde_json::Value;

use super::RequestRuntimeAdapter;
use crate::{error::DunkError, flock::panes::PaneInfoEnv};

impl RequestRuntimeAdapter<'_> {
    #[allow(dead_code)] // The complete RetireEnv implementation belongs to its assembly leaf.
    pub(crate) fn helper_retireenv_panes(&self) -> Vec<Value> {
        self.terminal
            .with_env(|env| PaneInfoEnv::list_panes_grouped(env))
    }

    #[allow(dead_code)] // The complete RetireEnv implementation belongs to its assembly leaf.
    pub(crate) fn helper_retireenv_server_error_hint(&self) -> String {
        self.terminal
            .with_terminal(|terminal| terminal.server_error_hint())
    }

    #[allow(dead_code)] // The complete RetireEnv implementation belongs to its assembly leaf.
    pub(crate) fn helper_retireenv_require_superuser(
        &self,
        self_pane_id: Option<&str>,
        action: &str,
    ) -> Result<(), DunkError> {
        self.terminal
            .flock
            .require_superuser(self_pane_id, action, &self.terminal)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use crate::{
        config::Env,
        flock::Flock,
        roles::Roles,
        server::RequestCtx,
        terminal::{fakes::FakeTerminal, Pane, Terminal},
    };

    use super::*;

    #[test]
    fn production_env_retire_roles_uses_concrete_services() {
        let dir = tempfile::tempdir().unwrap();
        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        );
        let mut roles = Roles::new(
            &Env::default(),
            Some(dir.path().join("roles.json")),
            Some(dir.path().join("tokens.json")),
            None,
        )
        .unwrap();
        roles
            .register("%3", "worker", None, None, None, None, None, None)
            .unwrap();
        roles
            .register("%4", "owner", None, None, None, None, None, None)
            .unwrap();
        flock.attach_roles(roles, None);

        let mut backend = FakeTerminal::new();
        backend.panes.clear();
        backend.panes.push(Pane {
            pane_id: "%3".into(),
            session_id: "w1".into(),
            window_id: "w1".into(),
            label: "writer".into(),
            tab: "code".into(),
            workspace: "project".into(),
            ..Pane::default()
        });
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(backend));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let request = RequestCtx::default();
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);

        let panes = adapter.helper_retireenv_panes();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0]["pane_id"], "%3");
        assert_eq!(panes[0]["tab_label"], "code");
        assert_eq!(
            adapter.helper_retireenv_server_error_hint(),
            "tmux server not running - start it with `tmux new -d`"
        );
        let denied = adapter
            .helper_retireenv_require_superuser(Some("%3"), "retire_pane")
            .unwrap_err();
        assert!(denied
            .to_string()
            .contains("retire_pane requires a superuser"));
    }
}
