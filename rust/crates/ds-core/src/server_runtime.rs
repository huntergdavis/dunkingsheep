//! Runtime backend configuration shared by the server and startup adapters.
//!
//! The Python selector trims and lowercases `DUNKINGSHEEP_TERMINAL_BACKEND`,
//! defaulting to tmux. Herdr is retired in the Rust runtime, so it is refused
//! explicitly instead of being silently replaced with tmux.

mod delivery;
mod inbox;
mod inbox_metrics;
mod inbox_stores;
mod layout;
mod push;
mod push_eligibility;
mod status;
mod stuck;
mod submit;
mod summary;

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;

use crate::error::{refusal, DunkError};
use crate::flock::lanes::TerminalEnv;
use crate::flock::loop_env::LoopFuture;
use crate::flock::loops::ReaperEnv;
use crate::flock::mailbox::MailboxEnv;
use crate::flock::panes::PaneInfoEnv;
use crate::flock::sends::SendEnv;
use crate::flock::status::ViewCache;
use crate::flock::Flock;
use crate::server::RequestCtx;
use crate::terminal::tmux::TmuxBackend;
use crate::terminal::Terminal;

mod reaper;

/// A request-scoped adapter borrowed on the server's existing connection thread.
///
/// It deliberately owns neither the connection nor its terminal: both remain local to
/// `serve_connection`'s standard thread. The runtime is the daemon runtime's existing
/// handle, used only for synchronous request work that must drive async stores.
#[allow(dead_code)] // This adapter is staged for the production dispatcher integration.
pub(crate) struct RequestRuntimeAdapter<'a> {
    terminal: TerminalCallAdapter<'a>,
    request: &'a RequestCtx,
    runtime: &'a Handle,
    inbox: Option<Arc<crate::inbox_store::Store>>,
}

/// Terminal access shared by the request and loop adapters. A lock is held only while a
/// concrete terminal operation runs; it is dropped before the caller continues.
pub(crate) struct TerminalCallAdapter<'a> {
    flock: &'a Flock,
    terminal: &'a Mutex<Box<dyn Terminal>>,
    runtime: &'a Handle,
}

/// Runtime services safe to move into a Tokio blocking worker. The non-Send terminal is
/// created, wrapped, used, and dropped wholly inside that worker.
#[allow(dead_code)] // Loop callers are assigned to the dependent runtime leaves.
pub(crate) struct LoopRuntimeAdapter {
    flock: Arc<Flock>,
    terminal_factory: Arc<dyn Fn() -> Box<dyn Terminal> + Send + Sync + 'static>,
}

impl<'a> RequestRuntimeAdapter<'a> {
    #[allow(dead_code)] // Wired by the production server dispatcher in the runtime integration.
    pub(crate) fn new(
        flock: &'a Flock,
        request: &'a RequestCtx,
        runtime: &'a Handle,
        terminal: &'a Mutex<Box<dyn Terminal>>,
    ) -> Self {
        let inbox = flock.stores().inbox;
        Self {
            terminal: TerminalCallAdapter {
                flock,
                terminal,
                runtime,
            },
            request,
            runtime,
            inbox,
        }
    }

    #[allow(dead_code)] // Wired by the production server dispatcher in the runtime integration.
    pub(crate) fn handler_context(&self) -> crate::api::HandlerContext<'_> {
        crate::api::HandlerContext {
            flock: self.terminal.flock,
            request: self.request,
            env: self,
            runtime: self.runtime,
        }
    }
}

