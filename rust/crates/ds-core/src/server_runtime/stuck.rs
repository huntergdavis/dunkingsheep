//! Concrete terminal-backed sources for the dialog observer.

use serde_json::Value;

use crate::error::DunkError;
use crate::flock::lanes::pane_to_json;
use crate::flock::loop_env::LoopFuture;
use crate::flock::sends::CONSUMPTION_READ_LINES;
use crate::server_runtime::LoopRuntimeAdapter;

impl LoopRuntimeAdapter {
    #[allow(dead_code)] // The complete StuckEnv assembly is owned by the dependent runtime leaf.
    pub(crate) fn helper_stuckenv_list_panes<'a>(
        &'a self,
    ) -> LoopFuture<'a, Result<Vec<Value>, DunkError>> {
        Box::pin(async move {
            self.with_loop_terminal(|_, terminal| {
                terminal.with_terminal(|backend| {
                    backend
                        .try_list_panes_with_status()
                        .map(|panes| {
                            panes
                                .iter()
                                .map(|(pane, status)| pane_to_json(pane, status.as_deref()))
                                .collect()
                        })
                        .map_err(|error| DunkError::Refused(error.to_string()))
                })
            })
            .await
            .map_err(|error| {
                DunkError::Refused(format!("stuck observer terminal worker failed: {error}"))
            })?
        })
    }

    #[allow(dead_code)] // The complete StuckEnv assembly is owned by the dependent runtime leaf.
    pub(crate) fn helper_stuckenv_read_screen<'a>(
        &'a self,
        pane_id: &'a str,
    ) -> LoopFuture<'a, Option<String>> {
        let pane_id = pane_id.to_owned();
        Box::pin(async move {
            self.with_loop_terminal(move |_, terminal| {
                terminal.with_terminal(|backend| {
                    backend
                        .read_pane(&pane_id, CONSUMPTION_READ_LINES, true)
                        .ok()
                        .flatten()
                })
            })
            .await
            .ok()
            .flatten()
        })
    }

    #[allow(dead_code)] // The complete StuckEnv assembly is owned by the dependent runtime leaf.
    pub(crate) fn helper_stuckenv_pane_generation<'a>(
        &'a self,
        pane_id: &'a str,
    ) -> LoopFuture<'a, Option<String>> {
        let pane_id = pane_id.to_owned();
        Box::pin(async move {
            self.with_loop_terminal(move |_, terminal| {
                terminal.with_env(|env| crate::flock::sends::pane_generation(env, &pane_id))
            })
            .await
            .ok()
            .flatten()
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::flock::Flock;
    use crate::terminal::fakes::FakeTerminal;
    use crate::terminal::{Pane, Terminal};

    fn make_adapter(pid: i64, unreadable: bool) -> LoopRuntimeAdapter {
        let base = std::env::temp_dir().join(format!("ds-stuck-test-{}", std::process::id()));
        let flock = Arc::new(Flock::new(
            base.join("launches.json"),
            base.join("lanes.json"),
            false,
        ));
        let pane_id = "%stuck-test".to_owned();
        let factory: Arc<dyn Fn() -> Box<dyn Terminal> + Send + Sync> = Arc::new(move || {
            let mut fake = FakeTerminal::new();
            fake.panes = vec![Pane {
                pane_id: pane_id.clone(),
                pane_pid: Some(pid),
                command: "claude".into(),
                ..Pane::default()
            }];
            fake.screens
                .insert(pane_id.clone(), "dialog line\nEsc to cancel\n".to_owned());
            if unreadable {
                fake.unreadable.insert(pane_id.clone());
            }
            Box::new(fake)
        });
        LoopRuntimeAdapter::new(flock, factory)
    }

    #[test]
    fn production_env_stuck_observe_uses_concrete_services() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let live_pid = std::process::id() as i64;
        let adapter = make_adapter(live_pid, false);
        let panes = runtime
            .block_on(adapter.helper_stuckenv_list_panes())
            .unwrap();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0]["pane_id"], "%stuck-test");
        assert_eq!(
            runtime.block_on(adapter.helper_stuckenv_read_screen("%stuck-test")),
            Some("dialog line\nEsc to cancel".to_owned())
        );
        let generation = runtime
            .block_on(adapter.helper_stuckenv_pane_generation("%stuck-test"))
            .expect("the current process has a generation");
        assert!(generation.starts_with(&format!("pid:{live_pid}:start:")));

        let unreadable = make_adapter(live_pid, true);
        assert_eq!(
            runtime.block_on(unreadable.helper_stuckenv_read_screen("%stuck-test")),
            None
        );

        let missing_process = make_adapter(i64::MAX, false);
        assert_eq!(
            runtime.block_on(missing_process.helper_stuckenv_pane_generation("%stuck-test")),
            None
        );
    }
}
