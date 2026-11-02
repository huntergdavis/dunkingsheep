//! Concrete delivery and mailbox services for request-thread handlers.
//!
//! The adapter borrows the connection's terminal. Every forwarded terminal operation takes
//! and releases its mutex independently, so no guard spans role-store work or another adapter
//! call. The terminal itself remains on its owning standard thread.

use super::{RequestRuntimeAdapter, TerminalCallAdapter};
use crate::error::DunkError;
use crate::flock::delivery::DeliveryEnv;
use crate::flock::mailbox::MailboxEnv;
use crate::flock::panes::PaneInfoEnv;
use serde_json::Value;

impl TerminalCallAdapter<'_> {
    fn helper_deliveryenv_get_pane(&self, pane_id: &str) -> Option<Value> {
        PaneInfoEnv::get_pane(self, pane_id)
    }

    fn helper_deliveryenv_agent_alive(&self, pane_id: &str) -> Option<bool> {
        let pane = self.helper_deliveryenv_get_pane(pane_id)?;
        let pane_pid = pane.get("pane_pid").and_then(Value::as_i64)?;
        let entries = crate::proc::pane_agent_pids(Some(pane_pid));
        let direct: Option<Vec<crate::push::ChildProc>> = entries.map(|entries| {
            entries
                .into_iter()
                .map(|entry| crate::push::ChildProc {
                    comm: entry.comm,
                    state: entry.state,
                })
                .collect()
        });
        crate::push::lane_agent_alive(direct.as_deref(), || {
            let parents = crate::load::ppid_map();
            let tree = crate::load::tree_of(pane_pid, &parents);
            Some(tree.into_iter().any(|pid| {
                let Some(process) = crate::proc::stat(pid) else {
                    return false;
                };
                let comm = process
                    .comm
                    .rsplit('/')
                    .next()
                    .unwrap_or(&process.comm)
                    .to_lowercase();
                crate::proc::AGENT_NAMES.contains(&comm.as_str())
                    && !crate::proc::DEAD_STATES.contains(&process.state.as_str())
                    && !matches!(process.state.as_str(), "T" | "t")
            }))
        })
    }

    fn helper_deliveryenv_has_web_token(&self) -> Result<bool, DunkError> {
        self.flock.with_roles(|roles, _| {
            Ok(roles
                .tokens()?
                .iter()
                .any(|token| token.get("role").and_then(Value::as_str) == Some("web")))
        })
    }

    fn helper_deliveryenv_server_error_hint(&self) -> String {
        self.with_terminal(|terminal| terminal.server_error_hint())
    }
}

impl MailboxEnv for RequestRuntimeAdapter<'_> {
    fn list_panes(&self) -> Vec<Value> {
        MailboxEnv::list_panes(&self.terminal)
    }

    fn list_panes_grouped(&self) -> Vec<Value> {
        MailboxEnv::list_panes_grouped(&self.terminal)
    }

    fn try_list_panes_grouped(&self) -> Result<Vec<Value>, DunkError> {
        MailboxEnv::try_list_panes_grouped(&self.terminal)
    }

    fn registered_pane(&self, pane_id: &str) -> (bool, bool, Option<String>) {
        MailboxEnv::registered_pane(&self.terminal, pane_id)
    }
}

impl DeliveryEnv for TerminalCallAdapter<'_> {
    fn get_pane(&self, pane_id: &str) -> Option<Value> {
        self.helper_deliveryenv_get_pane(pane_id)
    }

    fn agent_alive(&self, pane_id: &str) -> Option<bool> {
        self.helper_deliveryenv_agent_alive(pane_id)
    }

    fn has_web_token(&self) -> Result<bool, DunkError> {
        self.helper_deliveryenv_has_web_token()
    }

    fn server_error_hint(&self) -> String {
        self.helper_deliveryenv_server_error_hint()
    }
}