impl TerminalCallAdapter<'_> {
    fn with_terminal<T>(&self, call: impl FnOnce(&dyn Terminal) -> T) -> T {
        let terminal = self
            .terminal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        call(terminal.as_ref())
    }

    fn with_env<T>(&self, call: impl FnOnce(&TerminalEnv<'_>) -> T) -> T {
        self.with_terminal(|terminal| {
            let registered = |pane_id: &str| {
                self.flock
                    .with_roles(|roles, _| {
                        let Some(entry) = roles.entry(pane_id)? else {
                            return Ok((false, false, None));
                        };
                        let pid = entry.get("pane_pid").and_then(|value| {
                            value.as_i64().or_else(|| value.as_str()?.parse().ok())
                        });
                        let pid_text = entry.get("pane_pid").and_then(|value| match value {
                            Value::String(text) if !text.is_empty() && text != "None" => {
                                Some(text.clone())
                            }
                            Value::Number(number) => Some(number.to_string()),
                            _ => None,
                        });
                        let alive = pid.is_some_and(|pid| {
                            crate::proc::process_alive(
                                pid,
                                entry.get("start").and_then(Value::as_str),
                            )
                        });
                        Ok((true, alive, pid_text))
                    })
                    .unwrap_or((false, false, None))
            };
            let env = TerminalEnv {
                terminal,
                registered: &registered,
            };
            call(&env)
        })
    }

    fn with_terminal_mut<T>(&self, call: impl FnOnce(&mut dyn Terminal) -> T) -> T {
        let mut terminal = self
            .terminal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        call(terminal.as_mut())
    }

    fn helper_sendenv_read_pane(&self, pane_id: &str, lines: i64, ansi: bool) -> Option<String> {
        self.with_terminal(|terminal| terminal.read_pane(pane_id, lines, ansi).ok().flatten())
    }

    fn helper_sendenv_pane_harness(&self, pane_id: &str) -> Option<String> {
        self.with_terminal(|terminal| {
            let pane = terminal.get_pane(pane_id)?;
            let harness = crate::terminal::parse::agent_kind(&pane.command, &pane.title);
            (!harness.is_empty()).then_some(harness)
        })
    }

    fn helper_sendenv_read_status(&self, pane_id: &str) -> Option<String> {
        self.with_terminal(|terminal| {
            terminal
                .agent_status(pane_id)
                .map(|status| status.trim().to_lowercase())
                .filter(|status| !status.is_empty())
        })
    }

    fn helper_sendenv_session_process_gone(&self, pane_id: &str) -> bool {
        let Some(tasks) = self.flock.stores().tasks else {
            return false;
        };
        self.runtime
            .block_on(tasks.leases_for_pane(pane_id, None))
            .map(|leases| {
                leases.into_iter().any(|lease| {
                    lease.caller_pid.is_some_and(|pid| {
                        !crate::proc::process_alive(pid, lease.caller_start.as_deref())
                    })
                })
            })
            .unwrap_or(false)
    }

    fn helper_sendenv_federated_input(&self) -> bool {
        federated_input_enabled(std::env::var_os("DUNKINGSHEEP_FEDERATED_INPUT").as_deref())
    }

    fn helper_sendenv_send_key(&mut self, pane_id: &str, key: &str) -> (bool, String) {
        match self.with_terminal_mut(|terminal| terminal.send_keys(pane_id, &[key])) {
            Ok(()) => (true, "sent".into()),
            Err(error) => (false, error.to_string()),
        }
    }
}

fn federated_input_enabled(value: Option<&OsStr>) -> bool {
    value == Some(OsStr::new("1"))
}

impl SendEnv for TerminalCallAdapter<'_> {
    fn read_pane(&self, pane_id: &str, lines: i64, ansi: bool) -> Option<String> {
        self.helper_sendenv_read_pane(pane_id, lines, ansi)
    }

    fn pane_harness(&self, pane_id: &str) -> Option<String> {
        self.helper_sendenv_pane_harness(pane_id)
    }

    fn session_process_gone(&self, pane_id: &str) -> bool {
        self.helper_sendenv_session_process_gone(pane_id)
    }

    fn read_status(&self, pane_id: &str) -> Option<String> {
        self.helper_sendenv_read_status(pane_id)
    }

    fn send_key(&mut self, pane_id: &str, key: &str) -> (bool, String) {
        self.helper_sendenv_send_key(pane_id, key)
    }

    fn federated_input(&self) -> bool {
        self.helper_sendenv_federated_input()
    }
}

impl PaneInfoEnv for TerminalCallAdapter<'_> {
    fn list_panes_grouped(&self) -> Vec<Value> {
        self.with_env(|env| PaneInfoEnv::list_panes_grouped(env))
    }

    fn get_pane(&self, pane_id: &str) -> Option<Value> {
        self.with_env(|env| PaneInfoEnv::get_pane(env, pane_id))
    }
}

