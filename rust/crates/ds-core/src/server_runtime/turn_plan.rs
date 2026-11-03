//! Concrete request-thread sources for the turn-end dispatch plan.

use super::RequestRuntimeAdapter;

impl RequestRuntimeAdapter<'_> {
    #[allow(dead_code)] // The complete TurnDispatchEnv implementation belongs to its assembly leaf.
    pub(crate) fn helper_turndispatchenv_dispatch_enabled(&self) -> bool {
        self.terminal
            .flock
            .status_state()
            .ok()
            .and_then(|state| state.env.get("DUNKINGSHEEP_TURN_END_DISPATCH"))
            .map(dispatch_enabled_value)
            .unwrap_or(true)
    }

    #[allow(dead_code)] // The complete TurnDispatchEnv implementation belongs to its assembly leaf.
    pub(crate) fn helper_turndispatchenv_unread_count(
        &self,
        pane_id: &str,
    ) -> Result<Option<i64>, String> {
        if pane_id.is_empty() {
            return Err("no mailbox given and no caller pane to default to".into());
        }
        let flock = self.terminal.flock;
        let store = flock
            .stores()
            .inbox
            .ok_or_else(|| "inbox store is not wired".to_owned())?;
        let state = flock.status_state().map_err(|error| error.to_string())?;
        let env = &state.env;
        let mailbox = flock
            .own_mailbox_from_sources(
                &self.terminal,
                || store.read_pool(),
                pane_id,
                || {
                    env.get("DUNKINGSHEEP_MAILBOX")
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(str::to_owned)
                },
                || super::normalize_backend_name(env.get("DUNKINGSHEEP_TERMINAL_BACKEND")),
            )
            .map_err(|error| error.to_string())?;
        let count = crate::inbox::count(
            store.read_pool(),
            &mailbox,
            &self.helper_inboxenv_lane_panes(),
            self.helper_inboxenv_now(),
        )
        .map_err(|error| error.to_string())?;
        Ok(Some(count.unread))
    }

    #[allow(dead_code)] // The complete TurnDispatchEnv implementation belongs to its assembly leaf.
    pub(crate) fn helper_turndispatchenv_claimed_tasks(
        &self,
        pane_id: &str,
    ) -> Result<Vec<serde_json::Value>, String> {
        let flock = self.terminal.flock;
        let tasks = flock
            .stores()
            .tasks
            .ok_or_else(|| "the task store is not attached".to_owned())?;
        let leases = self
            .handler_context()
            .block_on(tasks.leases_for_pane(pane_id, None))
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        leases
            .into_iter()
            .map(|lease| {
                self.handler_context()
                    .block_on(flock.task_read_wire_with_notes(&lease.task_id, Some(pane_id)))
                    .map_err(|error| error.to_string())?
                    .map(|task| task.wire)
                    .map_err(|error| error.to_string())
            })
            .collect()
    }
}

fn dispatch_enabled_value(value: &str) -> bool {
    !matches!(
        value.trim().to_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::{
        config::Env,
        flock::{panes::Stores, status_runtime::StatusState, Flock},
        server::RequestCtx,
        tasks::{claim::ClaimOptions, Tasks},
        terminal::{fakes::FakeTerminal, Terminal},
    };

    use super::*;

    #[test]
    fn production_env_turn_plan_uses_concrete_services() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = Arc::new(
            crate::inbox_store::Store::open(dir.path().join("inbox.db"), dir.path().join("blobs"))
                .unwrap(),
        );
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let tasks = Arc::new(
            runtime
                .block_on(Tasks::open(dir.path().join("tasks.db")))
                .unwrap(),
        );
        runtime.block_on(tasks.allocation_init()).unwrap();
        let task = runtime
            .block_on(tasks.add("global", "held task", "task body"))
            .unwrap();
        runtime
            .block_on(tasks.claim_next("global", "%9", ClaimOptions::default()))
            .unwrap();

        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        );
        let env = Env::from_pairs(&[
            ("DUNKINGSHEEP_MAILBOX", "worker-mailbox"),
            ("DUNKINGSHEEP_TURN_END_DISPATCH", "off"),
        ]);
        flock
            .attach_status_state(StatusState::at_start(
                "test",
                dir.path().join("state.json"),
                dir.path().join("daemon"),
                env,
            ))
            .unwrap();
        flock.attach_stores(Stores {
            tasks: Some(tasks),
            inbox: Some(inbox),
            ..Stores::default()
        });
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let adapter =
            RequestRuntimeAdapter::new(&flock, &RequestCtx::default(), runtime.handle(), &terminal);

        assert!(!adapter.helper_turndispatchenv_dispatch_enabled());
        assert_eq!(
            adapter.helper_turndispatchenv_unread_count("%9"),
            Ok(Some(0))
        );
        let claimed = adapter.helper_turndispatchenv_claimed_tasks("%9").unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0]["id"], task.id);
        assert_eq!(claimed[0]["status"], "in_progress");
        assert_eq!(claimed[0]["title"], "held task");
        assert!(claimed[0].get("blocked_by").is_some());
    }

    #[test]
    fn turn_plan_unread_count_fails_when_the_inbox_store_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        );
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let adapter =
            RequestRuntimeAdapter::new(&flock, &RequestCtx::default(), runtime.handle(), &terminal);

        assert_eq!(
            adapter.helper_turndispatchenv_unread_count("%9"),
            Err("inbox store is not wired".into())
        );
    }

    #[test]
    fn turn_plan_switch_defaults_on_and_accepts_python_false_values() {
        assert!(dispatch_enabled_value("1"));
        for value in ["0", " false ", "NO", "off"] {
            assert!(!dispatch_enabled_value(value), "{value}");
        }
    }
}
