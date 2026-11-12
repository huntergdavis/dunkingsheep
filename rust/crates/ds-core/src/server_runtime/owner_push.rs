//! Canonical process-owned owner channel. Configuration is captured without I/O;
//! unsuccessful opens remain retryable, matching `dunk_owner.shared`.
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use crate::error::{refusal, DunkError};
use crate::owner::OwnerPush;

#[derive(Default)]
pub(super) struct OwnerPushService {
    path: OnceLock<PathBuf>,
    store: OnceLock<OwnerPush>,
    initialize: Mutex<()>,
}

impl OwnerPushService {
    pub(super) fn attach_path(&self, path: PathBuf) -> Result<(), DunkError> {
        self.path
            .set(path)
            .map_err(|_| refusal("runtime owner push path is already attached"))
    }

    pub(super) fn get(&self) -> Result<&OwnerPush, DunkError> {
        if let Some(store) = self.store.get() {
            return Ok(store);
        }
        let path = self
            .path
            .get()
            .ok_or_else(|| refusal("runtime owner push path is not wired"))?;
        let _initializing = self
            .initialize
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(store) = self.store.get() {
            return Ok(store);
        }
        let store = OwnerPush::open(path)?;
        // All initialization is serialized; only successful opens are published.
        let _ = self.store.set(store);
        Ok(self
            .store
            .get()
            .expect("owner store published under initialization lock"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Env;
    use crate::flock::panes::Stores;
    use crate::roles::Roles;
    use crate::server::RequestCtx;
    use crate::server_runtime::{RuntimeObservability, RuntimeServiceInputs, RuntimeServices};
    use crate::tasks::Tasks;
    use crate::terminal::fakes::FakeTerminal;
    use serde_json::json;
    use std::sync::{Arc, Barrier};

    use crate::terminal::{AgentSpec, Pane, Resize, Tab, Terminal, TerminalError, Workspace};
    use std::collections::BTreeMap;
    #[derive(Debug, Default)]
    struct Observations {
        keys: Vec<(String, Vec<String>)>,
        threads: Vec<std::thread::ThreadId>,
    }
    #[derive(Debug)]
    struct ObservedTerminal {
        inner: FakeTerminal,
        observations: Arc<Mutex<Observations>>,
        created_on: std::thread::ThreadId,
    }
    impl Drop for ObservedTerminal {
        fn drop(&mut self) {
            assert_eq!(self.created_on, std::thread::current().id());
            let mut observed = self.observations.lock().unwrap();
            observed.keys.extend(self.inner.keys_sent.clone());
            observed.threads.push(self.created_on);
        }
    }
    impl Terminal for ObservedTerminal {
        fn version(&self) -> String {
            self.inner.version()
        }
        fn is_available(&self) -> bool {
            self.inner.is_available()
        }
        fn server_error_hint(&self) -> String {
            self.inner.server_error_hint()
        }
        fn list_panes(&self) -> Vec<Pane> {
            self.inner.list_panes()
        }
        fn get_pane(&self, pane_id: &str) -> Option<Pane> {
            self.inner.get_pane(pane_id)
        }
        fn list_tabs(&self) -> Vec<Tab> {
            self.inner.list_tabs()
        }
        fn list_workspaces(&self) -> Vec<Workspace> {
            self.inner.list_workspaces()
        }
        fn list_panes_grouped(&self) -> BTreeMap<String, Vec<Pane>> {
            self.inner.list_panes_grouped()
        }
        fn agent_child_pids(&self, pane_pid: i64) -> Result<Vec<i64>, TerminalError> {
            self.inner.agent_child_pids(pane_pid)
        }
        fn create_workspace(
            &mut self,
            label: Option<&str>,
            cwd: Option<&str>,
            focus: bool,
        ) -> Result<Workspace, TerminalError> {
            self.inner.create_workspace(label, cwd, focus)
        }
        fn create_tab(
            &mut self,
            workspace_id: Option<&str>,
            label: Option<&str>,
            cwd: Option<&str>,
            focus: bool,
        ) -> Result<Tab, TerminalError> {
            self.inner.create_tab(workspace_id, label, cwd, focus)
        }
        fn split_pane(
            &mut self,
            pane_id: &str,
            direction: &str,
            cwd: Option<&str>,
        ) -> Result<Pane, TerminalError> {
            self.inner.split_pane(pane_id, direction, cwd)
        }
        fn start_agent(&mut self, spec: &AgentSpec) -> Result<Pane, TerminalError> {
            self.inner.start_agent(spec)
        }
        fn rename(
            &mut self,
            kind: &str,
            target_id: &str,
            label: &str,
        ) -> Result<(), TerminalError> {
            self.inner.rename(kind, target_id, label)
        }
        fn close(&mut self, kind: &str, target_id: &str) -> Result<(), TerminalError> {
            self.inner.close(kind, target_id)
        }
        fn focus(&mut self, kind: &str, target_id: &str) -> Result<(), TerminalError> {
            self.inner.focus(kind, target_id)
        }
        fn run_command(&mut self, pane_id: &str, command: &str) -> Result<(), TerminalError> {
            self.inner.run_command(pane_id, command)
        }
        fn send_text(&mut self, pane_id: &str, text: &str) -> Result<(), TerminalError> {
            self.inner.send_text(pane_id, text)
        }
        fn send_text_and_enter(&mut self, pane_id: &str, text: &str) -> Result<(), TerminalError> {
            self.inner.send_text_and_enter(pane_id, text)
        }
        fn send_keys(&mut self, pane_id: &str, keys: &[&str]) -> Result<(), TerminalError> {
            self.inner.send_keys(pane_id, keys)
        }
        fn read_pane(
            &self,
            pane_id: &str,
            lines: i64,
            ansi: bool,
        ) -> Result<Option<String>, TerminalError> {
            self.inner.read_pane(pane_id, lines, ansi)
        }
        fn agent_status(&self, pane_id: &str) -> Option<String> {
            self.inner.agent_status(pane_id)
        }
        fn resize(&mut self, pane_id: &str, cols: i64, rows: i64) -> Result<Resize, TerminalError> {
            self.inner.resize(pane_id, cols, rows)
        }
    }

    // Uses the same temporary-store construction as registrations::tests; all paths are
    // explicit fixture paths and every terminal is created on the dispatching thread.
    fn fixture() -> (tempfile::TempDir, Arc<RuntimeServices>) {
        let (dir, services, _) = observed_fixture();
        (dir, services)
    }
    fn observed_fixture() -> (
        tempfile::TempDir,
        Arc<RuntimeServices>,
        Arc<Mutex<Observations>>,
    ) {
        let observations = Arc::new(Mutex::new(Observations::default()));
        let terminal_observations = Arc::clone(&observations);
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let tasks = Arc::new(
            runtime
                .block_on(Tasks::open(dir.path().join("tasks.db")))
                .unwrap(),
        );
        runtime.block_on(tasks.allocation_init()).unwrap();
        let inbox = Arc::new(
            crate::inbox_store::Store::open(dir.path().join("inbox.db"), dir.path().join("blobs"))
                .unwrap(),
        );
        let env = Env::default();
        let mut roles = Roles::new(
            &env,
            Some(dir.path().join("roles.json")),
            Some(dir.path().join("tokens.json")),
            None,
        )
        .unwrap();
        for (pane, role) in [("w1:p1", "worker"), ("w2:p2", "owner"), ("w3:p3", "desk")] {
            roles
                .register(pane, role, None, None, None, None, None, None)
                .unwrap();
        }
        let executable = dir.path().join("test-daemon");
        std::fs::write(&executable, b"fixture identity").unwrap();
        let services = RuntimeServices::new(RuntimeServiceInputs {
            launches_file: dir.path().join("launches.json"),
            lane_registry_file: dir.path().join("lanes.json"),
            persist_enabled: false,
            stores: Stores {
                tasks: Some(tasks),
                inbox: Some(inbox),
                ..Stores::default()
            },
            roles,
            legacy_superusers: None,
            release_version: "fixture".into(),
            state_file: dir.path().join("state.json"),
            executable,
            env,
            observability: Some(RuntimeObservability {
                state_dir: dir.path().join("metrics"),
                memory_db: None,
            }),
            runtime,
            terminal_factory: Arc::new(move || {
                let mut terminal = FakeTerminal::default();
                // No fake pane PID may accidentally resolve to a live process.
                for pane in &mut terminal.panes {
                    pane.pane_pid = None;
                }
                terminal.screens.insert(
                    "w1:p1".into(),
                    "Context is full - please /clear\n│ ❯ fixture text\n──────────\n".into(),
                );
                Box::new(ObservedTerminal {
                    inner: terminal,
                    observations: Arc::clone(&terminal_observations),
                    created_on: std::thread::current().id(),
                })
            }),
        })
        .unwrap();
        (dir, services, observations)
    }

    fn request(pane: &str) -> RequestCtx {
        RequestCtx {
            self_pane_id: Some(pane.into()),
            socket_peer: Some(true),
            socket_peer_uid: Some(nix::unistd::geteuid().as_raw()),
            ..RequestCtx::default()
        }
    }

    #[test]
    fn owner_path_attachment_preserves_empty_and_whitespace_values_without_io() {
        for path in [PathBuf::new(), PathBuf::from("   ")] {
            let service = OwnerPushService::default();
            service.attach_path(path.clone()).unwrap();
            assert_eq!(service.path.get(), Some(&path));
            assert!(service.store.get().is_none());
        }
    }

    #[test]
    fn owner_store_concurrent_first_access_is_canonical_and_attachment_is_lazy() {
        let (dir, services) = fixture();
        let path = dir.path().join("explicit whitespace /owner-push.db");
        services.attach_owner_push_path(path.clone()).unwrap();
        assert!(!path.parent().unwrap().exists());
        assert!(services
            .attach_owner_push_path(dir.path().join("other.db"))
            .is_err());
        let barrier = Barrier::new(8);
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let barrier = &barrier;
                    let services = &services;
                    scope.spawn(move || {
                        barrier.wait();
                        let store = services.owner_push.get().unwrap();
                        store
                            .push(
                                "w1:p1",
                                "parallel fixture",
                                None,
                                None,
                                crate::clock::now_s(),
                            )
                            .unwrap();
                        store as *const OwnerPush as usize
                    })
                })
                .collect();
            let addresses: Vec<_> = handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect();
            assert!(addresses.iter().all(|address| *address == addresses[0]));
        });
        assert_eq!(
            services
                .owner_push
                .get()
                .unwrap()
                .stats(86400.0, crate::clock::now_s())
                .unwrap()
                .pushed,
            8
        );
        assert!(path.is_file());
    }

    #[test]
    fn owner_views_and_capability_share_store_retained_by_real_dispatcher() {
        let (dir, services) = fixture();
        services
            .attach_owner_push_path(dir.path().join("owner.db"))
            .unwrap();
        let now = crate::clock::now_s();
        let store = services.owner_push.get().unwrap();
        let pushed = store
            .push("w1:p1", "fixture push", None, None, now)
            .unwrap();
        let dispatcher = services.production_dispatcher();
        let desk = request("w3:p3");
        let stats = dispatcher
            .dispatch("owner_stats", &json!({}), &desk)
            .unwrap();
        assert_eq!(stats["stats"]["pushed"], 1);
        assert_eq!(stats["stats"]["pending"], 1);
        let receipts = dispatcher
            .dispatch("owner_receipts", &json!({"push_ids":[pushed.id]}), &desk)
            .unwrap();
        assert_eq!(receipts["returned"], 1);
        assert_eq!(receipts["receipts"][0]["taken_at"], serde_json::Value::Null);
        let args = json!({"target":"  w1:p1  "});
        let before = dispatcher
            .dispatch("delivery_capability", &args, &desk)
            .unwrap();
        assert_eq!(before["drains"], false);
        assert_eq!(before["send"], "guarded");
        store.poll("w1:p1", Some("fixture poller"), now).unwrap();
        let weak = Arc::downgrade(&services);
        drop(services);
        assert!(weak.upgrade().is_some());
        let after = dispatcher
            .dispatch("delivery_capability", &args, &desk)
            .unwrap();
        assert_eq!(after["pane_id"], "w1:p1");
        assert_eq!(after["drains"], true);
        assert_eq!(after["send"], "owner_push");
        assert!(after["drain_evidence"]
            .as_str()
            .unwrap()
            .starts_with("harness polled "));
        assert_eq!(after["desk_credential"], false);
        assert_eq!(after["desk_can_type"], false);
        drop(dispatcher);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn owner_store_target_errors_precede_open_and_failed_open_is_retryable() {
        let (dir, services) = fixture();
        let blocked_parent = dir.path().join("blocked");
        std::fs::write(&blocked_parent, b"not a directory").unwrap();
        let path = blocked_parent.join("owner.db");
        services.attach_owner_push_path(path.clone()).unwrap();
        let dispatcher = services.production_dispatcher();
        let desk = request("w3:p3");
        let error = dispatcher
            .dispatch(
                "delivery_capability",
                &json!({"target":"no-such-fixture-lane"}),
                &desk,
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("reason=unknown_name"), "{error}");
        assert!(services.owner_push.store.get().is_none());
        let error = dispatcher
            .dispatch("delivery_capability", &json!({"target":7}), &desk)
            .unwrap_err()
            .to_string();
        assert!(error.contains("has no attribute 'strip'"), "{error}");
        for command in ["owner_stats", "owner_receipts", "delivery_capability"] {
            let error = dispatcher
                .dispatch(
                    command,
                    &if command == "delivery_capability" {
                        json!({"target":"w1:p1"})
                    } else {
                        json!({})
                    },
                    &desk,
                )
                .unwrap_err()
                .to_string();
            assert!(!error.contains("not wired"), "{command}: {error}");
            assert!(services.owner_push.store.get().is_none());
        }
        std::fs::remove_file(&blocked_parent).unwrap();
        let value = dispatcher
            .dispatch("delivery_capability", &json!({"target":"w1:p1"}), &desk)
            .unwrap();
        assert_eq!(value["drains"], false);
        assert!(path.is_file());
        assert!(services.owner_push.store.get().is_some());
    }

    #[test]
    fn owner_store_missing_configuration_preserves_status_and_named_refusals() {
        let (_dir, services) = fixture();
        let dispatcher = services.production_dispatcher();
        let worker = request("w1:p1");
        assert!(dispatcher
            .dispatch("status", &json!({}), &worker)
            .unwrap()
            .is_object());
        for command in ["owner_stats", "owner_receipts", "delivery_capability"] {
            let args = if command == "delivery_capability" {
                json!({"target":"w1:p1"})
            } else {
                json!({})
            };
            let error = dispatcher
                .dispatch(command, &args, &worker)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("runtime owner push path is not wired"),
                "{command}: {error}"
            );
        }
        assert!(services.owner_push.store.get().is_none());
    }
    #[test]
    fn owner_store_binder_rejects_foreign_flock_and_unbound_adapter_refuses() {
        use crate::api::CommandEnv;
        use crate::server_runtime::RequestRuntimeAdapter;
        let (_dir, services) = fixture();
        let (_other_dir, other) = fixture();
        let request = request("w1:p1");
        let terminal = Mutex::new((services.terminal_factory)());
        let adapter = RequestRuntimeAdapter::new(
            &services.flock,
            &request,
            services.runtime.handle(),
            &terminal,
        );
        assert!(adapter
            .owner_push()
            .unwrap_err()
            .to_string()
            .contains("owner push adapter is not wired"));
        let error = adapter
            .with_owner_push(&other)
            .err()
            .expect("foreign Flock refused");
        assert!(error.to_string().contains("flock mismatch"));
        assert!(services.owner_push.store.get().is_none());
        assert!(other.owner_push.store.get().is_none());
    }

    #[test]
    fn delivery_group_real_role_order_and_clear_box_observations() {
        let (dir, services, observations) = observed_fixture();
        let path = dir.path().join("lazy-owner.db");
        services.attach_owner_push_path(path.clone()).unwrap();
        let dispatcher = services.production_dispatcher();
        // Destructive clear checks roles before resolving even a nonexistent target.
        for ctx in [RequestCtx::default(), request("w3:p3"), request("w1:p1")] {
            let error = dispatcher
                .dispatch("clear_box", &json!({"target":"not-a-pane"}), &ctx)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("not permitted for role") || error.contains("superuser"),
                "{error}"
            );
            assert!(!path.exists());
        }
        let owner = request("w2:p2");
        let error = dispatcher
            .dispatch("clear_box", &json!({"target":"not-a-pane"}), &owner)
            .unwrap_err()
            .to_string();
        assert!(!error.contains("superuser"), "{error}");
        let delivery = dispatcher
            .dispatch("delivery", &json!({"target":"w1:p1"}), &request("w3:p3"))
            .unwrap();
        assert_eq!(delivery["state"], "blocked");
        assert_eq!(delivery["foreign_text"], true);
        assert_eq!(delivery["foreign_excerpt"], "fixture text");
        let clear = dispatcher
            .dispatch(
                "clear_box",
                &json!({"target":"w1:p1", "reason":"fixture"}),
                &owner,
            )
            .unwrap();
        assert_eq!(clear["pane_id"], "w1:p1");
        assert_eq!(clear["flushed"], true);
        assert_eq!(clear["before"]["state"], "blocked");
        assert_eq!(clear["before"]["foreign_excerpt"], "fixture text");
        let observed = observations.lock().unwrap();
        assert_eq!(
            observed.keys,
            vec![
                ("w1:p1".into(), vec!["C-u".into()]),
                ("w1:p1".into(), vec!["Escape".into()])
            ]
        );
        assert!(observed
            .threads
            .iter()
            .all(|thread| *thread == std::thread::current().id()));
        drop(observed);
        assert!(
            !path.exists(),
            "clear and delivery do not need the owner store"
        );
        // Owner views are READ_ONLY in the real role table: preserve that policy rather
        // than inventing an authentication requirement in the service binder.
        for ctx in [RequestCtx::default(), request("w3:p3"), owner] {
            let stats = dispatcher
                .dispatch("owner_stats", &json!({}), &ctx)
                .unwrap();
            assert_eq!(stats["stats"]["pushed"], 0);
        }
    }
}