impl MailboxEnv for TerminalCallAdapter<'_> {
    fn list_panes(&self) -> Vec<Value> {
        self.with_env(|env| MailboxEnv::list_panes(env))
    }

    fn list_panes_grouped(&self) -> Vec<Value> {
        self.with_env(|env| MailboxEnv::list_panes_grouped(env))
    }

    fn try_list_panes_grouped(&self) -> Result<Vec<Value>, DunkError> {
        self.with_env(|env| MailboxEnv::try_list_panes_grouped(env))
    }

    fn registered_pane(&self, pane_id: &str) -> (bool, bool, Option<String>) {
        self.with_env(|env| MailboxEnv::registered_pane(env, pane_id))
    }
}

impl PaneInfoEnv for RequestRuntimeAdapter<'_> {
    fn list_panes_grouped(&self) -> Vec<Value> {
        PaneInfoEnv::list_panes_grouped(&self.terminal)
    }

    fn get_pane(&self, pane_id: &str) -> Option<Value> {
        PaneInfoEnv::get_pane(&self.terminal, pane_id)
    }
}

impl crate::api::CommandEnv for RequestRuntimeAdapter<'_> {
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
        self.terminal.terminal
    }

    fn now_s(&self) -> f64 {
        crate::clock::now_s()
    }

    fn status_env(&self) -> Result<&dyn crate::flock::status::StatusEnv, DunkError> {
        self.terminal.flock.status_state()?;
        Ok(self)
    }

    fn view_cache(&self) -> Result<&ViewCache, DunkError> {
        Ok(self.terminal.flock.view_cache())
    }
}

impl LoopRuntimeAdapter {
    #[allow(dead_code)] // Used by the dependent async runtime loop leaves.
    pub(crate) fn new(
        flock: Arc<Flock>,
        terminal_factory: Arc<dyn Fn() -> Box<dyn Terminal> + Send + Sync + 'static>,
    ) -> Self {
        Self {
            flock,
            terminal_factory,
        }
    }

    #[allow(dead_code)] // Used by the dependent async runtime loop leaves.
    pub(crate) fn with_loop_terminal<T, F>(&self, call: F) -> JoinHandle<T>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a Flock, &'a TerminalCallAdapter<'a>) -> T + Send + 'static,
    {
        let flock = Arc::clone(&self.flock);
        let factory = Arc::clone(&self.terminal_factory);
        let runtime = Handle::current();
        tokio::task::spawn_blocking(move || {
            let terminal = Mutex::new(factory());
            let adapter = TerminalCallAdapter {
                flock: &flock,
                terminal: &terminal,
                runtime: &runtime,
            };
            call(&flock, &adapter)
        })
    }
}

impl ReaperEnv for LoopRuntimeAdapter {
    fn list_panes(&self) -> LoopFuture<'_, Result<Vec<Value>, DunkError>> {
        self.helper_reaperenv_list_panes()
    }

    fn is_live(&self) -> bool {
        self.helper_reaperenv_is_live()
    }

    fn clear_historical_errors<'a>(&'a self, panes: &'a BTreeSet<String>) -> LoopFuture<'a, ()> {
        self.helper_reaperenv_clear_historical_errors(panes)
    }

    fn inbox_maintenance<'a>(
        &'a self,
        panes: &'a BTreeSet<String>,
        pane_list: &'a [Value],
    ) -> LoopFuture<'a, ()> {
        self.helper_reaperenv_inbox_maintenance(panes, pane_list)
    }

    fn memory_maintenance(&self) -> LoopFuture<'_, ()> {
        self.helper_reaperenv_memory_maintenance()
    }

    fn set_last_message(&self, message: &str) {
        self.helper_reaperenv_set_last_message(message);
    }
}

pub const TERMINAL_BACKEND_ENV: &str = "DUNKINGSHEEP_TERMINAL_BACKEND";
pub const DEFAULT_TERMINAL_BACKEND: &str = "tmux";

/// Return the normalized configured backend name, using tmux when unset or blank.
/// A configured value that cannot be represented as UTF-8 is a refusal, never a default.
pub fn configured_backend_name() -> Result<String, DunkError> {
    configured_backend_name_from(std::env::var_os(TERMINAL_BACKEND_ENV).as_deref())
}

/// Construct the configured backend, refusing retired and unknown names explicitly.
pub fn configured_backend() -> Result<TmuxBackend, DunkError> {
    let name = configured_backend_name()?;
    backend_from_name(Some(&name))
}

