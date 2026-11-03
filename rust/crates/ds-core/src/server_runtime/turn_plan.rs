//! Concrete request-thread sources for the turn-end dispatch plan.

use std::collections::BTreeMap;

use serde_json::Value;

use super::{RequestRuntimeAdapter, TerminalCallAdapter};
use crate::config::Env;
use crate::dispatch::{self, TurnEnd};
use crate::flock::clear_box::ClearBoxEnv;
use crate::flock::commands_turns::Readiness;

impl RequestRuntimeAdapter<'_> {
    #[allow(dead_code)] // The complete TurnDispatchEnv implementation belongs to its assembly leaf.
    pub(crate) fn helper_turndispatchenv_turn_end_plan(
        &self,
        pane_id: &str,
        tasks: &[Value],
        unread: Option<i64>,
        last_nudge_at: Option<f64>,
        task_nudges: &BTreeMap<String, f64>,
        now: f64,
    ) -> Value {
        dispatch::turn_end_plan(&TurnEnd {
            pane_id,
            tasks,
            unread,
            last_nudge_at,
            task_nudges: task_nudges.clone(),
            now: Some(now),
            ..TurnEnd::default()
        })
    }

    #[allow(dead_code)] // The complete TurnDispatchEnv implementation belongs to its assembly leaf.
    pub(crate) fn helper_turndispatchenv_pending_outbox(&self, pane_id: &str) -> Vec<String> {
        self.terminal
            .flock
            .outbox_snapshot()
            .remove(pane_id)
            .unwrap_or_default()
            .into_iter()
            .filter(|message| message.status == "queued")
            .map(|message| message.id)
            .collect()
    }

    #[allow(dead_code)] // The complete TurnDispatchEnv implementation belongs to its assembly leaf.
    pub(crate) fn helper_turndispatchenv_readiness(
        &self,
        pane_id: &str,
        unread: Option<i64>,
    ) -> Result<Readiness, String> {
        let env = crate::tasks::next_job_runtime::process_gate_env()
            .map_err(|_| "dispatch configuration environment is unreadable".to_owned())?;
        self.helper_turndispatchenv_readiness_with_env(pane_id, unread, &env)
    }

    fn helper_turndispatchenv_readiness_with_env(
        &self,
        pane_id: &str,
        unread: Option<i64>,
        env: &Env,
    ) -> Result<Readiness, String> {
        let flock = self.terminal.flock;
        let lanes = flock
            .resolve_lanes(self, crate::clock::now_s())
            .map_err(|error| error.to_string())?;
        let entries = crate::tasks::next_job_runtime::dispatch_entries(env)
            .map_err(|_| "dispatch lane configuration is unreadable".to_owned())?;
        let allowed_lanes = crate::tasks::next_job_runtime::dispatch_allowlist(&entries, &lanes);
        let allowed = allowed_lanes.contains(pane_id);

        let known_target = lanes.iter().find(|lane| {
            lane.get("pane_id").and_then(Value::as_str) == Some(pane_id)
                && lane
                    .get("agent_status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| status != "unknown")
        });
        let candidate = if known_target.is_some() {
            let path = crate::panes::default_path(env).map_err(|error| error.to_string())?;
            !crate::tasks::next_job_runtime::retirement_status(pane_id, &path)
                .map_err(|_| "retirement state is unreadable".to_owned())?
        } else {
            false
        };

        let candidate_lanes: Vec<Value> = if candidate {
            known_target.cloned().into_iter().collect()
        } else {
            Vec::new()
        };
        let plan = dispatch::lane_plan(
            &candidate_lanes,
            &|lane| {
                let status = lane.get("agent_status").and_then(Value::as_str);
                flock.delivery_state(&self.terminal, pane_id, status, Some(lane), None)
            },
            &|_| unread.unwrap_or(0),
            Some(&allowed_lanes),
        );
        let ready = plan["ready"]
            .as_array()
            .is_some_and(|lanes| !lanes.is_empty());
        let not_ready_reason = ["has_mail", "blocked", "dead", "working"]
            .into_iter()
            .find(|bucket| {
                plan[*bucket]
                    .as_array()
                    .is_some_and(|lanes| !lanes.is_empty())
            })
            .map(str::to_owned);

        Ok(Readiness {
            allowed,
            candidate,
            ready,
            not_ready_reason,
        })
    }
}