impl DeliveryEnv for RequestRuntimeAdapter<'_> {
    fn get_pane(&self, pane_id: &str) -> Option<Value> {
        DeliveryEnv::get_pane(&self.terminal, pane_id)
    }

    fn agent_alive(&self, pane_id: &str) -> Option<bool> {
        DeliveryEnv::agent_alive(&self.terminal, pane_id)
    }

    fn has_web_token(&self) -> Result<bool, DunkError> {
        DeliveryEnv::has_web_token(&self.terminal)
    }

    fn server_error_hint(&self) -> String {
        DeliveryEnv::server_error_hint(&self.terminal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Env;
    use crate::flock::commands_owner_views::{h_delivery_capability, h_owner_push};
    use crate::flock::Flock;
    use crate::owner::OwnerPush;
    use crate::roles::Roles;
    use crate::server::RequestCtx;
    use crate::terminal::fakes::FakeTerminal;
    use crate::terminal::{Terminal, TerminalError};
    use serde_json::json;
    use std::sync::Mutex;

    fn role_store(dir: &std::path::Path) -> Roles {
        Roles::new(
            &Env::default(),
            Some(dir.join("roles.json")),
            Some(dir.join("service-tokens.json")),
            None,
        )
        .expect("temporary role store")
    }

    fn request_adapter<'a>(
        flock: &'a Flock,
        request: &'a RequestCtx,
        runtime: &'a tokio::runtime::Runtime,
        terminal: &'a Mutex<Box<dyn Terminal>>,
    ) -> RequestRuntimeAdapter<'a> {
        RequestRuntimeAdapter::new(flock, request, runtime.handle(), terminal)
    }

    #[test]
    fn production_env_delivery_read_uses_concrete_services() {
        let dir = crate::testsupport::dir("runtime-delivery-read");
        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        );
        let mut fake = FakeTerminal::new();
        fake.fail_grouped_panes = Some(TerminalError::refused("fixture listing refusal"));
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(fake));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let request = RequestCtx::default();
        let adapter = request_adapter(&flock, &request, &runtime, &terminal);

        let pane = DeliveryEnv::get_pane(&adapter, "w2:p2").expect("concrete pane lookup");
        assert_eq!(pane["pane_id"], "w2:p2");
        assert_eq!(pane["agent"], "claude");
        assert_eq!(
            DeliveryEnv::server_error_hint(&adapter),
            "tmux server not running - start it with `tmux new -d`"
        );
        assert_eq!(
            MailboxEnv::try_list_panes_grouped(&adapter)
                .unwrap_err()
                .to_string(),
            "fixture listing refusal"
        );
        assert!(DeliveryEnv::get_pane(&adapter, "%missing").is_none());
        assert_eq!(
            DeliveryEnv::has_web_token(&adapter)
                .unwrap_err()
                .to_string(),
            "no role registry is attached to this flock"
        );
    }

    #[test]
    fn production_env_delivery_policy_uses_concrete_services() {
        let dir = crate::testsupport::dir("runtime-delivery-policy");
        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        );
        let mut roles = role_store(dir.path());
        let pid = i64::from(std::process::id());
        let start = crate::proc::process_start(pid);
        roles
            .register(
                "w2:p2",
                "worker",
                Some(pid),
                start.as_deref(),
                None,
                None,
                None,
                None,
            )
            .expect("live process registration");
        roles
            .register(
                "w3:p3",
                "worker",
                Some(i64::MAX),
                None,
                None,
                None,
                None,
                None,
            )
            .expect("unknown process registration");
        roles
            .issue_token("web", Some("fixture-web"), Some("test"))
            .expect("temporary fixture token");
        flock.attach_roles(roles, None);

        let mut fake = FakeTerminal::new();
        for pane in &mut fake.panes {
            match pane.pane_id.as_str() {
                "w1:p1" | "w2:p2" => pane.pane_pid = Some(pid),
                "w3:p3" => pane.pane_pid = None,
                _ => {}
            }
        }
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(fake));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let request = RequestCtx::default();
        let adapter = request_adapter(&flock, &request, &runtime, &terminal);
        assert_eq!(MailboxEnv::registered_pane(&adapter, "w2:p2").1, true);
        assert_eq!(DeliveryEnv::agent_alive(&adapter, "w2:p2"), Some(false));
        assert_eq!(DeliveryEnv::agent_alive(&adapter, "w1:p1"), Some(false)); // bare shell
        assert_eq!(DeliveryEnv::agent_alive(&adapter, "w3:p3"), None); // no pane process id
        assert!(DeliveryEnv::has_web_token(&adapter).expect("valid fixture token store"));

        let absent_dir = crate::testsupport::dir("runtime-delivery-no-web-token");
        let absent_flock = Flock::new(
            absent_dir.path().join("launches.json"),
            absent_dir.path().join("lanes.json"),
            false,
        );
        absent_flock.attach_roles(role_store(absent_dir.path()), None);
        let absent = request_adapter(&absent_flock, &request, &runtime, &terminal);
        assert!(!DeliveryEnv::has_web_token(&absent).expect("empty fixture token store"));

        std::fs::write(dir.path().join("service-tokens.json"), "{")
            .expect("corrupt only the temporary token fixture");
        let read_error = DeliveryEnv::has_web_token(&adapter).unwrap_err();
        assert!(read_error
            .to_string()
            .contains("service-tokens.json is not valid JSON"));
        let owner =
            OwnerPush::open(dir.path().join("owner-push.db")).expect("temporary owner push store");
        let capability = flock
            .delivery_capability(&adapter, &owner, "w2:p2", None, 10_000.0)
            .expect("capability absorbs role token read failures");
        assert_eq!(capability["desk_credential"], false);
        assert_eq!(capability["desk_can_type"], false);
    }

    #[test]
    fn production_delivery_env_is_complete_and_drives_real_handlers() {
        let dir = crate::testsupport::dir("runtime-delivery-handlers");
        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        );
        let mut role_store = role_store(dir.path());
        role_store
            .register(
                "w2:p2",
                "worker",
                None,
                None,
                Some("codex"),
                None,
                None,
                Some("worker two"),
            )
            .expect("temporary registered lane");
        role_store
            .issue_token("web", Some("fixture-web"), Some("test"))
            .expect("temporary fixture token");
        flock.attach_roles(role_store, None);
        flock.remember_lane("w2:p2", "worker two", true);

        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let request = RequestCtx::default();
        let adapter = request_adapter(&flock, &request, &runtime, &terminal);
        let owner =
            OwnerPush::open(dir.path().join("owner-push.db")).expect("temporary owner store");

        let target = flock
            .delivery_target(&adapter, "worker two", None)
            .expect("real delivery target handler path");
        assert_eq!(target["pane_id"], "w2:p2");
        let capability = h_delivery_capability(
            &flock,
            &adapter,
            &owner,
            &json!({"target": "worker two"}),
            None,
            10_000.0,
        )
        .expect("real capability handler");
        assert_eq!(capability["agent"], "claude");
        assert_eq!(capability["agent_alive"], false);
        assert_eq!(capability["desk_credential"], true);
        assert_eq!(capability["desk_can_type"], true);

        let pushed = h_owner_push(
            &owner,
            &json!({"target": "worker two", "body": "fixture delivery"}),
            &|name| {
                flock
                    .delivery_target(&adapter, name, None)
                    .and_then(|pane| {
                        pane["pane_id"].as_str().map(str::to_owned).ok_or_else(|| {
                            DunkError::Refused("resolved pane is missing pane_id".into())
                        })
                    })
            },
            10_000.0,
        )
        .expect("real owner push handler writes to the temporary store");
        assert_eq!(pushed["pane_id"], "w2:p2");
        assert_eq!(pushed["outcome"], "stored");

        let failing_dir = crate::testsupport::dir("runtime-delivery-backend-refusal");
        let failing_flock = Flock::new(
            failing_dir.path().join("launches.json"),
            failing_dir.path().join("lanes.json"),
            false,
        );
        let mut failing_fake = FakeTerminal::new();
        failing_fake.panes.clear();
        failing_fake.fail_grouped_panes =
            Some(TerminalError::refused("selected tmux listing failed"));
        let failing_terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(failing_fake));
        let failing = request_adapter(&failing_flock, &request, &runtime, &failing_terminal);
        let error = failing_flock
            .delivery_target(&failing, "unknown lane", None)
            .unwrap_err();
        assert!(error.to_string().contains("reason=unknown_name"));
    }
}
