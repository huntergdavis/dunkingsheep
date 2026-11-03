//! Reviewer eligibility and process evidence used by the task push loop.

use crate::server_runtime::LoopRuntimeAdapter;

impl LoopRuntimeAdapter {
    #[allow(dead_code)] // The complete PushEnv implementation belongs to its assembly leaf.
    pub(crate) fn helper_pushenv_may_review(&self, pane_id: &str) -> bool {
        self.flock
            .with_roles(|roles, _| roles.may_review(pane_id, Some("reviewer")))
            .unwrap_or(false)
    }

    #[allow(dead_code)] // The complete PushEnv implementation belongs to its assembly leaf.
    pub(crate) fn helper_pushenv_provider_of(&self, pane_id: &str) -> Option<String> {
        self.flock
            .with_roles(|roles, _| roles.provider_of(pane_id))
            .ok()
            .flatten()
    }

    #[allow(dead_code)] // The complete PushEnv implementation belongs to its assembly leaf.
    pub(crate) fn helper_pushenv_lane_agent_alive(&self, pane_id: &str) -> Option<bool> {
        let Some(registered_pane_pid) = self
            .flock
            .with_roles(|roles, _| {
                Ok(roles.entry(pane_id)?.map(|entry| {
                    entry.get("pane_pid").and_then(|pid| {
                        pid.as_i64()
                            .or_else(|| pid.as_str().and_then(|text| text.parse().ok()))
                    })
                }))
            })
            .ok()?
        else {
            return None;
        };
        let pane_pid = registered_pane_pid.or_else(|| {
            let terminal_factory = std::sync::Arc::clone(&self.terminal_factory);
            let pane_id = pane_id.to_owned();
            std::thread::Builder::new()
                .name("push-lane-pane-info".into())
                .spawn(move || {
                    (terminal_factory)()
                        .get_pane(&pane_id)
                        .and_then(|pane| pane.pane_pid)
                })
                .ok()?
                .join()
                .ok()
                .flatten()
        })?;
        super::delivery::lane_agent_alive_for_pane_pid(pane_pid)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::Env;
    use crate::flock::Flock;
    use crate::roles::Roles;
    use crate::terminal::fakes::FakeTerminal;

    #[test]
    fn production_env_push_eligibility_uses_concrete_services() {
        let dir = crate::testsupport::dir("runtime-push-eligibility");
        let flock = Arc::new(Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        ));
        let mut roles = Roles::new(
            &Env::default(),
            Some(dir.path().join("roles.json")),
            Some(dir.path().join("tokens.json")),
            None,
        )
        .expect("roles store");
        roles
            .register(
                "w1:p1",
                "reviewer",
                None,
                None,
                Some("CLAUDE"),
                None,
                None,
                None,
            )
            .expect("register allowed reviewer");
        roles
            .register(
                "w2:p2",
                "reviewer",
                None,
                None,
                Some("nvidia"),
                None,
                None,
                None,
            )
            .expect("register ineligible reviewer");
        roles
            .register(
                "w3:p3",
                "worker",
                Some(i64::from(std::process::id())),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("register live process root");
        flock.attach_roles(roles, None);

        let adapter = LoopRuntimeAdapter::new(
            Arc::clone(&flock),
            Arc::new(|| Box::new(FakeTerminal::new())),
        );
        assert!(adapter.helper_pushenv_may_review("w1:p1"));
        assert_eq!(
            adapter.helper_pushenv_provider_of("w1:p1"),
            Some("claude".into())
        );
        assert!(!adapter.helper_pushenv_may_review("w2:p2"));
        assert_eq!(
            adapter.helper_pushenv_provider_of("w2:p2"),
            Some("nvidia".into())
        );
        assert_eq!(adapter.helper_pushenv_provider_of("w9:p9"), None);

        // A registered root with an available process table and no agent evidence is dead;
        // an unknown pane has no PID evidence and remains unknown.
        assert_eq!(
            adapter.helper_pushenv_lane_agent_alive("w3:p3"),
            Some(false)
        );
        assert!(adapter.helper_pushenv_lane_agent_alive("w1:p1").is_some());
        assert_eq!(adapter.helper_pushenv_lane_agent_alive("w9:p9"), None);
        let active_agent = [crate::push::ChildProc {
            comm: "codex".into(),
            state: "S".into(),
        }];
        assert_eq!(
            crate::push::lane_agent_alive(Some(&active_agent), || None),
            Some(true)
        );
        assert_eq!(crate::push::lane_agent_alive(None, || None), None);
    }
}
