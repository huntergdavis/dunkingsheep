//! Request-scoped composition for the guarded `respawn_pane` API handler.

use super::{inbox_data::InboxRuntimeView, outbox_runtime::OutboxRuntime, RequestRuntimeAdapter};
use crate::error::DunkError;
use crate::flock::respawn::RespawnServices;
use std::sync::Arc;

impl RequestRuntimeAdapter<'_> {
    fn require_respawn_outbox(&self) -> Result<&OutboxRuntime, DunkError> {
        let outbox = self
            .outbox
            .ok_or_else(|| DunkError::Refused("respawn pane outbox is not attached".into()))?;
        if !std::ptr::eq(self.terminal.flock, outbox.flock.as_ref()) {
            return Err(DunkError::Refused(
                "respawn pane outbox flock mismatch".into(),
            ));
        }
        Ok(outbox)
    }

    pub(super) fn with_respawn_pane_env(
        &self,
        operation: &mut dyn FnMut(&RespawnServices<'_>) -> Result<serde_json::Value, DunkError>,
    ) -> Result<serde_json::Value, DunkError> {
        let outbox = self.require_respawn_outbox()?;
        let status = self.terminal.flock.status_state()?;
        let inbox_adapter = RequestRuntimeAdapter::new(
            self.terminal.flock,
            self.request,
            self.runtime,
            self.terminal.terminal,
        )
        .with_outbox(outbox)?;
        let inbox = InboxRuntimeView::new(
            inbox_adapter,
            Arc::clone(&outbox.flock),
            Arc::clone(&outbox.pool),
        )?;
        let services = RespawnServices {
            terminal: self.terminal.terminal,
            delivery: &self.terminal,
            inbox: &inbox,
            runtime: self.runtime,
            env: &status.env,
            verify_seconds: status.agent_verify_s,
            #[cfg(test)]
            before_native_recheck: None,
        };
        operation(&services)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api;
    use crate::config::Env;
    use crate::flock::loop_env::LoopTerminalFactory;
    use crate::flock::panes::Stores;
    use crate::flock::Flock;
    use crate::roles::Roles;
    use crate::server::{Dispatcher, RequestCtx};
    use crate::server_runtime::{RuntimeObservability, RuntimeServiceInputs, RuntimeServices};
    use crate::tasks::Tasks;
    use crate::terminal::{fakes::FakeTerminal, tmux::TmuxBackend, Terminal};
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Child, Command};
    use std::sync::Mutex;
    use tokio::runtime::Builder;

    struct OwnedAgents(Vec<Child>);

    impl Drop for OwnedAgents {
        fn drop(&mut self) {
            for child in &mut self.0 {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }

    fn make_services() -> (tempfile::TempDir, Arc<RuntimeServices>) {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Builder::new_multi_thread().enable_all().build().unwrap();
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
        roles
            .register(
                "w1:p1",
                "worker",
                None,
                None,
                Some("codex"),
                None,
                None,
                None,
            )
            .unwrap();
        roles
            .register("w2:p2", "owner", None, None, None, None, None, None)
            .unwrap();
        let executable = dir.path().join("daemon");
        std::fs::write(&executable, b"test daemon identity").unwrap();
        let stores = Stores {
            tasks: Some(tasks),
            inbox: Some(inbox),
            ..Stores::default()
        };
        let terminal_factory: LoopTerminalFactory = Arc::new(|| Box::new(FakeTerminal::new()));
        let services = RuntimeServices::new(RuntimeServiceInputs {
            launches_file: dir.path().join("launches.json"),
            lane_registry_file: dir.path().join("lanes.json"),
            persist_enabled: false,
            stores,
            roles,
            legacy_superusers: None,
            release_version: "test".into(),
            state_file: dir.path().join("state.json"),
            executable,
            env,
            observability: Some(RuntimeObservability {
                state_dir: dir.path().join("observability"),
                memory_db: None,
            }),
            runtime,
            terminal_factory,
        })
        .unwrap();
        (dir, services)
    }

    fn make_managed_respawn_services(
        tag: &str,
        fail_first_verification: bool,
    ) -> (
        tempfile::TempDir,
        Arc<RuntimeServices>,
        OwnedAgents,
        i64,
        String,
        i64,
        i64,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path().join("claude");
        std::fs::copy("/bin/sleep", &agent).unwrap();
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut owned = OwnedAgents(Vec::new());
        for _ in 0..3 {
            owned.0.push(
                crate::testsupport::exec_retrying_busy(|| Command::new(&agent).arg("300").spawn())
                    .unwrap(),
            );
        }
        let old_pid = i64::from(owned.0[0].id());
        let new_pid = i64::from(owned.0[1].id());
        let rollback_pid = i64::from(owned.0[2].id());
        let old_start = crate::proc::process_start(old_pid).unwrap();
        let cwd = dir.path().to_string_lossy().into_owned();
        std::fs::write(dir.path().join("pid"), old_pid.to_string()).unwrap();
        std::fs::write(dir.path().join("screen"), "› Ask Codex to do anything\n").unwrap();

        let root = crate::terminal::tmux::shell_join(&[dir.path().to_str().unwrap()]);
        let script = dir.path().join("tmux");
        let failure_setup = if fail_first_verification {
            format!(
                "if test \"$n\" = 1; then echo {new_pid} > pid; printf 'sh: claude: not found\\n' > screen; else echo {rollback_pid} > pid; printf '› Ask Codex to do anything\\n' > screen; fi"
            )
        } else {
            format!("echo {new_pid} > pid; printf '› Ask Codex to do anything\\n' > screen")
        };
        let sep = crate::terminal::parse::pane_separator();
        let body = format!(
            r##"#!/bin/sh
cd {root} || exit 1
case "$1" in
list-panes)
  p=$(cat pid)
  printf '%%2{sep}$1{sep}@1{sep}{cwd}{sep}claude{sep}worker{sep}1{sep}ws{sep}tab{sep}0{sep}0{sep}%s{sep}80{sep}24\n' "$p"
  ;;
capture-pane) cat screen ;;
respawn-pane)
  n=0; test ! -f count || n=$(cat count); n=$((n+1)); echo "$n" > count
  {failure_setup}
  ;;
esac
"##
        );
        std::fs::write(&script, body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A no-op run (no subcommand matches) that waits out ETXTBSY, so the backend's own exec
        // of the script later does not land in the window.
        crate::testsupport::exec_retrying_busy(|| Command::new(&script).arg("warm-up").status())
            .unwrap();
        let registry_path = dir.path().join("registry.json");
        TmuxBackend::with_registry(&script, &registry_path)
            .registry()
            .remember("pane", "%2", json!({"name":"claude"}))
            .unwrap();

        let runtime = Builder::new_multi_thread().enable_all().build().unwrap();
        let tasks = Arc::new(
            runtime
                .block_on(Tasks::open(dir.path().join(format!("{tag}-tasks.db"))))
                .unwrap(),
        );
        runtime.block_on(tasks.allocation_init()).unwrap();
        let inbox = Arc::new(
            crate::inbox_store::Store::open(
                dir.path().join(format!("{tag}-inbox.db")),
                dir.path().join(format!("{tag}-blobs")),
            )
            .unwrap(),
        );
        let home = dir.path().to_string_lossy().into_owned();
        let env = Env::from_pairs(&[("HOME", home.as_str())]);
        let mut roles = Roles::new(
            &env,
            Some(dir.path().join(format!("{tag}-roles.json"))),
            Some(dir.path().join(format!("{tag}-tokens.json"))),
            None,
        )
        .unwrap();
        roles
            .register(
                "%2",
                "worker",
                Some(old_pid),
                Some(&old_start),
                Some("claude"),
                None,
                None,
                None,
            )
            .unwrap();
        roles
            .register("%1", "owner", None, None, None, None, None, None)
            .unwrap();
        let executable = dir.path().join(format!("{tag}-daemon"));
        std::fs::write(&executable, b"test daemon identity").unwrap();
        let stores = Stores {
            tasks: Some(tasks),
            inbox: Some(inbox),
            ..Stores::default()
        };
        let observability = RuntimeObservability {
            state_dir: dir.path().join(format!("{tag}-observability")),
            memory_db: None,
        };
        let script_for_factory = script.clone();
        let registry_for_factory = registry_path.clone();
        let terminal_factory: LoopTerminalFactory = Arc::new(move || {
            Box::new(TmuxBackend::with_registry(
                &script_for_factory,
                &registry_for_factory,
            ))
        });
        let services = RuntimeServices::new(RuntimeServiceInputs {
            launches_file: dir.path().join(format!("{tag}-launches.json")),
            lane_registry_file: dir.path().join(format!("{tag}-lanes.json")),
            persist_enabled: false,
            stores,
            roles,
            legacy_superusers: None,
            release_version: "test".into(),
            state_file: dir.path().join(format!("{tag}-state.json")),
            executable,
            env,
            observability: Some(observability),
            runtime,
            terminal_factory,
        })
        .unwrap();
        services.flock.record_launch(
            "%2",
            &[
                ("agent", json!("claude")),
                ("argv", json!([agent.display().to_string(), "300"])),
                ("cwd", json!(cwd)),
            ],
        );
        (
            dir,
            services,
            owned,
            old_pid,
            old_start,
            new_pid,
            rollback_pid,
        )
    }

    #[test]
    fn respawn_dispatcher_uses_the_real_runtime_providers_and_guard() {
        let (_dir, services) = make_services();
        let mut handlers = api::Dispatcher::new();
        handlers.register_respawn_pane();
        let dispatcher =
            super::super::dispatcher::RuntimeDispatcher::new(Arc::clone(&services), handlers);
        let request = RequestCtx {
            caller: Some(crate::proc::Caller::new(
                Some(123),
                Some("start".into()),
                Some("codex".into()),
                Some("w2:p2".into()),
                "agent",
            )),
            self_pane_id: Some("w2:p2".into()),
            socket_peer: Some(true),
            socket_peer_uid: Some(nix::unistd::geteuid().as_raw()),
            ..RequestCtx::default()
        };
        let error = dispatcher
            .dispatch(
                "respawn_pane",
                &json!({
                    "pane": "w1:p1",
                    "expected_pid": 1101,
                    "expected_start": "unused",
                    "argv": ["custom-agent"],
                    "rollback_argv": ["old-custom-agent"],
                    "confirm_idle": 0
                }),
                &request,
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("respawn-pane requires --confirm-idle"),
            "runtime provider was not reached or guard ordering changed: {error}"
        );
        services.shutdown();
    }

    #[test]
    fn respawn_provider_refuses_when_canonical_outbox_is_missing() {
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        let flock = Flock::new("launches.json", "lanes.json", false);
        let request = RequestCtx::default();
        let terminal = Mutex::new(Box::new(FakeTerminal::new()) as Box<dyn Terminal>);
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);
        let mut called = false;
        let mut operation = |_services: &RespawnServices<'_>| {
            called = true;
            Ok(serde_json::Value::Null)
        };
        let error = adapter.with_respawn_pane_env(&mut operation).unwrap_err();
        assert!(error
            .to_string()
            .contains("respawn pane outbox is not attached"));
        assert!(!called, "operation ran without the canonical outbox");
    }

    #[test]
    fn respawn_provider_refuses_mismatched_outbox_and_missing_status() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Builder::new_current_thread().enable_all().build().unwrap();
        let store = Arc::new(
            crate::inbox_store::Store::open(dir.path().join("inbox.db"), dir.path().join("blobs"))
                .unwrap(),
        );
        let flock = Arc::new(Flock::new(
            dir.path().join("launches-a.json"),
            dir.path().join("lanes-a.json"),
            false,
        ));
        flock.attach_stores(Stores {
            inbox: Some(Arc::clone(&store)),
            ..Stores::default()
        });
        let other_flock = Arc::new(Flock::new(
            dir.path().join("launches-b.json"),
            dir.path().join("lanes-b.json"),
            false,
        ));
        other_flock.attach_stores(Stores {
            inbox: Some(store),
            ..Stores::default()
        });
        let factory: LoopTerminalFactory = Arc::new(|| Box::new(FakeTerminal::new()));
        let canonical_outbox = OutboxRuntime::new(
            Arc::clone(&flock),
            runtime.handle().clone(),
            Arc::clone(&factory),
        )
        .unwrap();
        let mismatched_outbox =
            OutboxRuntime::new(Arc::clone(&other_flock), runtime.handle().clone(), factory)
                .unwrap();
        let request = RequestCtx::default();
        let terminal = Mutex::new(Box::new(FakeTerminal::new()) as Box<dyn Terminal>);

        let mismatched = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal)
            .with_outbox(&mismatched_outbox);
        let mismatched = match mismatched {
            Err(error) => error,
            Ok(_) => panic!("mismatched outbox was accepted"),
        };
        assert!(mismatched
            .to_string()
            .contains("request outbox flock mismatch"));

        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal)
            .with_outbox(&canonical_outbox)
            .unwrap();
        let mut operation = |_services: &RespawnServices<'_>| Ok(serde_json::Value::Null);
        let error = adapter.with_respawn_pane_env(&mut operation).unwrap_err();
        assert!(error
            .to_string()
            .contains("status startup state is not configured"));
    }

    fn owner_request() -> RequestCtx {
        RequestCtx {
            caller: Some(crate::proc::Caller::new(
                Some(std::process::id() as i64),
                crate::proc::process_start(std::process::id() as i64),
                Some("codex".into()),
                Some("%1".into()),
                "agent",
            )),
            self_pane_id: Some("%1".into()),
            socket_peer: Some(true),
            socket_peer_uid: Some(nix::unistd::geteuid().as_raw()),
            ..RequestCtx::default()
        }
    }

    fn respawn_args(dir: &tempfile::TempDir, old_pid: i64, old_start: &str) -> serde_json::Value {
        let argv = [
            dir.path().join("claude").display().to_string(),
            "300".into(),
        ];
        json!({
            "pane": "%2",
            "expected_pid": old_pid,
            "expected_start": old_start,
            "argv": argv.clone(),
            "rollback_argv": argv,
            "confirm_idle": true,
            "reason": {"why": "api test"}
        })
    }

    #[test]
    fn production_dispatcher_respawn_succeeds_with_canonical_runtime_providers() {
        let (dir, services, _owned, old_pid, old_start, new_pid, _rollback_pid) =
            make_managed_respawn_services("respawn-success", false);
        let dispatcher = services.production_dispatcher();
        let result = dispatcher
            .dispatch(
                "respawn_pane",
                &respawn_args(&dir, old_pid, &old_start),
                &owner_request(),
            )
            .unwrap();
        assert_eq!(result["pane_id"], "%2");
        assert_eq!(result["old_pid"], old_pid);
        assert_eq!(result["pane_pid"], new_pid);
        assert_eq!(
            result["argv"][0],
            dir.path().join("claude").display().to_string()
        );
        assert_eq!(
            result["rollback_argv"][0],
            dir.path().join("claude").display().to_string()
        );
        assert_eq!(result["reason"], "{'why': 'api test'}");
        dispatcher.close();
        services.shutdown();
    }

    #[test]
    fn production_dispatcher_respawn_refusal_verifies_rollback() {
        let (dir, services, _owned, old_pid, old_start, _new_pid, rollback_pid) =
            make_managed_respawn_services("respawn-rollback", true);
        let dispatcher = services.production_dispatcher();
        let error = dispatcher
            .dispatch(
                "respawn_pane",
                &respawn_args(&dir, old_pid, &old_start),
                &owner_request(),
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains("did not start"), "{error}");
        assert!(error.contains("(rolled back)"), "{error}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("pid"))
                .unwrap()
                .trim(),
            rollback_pid.to_string()
        );
        dispatcher.close();
        services.shutdown();
    }
}
