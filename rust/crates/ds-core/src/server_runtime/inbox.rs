//! Concrete inbox routing sources for the request runtime adapter.

use super::RequestRuntimeAdapter;
use crate::flock::commands_inbox::RouteEntry;

impl RequestRuntimeAdapter<'_> {
    #[allow(dead_code)] // The complete InboxEnv assembly is owned by its dependent leaf.
    pub(crate) fn helper_inboxenv_lane_panes(&self) -> Vec<String> {
        let mut panes = self
            .helper_inboxenv_pm_routing()
            .into_iter()
            .flat_map(|entry| entry.members)
            .collect::<Vec<_>>();
        panes.sort();
        panes.dedup();
        panes
    }

    #[allow(dead_code)] // The complete InboxEnv assembly is owned by its dependent leaf.
    pub(crate) fn helper_inboxenv_pm_routing(&self) -> Vec<RouteEntry> {
        let Ok(state) = self.terminal.flock.status_state() else {
            return Vec::new();
        };
        crate::pm::load_routing(None, None, &state.env)
            .unwrap_or_default()
            .into_iter()
            .map(|(project, entry)| RouteEntry {
                project,
                pm: entry.pm,
                members: entry.members,
                mode: entry.mode,
                vp_pane: entry.vp_pane,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::config::Env;
    use crate::flock::{status_runtime::StatusState, Flock};
    use crate::server::RequestCtx;
    use crate::terminal::{fakes::FakeTerminal, Terminal};

    #[test]
    fn production_env_inbox_routing_uses_concrete_services() {
        let dir = tempfile::tempdir().unwrap();
        let routing_path = dir.path().join("pm-routing.json");
        std::fs::write(
            &routing_path,
            r#"{
                "alpha": {"pm": "%1", "members": ["%3", "%2", "%3"], "mode": "redirect", "vp_pane": "%0"},
                "beta": {"pm": "%4", "members": ["%5"], "mode": "flag", "vp_pane": "%0"}
            }"#,
        )
        .unwrap();
        let routing_path = routing_path.to_string_lossy().into_owned();
        let env = Env::from_pairs(&[("DUNKINGSHEEP_PM_ROUTING", &routing_path)]);
        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        );
        let executable = dir.path().join("daemon");
        std::fs::write(&executable, b"fixture").unwrap();
        flock
            .attach_status_state(StatusState::at_start(
                "test",
                dir.path().join("state.json"),
                executable,
                env,
            ))
            .unwrap();
        let request = RequestCtx::default();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);

        let routing = adapter.helper_inboxenv_pm_routing();
        assert_eq!(2, routing.len());
        assert_eq!("alpha", routing[0].project);
        assert_eq!("%1", routing[0].pm);
        assert_eq!(vec!["%3", "%2", "%3"], routing[0].members);
        assert_eq!("redirect", routing[0].mode);
        assert_eq!("%0", routing[0].vp_pane);
        assert_eq!(vec!["%2", "%3", "%5"], adapter.helper_inboxenv_lane_panes());

        let unreadable_path = dir.path().join("unreadable.json");
        std::fs::write(&unreadable_path, b"not valid JSON").unwrap();
        let unreadable_path = unreadable_path.to_string_lossy().into_owned();
        let unreadable_env = Env::from_pairs(&[("DUNKINGSHEEP_PM_ROUTING", &unreadable_path)]);
        let unreadable_flock = Flock::new(
            dir.path().join("unreadable-launches.json"),
            dir.path().join("unreadable-lanes.json"),
            false,
        );
        unreadable_flock
            .attach_status_state(StatusState::at_start(
                "test",
                dir.path().join("unreadable-state.json"),
                dir.path().join("daemon"),
                unreadable_env,
            ))
            .unwrap();
        let unreadable_adapter =
            RequestRuntimeAdapter::new(&unreadable_flock, &request, runtime.handle(), &terminal);
        assert!(unreadable_adapter.helper_inboxenv_pm_routing().is_empty());
        assert!(unreadable_adapter.helper_inboxenv_lane_panes().is_empty());
    }
}
