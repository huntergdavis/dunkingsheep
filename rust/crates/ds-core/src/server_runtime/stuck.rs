//! Concrete terminal-backed sources for the dialog observer.

use serde_json::Value;

use crate::error::DunkError;
use crate::flock::lanes::pane_to_json;
use crate::flock::loop_env::LoopFuture;
use crate::flock::sends::CONSUMPTION_READ_LINES;
use crate::server::RequestCtx;
use crate::server_runtime::{LoopRuntimeAdapter, RequestRuntimeAdapter};

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

    #[allow(dead_code)] // The complete StuckEnv assembly is owned by the dependent runtime leaf.
    pub(crate) fn helper_stuckenv_note_stale_panel<'a>(
        &'a self,
        pane_id: &'a str,
        signature_text: &'a str,
        since: f64,
    ) -> LoopFuture<'a, ()> {
        let Some(services) = self.runtime_services.clone() else {
            return Box::pin(async {
                eprintln!("stuck observer refused: canonical runtime services are not bound");
            });
        };
        let pane_id = pane_id.to_owned();
        let worker_pane_id = pane_id.clone();
        let signature_text = signature_text.to_owned();
        Box::pin(async move {
            let worker_services = std::sync::Arc::clone(&services);
            let worker = self.with_loop_terminal(move |flock, terminal| {
                if !std::ptr::eq(flock, worker_services.flock.as_ref())
                    || !std::ptr::eq(flock, worker_services.outbox.flock.as_ref())
                {
                    eprintln!("stuck observer refused: canonical runtime flock mismatch");
                    return;
                }
                let request = RequestCtx::default();
                let request_adapter = RequestRuntimeAdapter::new(
                    flock,
                    &request,
                    terminal.runtime,
                    terminal.terminal,
                );
                let inbox = match super::inbox_data::InboxRuntimeView::new(
                    request_adapter,
                    std::sync::Arc::clone(&worker_services.flock),
                    std::sync::Arc::clone(&worker_services.outbox.pool),
                ) {
                    Ok(inbox) => inbox,
                    Err(error) => {
                        eprintln!("stuck observer inbox services unavailable: {error}");
                        return;
                    }
                };
                let generation = crate::flock::sends::pane_generation(terminal, &worker_pane_id);
                let mut reporter = worker_services.stuck_reporter.clone();
                reporter.note(
                    flock,
                    &inbox,
                    &worker_pane_id,
                    "modal",
                    generation.as_deref(),
                    Some(&signature_text),
                    since,
                );
            });
            if let Err(error) = worker.await {
                eprintln!("stuck observer alert worker failed for {pane_id}: {error}");
            }
        })
    }

    #[allow(dead_code)] // The complete StuckEnv assembly is owned by the dependent runtime leaf.
    pub(crate) fn helper_stuckenv_closed(&self) -> bool {
        self.runtime_services
            .as_ref()
            .is_none_or(|services| services.is_stopped())
    }

    #[allow(dead_code)] // The complete StuckEnv assembly is owned by the dependent runtime leaf.
    pub(crate) fn helper_stuckenv_stuck_recovery_enabled(&self) -> bool {
        self.flock
            .stuck_config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .enabled
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use super::*;
    use crate::config::Env;
    use crate::flock::{panes::Stores, Flock};
    use crate::roles::Roles;
    use crate::server_runtime::{RuntimeObservability, RuntimeServiceInputs, RuntimeServices};
    use crate::superusers::Superusers;
    use crate::tasks::Tasks;
    use crate::terminal::fakes::FakeTerminal;
    use crate::terminal::{Pane, Terminal};

    fn make_adapter(pid: i64, unreadable: bool) -> (LoopRuntimeAdapter, tempfile::TempDir) {
        let dir = crate::testsupport::dir("stuck-observe");
        let flock = Arc::new(Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
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
        (LoopRuntimeAdapter::new(flock, factory), dir)
    }

    fn make_runtime_services(
        dir: &Path,
        runtime: tokio::runtime::Runtime,
        persist_enabled: bool,
        terminal_factory: Option<crate::flock::loop_env::LoopTerminalFactory>,
    ) -> Arc<RuntimeServices> {
        let env = Env::from_pairs(&[(
            "DUNKINGSHEEP_DIR",
            dir.to_str().expect("test directory is UTF-8"),
        )]);
        let tasks = Arc::new(
            runtime
                .block_on(Tasks::open(dir.join("tasks.db")))
                .expect("tasks store"),
        );
        let inbox = Arc::new(
            crate::inbox_store::Store::open(dir.join("inbox.db"), dir.join("blobs"))
                .expect("inbox store"),
        );
        let pid = i64::from(std::process::id());
        let mut roles = Roles::new(
            &env,
            Some(dir.join("roles.json")),
            Some(dir.join("tokens.json")),
            None,
        )
        .expect("roles");
        roles
            .register(
                "w1:p1",
                "worker",
                Some(pid),
                crate::proc::process_start(pid).as_deref(),
                Some("codex"),
                None,
                None,
                None,
            )
            .expect("worker role");
        roles
            .register("w9:pH", "owner", None, None, None, None, None, Some("VP"))
            .expect("VP role");
        let executable = dir.join("daemon-executable");
        std::fs::write(&executable, b"test daemon identity").expect("executable fixture");
        let factory = terminal_factory.unwrap_or_else(|| {
            Arc::new(|| {
                let mut terminal = FakeTerminal::new();
                terminal.panes.push(Pane {
                    pane_id: "%stuck-test".into(),
                    pane_pid: Some(i64::from(std::process::id())),
                    command: "codex".into(),
                    ..Pane::default()
                });
                terminal.panes.push(Pane {
                    pane_id: "w9:pH".into(),
                    command: "codex".into(),
                    ..Pane::default()
                });
                Box::new(terminal)
            })
        });
        let state_dir = dir.join("state-dir");
        RuntimeServices::new(RuntimeServiceInputs {
            launches_file: dir.join("launches.json"),
            lane_registry_file: dir.join("lanes.json"),
            persist_enabled,
            stores: Stores {
                tasks: Some(tasks),
                inbox: Some(inbox),
                ..Stores::default()
            },
            roles,
            legacy_superusers: Some(
                Superusers::new(dir.join("superusers.json"), true).expect("superusers"),
            ),
            release_version: "test".into(),
            state_file: dir.join("whole-state.json"),
            executable,
            env,
            observability: Some(RuntimeObservability {
                state_dir,
                memory_db: Some(dir.join("memory.db")),
            }),
            runtime,
            terminal_factory: factory,
        })
        .expect("canonical runtime services")
    }

    #[test]
    fn production_env_stuck_observe_uses_concrete_services() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let live_pid = std::process::id() as i64;
        let (adapter, _adapter_dir) = make_adapter(live_pid, false);
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

        let (unreadable, _unreadable_dir) = make_adapter(live_pid, true);
        assert_eq!(
            runtime.block_on(unreadable.helper_stuckenv_read_screen("%stuck-test")),
            None
        );

        let (missing_process, _missing_process_dir) = make_adapter(i64::MAX, false);
        assert_eq!(
            runtime.block_on(missing_process.helper_stuckenv_pane_generation("%stuck-test")),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn production_env_stuck_sink_uses_concrete_services() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        const CHILD: &str = "DS_STUCK_SINK_TEST_CHILD";
        const TEST_NAME: &str =
            "server_runtime::stuck::tests::production_env_stuck_sink_uses_concrete_services";
        if std::env::var_os(CHILD).is_none() {
            let config = tempfile::tempdir().expect("isolated alert config");
            let owner_calls = config.path().join("owner-calls");
            let owner = config.path().join("fake-vpctl");
            std::fs::write(
                &owner,
                format!(
                    "#!/bin/sh\ncount=0\nif [ -f '{}' ]; then count=$(cat '{}'); fi\ncount=$((count + 1))\nprintf '%s' \"$count\" > '{}'\n[ \"$count\" -eq 1 ] && exit 1\nexit 0\n",
                    owner_calls.display(),
                    owner_calls.display(),
                    owner_calls.display()
                ),
            )
            .expect("fake owner process");
            let mut permissions = std::fs::metadata(&owner)
                .expect("fake owner metadata")
                .permissions();
            permissions.set_mode(0o700);
            std::fs::set_permissions(&owner, permissions).expect("executable fake owner");
            let output = Command::new(std::env::current_exe().expect("test binary"))
                .args(["--exact", TEST_NAME, "--nocapture"])
                .env(CHILD, "1")
                .env("DUNKINGSHEEP_DIR", config.path())
                .env("DUNKINGSHEEP_VPCTL", &owner)
                .env("DUNKINGSHEEP_VP_PANE", "w9:pH")
                .output()
                .expect("isolated child test");
            assert!(
                output.status.success(),
                "child test failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(
                std::fs::read_to_string(owner_calls).expect("owner call count"),
                "2",
                "the failed owner sink is retried once"
            );
            return;
        }

        let dir = tempfile::tempdir().expect("runtime state");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let services = make_runtime_services(dir.path(), runtime, false, None);
        let adapter = services
            .stuck_loop_adapter()
            .expect("canonical stuck adapter");
        assert!(!adapter.helper_stuckenv_closed());
        services.flock.stuck_config.lock().unwrap().enabled = false;
        assert!(!adapter.helper_stuckenv_stuck_recovery_enabled());
        services.flock.stuck_config.lock().unwrap().enabled = true;
        assert!(adapter.helper_stuckenv_stuck_recovery_enabled());

        let now = crate::clock::now_s();
        services
            .runtime
            .block_on(adapter.helper_stuckenv_note_stale_panel(
                "%stuck-test",
                "Allow command?",
                now + 10.0,
            ));
        let owner_calls = std::env::var("DUNKINGSHEEP_DIR")
            .map(std::path::PathBuf::from)
            .expect("isolated alert config")
            .join("owner-calls");
        assert!(!owner_calls.exists(), "modal delay suppresses alert sinks");

        let missing_identity_factory: crate::flock::loop_env::LoopTerminalFactory =
            Arc::new(|| {
                let mut terminal = FakeTerminal::new();
                terminal.panes.push(Pane {
                    pane_id: "%stuck-test".into(),
                    pane_pid: Some(i64::MAX),
                    command: "codex".into(),
                    ..Pane::default()
                });
                Box::new(terminal)
            });
        let missing_identity =
            LoopRuntimeAdapter::new(Arc::clone(&services.flock), missing_identity_factory)
                .with_stuck_services(Arc::clone(&services))
                .expect("same canonical services");
        services
            .runtime
            .block_on(missing_identity.helper_stuckenv_note_stale_panel(
                "%stuck-test",
                "Allow command?",
                now - 31.0,
            ));
        assert!(
            !owner_calls.exists(),
            "missing process identity refuses alert sinks"
        );

        services
            .runtime
            .block_on(adapter.helper_stuckenv_note_stale_panel(
                "%stuck-test",
                "Allow command?",
                now - 31.0,
            ));
        assert_eq!(
            std::fs::read_to_string(&owner_calls).expect("first owner attempt"),
            "1"
        );
        let inbox_db = rusqlite::Connection::open(dir.path().join("inbox.db"))
            .expect("canonical inbox database");
        let vp_messages: i64 = inbox_db
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE sender = 'watchdog'",
                [],
                |row| row.get(0),
            )
            .expect("count durable VP alerts");
        assert_eq!(vp_messages, 1, "the real inbox accepted the VP alert");
        drop(inbox_db);
        let alert_db = std::path::PathBuf::from(std::env::var("DUNKINGSHEEP_DIR").unwrap())
            .join("dialog-alerts.db");
        let connection = rusqlite::Connection::open(alert_db).expect("alert database");
        connection
            .execute(
                "UPDATE alerts SET last_attempt = ?1 WHERE sink = 'owner'",
                [crate::clock::now_s() - crate::dialog_alerts::RETRY_S - 1.0],
            )
            .expect("make failed owner sink due for retry");
        drop(connection);
        services
            .runtime
            .block_on(adapter.helper_stuckenv_note_stale_panel(
                "%stuck-test",
                "Allow command?",
                now - 31.0,
            ));
        assert_eq!(
            std::fs::read_to_string(&owner_calls).expect("retried owner attempt"),
            "2"
        );
        let inbox_db = rusqlite::Connection::open(dir.path().join("inbox.db"))
            .expect("canonical inbox database after retry");
        let vp_messages: i64 = inbox_db
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE sender = 'watchdog'",
                [],
                |row| row.get(0),
            )
            .expect("count durable VP alerts after retry");
        assert_eq!(vp_messages, 1, "a successful VP alert is not repeated");
        let metrics = services.flock.stores().metrics.expect("runtime metrics");
        let events = metrics
            .events_between(0.0, crate::clock::now_s() + 1.0)
            .expect("panel metrics");
        assert_eq!(
            events
                .iter()
                .filter(|event| {
                    event.kind == "panel.blocking"
                        && event.pane_id.as_deref() == Some("%stuck-test")
                })
                .count(),
            1,
            "the shared reporter records the warning metric once across retries"
        );

        let other_flock = Arc::new(Flock::new(
            dir.path().join("other-launches"),
            dir.path().join("other-lanes"),
            false,
        ));
        assert!(
            LoopRuntimeAdapter::new(other_flock, Arc::new(|| Box::new(FakeTerminal::new())))
                .with_stuck_services(Arc::clone(&services))
                .is_err()
        );
        services.shutdown();
        assert!(adapter.helper_stuckenv_closed());
    }

    #[test]
    fn production_env_stuck_recovery_uses_concrete_services() {
        use std::process::Command;

        const CHILD: &str = "DS_STUCK_RECOVERY_TEST_CHILD";
        const TEST_NAME: &str =
            "server_runtime::stuck::tests::production_env_stuck_recovery_uses_concrete_services";
        if std::env::var_os(CHILD).is_none() {
            let config = tempfile::tempdir().expect("isolated recovery config");
            let output = Command::new(std::env::current_exe().expect("test binary"))
                .args(["--exact", TEST_NAME, "--nocapture"])
                .env(CHILD, "1")
                .env("DUNKINGSHEEP_DIR", config.path())
                .env("DUNKINGSHEEP_FEDERATED_INPUT", "1")
                .output()
                .expect("isolated child test");
            assert!(
                output.status.success(),
                "child test failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let dir = tempfile::tempdir().expect("runtime state");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let pane_id = "%stuck-test".to_owned();
        let strict_pane_id = "%strict-test".to_owned();
        let pid = i64::from(std::process::id());
        let generation = format!(
            "pid:{pid}:start:{}",
            crate::proc::process_start(pid).expect("current process start")
        );
        let frame = "  >_ OpenAI Codex (v0.165.0)\n\n\x1b[1m› pending text\x1b[0m\n\n  GPT-6-Astra high · ~/workspace/x\n  ? for shortcuts\n";
        let strict_empty_frame = format!(
            "synthetic transcript\n{}› Ask Codex to do anything\n\n\n{}\n",
            "\n".repeat(17),
            "  model · /tmp/synthetic-fixture · task"
        );
        let factory: crate::flock::loop_env::LoopTerminalFactory = Arc::new(move || {
            let mut terminal = FakeTerminal::new();
            terminal.panes = [pane_id.clone(), strict_pane_id.clone()]
                .into_iter()
                .map(|pane_id| Pane {
                    pane_id,
                    pane_pid: Some(pid),
                    command: "codex".into(),
                    ..Pane::default()
                })
                .collect();
            terminal.screens.insert(pane_id.clone(), frame.to_owned());
            terminal
                .screens
                .insert(strict_pane_id.clone(), strict_empty_frame.clone());
            Box::new(terminal)
        });
        let services = make_runtime_services(dir.path(), runtime, true, Some(factory));
        let adapter: Arc<dyn crate::flock::loops_stuck::StuckEnv> = Arc::new(
            services
                .stuck_loop_adapter()
                .expect("canonical stuck adapter"),
        );
        let now = crate::clock::now_s();
        let mut message = crate::flock::sends::Message::new(
            "m1",
            "%stuck-test",
            "lane",
            "pending text",
            "send_text",
        );
        message.status = "failed".into();
        message.attempt_generation = Some(generation.clone());
        services.flock.note_stuck_candidate(&message);
        let mut strict_message = crate::flock::sends::Message::new(
            "m2",
            "%strict-test",
            "lane",
            "strict pending text",
            "send_text",
        );
        strict_message.status = "failed".into();
        strict_message.attempt_generation = Some(generation.clone());
        services.flock.note_stuck_candidate(&strict_message);
        services.flock.stuck_tracker.lock().unwrap().records.insert(
            "%stuck-test".into(),
            crate::stuck::Record {
                message_id: Some("federated:m1".into()),
                generation: Some(generation.clone()),
                fingerprint: Some(crate::stuck::fingerprint("pending text")),
                first_seen: Some(now - 121.0),
                ..crate::stuck::Record::default()
            },
        );
        services.flock.stuck_tracker.lock().unwrap().records.insert(
            "%strict-test".into(),
            crate::stuck::Record {
                message_id: Some("m2".into()),
                generation: Some(generation),
                fingerprint: Some(crate::stuck::fingerprint("strict pending text")),
                first_seen: Some(now - 121.0),
                ..crate::stuck::Record::default()
            },
        );

        let recovery_started_at = crate::clock::now_s();
        let acted =
            services
                .runtime
                .block_on(crate::flock::loops_stuck::StuckEnv::stuck_observer_step(
                    adapter.as_ref(),
                ));
        let recovery_finished_at = crate::clock::now_s();
        assert_eq!(1, acted);
        let tracker = services.flock.stuck_tracker.lock().unwrap();
        assert_eq!(1, tracker.records["%stuck-test"].attempts);
        assert!(tracker.records["%stuck-test"].last_attempt_at.is_some());
        let candidates = services.flock.stuck_candidates.lock().unwrap();
        let strict_candidate = &candidates["%strict-test"];
        let consumed_at = strict_candidate
            .consumed_at
            .expect("strict empty-composer receipt is consumed");
        assert!(consumed_at.is_finite());
        assert!((recovery_started_at..=recovery_finished_at).contains(&consumed_at));
        assert!(strict_candidate.retired_at.is_none());
    }
}
