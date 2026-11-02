//! Concrete sources used by the background task push loop.

use std::collections::BTreeSet;

use serde_json::Value;

use super::LoopRuntimeAdapter;
use crate::error::{refusal, DunkError};
use crate::flock::lanes::pane_to_json;
use crate::flock::loop_env::LoopFuture;
use crate::roles::DEFAULT_ROLE;

impl LoopRuntimeAdapter {
    #[allow(dead_code)] // The complete PushEnv implementation belongs to its assembly leaf.
    pub(crate) fn helper_pushenv_list_panes<'a>(
        &'a self,
    ) -> LoopFuture<'a, Result<Vec<Value>, DunkError>> {
        let worker = self.with_loop_terminal(|_, terminal| {
            terminal.with_terminal(|backend| {
                backend
                    .try_list_panes_grouped()
                    .map(|grouped| {
                        grouped
                            .into_values()
                            .flatten()
                            .map(|pane| pane_to_json(&pane, None))
                            .collect()
                    })
                    .map_err(|error| DunkError::Refused(error.to_string()))
            })
        });
        Box::pin(async move {
            worker
                .await
                .map_err(|error| refusal(format!("terminal pane-list worker failed: {error}")))?
        })
    }

    #[allow(dead_code)] // The complete PushEnv implementation belongs to its assembly leaf.
    pub(crate) fn helper_pushenv_caller_role(
        &self,
        pane_id: &str,
        pane_info: Option<&Value>,
    ) -> String {
        self.flock
            .with_roles(|roles, legacy| {
                let role = roles.role_of(Some(pane_id), pane_info)?;
                Ok(
                    if role == roles.default
                        && legacy.is_some_and(|allowlist| allowlist.panes.contains(pane_id))
                    {
                        "owner".to_owned()
                    } else {
                        role
                    },
                )
            })
            .unwrap_or_else(|_| DEFAULT_ROLE.to_owned())
    }

    #[allow(dead_code)] // The complete PushEnv implementation belongs to its assembly leaf.
    pub(crate) fn helper_pushenv_registered_lanes(&self) -> Option<BTreeSet<String>> {
        self.flock
            .with_roles(|roles, _| {
                let described = roles.describe()?;
                let lanes = described.get("lanes").and_then(Value::as_object);
                Ok(lanes.map(|lanes| lanes.keys().cloned().collect()))
            })
            .ok()
            .flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::config::Env;
    use crate::flock::Flock;
    use crate::roles::Roles;
    use crate::terminal::{fakes::FakeTerminal, Terminal};

    #[test]
    fn production_env_push_core_uses_concrete_services() {
        let dir = crate::testsupport::dir("runtime-push-core");
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
                "w2:p2",
                "reviewer",
                Some(2202),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("register reviewer");
        roles
            .register("w1:p1", "worker", Some(1101), None, None, None, None, None)
            .expect("register worker");
        roles
            .register("w3:p3", "worker", Some(9999), None, None, None, None, None)
            .expect("register worker");
        flock.attach_roles(roles, None);

        let adapter = LoopRuntimeAdapter::new(
            Arc::clone(&flock),
            Arc::new(|| Box::new(FakeTerminal::new())),
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let panes = adapter
                .helper_pushenv_list_panes()
                .await
                .expect("concrete terminal pane list");
            assert!(!panes.is_empty());
        });
        assert_eq!(
            adapter.helper_pushenv_caller_role(
                "w2:p2",
                panes.iter().find(|p| p["pane_id"] == "w2:p2")
            ),
            "reviewer"
        );
        assert_eq!(
            adapter.helper_pushenv_caller_role(
                "w1:p1",
                panes.iter().find(|p| p["pane_id"] == "w1:p1")
            ),
            "worker"
        );
        assert_eq!(
            adapter.helper_pushenv_registered_lanes(),
            Some(BTreeSet::from([
                "w1:p1".into(),
                "w2:p2".into(),
                "w3:p3".into()
            ]))
        );
        assert_eq!(
            adapter.helper_pushenv_caller_role(
                "w3:p3",
                panes.iter().find(|p| p["pane_id"] == "w3:p3")
            ),
            DEFAULT_ROLE
        );
        assert_eq!(
            adapter.helper_pushenv_caller_role("w1:p1", None),
            DEFAULT_ROLE
        );

        let unavailable = LoopRuntimeAdapter::new(
            Arc::clone(&flock),
            Arc::new(|| {
                let mut terminal = FakeTerminal::new();
                terminal.fail_grouped_panes = Some(crate::terminal::TerminalError::unavailable(
                    "pane listing failed",
                ));
                Box::new(terminal) as Box<dyn Terminal>
            }),
        );
        let error = runtime
            .block_on(async { unavailable.helper_pushenv_list_panes().await })
            .expect_err("listing failure is surfaced");
        assert!(error.to_string().contains("pane listing failed"));

        let no_roles_flock = Arc::new(Flock::new(
            dir.path().join("no-roles-launches.json"),
            dir.path().join("no-roles.json"),
            false,
        ));
        let without_registry = LoopRuntimeAdapter::new(
            Arc::clone(&no_roles_flock),
            Arc::new(|| Box::new(FakeTerminal::new())),
        );
        assert_eq!(without_registry.helper_pushenv_registered_lanes(), None);
        assert_eq!(
            without_registry.helper_pushenv_caller_role("%unregistered", None),
            DEFAULT_ROLE
        );
        let followers = std::collections::BTreeMap::from([("%follow".into(), "global".into())]);
        let targets = crate::push::push_targets(&followers, None, |_| {
            panic!("unavailable registry must not discover registered lanes")
        });
        assert_eq!(targets.len(), 1);
        assert!(targets.contains_key("%follow"));
    }
}