impl ClearBoxEnv for TerminalCallAdapter<'_> {
    fn server_error_hint(&self) -> String {
        self.with_terminal(|terminal| terminal.server_error_hint())
    }
}

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
                || {
                    Ok(super::normalize_backend_name(
                        env.get("DUNKINGSHEEP_TERMINAL_BACKEND"),
                    ))
                },
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
    use std::{
        collections::BTreeMap,
        path::Path,
        sync::{Arc, Mutex},
    };

    use crate::{
        config::Env,
        flock::{panes::Stores, status_runtime::StatusState, Flock},
        server::RequestCtx,
        tasks::{ClaimOptions, Tasks},
        terminal::{fakes::FakeTerminal, Terminal, TerminalError},
    };

    use super::*;

    fn with_adapter(
        terminal_backend: FakeTerminal,
        run: impl FnOnce(&RequestRuntimeAdapter<'_>, &Path, &tokio::runtime::Runtime),
    ) {
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
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(terminal_backend));
        let request = RequestCtx::default();
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);
        run(&adapter, dir.path(), &runtime);
    }

    fn gate_env(dir: &Path, lanes: Option<&str>) -> Env {
        let dir = dir.to_string_lossy().into_owned();
        match lanes {
            Some(lanes) => Env::from_pairs(&[
                ("DUNKINGSHEEP_DIR", dir.as_str()),
                ("DUNKINGSHEEP_DISPATCH_LANES", lanes),
            ]),
            None => Env::from_pairs(&[("DUNKINGSHEEP_DIR", dir.as_str())]),
        }
    }

    fn active_terminal() -> FakeTerminal {
        let mut terminal = FakeTerminal::new();
        terminal.statuses.insert("w1:p1".into(), "idle".into());
        terminal
    }

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
        let request = RequestCtx::default();
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);

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
        let request = RequestCtx::default();
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);

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

    #[test]
    fn production_env_turn_decide_uses_concrete_services() {
        with_adapter(FakeTerminal::new(), |adapter, dir, runtime| {
            let tasks = Arc::new(runtime.block_on(Tasks::open(dir.join("tasks.db"))).unwrap());
            runtime.block_on(tasks.allocation_init()).unwrap();
            let task = runtime
                .block_on(tasks.add("global", "turn-end task", "body"))
                .unwrap();
            runtime
                .block_on(tasks.claim_next("global", "%9", ClaimOptions::default()))
                .unwrap();
            adapter.terminal.flock.attach_stores(Stores {
                tasks: Some(tasks),
                ..Stores::default()
            });

            let claimed = adapter.helper_turndispatchenv_claimed_tasks("%9").unwrap();
            let plan = adapter.helper_turndispatchenv_turn_end_plan(
                "%9",
                &claimed,
                Some(0),
                None,
                &BTreeMap::new(),
                10_000.0,
            );
            assert_eq!(plan["action"], "nudge");
            assert_eq!(plan["item"]["id"], task.id);
            assert_eq!(plan["reason"], Value::Null);
        });
    }

    #[test]
    fn production_env_turn_ready_uses_concrete_services() {
        with_adapter(active_terminal(), |adapter, dir, _| {
            let allowed = gate_env(dir, Some("w1:p1"));
            let readiness = adapter
                .helper_turndispatchenv_readiness_with_env("w1:p1", Some(0), &allowed)
                .unwrap();
            assert!(readiness.allowed);
            assert!(readiness.candidate);
            assert!(readiness.ready);
            assert_eq!(readiness.not_ready_reason, None);

            let denied = gate_env(dir, Some("w2:p2"));
            let readiness = adapter
                .helper_turndispatchenv_readiness_with_env("w1:p1", Some(0), &denied)
                .unwrap();
            assert!(!readiness.allowed);
            assert!(readiness.candidate);
            assert!(!readiness.ready);
            assert_eq!(readiness.not_ready_reason, None);
        });

        for (status, screen, unread, expected_reason) in [
            ("idle", None, Some(1), "has_mail"),
            (
                "idle",
                Some("Goal paused: wait for input\n› Ask Codex to do anything\n"),
                Some(0),
                "blocked",
            ),
            ("idle", Some("Session expired\n"), Some(0), "dead"),
            ("busy", None, Some(0), "working"),
        ] {
            let mut terminal = FakeTerminal::new();
            terminal.statuses.insert("w1:p1".into(), status.into());
            if let Some(screen) = screen {
                terminal.screens.insert("w1:p1".into(), screen.into());
            }
            with_adapter(terminal, |adapter, dir, _| {
                let env = gate_env(dir, Some("w1:p1"));
                let readiness = adapter
                    .helper_turndispatchenv_readiness_with_env("w1:p1", unread, &env)
                    .unwrap();
                assert!(readiness.allowed);
                assert!(readiness.candidate);
                assert!(!readiness.ready);
                assert_eq!(readiness.not_ready_reason.as_deref(), Some(expected_reason));
            });
        }
    }

    #[test]
    fn turn_readiness_rejects_missing_retired_and_unknown_status_candidates() {
        with_adapter(active_terminal(), |adapter, dir, _| {
            let missing_env = gate_env(dir, Some("missing"));
            let missing = adapter
                .helper_turndispatchenv_readiness_with_env("missing", Some(0), &missing_env)
                .unwrap();
            assert!(missing.allowed);
            assert!(!missing.candidate);

            let path = crate::panes::default_path(&missing_env).unwrap();
            crate::panes::retire(
                "w1:p1",
                "fixture retirement",
                None,
                &[],
                &[],
                &path,
                Some(1.0),
            )
            .unwrap();
            let retired_env = gate_env(dir, Some("w1:p1"));
            let retired = adapter
                .helper_turndispatchenv_readiness_with_env("w1:p1", Some(0), &retired_env)
                .unwrap();
            assert!(retired.allowed);
            assert!(!retired.candidate);
        });

        with_adapter(FakeTerminal::new(), |adapter, dir, _| {
            let env = gate_env(dir, Some("w1:p1"));
            let unknown = adapter
                .helper_turndispatchenv_readiness_with_env("w1:p1", Some(0), &env)
                .unwrap();
            assert!(unknown.allowed);
            assert!(!unknown.candidate);
            assert!(!unknown.ready);
        });
    }

    #[test]
    fn turn_readiness_preserves_source_failures() {
        let mut listing_failure = FakeTerminal::new();
        listing_failure.fail_grouped_panes =
            Some(TerminalError::refused("fixture listing failure"));
        with_adapter(listing_failure, |adapter, dir, _| {
            let env = gate_env(dir, Some("w1:p1"));
            let error = adapter
                .helper_turndispatchenv_readiness_with_env("w1:p1", Some(0), &env)
                .unwrap_err();
            assert!(error.contains("cannot list panes"));
            assert!(error.contains("fixture listing failure"));
        });

        with_adapter(active_terminal(), |adapter, dir, _| {
            std::fs::write(dir.join("dispatch_lanes.json"), b"{").unwrap();
            let env = gate_env(dir, None);
            let error = adapter
                .helper_turndispatchenv_readiness_with_env("w1:p1", Some(0), &env)
                .unwrap_err();
            assert_eq!(error, "dispatch lane configuration is unreadable");
        });

        with_adapter(active_terminal(), |adapter, dir, _| {
            let env = gate_env(dir, Some("w1:p1"));
            let path = crate::panes::default_path(&env).unwrap();
            std::fs::write(path, b"not json").unwrap();
            let error = adapter
                .helper_turndispatchenv_readiness_with_env("w1:p1", Some(0), &env)
                .unwrap_err();
            assert_eq!(error, "retirement state is unreadable");
        });

        let mut unreadable = active_terminal();
        unreadable.unreadable.insert("w1:p1".into());
        with_adapter(unreadable, |adapter, dir, _| {
            let env = gate_env(dir, Some("w1:p1"));
            let readiness = adapter
                .helper_turndispatchenv_readiness_with_env("w1:p1", Some(0), &env)
                .unwrap();
            assert!(readiness.candidate);
            assert!(!readiness.ready);
            assert_eq!(readiness.not_ready_reason.as_deref(), Some("blocked"));
        });
    }

    #[test]
    fn pending_outbox_only_returns_queued_ids_in_queue_order() {
        with_adapter(FakeTerminal::new(), |adapter, _, _| {
            let flock = adapter.terminal.flock;
            let mut first =
                crate::flock::sends::Message::new("m1", "w1:p1", "one", "one", "send_text");
            first.holds = 2;
            let second =
                crate::flock::sends::Message::new("m2", "w1:p1", "two", "two", "send_text");
            let mut sent =
                crate::flock::sends::Message::new("m3", "w1:p1", "three", "three", "send_text");
            sent.status = "sent".into();
            let other =
                crate::flock::sends::Message::new("m4", "w2:p2", "other", "other", "send_text");
            let mut cancelled =
                crate::flock::sends::Message::new("m5", "w1:p1", "five", "five", "send_text");
            cancelled.status = "cancelled".into();
            flock
                .outbox
                .lock()
                .unwrap()
                .insert("w1:p1".into(), vec![first, second, sent, cancelled]);
            flock
                .outbox
                .lock()
                .unwrap()
                .insert("w2:p2".into(), vec![other]);

            assert_eq!(
                adapter.helper_turndispatchenv_pending_outbox("w1:p1"),
                ["m1", "m2"]
            );
            assert_eq!(
                adapter.helper_turndispatchenv_pending_outbox("w2:p2"),
                ["m4"]
            );
            assert!(adapter
                .helper_turndispatchenv_pending_outbox("missing")
                .is_empty());
        });
    }
}