fn configured_backend_name_from(configured: Option<&OsStr>) -> Result<String, DunkError> {
    let Some(configured) = configured else {
        return Ok(DEFAULT_TERMINAL_BACKEND.to_owned());
    };
    let Some(configured) = configured.to_str() else {
        return Err(refusal(
            "terminal backend setting is not valid UTF-8; expected 'tmux'",
        ));
    };
    Ok(normalize_backend_name(Some(configured)))
}

fn normalize_backend_name(configured: Option<&str>) -> String {
    let name = configured.unwrap_or_default().trim().to_lowercase();
    if name.is_empty() {
        DEFAULT_TERMINAL_BACKEND.to_owned()
    } else {
        name
    }
}

fn backend_from_name(configured: Option<&str>) -> Result<TmuxBackend, DunkError> {
    let name = normalize_backend_name(configured);
    match name.as_str() {
        "tmux" => Ok(TmuxBackend::default()),
        "herdr" => Err(refusal(
            "terminal backend 'herdr' is retired; expected 'tmux'",
        )),
        _ => Err(refusal(format!(
            "unknown terminal backend {name:?}; expected 'tmux'"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flock::panes::PaneInfoEnv;
    use crate::flock::panes::Stores;
    use crate::flock::sends::SendEnv;
    use crate::tasks::{ClaimOptions, ClaimOutcome, Tasks};
    use crate::terminal::fakes::FakeTerminal;

    #[test]
    fn production_env_send_read_uses_concrete_services() {
        let dir = crate::testsupport::dir("runtime-send-read");
        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("roles.json"),
            false,
        );
        let mut fake = FakeTerminal::new();
        fake.screens
            .insert("w2:p2".into(), "first\nsecond\nthird".into());
        fake.statuses.insert("w2:p2".into(), " Working ".into());
        let reads = Arc::clone(&fake.reads);
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(fake));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let request = RequestCtx::default();
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);

        assert_eq!(
            SendEnv::read_pane(&adapter.terminal, "w2:p2", 2, true),
            Some("second\nthird".into())
        );
        assert_eq!(
            *reads.lock().expect("reads lock"),
            vec![("w2:p2".into(), 2, true)]
        );
        assert_eq!(
            SendEnv::pane_harness(&adapter.terminal, "w2:p2"),
            Some("claude".into())
        );
        assert_eq!(SendEnv::pane_harness(&adapter.terminal, "w1:p1"), None);
        assert_eq!(SendEnv::pane_harness(&adapter.terminal, "%missing"), None);
        assert_eq!(
            SendEnv::read_status(&adapter.terminal, "w2:p2"),
            Some("working".into())
        );
        assert_eq!(SendEnv::read_status(&adapter.terminal, "%missing"), None);

        let mut unreadable_fake = FakeTerminal::new();
        unreadable_fake.unreadable.insert("w2:p2".into());
        let unreadable_terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(unreadable_fake));
        let unreadable_adapter =
            RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &unreadable_terminal);
        assert_eq!(
            SendEnv::read_pane(&unreadable_adapter.terminal, "w2:p2", 40, false),
            None
        );
    }

    #[test]
    fn production_env_send_state_uses_concrete_services() {
        assert!(federated_input_enabled(Some(OsStr::new("1"))));
        assert!(!federated_input_enabled(None));
        assert!(!federated_input_enabled(Some(OsStr::new("true"))));
        assert!(!federated_input_enabled(Some(OsStr::new(" 1 "))));

        let dir = crate::testsupport::dir("runtime-send-state");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("test runtime");
        let tasks = Arc::new(
            runtime
                .block_on(Tasks::open(dir.path().join("tasks.db")))
                .expect("tasks store"),
        );
        let live_pid = i64::from(std::process::id());
        let live_start = crate::proc::process_start(live_pid);
        let live_task = runtime
            .block_on(tasks.add("global", "live process lease", "test"))
            .expect("live task");
        let live = runtime
            .block_on(tasks.claim_next(
                "global",
                "%live",
                ClaimOptions {
                    caller: Some((live_pid, live_start, Some("test".into()))),
                    ..ClaimOptions::default()
                },
            ))
            .expect("claim live lease");
        assert!(matches!(live, ClaimOutcome::Claimed { .. }));

        let dead_task = runtime
            .block_on(tasks.add("global", "dead process lease", "test"))
            .expect("dead task");
        let dead = runtime
            .block_on(tasks.claim_next(
                "global",
                "%dead",
                ClaimOptions {
                    caller: Some((i64::MAX, None, Some("test".into()))),
                    ..ClaimOptions::default()
                },
            ))
            .expect("claim dead lease");
        assert!(matches!(dead, ClaimOutcome::Claimed { .. }));
        assert_ne!(live_task.id, dead_task.id);

        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("roles.json"),
            false,
        );
        flock.attach_stores(Stores {
            tasks: Some(tasks),
            ..Stores::default()
        });
        let fake = FakeTerminal::new();
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(fake));
        let request = RequestCtx::default();
        let mut adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);
        assert!(!SendEnv::session_process_gone(
            &adapter.terminal,
            "%missing"
        ));
        assert!(!SendEnv::session_process_gone(&adapter.terminal, "%live"));
        assert!(SendEnv::session_process_gone(&adapter.terminal, "%dead"));
        assert_eq!(
            SendEnv::send_key(&mut adapter.terminal, "w1:p1", "Enter"),
            (true, "sent".into())
        );

        let mut unavailable = FakeTerminal::new();
        unavailable.available = false;
        let unavailable_terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(unavailable));
        let mut unavailable_adapter =
            RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &unavailable_terminal);
        assert_eq!(
            SendEnv::send_key(&mut unavailable_adapter.terminal, "w1:p1", "Enter"),
            (
                false,
                "tmux server not running - start it with `tmux new -d`".into()
            )
        );
    }

    #[test]
    fn configured_backend_selects_tmux_by_default_and_refuses_retired_or_unknown_names() {
        let default = backend_from_name(None).expect("unset backend selects tmux");
        assert_eq!(default.tmux_bin, std::path::PathBuf::from("tmux"));
        assert!(backend_from_name(Some("   ")).is_ok());
        assert!(backend_from_name(Some(" TMUX ")).is_ok());
        assert_eq!(
            backend_from_name(Some(" HerDr ")).unwrap_err().to_string(),
            "terminal backend 'herdr' is retired; expected 'tmux'"
        );
        assert_eq!(
            backend_from_name(Some("other")).unwrap_err().to_string(),
            "unknown terminal backend \"other\"; expected 'tmux'"
        );
    }

    #[cfg(unix)]
    #[test]
    fn configured_backend_refuses_non_utf8_environment_names() {
        use std::os::unix::ffi::OsStrExt;

        let invalid_value = OsStr::from_bytes(b"tmux\xff");
        let error = configured_backend_name_from(Some(invalid_value)).unwrap_err();
        assert_eq!(
            error.to_string(),
            "terminal backend setting is not valid UTF-8; expected 'tmux'"
        );
    }

    #[test]
    fn production_request_context_keeps_terminal_on_connection_thread() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<LoopRuntimeAdapter>();

        let dir = crate::testsupport::dir("request-runtime-context");
        let flock = Arc::new(Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("roles.json"),
            false,
        ));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let runtime_handle = runtime.handle().clone();
        let request = RequestCtx {
            self_pane_id: Some("w1:p1".into()),
            ..RequestCtx::default()
        };
        let request_flock = Arc::clone(&flock);
        let connection_thread = std::thread::spawn(move || {
            let before = std::thread::current().id();
            let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
            let adapter =
                RequestRuntimeAdapter::new(&request_flock, &request, &runtime_handle, &terminal);
            let context = adapter.handler_context();
            assert_eq!(context.self_pane_id(), Some("w1:p1"));
            assert!(std::ptr::eq(context.env.terminal(), &terminal));
            assert!(std::ptr::eq(
                context.env.view_cache().expect("Flock cache"),
                request_flock.view_cache()
            ));
            let _ = context.env.list_panes_grouped();
            assert_eq!(before, std::thread::current().id());
        });
        connection_thread.join().expect("connection thread");

        let loop_adapter = LoopRuntimeAdapter::new(
            Arc::clone(&flock),
            Arc::new(|| Box::new(FakeTerminal::new())),
        );
        runtime.block_on(async {
            let count = loop_adapter
                .with_loop_terminal(|_, terminal| PaneInfoEnv::list_panes_grouped(terminal).len())
                .await
                .expect("blocking terminal worker");
            assert!(count > 0);
        });
    }
}
