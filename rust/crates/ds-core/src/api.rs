//! `api.rs` (T39 slice 1): the dispatch CORE. Ports `dunk_api.dispatch`: unknown-command
//! refusal, the owner gate / per-role gate, the desk argument clamp, the required and unknown
//! argument checks, the handler call, and the desk result clamp, in exactly that order.
//!
//! Handlers live in a table keyed by registry command name. The production table registers
//! every registry command (`server_runtime::registrations::production_handlers`, enforced by a
//! test over [`Dispatcher::missing_handlers`]); a partial table, which only tests build, refuses
//! a missing handler by name ("internal error: no handler registered for <x>"), never a guess.
//!
//! NOT in this slice: the handlers themselves.

mod context_mutations;
pub mod health_state;
pub use context_mutations::{h_context_add, h_context_promote, h_memory_backfill};
pub mod inbox_messages;
pub use health_state::{h_health, h_state_capture, h_state_latest};
pub mod outbox_messages;
pub use outbox_messages::{h_cancel_message, h_get_message, h_list_messages};
mod poll_roster_turn_end;
pub use poll_roster_turn_end::{h_poll, h_roster, h_turn_end};
mod owner_push;
pub use owner_push::{h_owner_poll, h_owner_push, h_owner_take};
mod recovery;
pub use recovery::{h_doctor, h_recover};
pub mod runtime_control;
pub use runtime_control::{h_allocation, h_dispatch, h_shutdown};
mod metrics;
pub use metrics::h_metrics;
mod dunks;
pub use dunks::{
    h_add_dunk, h_fire_dunk, h_list_dunks, h_remove_dunk, h_start_dunk, h_stop_all, h_stop_dunk,
    h_toggle_dunk, h_update_dunk,
};
mod dunk_get;
pub use dunk_get::h_get_dunk;
mod project_summary;
pub use project_summary::h_project_summary;
mod project_spawn;
pub use project_spawn::h_project_spawn;
mod layout_agent;
pub mod layout_close;
pub use layout_close::{h_close, h_herdr, h_herdr_help};
mod layout_create;
pub use layout_agent::{h_focus, h_rename, h_start_agent};
pub use layout_create::{h_create_tab, h_create_workspace, h_split_pane};
mod respawn;
pub use respawn::h_respawn_pane;

#[path = "api_owner_handoff.rs"]
pub(crate) mod owner_handoff;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::{json, Map, Value};
use tokio::runtime::Handle;

use crate::error::DunkError;
use crate::flock::commands_tasks::TaskAddArgs;
use crate::flock::panes::PaneInfoEnv;
use crate::flock::Flock;
use crate::proc::Caller;
use crate::server::RequestCtx;
use crate::terminal::Terminal;
use crate::{registry, roles};

/// A request's arguments (the Python `args` dict; insertion-ordered).
pub type Args = Map<String, Value>;

/// The three send operations exposed to T39 handlers. The API layer owns command parsing and
/// role/target gates; the daemon composition supplies the real terminal and queue behavior.
/// `target` is the resolved pane row, and `self_pane_id` is copied from the request context.
pub trait SendCommandEnv {
    fn send_text(
        &self,
        target: &Value,
        text: &str,
        self_pane_id: Option<&str>,
        hold_while_typing: bool,
        wait_s: f64,
        press_enter: bool,
    ) -> Result<Value, DunkError>;

    fn send_keys(
        &self,
        target: &Value,
        keys: &str,
        self_pane_id: Option<&str>,
        hold_while_typing: bool,
        wait_s: f64,
    ) -> Result<Value, DunkError>;

    /// Raw JSON keys are normalized only by the concrete Flock path, after target resolution.
    /// Existing string-only adapters remain compatible; non-string input fails closed unless wired.
    fn send_keys_raw(
        &self,
        target: &Value,
        keys: &Value,
        self_pane_id: Option<&str>,
        hold_while_typing: bool,
        wait_s: f64,
    ) -> Result<Value, DunkError> {
        match keys {
            Value::String(keys) => {
                self.send_keys(target, keys, self_pane_id, hold_while_typing, wait_s)
            }
            _ => Err(DunkError::Refused(
                "raw key command adapter is not wired".into(),
            )),
        }
    }

    fn flush_box(&self, target: &Value, self_pane_id: Option<&str>) -> Result<Value, DunkError>;
}

/// Read-only data sources composed by T39 owner views. Inbox/app lookup errors stay fallible;
/// production composition belongs to t1605, while the pure response transformations remain in
/// `flock::views`.
pub trait OwnerViewEnv {
    fn app_db_path(&self) -> Result<PathBuf, DunkError>;
    fn inbox_status(&self, message_id: &str, now: f64) -> Result<Option<Value>, DunkError>;
    fn inbox_stats(&self, now: f64) -> Result<Vec<Value>, DunkError>;
    fn seen_latencies(&self, mailbox: &str, limit: usize) -> Result<Vec<f64>, DunkError>;
    fn now(&self) -> f64;
}

/// Explicit daemon-owned configuration for `project_spawn`. Production construction is gated
/// with the project runtime/config slice; this API layer supplies no path, amount, provider, or
/// usage default of its own.
#[derive(Debug, Clone)]
pub struct ProjectSpawnConfig {
    pub project_root: PathBuf,
    pub store_path: PathBuf,
    pub budget_json: String,
    pub provider: String,
    pub provider_home: PathBuf,
    pub five_hour_usage: crate::projects::Usage,
    /// A request-time reading from the selected provider. Production adapters supply this so
    /// admission can happen before opening the durable project database.
    pub provider_reading: Option<Value>,
    pub approved: bool,
    pub now: f64,
}

/// A synchronous adapter for command dependencies. All methods are object-safe so a daemon can
/// supply one per request without putting backend types in handler signatures.
pub trait CommandEnv: PaneInfoEnv {
    fn layout_close_env(&self) -> Result<&dyn layout_close::LayoutCloseEnv, DunkError> {
        Err(crate::error::refusal("layout close adapter is not wired"))
    }

    /// Legacy typed project config hook. The production runtime resolves request-specific policy
    /// through `project_spawn_config_for`; this default remains fail-closed.
    fn project_spawn_config(&self) -> Result<ProjectSpawnConfig, DunkError> {
        Err(DunkError::Refused(
            "project spawn config adapter is not wired".into(),
        ))
    }

    /// Resolve trusted project/provider policy using only the desk's bounded selection fields.
    /// Existing typed adapters remain compatible; production overrides this request-aware seam.
    fn project_spawn_config_for(
        &self,
        _project_name: &str,
        _template: &str,
    ) -> Result<ProjectSpawnConfig, DunkError> {
        self.project_spawn_config()
    }

    /// Serialize project creation across requests on the canonical runtime. The default is for
    /// isolated command fixtures; production overrides it with the Flock-owned gate.
    fn with_project_spawn_lock(
        &self,
        _project_name: &str,
        operation: &mut dyn FnMut() -> Result<(), DunkError>,
    ) -> Result<(), DunkError> {
        operation()
    }

    /// Project execution uses only workspace creation and agent start. Runtime implementations
    /// hold the terminal mutex for each operation, releasing it before role or inbox callbacks.
    fn with_project_terminal(
        &self,
        operation: &mut dyn FnMut(
            &mut dyn crate::projects::ProjectTerminal,
        ) -> Result<(), DunkError>,
    ) -> Result<(), DunkError> {
        let mut terminal = self
            .terminal()
            .lock()
            .map_err(|_| DunkError::Refused("terminal lock poisoned".into()))?;
        let mut project_terminal = ProjectTerminalAdapter(&mut **terminal);
        operation(&mut project_terminal)
    }

    fn health_state_env(&self) -> Result<&dyn health_state::HealthStateEnv, DunkError> {
        Err(DunkError::Refused(
            "health/state adapter is not wired".into(),
        ))
    }

    fn with_layout_agent(
        &self,
        _operation: &mut crate::flock::commands_layout::AgentOperation<'_>,
    ) -> Result<Value, DunkError> {
        Err(DunkError::Refused(
            "layout agent adapter is not wired".into(),
        ))
    }

    /// Borrow the request's layout and pane services, with confirmed command submission.
    fn with_layout_create(
        &self,
        _operation: &mut crate::flock::commands_layout::CreateOperation<'_>,
    ) -> Result<Value, DunkError> {
        Err(DunkError::Refused(
            "layout create adapter is not wired".into(),
        ))
    }

    fn with_outbox_commands(
        &self,
        _operation: &mut dyn FnMut(
            &dyn outbox_messages::OutboxCommandEnv,
        ) -> Result<Value, DunkError>,
    ) -> Result<Value, DunkError> {
        Err(DunkError::Refused(
            "outbox command adapter is not wired".into(),
        ))
    }

    /// Borrow the validated inbox services for this request on its owning thread.
    fn with_inbox_env(
        &self,
        _operation: &mut dyn FnMut(
            &dyn crate::flock::commands_inbox::InboxEnv,
        ) -> Result<Value, DunkError>,
    ) -> Result<Value, DunkError> {
        Err(DunkError::Refused(
            "inbox command adapter is not wired".into(),
        ))
    }

    /// The daemon's terminal adapter. The mutex permits the existing mutable terminal API while
    /// keeping the request context shared with role and read-only adapters.
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>>;

    /// Delivery-target evidence composed by the request runtime. The existing Flock
    /// delivery-capability method owns target resolution and response shaping.
    fn delivery_env(&self) -> Result<&dyn crate::flock::delivery::DeliveryEnv, DunkError> {
        Err(DunkError::Refused("delivery adapter is not wired".into()))
    }

    /// Run one existing `Flock::clear_box` operation with the request-local mutable
    /// composer adapter. Keeping adapter construction behind this callback lets the runtime
    /// borrow its non-Send terminal only for this operation.
    fn with_clear_box_env(
        &self,
        _operation: &mut dyn FnMut(
            &mut dyn crate::flock::clear_box::ClearBoxEnv,
        ) -> Result<Value, DunkError>,
    ) -> Result<Value, DunkError> {
        Err(DunkError::Refused("clear box adapter is not wired".into()))
    }

    /// Run the guarded respawn operation with the request's canonical runtime providers.
    /// The runtime owns the temporary inbox view and terminal adapter for this callback.
    fn with_respawn_pane_env(
        &self,
        _operation: &mut dyn FnMut(
            &crate::flock::respawn::RespawnServices<'_>,
        ) -> Result<Value, DunkError>,
    ) -> Result<Value, DunkError> {
        Err(DunkError::Refused(
            "respawn pane adapter is not wired".into(),
        ))
    }

    /// Execute one already-implemented dunk mutation through the request's canonical runtime.
    /// Target lookup stays in the API wrapper; timer, persistence and cache behavior stay in
    /// RuntimeServices.
    fn add_dunk(
        &self,
        _pane: Option<&serde_json::Map<String, Value>>,
        _args: &crate::flock::dunks::AddArgs,
    ) -> Result<serde_json::Map<String, Value>, DunkError> {
        Err(DunkError::Refused(
            "dunk mutation adapter is not wired".into(),
        ))
    }

    fn update_dunk(
        &self,
        _id: &str,
        _pane: Option<&serde_json::Map<String, Value>>,
        _args: &crate::flock::dunks::UpdateArgs,
    ) -> Result<serde_json::Map<String, Value>, DunkError> {
        Err(DunkError::Refused(
            "dunk mutation adapter is not wired".into(),
        ))
    }

    fn remove_dunk(&self, _id: &str) -> Result<serde_json::Map<String, Value>, DunkError> {
        Err(DunkError::Refused(
            "dunk mutation adapter is not wired".into(),
        ))
    }

    fn start_dunk(&self, _id: &str) -> Result<serde_json::Map<String, Value>, DunkError> {
        Err(DunkError::Refused(
            "dunk lifecycle adapter is not wired".into(),
        ))
    }

    fn stop_dunk(&self, _id: &str) -> Result<serde_json::Map<String, Value>, DunkError> {
        Err(DunkError::Refused(
            "dunk lifecycle adapter is not wired".into(),
        ))
    }

    fn toggle_dunk(&self, _id: &str) -> Result<serde_json::Map<String, Value>, DunkError> {
        Err(DunkError::Refused(
            "dunk lifecycle adapter is not wired".into(),
        ))
    }

    fn stop_all_dunks(&self) -> Result<Vec<serde_json::Map<String, Value>>, DunkError> {
        Err(DunkError::Refused(
            "dunk lifecycle adapter is not wired".into(),
        ))
    }

    fn fire_dunk(&self, _id: &str, _countdown_s: i64, _wait: bool) -> Result<Value, DunkError> {
        Err(DunkError::Refused(
            "manual dunk fire adapter is not wired".into(),
        ))
    }

    /// Wall-clock time, in epoch seconds, for request-scoped caches and command timestamps.
    fn now_s(&self) -> f64;

    /// The accepted GitHub App token cache used by `gh_token`.
    fn github_tokens(&self) -> Result<&crate::github::Tokens, DunkError> {
        Err(DunkError::Refused(
            "GitHub token adapter is not wired".into(),
        ))
    }

    /// HTTP transport for the canonical review synchronization service. The default is the
    /// shared real reqwest client; tests can inject a deterministic fake.
    fn review_transport(&self) -> Result<&dyn crate::github::GitHubTransport, DunkError> {
        static TRANSPORT: std::sync::OnceLock<crate::github::ReqwestTransport> =
            std::sync::OnceLock::new();
        Ok(TRANSPORT.get_or_init(crate::github::ReqwestTransport::default))
    }

    /// Borrow the canonical recovery providers for this request. Live composition is t1605.
    fn recover_env(&self) -> Result<&dyn crate::flock::recovery_commands::RecoverEnv, DunkError> {
        Err(DunkError::Refused("recovery adapter is not wired".into()))
    }

    /// Status sources for the `status` handler. Existing fixtures may stage this adapter, but
    /// production dispatch must supply the real daemon snapshot.
    fn status_env(&self) -> Result<&dyn crate::flock::status::StatusEnv, DunkError> {
        Err(DunkError::Refused(
            "status command adapter is not wired".into(),
        ))
    }

    /// Borrow canonical control services; production composition is supplied by t1605.
    fn runtime_control_env(&self) -> Result<&dyn runtime_control::RuntimeControlEnv, DunkError> {
        Err(DunkError::Refused(
            "runtime control adapter is not wired".into(),
        ))
    }

    /// Existing fallible project summary sources, including captured startup paths.
    fn project_summary_env(
        &self,
    ) -> Result<&dyn crate::flock::project_summary::SummaryEnv, DunkError> {
        Err(DunkError::Refused(
            "project summary adapter is not wired".into(),
        ))
    }

    /// The daemon-owned cache shared by status/view calls.
    fn view_cache(&self) -> Result<&crate::flock::status::ViewCache, DunkError> {
        Err(DunkError::Refused("status view cache is not wired".into()))
    }

    /// Real turn side effects (metrics and next-job observation).
    fn turn_hooks(&self) -> Result<&dyn crate::flock::commands_turns::TurnHooks, DunkError> {
        Err(DunkError::Refused("turn hooks are not wired".into()))
    }

    /// The concrete data sources used by the `roster` view.
    fn roster_env(&self) -> Result<&dyn crate::flock::roster::RosterEnv, DunkError> {
        Err(DunkError::Refused("roster adapter is not wired".into()))
    }

    /// The request's shared next-job manager. Completion must use the same manager that
    /// services turn-end decisions so its per-pane lock and handoff state remain canonical.
    fn next_jobs(&self) -> Result<&crate::tasks::next_job_runtime::NextJobs, DunkError> {
        Err(DunkError::Refused(
            "request next-job service is not attached".into(),
        ))
    }

    /// Preserve the runtime's lazy module/config error boundary before converting
    /// the limit. Other adapters retain the accepted argument conversion here.
    fn poolrad_sync_args(&self, args: &Args) -> Result<Value, DunkError> {
        self.poolrad_sync(poolrad_limit(args)?, poolrad_dry_run(args.get("dry_run")))
    }

    /// The accepted T52 Poolrad sync service used by the T39 command adapter.
    fn poolrad_sync(&self, _limit: usize, _dry_run: bool) -> Result<Value, DunkError> {
        Err(DunkError::Refused(
            "poolrad sync adapter is not wired".into(),
        ))
    }

    /// The accepted T52 Poolrad progress service used by the T39 command adapter.
    fn poolrad_progress(&self) -> Result<Value, DunkError> {
        Err(DunkError::Refused(
            "poolrad progress adapter is not wired".into(),
        ))
    }

    /// Raw store and cache inputs for the report backend. Production wiring is owned by t1605.
    fn report_sources(&self) -> Result<&dyn crate::report::ReportSources, DunkError> {
        Err(DunkError::Refused(
            "report sources adapter is not wired".into(),
        ))
    }

    /// The daemon-owned owner-push store used by the T39 owner stats and receipt views.
    fn owner_push(&self) -> Result<&crate::owner::OwnerPush, DunkError> {
        Err(DunkError::Refused("owner push adapter is not wired".into()))
    }

    /// The retired-pane archive used by `retire_pane`.
    fn retired_panes_path(&self) -> Result<PathBuf, DunkError> {
        crate::panes::default_path(&crate::config::Env::try_from_process()?)
    }

    /// The terminal send/queue service. Production wiring is owned by t1605.
    fn send_commands(&self) -> Result<&dyn SendCommandEnv, DunkError> {
        Err(DunkError::Refused(
            "send command adapter is not wired".into(),
        ))
    }

    /// Read-only data sources for the owner message-state and reply-stat views.
    fn owner_views(&self) -> Result<&dyn OwnerViewEnv, DunkError> {
        Err(DunkError::Refused("owner view adapter is not wired".into()))
    }

    /// Fallible PM routing and digest services, lent for one request. The callback shape matches
    /// `with_inbox_env`, so the PM view cannot outlive the request's inbox view.
    fn with_pm_env(
        &self,
        _operation: &mut dyn FnMut(&dyn crate::pm::PmEnv) -> Result<Value, DunkError>,
    ) -> Result<Value, DunkError> {
        Err(DunkError::Refused("PM adapter is not wired".into()))
    }
}

struct ProjectTerminalAdapter<'a>(&'a mut dyn Terminal);

impl crate::projects::ProjectTerminal for ProjectTerminalAdapter<'_> {
    fn create_workspace(
        &mut self,
        label: Option<&str>,
        cwd: Option<&str>,
        focus: bool,
    ) -> Result<crate::terminal::Workspace, crate::terminal::TerminalError> {
        self.0.create_workspace(label, cwd, focus)
    }

    fn start_agent(
        &mut self,
        spec: &crate::terminal::AgentSpec,
    ) -> Result<crate::terminal::Pane, crate::terminal::TerminalError> {
        self.0.start_agent(spec)
    }
}

/// Context shared by every handler in one synchronous request. The server must keep this whole
/// dispatch on its dedicated `std::thread` connection worker. In particular, a Tokio
/// `spawn_blocking` worker is not sufficient: it still has a current Tokio handle and is rejected
/// by [`HandlerContext::block_on`].
pub struct HandlerContext<'a> {
    pub flock: &'a Flock,
    pub request: &'a RequestCtx,
    pub env: &'a dyn CommandEnv,
    pub runtime: &'a Handle,
}

// Refusal logging is scoped to this privileged body verb and selects metadata fields explicitly.
fn audit_inbox_get_refusal(context: &HandlerContext<'_>, role: &str, args: Option<&Args>) {
    let field = |key| args.and_then(|args| args.get(key));
    eprintln!(
        "{}",
        crate::inbox::body_view_refusal_audit(
            role,
            context.self_pane_id(),
            field("mailbox"),
            field("message_id"),
            field("include_agent")
        )
    );
}

impl HandlerContext<'_> {
    pub fn self_pane_id(&self) -> Option<&str> {
        self.request.self_pane_id.as_deref()
    }

    /// Drive an async store/Flock call on the dedicated standard thread running this request.
    /// This rejects Tokio tasks and `spawn_blocking` workers, where caller-sensitive work could
    /// otherwise be moved to a different thread between synchronous handler calls.
    pub fn block_on<F: std::future::Future>(&self, future: F) -> Result<F::Output, DunkError> {
        if Handle::try_current().is_ok() {
            return Err(DunkError::Refused(
                "async command work requires a dedicated blocking request thread".into(),
            ));
        }
        Ok(self.runtime.block_on(future))
    }
}

/// A handler receives the complete per-request context, not ambient caller arguments.
pub type Handler = fn(&HandlerContext<'_>, Args) -> Result<Value, DunkError>;

/// Installs request identity into Flock's thread-local and clears all fields on every exit path.
struct RequestCallerGuard<'a>(&'a Flock);

impl<'a> RequestCallerGuard<'a> {
    fn install(flock: &'a Flock, request: &RequestCtx) -> Self {
        let caller = request.caller.as_ref().map(caller_value);
        flock.set_caller(caller);
        flock.set_service_token(request.service_token.as_deref());
        flock.with_caller(|state| {
            state.identity_bound = request.identity_bound;
            state.identity_note = request.identity_note.clone();
            state.socket_peer_uid = request.socket_peer_uid;
        });
        flock.set_socket_peer(request.socket_peer);
        Self(flock)
    }
}

impl Drop for RequestCallerGuard<'_> {
    fn drop(&mut self) {
        self.0.clear_caller();
        self.0.clear_socket_peer();
    }
}

fn caller_value(caller: &Caller) -> Value {
    json!({
        "pane_id": caller.pane_id,
        "pid": caller.pid,
        "start": caller.start,
        "name": caller.name,
        "source": caller.source,
    })
}

/// The two caller-identity calls `dispatch` makes on the flock (`Flock.caller_role`,
/// `Flock.require_superuser`). A trait so this slice does not wait for flock/roles_gate.rs.
pub trait CallerGate {
    fn caller_role(&self, self_pane_id: Option<&str>) -> Result<String, DunkError>;
    fn require_superuser(&self, self_pane_id: Option<&str>, name: &str) -> Result<(), DunkError>;
}

/// The real gate: the flock's own role methods over the request's pane adapter and clock.
pub struct FlockGate<'a>(pub &'a HandlerContext<'a>);

impl CallerGate for FlockGate<'_> {
    fn caller_role(&self, self_pane_id: Option<&str>) -> Result<String, DunkError> {
        self.0.flock.caller_role(self_pane_id, self.0.env)
    }
    fn require_superuser(&self, self_pane_id: Option<&str>, name: &str) -> Result<(), DunkError> {
        self.0
            .flock
            .require_superuser(self_pane_id, name, self.0.env)
    }
}

/// `dunk_roles.DESK_LIMIT`.
const DESK_LIMIT: i64 = 5;

/// Python `int(x)` on a JSON value, for the clamp: `None` when int() would raise.
fn py_int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        Value::Bool(b) => Some(*b as i64),
        _ => None,
    }
}

/// Python truthiness of a JSON value (`args.get(k) or default`).
pub(crate) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_none_or(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

fn python_scope_arg(value: Option<&Value>) -> Option<String> {
    let value = value?;
    Some(match value {
        Value::Null => return None,
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(python_value_repr)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| format!("{}: {}", py_repr(key), python_value_repr(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    })
}

pub(crate) fn python_value_repr(value: &Value) -> String {
    match value {
        Value::String(value) => py_repr(value),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(python_value_repr)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| format!("{}: {}", py_repr(key), python_value_repr(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(value) => crate::github::python_number_string(value),
    }
}

/// Python string coercion for JSON values at shared protocol boundaries.
pub fn python_value_string(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Array(_) | Value::Object(_) => python_value_repr(value),
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(value) => crate::github::python_number_string(value),
    }
}

fn python_optional_string_arg(args: &Args, key: &str) -> Option<String> {
    args.get(key)
        .filter(|value| !value.is_null())
        .map(python_value_string)
}

fn task_values(value: Option<&Value>) -> Option<Vec<String>> {
    let value = value.filter(|value| !value.is_null())?;
    let values: Vec<String> = match value {
        Value::Array(values) => values.iter().map(python_value_string).collect(),
        _ => python_value_string(value)
            .split(',')
            .map(str::to_string)
            .collect(),
    };
    Some(
        values
            .into_iter()
            .map(|value: String| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .collect(),
    )
}

fn task_add_string<'a>(args: &'a Args, key: &str) -> Result<Option<&'a str>, DunkError> {
    task_string_arg(args, key, "task_add")
}

fn task_string_arg<'a>(
    args: &'a Args,
    key: &str,
    command: &str,
) -> Result<Option<&'a str>, DunkError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(DunkError::Refused(format!(
            "{command}: {key} must be a string"
        ))),
    }
}

fn python_int_arg(args: &Args, key: &str, default: i64) -> Result<i64, DunkError> {
    let Some(value) = args.get(key).filter(|value| !value.is_null()) else {
        return Ok(default);
    };
    match value {
        Value::Bool(value) => Ok(i64::from(*value)),
        Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                Ok(value)
            } else if value.as_u64().is_some() {
                Ok(i64::MAX)
            } else if let Some(value) = value.as_f64() {
                let value = value.trunc();
                Ok(if value >= i64::MAX as f64 {
                    i64::MAX
                } else if value <= i64::MIN as f64 {
                    i64::MIN
                } else {
                    value as i64
                })
            } else {
                Err(DunkError::Refused("cannot convert value to int".into()))
            }
        }
        Value::String(value) => parse_python_decimal_integer(value).ok_or_else(|| {
            DunkError::Refused(format!(
                "invalid literal for int() with base 10: {}",
                py_repr(value)
            ))
        }),
        Value::Array(_) => Err(DunkError::Refused(
            "int() argument must be a string, a bytes-like object or a real number, not 'list'"
                .into(),
        )),
        Value::Object(_) => Err(DunkError::Refused(
            "int() argument must be a string, a bytes-like object or a real number, not 'dict'"
                .into(),
        )),
        Value::Null => unreachable!("null limit uses the default"),
    }
}

fn parse_python_decimal_integer(value: &str) -> Option<i64> {
    match parse_python_decimal_integer_checked(value) {
        Ok(value) => Some(value),
        Err(DecimalIntError::Invalid) => None,
        Err(DecimalIntError::Overflow) if value.trim_start().starts_with('-') => Some(i64::MIN),
        Err(DecimalIntError::Overflow) => Some(i64::MAX),
    }
}

/// `dunk_roles.clamp_desk_args`: daemon-side result caps for the desk role.
pub fn clamp_desk_args(command: &str, args: &Args) -> Args {
    let mut args = args.clone();
    let listing = matches!(
        command,
        "memory_recall"
            | "memory_search"
            | "memory_context"
            | "task_list"
            | "inbox_read"
            | "inbox_list"
    );
    let clamp = |args: &Args, key: &str, default: i64, cap: i64| -> i64 {
        let wanted = args
            .get(key)
            .filter(|v| truthy(v))
            .and_then(py_int)
            .unwrap_or(default);
        wanted.min(cap).max(1)
    };
    if args.contains_key("limit") || listing {
        let limit = clamp(&args, "limit", DESK_LIMIT, DESK_LIMIT);
        args.insert("limit".into(), Value::from(limit));
    }
    if command == "read_pane" {
        let lines = clamp(&args, "lines", 40, 40);
        args.insert("lines".into(), Value::from(lines));
    }
    args
}

/// The dispatch table plus the gate.
#[derive(Default)]
pub struct Dispatcher {
    handlers: HashMap<&'static str, Handler>,
    #[cfg(test)]
    registration_attempts: Vec<&'static str>,
}

impl Dispatcher {
    pub fn register_health_state(&mut self) {
        self.register("health", h_health);
        self.register("state_capture", h_state_capture);
        self.register("state_latest", h_state_latest);
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Register the handler for a registry command (later slices call this).
    pub fn register(&mut self, name: &'static str, handler: Handler) {
        #[cfg(test)]
        self.registration_attempts.push(name);
        self.handlers.insert(name, handler);
    }

    /// Registry commands this table has no handler for: empty for the production assembly.
    pub fn missing_handlers(&self) -> Vec<&'static str> {
        registry::COMMANDS
            .iter()
            .map(|command| command.name)
            .filter(|name| !self.handlers.contains_key(name))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn registration_attempts(&self) -> &[&'static str] {
        &self.registration_attempts
    }

    /// Register only the T39 status, identity and turn-start handler leaf.
    pub fn register_status_whoami_turn_start(&mut self) {
        self.register("status", h_status);
        self.register("whoami", h_whoami);
        self.register("turn_start", h_turn_start);
    }

    /// Register only the T39 turn-end, sidebar poll, and roster handlers.
    pub fn register_turn_end_poll_roster(&mut self) {
        self.register("turn_end", h_turn_end);
        self.register("poll", h_poll);
        self.register("roster", h_roster);
    }

    /// Register only the T39 project summary handler.
    pub fn register_project_summary(&mut self) {
        self.register("project_summary", h_project_summary);
    }

    /// Register only the T39 Rust project-spawn handler. Runtime config remains independently
    /// gated; an unwired adapter refuses before opening the store or touching the terminal.
    pub fn register_project_spawn(&mut self) {
        self.register("project_spawn", h_project_spawn);
    }

    /// Register only the T39 help handler leaf.
    pub fn register_help(&mut self) {
        self.register("help", h_help);
    }

    /// Register only the T39 lane-unregister handler.
    pub fn register_lane_unregister(&mut self) {
        self.register("lane_unregister", h_lane_unregister);
    }

    /// Register only the T39 lane registry handlers.
    pub fn register_lanes(&mut self) {
        self.register("lanes", h_lanes);
        self.register("lane_set", h_lane_set);
        self.register("lane_register", h_lane_register);
        self.register("lane_unregister", h_lane_unregister);
    }

    /// Register only the T39 context supersede and retire handlers.
    pub fn register_context_mutations(&mut self) {
        self.register("context_add", h_context_add);
        self.register("context_promote", h_context_promote);
        self.register("memory_backfill", h_memory_backfill);
    }

    pub fn register_context_supersede_retire(&mut self) {
        self.register("context_supersede", h_context_supersede);
        self.register("context_retire", h_context_retire);
    }

    /// Register only the T39 memory recall, search and context handlers.
    pub fn register_memory_recall_search_context(&mut self) {
        self.register("memory_recall", h_memory_recall);
        self.register("memory_search", h_memory_search);
        self.register("memory_context", h_memory_context);
    }

    /// Register the memory management handlers owned by the T39 memory API leaf.
    pub fn register_memory_stats_status_ingest(&mut self) {
        self.register("memory_stats", h_memory_stats);
        self.register("memory_status", h_memory_status);
        self.register("memory_ingest", h_memory_ingest);
    }

    /// Register only the T39 pane and workspace listing handlers.
    pub fn register_list_panes_read_pane_workspaces(&mut self) {
        self.register("list_panes", h_list_panes);
        self.register("read_pane", h_read_pane);
        self.register("list_workspaces", h_list_workspaces);
    }

    /// Register only the T37c retire-pane handler.
    pub fn register_retire_pane(&mut self) {
        self.register("retire_pane", h_retire_pane);
    }

    /// Register only the T39 superuser status/add/remove handlers.
    pub fn register_superuser_status_add_remove(&mut self) {
        self.register("superuser_status", h_superuser_status);
        self.register("superuser_add", h_superuser_add);
        self.register("superuser_remove", h_superuser_remove);
    }

    /// Register only the T39 superuser recovery and service-token handlers.
    pub fn register_superuser_recover_tokens(&mut self) {
        self.register("superuser_recover", h_superuser_recover);
        self.register("token_issue", h_token_issue);
        self.register("token_revoke", h_token_revoke);
    }

    /// Register only the T39 GitHub App token handler.
    pub fn register_gh_token(&mut self) {
        self.register("gh_token", h_gh_token);
    }

    /// Register only the T39 review synchronization and list handlers.
    pub fn register_review_sync_list(&mut self) {
        self.register("pr_wait", h_pr_wait);
        self.register("review_sync", h_review_sync);
        self.register("review_list", h_review_list);
    }

    /// Register the recovery handlers; metrics keeps its existing registration.
    pub fn register_doctor_recover(&mut self) {
        self.register("doctor", h_doctor);
        self.register("recover", h_recover);
    }

    /// Register only dispatch, allocation, and shutdown.
    pub fn register_runtime_control(&mut self) {
        self.register("dispatch", h_dispatch);
        self.register("allocation", h_allocation);
        self.register("shutdown", h_shutdown);
    }

    /// Register only the T39 operations metrics summary handler.
    pub fn register_metrics(&mut self) {
        self.register("metrics", h_metrics);
    }

    /// Register only the T39 Poolrad sync and progress handlers.
    pub fn register_poolrad_sync_progress(&mut self) {
        self.register("poolrad_sync", h_poolrad_sync);
        self.register("poolrad_progress", h_poolrad_progress);
    }

    /// Register the T39 report run/list handlers over an explicit source adapter.
    pub fn register_report_run_list(&mut self) {
        self.register("report_run", crate::report::report_run_handler);
        self.register("report_list", h_report_list);
    }

    /// Register only the T39 report-list handler.
    pub fn register_report_list(&mut self) {
        self.register("report_list", h_report_list);
    }

    /// Direct owner queue commands; production assembly is owned separately.
    pub fn register_owner_push_poll_take(&mut self) {
        self.register("owner_push", h_owner_push);
        self.register("owner_poll", h_owner_poll);
        self.register("owner_take", h_owner_take);
    }

    /// Register the T39 owner statistics, receipts and conversation views.
    pub fn register_owner_stats_receipts_conversation(&mut self) {
        self.register("owner_stats", h_owner_stats);
        self.register("owner_receipts", h_owner_receipts);
        self.register("owner_conversation", h_owner_conversation);
    }

    /// Register the T39 owner delivery and mailbox view handlers.
    pub fn register_owner_delivered_message_state_reply_stats(&mut self) {
        self.register("owner_delivered", h_owner_delivered);
        self.register("owner_page", h_owner_page);
        self.register("owner_page_confirmed", h_owner_page_confirmed);
        self.register("message_state", h_message_state);
        self.register("reply_stats", h_reply_stats);
    }

    /// Register only the T39 owner reply, ingest and outbox handlers.
    pub fn register_owner_reply_ingest_outbox(&mut self) {
        self.register("owner_handoff", h_owner_handoff);
        self.register("owner_reply", h_owner_reply);
        self.register("owner_ingest", h_owner_ingest);
        self.register("owner_outbox", h_owner_outbox);
    }

    /// Register only the T39 PM digest and routing handlers.
    pub fn register_pm_digest_routing(&mut self) {
        self.register("pm_digest", h_pm_digest);
        self.register("pm_routing", h_pm_routing);
    }

    /// Register only the T39 task-following API leaf.
    pub fn register_task_follow_unfollow_followers(&mut self) {
        self.register("task_follow", h_task_follow);
        self.register("task_unfollow", h_task_unfollow);
        self.register("task_followers", h_task_followers);
    }

    /// Register the T39 task creation, listing and read handler group.
    pub fn register_task_add_list_read(&mut self) {
        self.register("task_add", h_task_add);
        self.register("task_list", h_task_list);
        self.register("task_read", h_task_read);
    }

    /// Register only the T39 task-delete handler leaf.
    pub fn register_task_delete(&mut self) {
        self.register("task_delete", h_task_delete);
    }

    /// Register only the T39 task mutation and lease handlers assigned to t2966.
    pub fn register_task_mutation_leases(&mut self) {
        self.register("task_owner_question", h_task_owner_question);
        self.register("task_owner_answer", h_task_owner_answer);
        self.register("task_update", h_task_update);
        self.register("task_assign", h_task_assign);
        self.register("task_note", h_task_note);
        self.register("task_block", h_task_block);
        self.register("task_unblock", h_task_unblock);
        self.register("task_cancel", h_task_cancel);
        self.register("task_next", h_task_next);
        self.register("task_batch_claim", h_task_batch_claim);
        self.register("task_heartbeat", h_task_heartbeat);
        self.register("task_release", h_task_release);
    }

    /// Register only the T39 direct-send handlers.
    pub fn register_outbox_messages(&mut self) {
        self.register("list_messages", h_list_messages);
        self.register("get_message", h_get_message);
        self.register("cancel_message", h_cancel_message);
    }

    pub fn register_send_text_keys_flush_box(&mut self) {
        self.register("send_text", h_send_text);
        self.register("send_keys", h_send_keys);
        self.register("flush_box", h_flush_box);
    }

    /// Register only the T39 clear-box and delivery handlers.
    pub fn register_clear_box_delivery_capability(&mut self) {
        self.register("clear_box", h_clear_box);
        self.register("delivery", h_delivery);
        self.register("delivery_capability", h_delivery_capability);
    }

    /// Register only the T37c guarded respawn-pane handler.
    pub fn register_respawn_pane(&mut self) {
        self.register("respawn_pane", h_respawn_pane);
    }

    /// Register only the T39 add, update and remove dunk handler group.
    pub fn register_dunk_mutations(&mut self) {
        self.register("add_dunk", h_add_dunk);
        self.register("update_dunk", h_update_dunk);
        self.register("remove_dunk", h_remove_dunk);
    }

    /// Register only the T39 start, stop and toggle dunk lifecycle handlers.
    pub fn register_dunk_lifecycle(&mut self) {
        self.register("start_dunk", h_start_dunk);
        self.register("stop_dunk", h_stop_dunk);
        self.register("toggle_dunk", h_toggle_dunk);
    }

    /// Register only the T39 get-dunk handler.
    pub fn register_get_dunk(&mut self) {
        self.register("get_dunk", h_get_dunk);
    }

    /// Register only the T39 stop-all, manual-fire and list-dunks handlers.
    pub fn register_stop_all_fire_list_dunks(&mut self) {
        self.register("stop_all", h_stop_all);
        self.register("fire_dunk", h_fire_dunk);
        self.register("list_dunks", h_list_dunks);
    }

    /// Register only the three create-layout commands.
    pub fn register_layout_close(&mut self) {
        self.register("close", h_close);
        self.register("herdr", h_herdr);
        self.register("herdr_help", h_herdr_help);
    }

    pub fn register_layout_agent(&mut self) {
        self.register("start_agent", h_start_agent);
        self.register("rename", h_rename);
        self.register("focus", h_focus);
    }

    pub fn register_layout_create(&mut self) {
        self.register("create_workspace", h_create_workspace);
        self.register("create_tab", h_create_tab);
        self.register("split_pane", h_split_pane);
    }

    /// `dunk_api.dispatch(flock, name, args, self_pane_id)`, with the acting pane read only from
    /// this request's [`RequestCtx`] and caller state scoped to this synchronous request thread.
    pub fn dispatch<G: CallerGate + ?Sized>(
        &self,
        context: &HandlerContext<'_>,
        gate: &G,
        name: &str,
        args: Option<Args>,
    ) -> Result<Value, DunkError> {
        let _caller = RequestCallerGuard::install(context.flock, context.request);
        let self_pane_id = context.self_pane_id();
        let command = registry::command(name)
            .ok_or_else(|| DunkError::Refused(format!("unknown command {}", py_repr(name))))?;
        let role = match gate.caller_role(self_pane_id) {
            Ok(role) => role,
            Err(error) => {
                if name == "inbox_get" {
                    audit_inbox_get_refusal(context, "unresolved", args.as_ref());
                }
                return Err(error);
            }
        };
        if command.owner_only && role != "system" && role != "web" {
            // The owner gate: passing it IS the permission for these verbs.
            gate.require_superuser(self_pane_id, name)?;
        } else if !roles::allowed(&role, name) {
            if name == "inbox_get" {
                audit_inbox_get_refusal(context, &role, args.as_ref());
            }
            return Err(DunkError::Refused(format!(
                "{name} is not permitted for role {role}{}; see docs/FLAT_FLEET.md section 4.3",
                match self_pane_id.filter(|p| !p.is_empty()) {
                    Some(pane) => format!(" (pane {pane})"),
                    None => " (no pane, no token)".to_string(),
                }
            )));
        }
        let mut args = args.unwrap_or_default();
        if role == "desk" {
            args = clamp_desk_args(name, &args);
        }
        let missing: Vec<&str> = command
            .dispatch_required
            .iter()
            .copied()
            .filter(|key| match args.get(*key) {
                None => true,
                Some(Value::Null) => true,
                Some(Value::String(s)) => s.is_empty(),
                _ => false,
            })
            .collect();
        if !missing.is_empty() {
            if name == "inbox_get" {
                audit_inbox_get_refusal(context, &role, Some(&args));
            }
            return Err(DunkError::Refused(format!(
                "{name}: missing required argument(s): {}",
                missing.join(", ")
            )));
        }
        let mut unknown: Vec<&str> = args
            .keys()
            .map(String::as_str)
            .filter(|key| !command.params.iter().any(|p| p.name == *key))
            .collect();
        if !unknown.is_empty() {
            unknown.sort_unstable();
            if name == "inbox_get" {
                audit_inbox_get_refusal(context, &role, Some(&args));
            }
            return Err(DunkError::Refused(format!(
                "{name}: unknown argument(s): {}",
                unknown.join(", ")
            )));
        }
        // The production table is total (registrations.rs enforces it); only a partial test
        // table can land here, and it is told so by name rather than given a guess.
        let handler = self.handlers.get(command.name).ok_or_else(|| {
            if name == "inbox_get" {
                audit_inbox_get_refusal(context, &role, Some(&args));
            }
            DunkError::Refused(format!(
                "internal error: no handler registered for {}",
                command.name
            ))
        })?;
        if name == "inbox_read" {
            // The desk clamp runs after the handler returns, so the handler must know whether to mark
            // only the messages the clamp keeps (t3539). Set here from the resolved role, never taken from
            // the caller.
            args.insert("desk_clamp".into(), json!(role == "desk"));
        }
        let result = handler(context, args)?;
        Ok(if role == "desk" {
            roles::clamp_desk_result(&result)
        } else {
            result
        })
    }
}

/// `dunk_api.h_whoami`.
pub fn h_whoami(context: &HandlerContext<'_>, _: Args) -> Result<Value, DunkError> {
    context.flock.whoami(context.self_pane_id(), context.env)
}

/// `dunk_api.h_help`.
pub fn h_help(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let version = context.env.status_env()?.version();
    let commands: Vec<Value> = registry::COMMANDS
        .iter()
        .map(|command| {
            json!({
                "name": command.name,
                "description": command.description,
                "input_schema": registry::input_schema(command.name)
                    .expect("registered command must have an input schema"),
                "mcp_tool": command.mcp,
            })
        })
        .collect();
    Ok(json!({
        "guide": crate::help::guide(python_bool_arg(&args, "markdown", false), &version),
        // The Rust runtime currently supports the tmux terminal only; Python's herdr_help
        // raises DunkError on tmux, which the help handler turns into JSON null.
        "herdr_help": Value::Null,
        "protocol": {
            "transport": "unix socket, newline-delimited JSON",
            "request": {"id": "any", "cmd": "<command name>", "args": {},
                        "self_pane_id": "optional pane id that 'self' resolves to"},
            "response": {"id": "echoed", "ok": true, "result": {}},
            "error": {"id": "echoed", "ok": false, "error": "message"},
        },
        "commands": commands,
    }))
}

/// `dunk_api.h_lanes`: list the registered lanes or declare a pane/mailbox pair.
pub fn h_list_panes(context: &HandlerContext<'_>, _: Args) -> Result<Value, DunkError> {
    const KEEP: &[&str] = &[
        "pane_id",
        "is_self",
        "workspace_label",
        "tab_label",
        "agent",
        "agent_status",
        "agent_status_source",
        "agent_status_reason",
        "agent_status_inferred",
        "cwd",
        "terminal_title_stripped",
        "terminal_id",
        "workspace_id",
        "tab_id",
        "pane_pid",
    ];
    let panes = context.env.list_panes_grouped();
    let mut projected = Vec::with_capacity(panes.len());
    for mut pane in panes {
        let self_pane_id = context.self_pane_id().filter(|id| !id.is_empty());
        pane["is_self"] = json!(self_pane_id.is_some_and(|id| pane["pane_id"] == id));
        let activity = context.flock.lane_activity(&pane, context.env.now_s());
        pane["agent_status"] = activity["state"].clone();
        pane["agent_status_source"] = activity["source"].clone();
        pane["agent_status_reason"] = activity["reason"].clone();
        pane["agent_status_inferred"] = activity["inferred"].clone();
        let row = pane
            .as_object()
            .expect("pane info rows are JSON objects")
            .iter()
            .filter(|(key, _)| KEEP.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<Map<String, Value>>();
        projected.push(Value::Object(row));
    }
    let mut result = json!({
        "self_pane_id": context.self_pane_id(),
        "panes": projected,
    });

    // Match Flock.registry_pane_gap: this is diagnostic only, so an unreadable
    // registry/list does not turn an otherwise useful pane listing into failure.
    if let (Ok(registry), Ok(terminal)) = (
        context.flock.lane_registry(false),
        context.env.terminal().lock(),
    ) {
        let live = terminal
            .list_panes()
            .into_iter()
            .map(|pane| pane.pane_id)
            .collect::<std::collections::BTreeSet<_>>();
        let mut gap = Vec::new();
        if let Some(lanes) = registry.get("lanes").and_then(Value::as_object) {
            // Python sorts `lanes.items()` before constructing this public list.
            // The JSON object preserves insertion order, so sort explicitly here.
            let mut lanes = lanes.iter().collect::<Vec<_>>();
            lanes.sort_by_key(|(pane_id, _)| *pane_id);
            for (pane_id, entry) in lanes {
                if live.contains(pane_id) {
                    continue;
                }
                let pid = entry.get("pane_pid").and_then(Value::as_i64);
                let alive = pid.is_some_and(|pid| {
                    crate::proc::process_alive(pid, entry.get("start").and_then(Value::as_str))
                });
                gap.push(json!({
                    "pane": pane_id,
                    "label": entry.get("label").cloned().unwrap_or(Value::Null),
                    "role": entry.get("role").cloned().unwrap_or(Value::Null),
                    "pid": pid,
                    "alive": alive,
                }));
            }
        }
        if !gap.is_empty() {
            result["registry_gap"] = Value::Array(gap);
        }
    }
    Ok(result)
}

fn python_read_pane_lines(args: &Args) -> Result<i64, DunkError> {
    let Some(value) = args.get("lines").filter(|value| !value.is_null()) else {
        return Ok(40);
    };
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|number| number as i64))
            .ok_or_else(|| DunkError::Refused("cannot convert value to int".into())),
        Value::Bool(value) => Ok(i64::from(*value)),
        Value::String(value) => value.trim().parse::<i64>().map_err(|_| {
            DunkError::Refused(format!(
                "invalid literal for int() with base 10: '{}'",
                value
            ))
        }),
        Value::Array(_) => Err(DunkError::Refused(
            "int() argument must be a string, a bytes-like object or a real number, not 'list'"
                .into(),
        )),
        Value::Object(_) => Err(DunkError::Refused(
            "int() argument must be a string, a bytes-like object or a real number, not 'dict'"
                .into(),
        )),
        Value::Null => unreachable!("null uses the default above"),
    }
}

/// `dunk_api.h_read_pane`.
pub fn h_read_pane(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let target = args
        .get("target")
        .and_then(Value::as_str)
        .ok_or_else(|| DunkError::Refused("target is required".into()))?;
    let lines = python_read_pane_lines(&args)?.saturating_abs();
    let panes = context.env.list_panes_grouped();
    let hint = context
        .env
        .terminal()
        .lock()
        .map_err(|_| DunkError::Refused("terminal adapter lock poisoned".into()))?
        .server_error_hint();
    let pane =
        crate::flock::resolve::resolve_target(&panes, target, context.self_pane_id(), &hint)?;
    let pane_id = pane["pane_id"]
        .as_str()
        .ok_or_else(|| DunkError::Refused("resolved pane is missing pane_id".into()))?;
    let text = context
        .env
        .terminal()
        .lock()
        .map_err(|_| DunkError::Refused("terminal adapter lock poisoned".into()))?
        .read_pane(pane_id, lines, false)
        .map_err(|error| DunkError::Refused(error.message))?
        .ok_or_else(|| DunkError::Refused(format!("could not read pane {pane_id}")))?;
    Ok(json!({
        "pane_id": pane_id,
        "agent": pane.get("agent"),
        "agent_status": pane.get("agent_status"),
        "tab_label": pane.get("tab_label"),
        "text": text,
    }))
}

const DEFAULT_SEND_WAIT_S: f64 = 3.0;

fn send_target(
    context: &HandlerContext<'_>,
    args: &Args,
    command: &str,
) -> Result<Value, DunkError> {
    let target = args
        .get("target")
        .and_then(Value::as_str)
        .ok_or_else(|| DunkError::Refused(format!("{command}: target must be a string")))?;
    let self_pane_id = context.self_pane_id().filter(|pane| !pane.is_empty());
    let role = context
        .flock
        .caller_role(context.self_pane_id(), context.env)?;

    // Python's Flock.require_send_self lets owner/web/system act on any pane. Worker and
    // reviewer service tokens are constrained to their bound pane before adapter invocation.
    if matches!(role.as_str(), "worker" | "reviewer") {
        let self_pane_id = self_pane_id.ok_or_else(|| {
            DunkError::Refused(format!(
                "{command} requires the owner role or a service token; anonymous callers use the inbox and task queue"
            ))
        })?;
        let panes = context.env.list_panes_grouped();
        let hint = context
            .env
            .terminal()
            .lock()
            .map_err(|_| DunkError::Refused("terminal adapter lock poisoned".into()))?
            .server_error_hint();
        let resolved =
            crate::flock::resolve::resolve_target(&panes, target, Some(self_pane_id), &hint)?;
        if resolved.get("pane_id").and_then(Value::as_str) != Some(self_pane_id) {
            return Err(DunkError::Refused(format!(
                "{command} to another pane requires a superuser; use `dunkingsheep inbox send` instead"
            )));
        }
        return Ok(resolved.clone());
    }

    let panes = context.env.list_panes_grouped();
    let hint = context
        .env
        .terminal()
        .lock()
        .map_err(|_| DunkError::Refused("terminal adapter lock poisoned".into()))?
        .server_error_hint();
    Ok(
        crate::flock::resolve::resolve_target(&panes, target, context.self_pane_id(), &hint)?
            .clone(),
    )
}

fn send_wait_s(args: &Args, command: &str) -> Result<f64, DunkError> {
    let wait_s = match args.get("wait_s").filter(|value| !value.is_null()) {
        Some(value) => python_float(value)?,
        None => DEFAULT_SEND_WAIT_S,
    };
    if !wait_s.is_finite() || !(0.0..=3600.0).contains(&wait_s) {
        return Err(DunkError::Refused(format!(
            "{command}: wait_s must be between 0 and 3600"
        )));
    }
    Ok(wait_s)
}

fn reject_retired_desk_token(args: &Args) -> Result<(), DunkError> {
    let Some(value) = args.get("desk_token") else {
        return Ok(());
    };
    if value.is_null() || value.as_str().is_some_and(|token| token.trim().is_empty()) {
        return Ok(());
    }
    if value.as_str().is_none() {
        return Err(DunkError::Refused("desk_token must be a string".into()));
    }
    Err(DunkError::Refused(
        "desk_token is retired; present a web service token instead (dunkingsheep token issue --role web)".into(),
    ))
}

/// `dunk_api.h_send_text`: validate API inputs and gates, then delegate the actual send.
pub fn h_send_text(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    reject_retired_desk_token(&args)?;
    let target = send_target(context, &args, "send_text")?;
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| DunkError::Refused("send_text: text must be a string".into()))?;
    let hold_while_typing = python_bool_arg(&args, "hold_while_typing", true);
    let wait_s = send_wait_s(&args, "send_text")?;
    let press_enter = python_bool_arg(&args, "press_enter", true);
    context.env.send_commands()?.send_text(
        &target,
        text,
        context.self_pane_id(),
        hold_while_typing,
        wait_s,
        press_enter,
    )
}

/// `dunk_api.h_send_keys`: validate API inputs and gates, then delegate the key send.
pub fn h_send_keys(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    reject_retired_desk_token(&args)?;
    let target = send_target(context, &args, "send_keys")?;
    let keys = args
        .get("keys")
        .ok_or_else(|| DunkError::Refused("'keys'".into()))?;
    let hold_while_typing = python_bool_arg(&args, "hold_while_typing", true);
    let wait_s = send_wait_s(&args, "send_keys")?;
    context.env.send_commands()?.send_keys_raw(
        &target,
        keys,
        context.self_pane_id(),
        hold_while_typing,
        wait_s,
    )
}

/// `dunk_api.h_flush_box`: enforce the pane gate and delegate the flush operation.
pub fn h_flush_box(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let target = send_target(context, &args, "flush_box")?;
    context
        .env
        .send_commands()?
        .flush_box(&target, context.self_pane_id())
}

/// `dunk_api.h_clear_box`: preserve the raw target/reason and call the existing guarded Flock
/// operation exactly once. Its superuser-before-resolution ordering remains in Flock.
pub fn h_clear_box(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let target = args
        .get("target")
        .ok_or_else(|| DunkError::Refused("'target'".into()))?;
    let reason = args.get("reason");
    let mut operation = |env: &mut dyn crate::flock::clear_box::ClearBoxEnv| {
        context
            .flock
            .clear_box(env, target, context.self_pane_id(), reason)
    };
    context.env.with_clear_box_env(&mut operation)
}

/// Resolve the raw `dunk_core.Flock.resolve_target` input used by `h_delivery`.
fn delivery_target(context: &HandlerContext<'_>, target: &Value) -> Result<Value, DunkError> {
    if let Value::Object(_) = target {
        let pane_id = target
            .get("pane_id")
            .filter(|value| truthy(value))
            .ok_or_else(|| DunkError::Refused("target dict has no pane_id".into()))?;
        let panes = context.env.list_panes_grouped();
        return Ok(panes
            .iter()
            .find(|pane| pane.get("pane_id") == Some(pane_id))
            .cloned()
            .unwrap_or_else(|| target.clone()));
    }
    let spec = Some(target)
        .filter(|value| truthy(value))
        .map(python_value_string)
        .unwrap_or_default();
    if spec.is_empty() {
        return Err(DunkError::Refused("target is required".into()));
    }
    if matches!(spec.to_lowercase().as_str(), "self" | "me" | "here")
        && context
            .self_pane_id()
            .filter(|pane| !pane.is_empty())
            .is_none()
    {
        return Err(DunkError::Refused(
            "target 'self' needs TMUX_PANE (or HERDR_PANE_ID on the herdr rollback; run from inside a pane)".into(),
        ));
    }
    let panes = context.env.list_panes_grouped();
    if panes.is_empty() {
        let error = context
            .env
            .terminal()
            .lock()
            .map_err(|_| DunkError::Refused("terminal adapter lock poisoned".into()))?
            .server_error_hint();
        return Err(DunkError::Refused(error));
    }
    crate::flock::resolve::resolve_target(&panes, &spec, context.self_pane_id(), "").cloned()
}

/// `dunk_api.h_delivery`: resolve the raw target first; only absent or null status requests
/// fallback to terminal status. Explicit falsey values still suppress that fallback.
pub fn h_delivery(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let target = args
        .get("target")
        .ok_or_else(|| DunkError::Refused("'target'".into()))?;
    let pane = delivery_target(context, target)?;
    let pane_id = pane
        .get("pane_id")
        .and_then(Value::as_str)
        .ok_or_else(|| DunkError::Refused("resolved target has no pane_id".into()))?;
    let status = args
        .get("status")
        .filter(|value| !value.is_null())
        .map(|value| {
            if truthy(value) {
                python_value_string(value)
            } else {
                String::new()
            }
        });
    let mut operation = |env: &mut dyn crate::flock::clear_box::ClearBoxEnv| {
        Ok(context
            .flock
            .delivery_state(env, pane_id, status.as_deref(), Some(&pane), None))
    };
    context.env.with_clear_box_env(&mut operation)
}

fn python_value_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(number) if number.is_i64() || number.is_u64() => "int",
        Value::Number(_) => "float",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// `dunk_api.h_delivery_capability`: preserve Python's falsey default and `.strip()` type error
/// before consulting the Flock delivery service.
pub fn h_delivery_capability(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let raw = args.get("target").unwrap_or(&Value::Null);
    let target = if truthy(raw) {
        let Some(target) = raw.as_str() else {
            return Err(DunkError::Refused(format!(
                "'{}' object has no attribute 'strip'",
                python_value_type_name(raw)
            )));
        };
        target.trim()
    } else {
        ""
    };
    if target.is_empty() {
        return Err(DunkError::Refused(
            "delivery_capability must be given a target (a pane id like %9 or a pinned lane name)"
                .into(),
        ));
    }
    context.flock.delivery_capability_with_owner(
        context.env.delivery_env()?,
        || context.env.owner_push(),
        target,
        context.self_pane_id(),
        context.env.now_s(),
    )
}

struct ApiRetireEnv<'ctx, 'request> {
    context: &'ctx HandlerContext<'request>,
}

impl crate::flock::retire::RetireEnv for ApiRetireEnv<'_, '_> {
    fn panes(&self) -> Vec<Value> {
        self.context.env.list_panes_grouped()
    }

    fn server_error_hint(&self) -> String {
        self.context
            .env
            .terminal()
            .lock()
            .map(|terminal| terminal.server_error_hint())
            .unwrap_or_else(|_| "terminal adapter lock poisoned".into())
    }

    fn require_superuser(&self, self_pane_id: Option<&str>, action: &str) -> Result<(), DunkError> {
        self.context
            .flock
            .require_superuser(self_pane_id, action, self.context.env)
    }

    fn mailboxes_of_pane(&self, pane_id: &str) -> Vec<String> {
        self.context
            .flock
            .stores()
            .inbox
            .and_then(|store| store.mailboxes_for_pane(pane_id).ok())
            .unwrap_or_default()
    }

    fn unread(&self, pane_id: &str) -> Result<Option<i64>, DunkError> {
        match self.context.flock.stores().inbox {
            Some(store) => store.unread_for_pane(pane_id).map(Some),
            None => Ok(None),
        }
    }

    fn retired_path(&self) -> Result<PathBuf, DunkError> {
        self.context.env.retired_panes_path()
    }
}

/// `dunk_api.h_retire_pane`: the flock handler owns the state transition; this adapter supplies
/// request-scoped pane, role, inbox and config dependencies.
pub fn h_retire_pane(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let env = ApiRetireEnv { context };
    crate::flock::retire::h_retire_pane(
        &env,
        &Value::Object(args),
        context.self_pane_id(),
        Some(context.env.now_s()),
    )
}

/// `dunk_api.h_list_workspaces`.
pub fn h_list_workspaces(context: &HandlerContext<'_>, _: Args) -> Result<Value, DunkError> {
    let terminal = context
        .env
        .terminal()
        .lock()
        .map_err(|_| DunkError::Refused("terminal adapter lock poisoned".into()))?;
    let tabs = terminal.list_tabs();
    let mut workspaces = terminal
        .list_workspaces()
        .into_iter()
        .map(|workspace| {
            let mut tabs_for_workspace = tabs
                .iter()
                .filter(|tab| {
                    let workspace_id = if tab.workspace_id.is_empty() {
                        tab.tab_id.split(':').next().unwrap_or("")
                    } else {
                        &tab.workspace_id
                    };
                    workspace_id == workspace.workspace_id
                })
                .collect::<Vec<_>>();
            tabs_for_workspace.sort_by_key(|tab| tab.index);
            let active_tab_id = tabs_for_workspace
                .iter()
                .find(|tab| tab.active)
                .map(|tab| Value::String(tab.tab_id.clone()))
                .unwrap_or(Value::Null);
            let tabs = tabs_for_workspace
                .iter()
                .map(|tab| {
                    json!({
                        "tab_id": tab.tab_id,
                        "workspace_id": if tab.workspace_id.is_empty() {
                            tab.tab_id.split(':').next().unwrap_or("")
                        } else {
                            &tab.workspace_id
                        },
                        "label": tab.label,
                        "number": tab.index,
                        "active": tab.active,
                    })
                })
                .collect::<Vec<_>>();
            json!({
                "workspace_id": workspace.workspace_id,
                "label": workspace.label,
                "number": workspace.index,
                "active_tab_id": active_tab_id,
                "tabs": tabs,
            })
        })
        .collect::<Vec<_>>();
    workspaces.sort_by_key(|workspace| workspace["number"].as_i64().unwrap_or(0));
    let own = context
        .self_pane_id()
        .and_then(|pane_id| context.env.get_pane(pane_id))
        .map(|pane| {
            json!({
                "workspace_id": pane.get("workspace_id"),
                "tab_id": pane.get("tab_id"),
            })
        });
    Ok(json!({
        "self": own,
        "workspaces": workspaces,
    }))
}

/// `dunk_api.h_superuser_status`.
pub fn h_superuser_status(context: &HandlerContext<'_>, _: Args) -> Result<Value, DunkError> {
    context
        .flock
        .superuser_status(context.self_pane_id(), context.env)
}

/// `dunk_api.h_superuser_add`.
pub fn h_superuser_add(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let entry = args
        .get("entry")
        .and_then(Value::as_str)
        .ok_or_else(|| DunkError::Refused("superuser_add: entry must be a string".into()))?;
    context
        .flock
        .superuser_add(entry, context.self_pane_id(), context.env)
}

/// `dunk_api.h_superuser_remove`.
pub fn h_superuser_remove(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let entry = args
        .get("entry")
        .and_then(Value::as_str)
        .ok_or_else(|| DunkError::Refused("superuser_remove: entry must be a string".into()))?;
    context
        .flock
        .superuser_remove(entry, context.self_pane_id(), context.env)
}

/// `dunk_api.h_superuser_recover`.
pub fn h_superuser_recover(context: &HandlerContext<'_>, _: Args) -> Result<Value, DunkError> {
    context
        .flock
        .superuser_recover(context.self_pane_id(), context.env)
}

/// `dunk_api.h_token_issue`.
pub fn h_token_issue(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let role = args
        .get("role")
        .and_then(Value::as_str)
        .ok_or_else(|| DunkError::Refused("token_issue: role must be a string".into()))?;
    let label = match args.get("label") {
        None | Some(Value::Null) => None,
        Some(Value::String(label)) => Some(label.as_str()),
        Some(_) => {
            return Err(DunkError::Refused(
                "token_issue: label must be a string".into(),
            ));
        }
    };
    context
        .flock
        .token_issue(role, label, context.self_pane_id(), context.env)
}

/// `dunk_api.h_token_revoke`.
pub fn h_token_revoke(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let label = args
        .get("label")
        .and_then(Value::as_str)
        .ok_or_else(|| DunkError::Refused("token_revoke: label must be a string".into()))?;
    context
        .flock
        .token_revoke(label, context.self_pane_id(), context.env)
}

/// `dunk_api.h_owner_stats`.
pub fn h_owner_stats(context: &HandlerContext<'_>, _: Args) -> Result<Value, DunkError> {
    crate::flock::commands_owner_views::h_owner_stats(
        context.env.owner_push()?,
        context.env.now_s(),
    )
}

/// `dunk_api.h_pm_routing`.
pub fn h_pm_routing(context: &HandlerContext<'_>, _: Args) -> Result<Value, DunkError> {
    context.env.with_pm_env(&mut |env| {
        let config = env.config()?;
        let path = crate::pm::default_routing_path(config);
        let routing = crate::pm::load_routing(Some(&path), None, config)
            .map_err(|error| DunkError::Refused(error.to_string()))?;
        let routing = routing
            .into_iter()
            .map(|(project, entry)| {
                (
                    project,
                    json!({
                        "pm": entry.pm,
                        "members": entry.members,
                        "mode": entry.mode,
                        "vp_pane": entry.vp_pane,
                    }),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        Ok(json!({
            "routing": routing,
            "registry": path.to_string_lossy(),
            "vp_pane": crate::pm::default_vp_pane(config),
        }))
    })
}

/// `dunk_api.h_pm_digest`.
pub fn h_pm_digest(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let project = args
        .get("project")
        .map(python_value_string)
        .ok_or_else(|| DunkError::Refused("'project' is required".into()))?;
    let payload = args.get("payload").filter(|value| truthy(value));
    let target = args
        .get("to")
        .filter(|value| truthy(value))
        .map(python_value_string);
    context.env.with_pm_env(&mut |env| {
        context.flock.pm_digest(
            env,
            &project,
            payload,
            target.as_deref(),
            context.self_pane_id(),
        )
    })
}

/// `dunk_api.h_owner_receipts`.
pub fn h_owner_receipts(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    crate::flock::commands_owner_views::h_owner_receipts(
        context.env.owner_push()?,
        &Value::Object(args),
    )
}

fn view_string_list(args: &Args, key: &str, command: &str) -> Result<Vec<String>, DunkError> {
    let Some(value) = args.get(key).filter(|value| truthy(value)) else {
        return Ok(Vec::new());
    };
    let values = match value {
        Value::String(text) => text
            .split(',')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .map(str::to_string)
            .collect(),
        Value::Array(items) => items
            .iter()
            .map(python_value_string)
            .filter(|item| !item.trim().is_empty())
            .collect(),
        _ => {
            return Err(DunkError::Refused(format!(
                "{command}: {key} must be a string or array"
            )))
        }
    };
    Ok(values)
}

/// `dunk_api.h_message_state`: combine application rows with inbox receipt/read state.
pub fn h_message_state(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let ids = view_string_list(&args, "message_ids", "message_state")?;
    let views = context.env.owner_views()?;
    let now = views.now();
    let app_db = views.app_db_path()?;
    let mut inbox_rows = std::collections::HashMap::new();
    for id in &ids {
        if !id.is_empty() && id.chars().all(|character| character.is_ascii_digit()) {
            continue;
        }
        // Python's `message_state` treats inbox.get failures like missing rows.
        // Keep the adapter fallible for other operations, but preserve that local boundary.
        if let Ok(Some(row)) = views.inbox_status(id, now) {
            inbox_rows.insert(id.clone(), row);
        }
    }
    Ok(crate::flock::views::message_state(
        &app_db,
        &|message_id| inbox_rows.get(message_id).cloned(),
        &ids,
        now,
    ))
}

/// `dunk_api.h_reply_stats`: aggregate inbox stats and bounded read-latency samples.
pub fn h_reply_stats(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let wanted = view_string_list(&args, "mailboxes", "reply_stats")?;
    let views = context.env.owner_views()?;
    let now = views.now();
    let rows = views.inbox_stats(now)?;
    let mut latencies = std::collections::HashMap::new();
    for row in &rows {
        let Some(mailbox) = row.get("mailbox").and_then(Value::as_str) else {
            continue;
        };
        if !wanted.is_empty() && !wanted.iter().any(|name| name == mailbox) {
            continue;
        }
        latencies.insert(mailbox.to_string(), views.seen_latencies(mailbox, 200)?);
    }
    Ok(crate::flock::views::reply_stats(
        &rows,
        &|mailbox| latencies.get(mailbox).cloned().unwrap_or_default(),
        &wanted,
        now,
    ))
}

/// `dunk_api.h_report_list`.
pub fn h_report_list(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    crate::report::report_list_handler(context, args)
}

/// `dunk_api.h_owner_delivered`.
pub fn h_owner_delivered(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let message_id = args
        .get("message_id")
        .map(python_value_string)
        .ok_or_else(|| DunkError::Refused("'message_id' is required".into()))?;
    let transport_value = args.get("transport");
    let transport = transport_value
        .filter(|value| !value.is_null())
        .map(python_value_string);
    let external_id = args
        .get("external_id")
        .filter(|value| truthy(value))
        .map(python_value_string);
    let delivered = crate::owner::OwnerConversations::new(context.flock, context.env).delivered(
        &message_id,
        transport.as_deref(),
        external_id.as_deref(),
        context.self_pane_id(),
    )?;
    Ok(json!({
        "message": owner_message_value(&delivered.message),
        "transport": transport_value.cloned().unwrap_or(Value::Null),
        "delivered_at": delivered.delivered_at,
    }))
}

/// `dunk_api.h_owner_conversation`.
pub fn h_owner_conversation(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let limit = match args.get("limit").filter(|value| truthy(value)) {
        None => 50,
        Some(value) => {
            let mut limit_args = Args::new();
            limit_args.insert("limit".into(), value.clone());
            memory_int_arg(&limit_args, "limit", 50)?
        }
    };
    let before = match args.get("before") {
        None | Some(Value::Null) => None,
        // `as_f64` refuses nonfinite and overflowing numbers. An integer token past the float
        // range is refused like Python's float(int): OverflowError, which the daemon answers as
        // an internal error. NaN, Infinity and float tokens such as 1e309 keep their value.
        Some(Value::Number(number)) => match number.as_f64() {
            Some(value) => Some(value),
            None => {
                let token = number.to_string();
                let digits = token.strip_prefix('-').unwrap_or(token.as_str());
                if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(DunkError::Refused(
                        "internal error: int too large to convert to float".into(),
                    ));
                }
                Some(token.parse::<f64>().map_err(|_| {
                    DunkError::Refused(format!("could not convert number to float: {token}"))
                })?)
            }
        },
        Some(Value::Bool(value)) => Some(if *value { 1.0 } else { 0.0 }),
        Some(Value::String(text)) => Some(parse_python_float_string(text).ok_or_else(|| {
            DunkError::Refused(format!(
                "could not convert string to float: {}",
                py_repr(text)
            ))
        })?),
        Some(value) => {
            return Err(DunkError::Refused(match value {
                Value::Array(_) => {
                    "float() argument must be a string or a real number, not 'list'".into()
                }
                Value::Object(_) => {
                    "float() argument must be a string or a real number, not 'dict'".into()
                }
                _ => "float() argument must be a string or a real number".into(),
            }));
        }
    };
    let conversation = crate::owner::OwnerConversations::new(context.flock, context.env)
        .conversation(Some(limit), before, context.self_pane_id())?;
    serde_json::to_value(conversation).map_err(DunkError::from)
}

/// Python `float(string)` permits underscores between decimal digits and accepts Unicode
/// decimal digits. Normalize those forms before Rust's decimal float parser.
fn parse_python_float_string(value: &str) -> Option<f64> {
    let value = value.trim();
    let mut normalized = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    let mut previous_was_digit = false;
    while let Some(character) = chars.next() {
        if character == '_' {
            let next_is_digit = chars
                .peek()
                .is_some_and(|next| python_decimal_digit(*next).is_some());
            if !previous_was_digit || !next_is_digit {
                return None;
            }
            previous_was_digit = false;
            continue;
        }
        if let Some(digit) = python_decimal_digit(character) {
            normalized.push(char::from(b'0' + digit));
            previous_was_digit = true;
        } else {
            normalized.push(character);
            previous_was_digit = false;
        }
    }
    normalized.parse().ok()
}

/// Hand an original owner question to the fixed registered supervisor.
pub fn h_owner_handoff(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    if args.len() != 1 {
        return Err(DunkError::Refused("handoff accepts only message_id".into()));
    }
    let id = args
        .get("message_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| DunkError::Refused("message_id must be a nonempty string".into()))?;
    owner_handoff::handoff(context, id)
}

/// `dunk_api.h_owner_reply`.
pub fn h_owner_reply(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let message_id = args
        .get("message_id")
        .and_then(Value::as_str)
        .ok_or_else(|| DunkError::Refused("message_id must be a string".into()))?;
    let reply = crate::owner::OwnerConversations::new(context.flock, context.env).reply(
        message_id,
        args.get("body"),
        context.self_pane_id(),
    )?;
    Ok(json!({
        "reply": owner_message_value(&reply.reply),
        "question": reply.question,
        "transport": reply.transport,
        "acked": reply.acked,
    }))
}

fn owner_message_value(message: &crate::inbox_store::Message) -> Value {
    let summary = &message.summary;
    json!({
        "message_id": summary.message_id,
        "mailbox": summary.mailbox,
        "destination": summary.destination,
        "sender": summary.sender,
        "subject": summary.subject,
        "state": summary.state,
        "created_at": summary.created_at,
        "waiting_s": summary.waiting_s,
        "body_bytes": summary.body_bytes,
        "digest_id": summary.digest_id,
        "transport": summary.transport,
        "reply_to": summary.reply_to,
        "external_id": summary.external_id,
        "delivered_at": summary.delivered_at,
        "delivery_transport": summary.delivery_transport,
        "attachments": message.attachments.iter().map(|attachment| json!({
            "id": attachment.id,
            "filename": attachment.filename,
            "media_type": attachment.media_type,
            "size": attachment.size,
        })).collect::<Vec<_>>(),
        "body": message.body,
        "read_at": message.read_at,
    })
}

/// `owner_page` (t665): record one owner page for the Telegram outbox. Owner and system only.
pub fn h_owner_page(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    // t3700: `class` (security, outage, money) marks a critical emergency page; anything else is
    // recorded to the owner mailbox only.
    let paged = crate::owner::OwnerConversations::new(context.flock, context.env).page(
        args.get("body"),
        args.get("external_id"),
        context.self_pane_id(),
        args.get("class"),
    )?;
    Ok(json!({
        "message": owner_message_value(&paged.message),
        "duplicate": paged.duplicate,
        "transport": paged.transport,
    }))
}

/// `owner_page_confirmed` (t665): the highest owner page the Telegram transport has delivered,
/// as the watchdog compares it. Owner and system only.
pub fn h_owner_page_confirmed(
    context: &HandlerContext<'_>,
    _args: Args,
) -> Result<Value, DunkError> {
    let through = crate::owner::OwnerConversations::new(context.flock, context.env)
        .page_confirmed_through(context.self_pane_id())?;
    Ok(json!({
        "through": through,
        "transport": crate::owner::conversations::OWNER_PAGE_TRANSPORT,
    }))
}

/// `dunk_api.h_owner_ingest`.
pub fn h_owner_ingest(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let ingested = crate::owner::OwnerConversations::new(context.flock, context.env).ingest_wire(
        crate::owner::conversations::OwnerIngestWire {
            body: args.get("body"),
            transport: args.get("transport"),
            external_id: args.get("external_id"),
            subject: args.get("subject"),
            attachments: args.get("attach"),
        },
        context.self_pane_id(),
    )?;
    // t1613: a new Telegram question nudges the desk once. The web route tells the desk itself.
    // t4645/t4671: tell the single registered owner lane for new Telegram and web questions. For
    // web the desk pane is only looked up, never nudged here, so an owner lane that is also the
    // desk the web route tells is not told twice.
    if !ingested.duplicate {
        let desk_pane = match ingested.transport.as_str() {
            "telegram" => Some(nudge_desk_for_telegram(context, &ingested.message)),
            "web" => Some(default_desk_pane_id(context)),
            _ => None,
        };
        if let Some(desk_pane) = desk_pane {
            nudge_owner_lane_for_owner_message(
                context,
                &ingested.message,
                &ingested.transport,
                desk_pane.as_deref(),
            );
        }
    }
    Ok(json!({
        "message": owner_message_value(&ingested.message),
        "duplicate": ingested.duplicate,
        "transport": ingested.transport,
    }))
}

/// Best effort: a missing desk or a refused write never fails the ingest, because the mailbox row is
/// already the record. The write is the one the web route makes (no pane, no system sender, notify
/// on), so the desk gets one inbox line and nothing is typed into a pane.
/// Returns the desk pane it chose, so the owner-lane nudge does not tell the same pane twice.
fn nudge_desk_for_telegram(
    context: &HandlerContext<'_>,
    message: &crate::inbox_store::Message,
) -> Option<String> {
    let panes = context.env.list_panes_grouped();
    let desk = desk_nudge::desk_pane_for(&panes)?;
    let pane_id = desk.get("pane_id").and_then(Value::as_str)?;
    let message_id = message.summary.message_id.clone();
    let body = desk_nudge::nudge_body(&message_id, "telegram", &message.body);
    let subject = desk_nudge::nudge_subject(&message_id);
    let _ = context.env.with_inbox_env(&mut |env| {
        context.flock.inbox_send_wire(
            env,
            &crate::flock::commands_inbox::SendArgs {
                to: pane_id,
                body: &body,
                subject: Some(&subject),
                attach: &[],
                notify: true,
                self_pane_id: None,
                system_sender: None,
                agent_only: false,
                role_sender: None,
                wake_quiet: false,
            },
            None,
            None,
        )?;
        Ok(Value::Null)
    });
    Some(pane_id.to_string())
}

/// t4671: the pane the web route tells for the default desk channel (`match: desk`), looked up
/// without nudging it. The route's channel can also be pinned to a pane in the web's own config,
/// which the daemon does not see; only the default match is deduplicated here.
fn default_desk_pane_id(context: &HandlerContext<'_>) -> Option<String> {
    let panes = context.env.list_panes_grouped();
    let desk = desk_nudge::desk_pane_for(&panes)?;
    Some(desk.get("pane_id").and_then(Value::as_str)?.to_string())
}

/// t4645/t4671: a Telegram or web message from the owner also tells the ONE registered owner lane,
/// with a one-line inbox notice, even though that lane's mailboxes are configured silent
/// (`DUNKINGSHEEP_INBOX_SILENT_MAILBOXES`). Only this nudge wakes a silent recipient; every other
/// send to it stays silent. Best effort, as the desk nudge is: no owner lane, more than one, or a
/// refused write never fails the ingest.
fn nudge_owner_lane_for_owner_message(
    context: &HandlerContext<'_>,
    message: &crate::inbox_store::Message,
    transport: &str,
    desk_pane: Option<&str>,
) {
    let Ok(owners) = context.flock.with_roles(|roles, _| roles.owners()) else {
        return;
    };
    let Some(pane_id) = desk_nudge::owner_lane_for(&owners, desk_pane) else {
        return;
    };
    let message_id = message.summary.message_id.clone();
    let body = desk_nudge::owner_lane_nudge_body(&message_id, transport, &message.body);
    let subject = desk_nudge::nudge_subject(&message_id);
    let _ = context.env.with_inbox_env(&mut |env| {
        context.flock.inbox_send_wire(
            env,
            &crate::flock::commands_inbox::SendArgs {
                to: pane_id,
                body: &body,
                subject: Some(&subject),
                attach: &[],
                notify: true,
                self_pane_id: None,
                system_sender: None,
                agent_only: false,
                role_sender: None,
                wake_quiet: true,
            },
            None,
            None,
        )?;
        Ok(Value::Null)
    });
}

/// `dunk_api.h_owner_outbox`.
pub fn h_owner_outbox(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let transport = args
        .get("transport")
        .filter(|value| !value.is_null())
        .map(python_value_string);
    let limit = owner_outbox_limit_arg(&args, 20)?;
    let conversations = crate::owner::OwnerConversations::new(context.flock, context.env);
    let limit = match limit {
        OwnerOutboxLimit::Sqlite(limit) => limit,
        OwnerOutboxLimit::Overflow => {
            // Python evaluates int() before calling owner_outbox, but SQLite
            // binds the resulting integer only after role and transport
            // validation. Reuse the existing read-only service operation to
            // preserve those earlier errors before reporting bind overflow.
            conversations.outbox(transport.as_deref(), Some(20), context.self_pane_id())?;
            return Err(DunkError::Refused(
                "Python int too large to convert to SQLite INTEGER".into(),
            ));
        }
    };
    let outbox = conversations.outbox(transport.as_deref(), Some(limit), context.self_pane_id())?;
    Ok(json!({
        "transport": outbox.transport,
        "messages": outbox.messages.iter().map(owner_message_value).collect::<Vec<_>>(),
        "count": outbox.count,
    }))
}

enum OwnerOutboxLimit {
    Sqlite(i64),
    Overflow,
}

fn owner_outbox_limit_arg(args: &Args, default: i64) -> Result<OwnerOutboxLimit, DunkError> {
    let Some(value) = args.get("limit").filter(|value| !value.is_null()) else {
        return Ok(OwnerOutboxLimit::Sqlite(default));
    };
    match value {
        Value::Bool(value) => Ok(OwnerOutboxLimit::Sqlite(i64::from(*value))),
        Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                return Ok(OwnerOutboxLimit::Sqlite(value));
            }
            if let Some(value) = value.as_u64() {
                return Ok(i64::try_from(value)
                    .map(OwnerOutboxLimit::Sqlite)
                    .unwrap_or(OwnerOutboxLimit::Overflow));
            }
            let token = value.to_string();
            if !token.bytes().any(|byte| matches!(byte, b'.' | b'e' | b'E')) {
                return Ok(match parse_python_decimal_integer_checked(&token) {
                    Ok(value) => OwnerOutboxLimit::Sqlite(value),
                    Err(DecimalIntError::Overflow) => OwnerOutboxLimit::Overflow,
                    Err(DecimalIntError::Invalid) => {
                        return Err(DunkError::Refused("cannot convert value to int".into()))
                    }
                });
            }
            let value = value
                .as_f64()
                .ok_or_else(|| DunkError::Refused("cannot convert value to int".into()))?
                .trunc();
            if value >= 9_223_372_036_854_775_808.0 || value < -9_223_372_036_854_775_808.0 {
                return Ok(OwnerOutboxLimit::Overflow);
            }
            Ok(OwnerOutboxLimit::Sqlite(value as i64))
        }
        Value::String(value) => match parse_python_decimal_integer_checked(value) {
            Ok(value) => Ok(OwnerOutboxLimit::Sqlite(value)),
            Err(DecimalIntError::Overflow) => Ok(OwnerOutboxLimit::Overflow),
            Err(DecimalIntError::Invalid) => Err(DunkError::Refused(format!(
                "invalid literal for int() with base 10: {}",
                py_repr(value)
            ))),
        },
        Value::Array(_) => Err(DunkError::Refused(
            "int() argument must be a string, a bytes-like object or a real number, not 'list'"
                .into(),
        )),
        Value::Object(_) => Err(DunkError::Refused(
            "int() argument must be a string, a bytes-like object or a real number, not 'dict'"
                .into(),
        )),
        Value::Null => unreachable!("null limit uses the default"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DecimalIntError {
    Invalid,
    Overflow,
}

fn parse_python_decimal_integer_checked(value: &str) -> Result<i64, DecimalIntError> {
    let literal = value.trim();
    let sign_len = usize::from(matches!(literal.as_bytes().first(), Some(b'+' | b'-')));
    let negative = literal.as_bytes().first() == Some(&b'-');
    let (_, digits) = literal.split_at(sign_len);
    if digits.is_empty() {
        return Err(DecimalIntError::Invalid);
    }
    let maximum = if negative {
        (i64::MAX as u64) + 1
    } else {
        i64::MAX as u64
    };
    let mut magnitude = 0_u64;
    let mut overflow = false;
    let mut previous_was_digit = false;
    let mut chars = digits.chars().peekable();
    while let Some(character) = chars.next() {
        if let Some(digit) = python_decimal_digit(character) {
            match magnitude
                .checked_mul(10)
                .and_then(|number| number.checked_add(u64::from(digit)))
            {
                Some(number) if number <= maximum => magnitude = number,
                _ => {
                    overflow = true;
                    magnitude = maximum;
                }
            }
            previous_was_digit = true;
        } else if character == '_'
            && previous_was_digit
            && chars
                .peek()
                .and_then(|next| python_decimal_digit(*next))
                .is_some()
        {
            previous_was_digit = false;
        } else {
            return Err(DecimalIntError::Invalid);
        }
    }
    if !previous_was_digit {
        return Err(DecimalIntError::Invalid);
    }
    if overflow {
        return Err(DecimalIntError::Overflow);
    }
    Ok(if negative {
        if magnitude > i64::MAX as u64 {
            i64::MIN
        } else {
            -(magnitude as i64)
        }
    } else {
        magnitude as i64
    })
}

pub(crate) fn poolrad_limit(args: &Args) -> Result<usize, DunkError> {
    let Some(value) = args.get("limit").filter(|value| truthy(value)) else {
        return Ok(6);
    };
    match value {
        Value::Number(number) => python_number_limit(number),
        Value::Bool(value) => Ok(usize::from(*value)),
        Value::String(value) => parse_python_decimal_limit(value).ok_or_else(|| {
            DunkError::Refused(format!(
                "invalid literal for int() with base 10: {}",
                py_repr(value)
            ))
        }),
        Value::Array(_) => Err(DunkError::Refused(
            "int() argument must be a string, a bytes-like object or a real number, not 'list'"
                .into(),
        )),
        Value::Object(_) => Err(DunkError::Refused(
            "int() argument must be a string, a bytes-like object or a real number, not 'dict'"
                .into(),
        )),
        Value::Null => unreachable!("falsey limit uses the default"),
    }
}

fn python_number_limit(number: &serde_json::Number) -> Result<usize, DunkError> {
    if let Some(value) = number.as_i64() {
        return Ok(if value <= 0 {
            1
        } else {
            usize::try_from(value).unwrap_or(usize::MAX)
        });
    }
    if let Some(value) = number.as_u64() {
        return Ok(usize::try_from(value).unwrap_or(usize::MAX));
    }
    if let Some(value) = number.as_f64() {
        let integer = value.trunc();
        return Ok(if integer <= 0.0 {
            1
        } else if integer >= usize::MAX as f64 {
            usize::MAX
        } else {
            integer as usize
        });
    }
    Err(DunkError::Refused("cannot convert value to int".into()))
}

fn parse_python_decimal_limit(value: &str) -> Option<usize> {
    let literal = value.trim();
    let sign_len = usize::from(matches!(literal.as_bytes().first(), Some(b'+' | b'-')));
    let negative = literal.as_bytes().first() == Some(&b'-');
    let (_, digits) = literal.split_at(sign_len);
    if digits.is_empty() {
        return None;
    }

    let mut previous_was_digit = false;
    let mut magnitude = 0usize;
    let mut chars = digits.chars().peekable();
    while let Some(character) = chars.next() {
        if let Some(digit) = python_decimal_digit(character) {
            magnitude = magnitude
                .saturating_mul(10)
                .saturating_add(usize::from(digit));
            previous_was_digit = true;
        } else if character == '_'
            && previous_was_digit
            && chars
                .peek()
                .and_then(|next| python_decimal_digit(*next))
                .is_some()
        {
            previous_was_digit = false;
        } else {
            return None;
        }
    }
    if !previous_was_digit {
        return None;
    }
    if negative || magnitude == 0 {
        // Python's candidates() appends a row before comparing len(picked) to limit,
        // so any nonpositive converted limit still selects one eligible row.
        Some(1)
    } else {
        Some(magnitude)
    }
}

fn python_decimal_digit(character: char) -> Option<u8> {
    // Unicode 16.0 Decimal_Number ranges, matching the Python 3.14 runtime used by this port.
    // The two multi-set ranges are mathematical styled digits and segmented digits.
    const DECIMAL_RANGES: &[(u32, u32)] = &[
        (0x0030, 0x0039),
        (0x0660, 0x0669),
        (0x06F0, 0x06F9),
        (0x07C0, 0x07C9),
        (0x0966, 0x096F),
        (0x09E6, 0x09EF),
        (0x0A66, 0x0A6F),
        (0x0AE6, 0x0AEF),
        (0x0B66, 0x0B6F),
        (0x0BE6, 0x0BEF),
        (0x0C66, 0x0C6F),
        (0x0CE6, 0x0CEF),
        (0x0D66, 0x0D6F),
        (0x0DE6, 0x0DEF),
        (0x0E50, 0x0E59),
        (0x0ED0, 0x0ED9),
        (0x0F20, 0x0F29),
        (0x1040, 0x1049),
        (0x1090, 0x1099),
        (0x17E0, 0x17E9),
        (0x1810, 0x1819),
        (0x1946, 0x194F),
        (0x19D0, 0x19D9),
        (0x1A80, 0x1A89),
        (0x1A90, 0x1A99),
        (0x1B50, 0x1B59),
        (0x1BB0, 0x1BB9),
        (0x1C40, 0x1C49),
        (0x1C50, 0x1C59),
        (0xA620, 0xA629),
        (0xA8D0, 0xA8D9),
        (0xA900, 0xA909),
        (0xA9D0, 0xA9D9),
        (0xA9F0, 0xA9F9),
        (0xAA50, 0xAA59),
        (0xABF0, 0xABF9),
        (0xFF10, 0xFF19),
        (0x104A0, 0x104A9),
        (0x10D30, 0x10D39),
        (0x10D40, 0x10D49),
        (0x11066, 0x1106F),
        (0x110F0, 0x110F9),
        (0x11136, 0x1113F),
        (0x111D0, 0x111D9),
        (0x112F0, 0x112F9),
        (0x11450, 0x11459),
        (0x114D0, 0x114D9),
        (0x11650, 0x11659),
        (0x116C0, 0x116C9),
        (0x116D0, 0x116E3),
        (0x11730, 0x11739),
        (0x118E0, 0x118E9),
        (0x11950, 0x11959),
        (0x11BF0, 0x11BF9),
        (0x11C50, 0x11C59),
        (0x11D50, 0x11D59),
        (0x11DA0, 0x11DA9),
        (0x11F50, 0x11F59),
        (0x16130, 0x16139),
        (0x16A60, 0x16A69),
        (0x16AC0, 0x16AC9),
        (0x16B50, 0x16B59),
        (0x16D70, 0x16D79),
        (0x1CCF0, 0x1CCF9),
        (0x1D7CE, 0x1D7FF),
        (0x1E140, 0x1E149),
        (0x1E2F0, 0x1E2F9),
        (0x1E4F0, 0x1E4F9),
        (0x1E5F1, 0x1E5FA),
        (0x1E950, 0x1E959),
        (0x1FBF0, 0x1FBF9),
    ];

    let codepoint = character as u32;
    DECIMAL_RANGES.iter().find_map(|&(start, end)| {
        if (start..=end).contains(&codepoint) {
            Some(((codepoint - start) % 10) as u8)
        } else {
            None
        }
    })
}

pub(crate) fn poolrad_dry_run(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::String(value)) => python_bool_string(value),
        Some(Value::Number(value)) => python_bool_string(&value.to_string()),
        Some(Value::Array(_) | Value::Object(_)) => false,
    }
}

/// `dunk_api.h_review` sync action through the canonical ReviewSync engine.
pub fn h_review_sync(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let _guard = context
        .flock
        .review_sweep
        .try_lock()
        .map_err(|_| DunkError::Refused("review sweep already running".into()))?;
    let tokens = context.env.github_tokens()?;
    let transport = context.env.review_transport()?;
    let policy = crate::review::ConfiguredReviewPolicy {
        config_dir: tokens.config_dir().to_path_buf(),
    };
    let mut sweep = crate::pr_wait::SweepTransport::new(transport);
    let dry_run = python_bool_arg(&args, "dry_run", false);
    let result = context.block_on(context.flock.review_sync(
        tokens,
        policy,
        tokens.api_root(),
        crate::flock::commands_review::BorrowedGitHubTransport(&sweep),
        dry_run,
    ))?;
    if !dry_run {
        if let Some(store) = context.flock.stores().inbox {
            if let Some(tasks) = context.flock.stores().tasks {
                match context.block_on(tasks.list_review_rows_visible())? {
                    Ok(rows) => sweep.set_reviews(rows),
                    Err(_) => eprintln!(
                        "pr-wait: local review snapshot unavailable; observing GitHub status"
                    ),
                }
            }
            let token =
                context.block_on(crate::review::ReviewTokenSource::reviewer_token(tokens))?;
            context
                .block_on(sweep.poll(&store, tokens.api_root(), token, context.env.now_s()))?
                .map_err(DunkError::Refused)?;
            for (id, mailbox, body) in store.pr_wait_pending(context.env.now_s())? {
                let sent = context.env.with_inbox_env(&mut |env| {
                    context.flock.inbox_send(
                        env,
                        &crate::flock::commands_inbox::SendArgs {
                            to: &mailbox,
                            body: &body,
                            subject: Some("PR wait finished"),
                            attach: &[],
                            notify: true,
                            self_pane_id: context.self_pane_id(),
                            system_sender: None,
                            agent_only: false,
                            role_sender: None,
                            wake_quiet: false,
                        },
                    )?;
                    Ok(Value::Null)
                });
                if sent.is_ok() {
                    store.pr_wait_delivered(id)?;
                }
            }
        }
    }
    result.map_err(DunkError::Refused)
}

/// Register, inspect or cancel only the authenticated caller's one-shot PR interests.
pub fn h_pr_wait(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let pane = context
        .self_pane_id()
        .ok_or_else(|| DunkError::Refused("pr wait needs a caller pane".into()))?;
    let mailbox = context
        .env
        .with_inbox_env(&mut |env| context.flock.own_mailbox(env, pane).map(Value::String))?;
    let mailbox = mailbox
        .as_str()
        .ok_or_else(|| DunkError::Refused("invalid mailbox".into()))?;
    let store = context
        .flock
        .stores()
        .inbox
        .ok_or_else(|| DunkError::Refused("inbox unavailable".into()))?;
    if args.get("list").and_then(Value::as_bool) == Some(true) {
        return store.pr_wait_list(mailbox, None);
    }
    if let Some(id) = args.get("cancel").and_then(Value::as_i64) {
        return store.pr_wait_list(mailbox, Some(id));
    }
    let target = args
        .get("target")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (repo, number) = target
        .rsplit_once('#')
        .ok_or_else(|| DunkError::Refused("expected owner/repo#PR".into()))?;
    let pr = number
        .parse::<i64>()
        .ok()
        .filter(|p| *p > 0)
        .ok_or_else(|| DunkError::Refused("expected positive PR number".into()))?;
    let tokens = context.env.github_tokens()?;
    let policy = crate::review::ConfiguredReviewPolicy {
        config_dir: tokens.config_dir().to_path_buf(),
    };
    if !crate::review::ReviewRepoConfig::repos(&policy)
        .map_err(DunkError::Refused)?
        .iter()
        .any(|r| r == repo)
    {
        return Err(DunkError::Refused(
            "repository is not in the review registry".into(),
        ));
    }
    let head = args.get("head").and_then(Value::as_str);
    if head.is_some_and(|h| h.len() != 40 || !h.bytes().all(|b| b.is_ascii_hexdigit())) {
        return Err(DunkError::Refused(
            "head must be a full 40 character SHA".into(),
        ));
    }
    let head = head.map(str::to_ascii_lowercase);
    store.pr_wait_register(repo, pr, mailbox, pane, head.as_deref())
}

/// `dunk_api.h_review` list action through the attached Tasks actor.
pub fn h_review_list(context: &HandlerContext<'_>, _: Args) -> Result<Value, DunkError> {
    context.block_on(context.flock.review_list(context.env.now_s()))?
}

fn python_bool_string(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "y" | "on"
    )
}

/// `dunk_core.Flock.gh_token` through the accepted T52 token cache.
pub fn h_gh_token(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let role = context
        .flock
        .caller_role(context.self_pane_id(), context.env)?;
    let requested = args.get("kind").filter(|value| truthy(value));
    let requested_kind = match requested {
        None => None,
        Some(Value::String(kind)) => Some(kind.as_str()),
        Some(value) if matches!(role.as_str(), "owner" | "system") => {
            return Err(crate::github::GitHubError(format!(
                "unknown app kind {}",
                python_value_repr(value)
            ))
            .into());
        }
        Some(value) => {
            let implied = crate::github::kind_for_role(&role, None)?;
            if let Some(implied) = implied {
                return Err(crate::github::GitHubError(format!(
                    "role {role} may only act as the {implied} App, not {}",
                    python_value_string(value)
                ))
                .into());
            }
            return Ok(json!({
                "token": null,
                "kind": null,
                "role": role,
                "note": "this role uses no App token (the owner keeps their own login)"
            }));
        }
    };
    let Some(kind) = crate::github::kind_for_role(&role, requested_kind)? else {
        return Ok(json!({
            "token": null,
            "kind": null,
            "role": role,
            "note": "this role uses no App token (the owner keeps their own login)"
        }));
    };
    let tokens = context.env.github_tokens()?;
    let entry = context.block_on(tokens.get(kind, None))??;
    let token = entry
        .get("token")
        .cloned()
        .ok_or_else(|| crate::github::GitHubError("'token'".into()))?;
    let mut response = json!({
        "token": token,
        "kind": kind,
        "role": role,
        "slug": entry.get("slug").cloned().unwrap_or(Value::Null),
        "expires_at": entry.get("expires_at").cloned().unwrap_or(Value::Null),
        "cached": entry.get("cached").cloned().unwrap_or(Value::Bool(false)),
    });
    if kind == "author" {
        // The mint record is best effort: a lost record never fails the token, but the
        // response says what happened (t3241 review finding 2).
        let provenance = match context.self_pane_id().filter(|pane| !pane.is_empty()) {
            Some(pane_id) => {
                let now = context.env.now_s();
                context.flock.record_gh_token_mint(pane_id, now);
                author_mint_provenance(crate::github::record_author_mint_outcome(
                    pane_id,
                    Some(now),
                    None,
                    tokens.config_dir(),
                ))
            }
            None => no_pane_provenance(),
        };
        response["provenance"] = provenance;
    }
    Ok(response)
}

/// The provenance answer for an author mint: whether the record was written, and why not.
fn author_mint_provenance(
    outcome: Result<crate::github::AuthorMintRecord, crate::github::GitHubError>,
) -> Value {
    use crate::github::AuthorMintRecord;
    match outcome {
        Ok(AuthorMintRecord::Recorded(_)) => json!({"recorded": true}),
        Ok(AuthorMintRecord::NotApplicable) => no_pane_provenance(),
        Ok(AuthorMintRecord::WriteFailed { reason, .. }) => json!({
            "recorded": false,
            "reason": format!("could not write the author mint record: {reason}"),
        }),
        Err(error) => json!({"recorded": false, "reason": error.to_string()}),
    }
}

fn no_pane_provenance() -> Value {
    json!({"recorded": false, "reason": "no pane identity, so no author mint was recorded"})
}

/// `dunk_api.h_poolrad_sync` through the accepted T52 Poolrad service.
pub fn h_poolrad_sync(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    context.env.poolrad_sync_args(&args)
}

/// `dunk_api.h_poolrad_progress` through the accepted T52 Poolrad service.
pub fn h_poolrad_progress(context: &HandlerContext<'_>, _args: Args) -> Result<Value, DunkError> {
    context.env.poolrad_progress()
}

pub fn h_lanes(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or("list")
        .trim()
        .to_lowercase();
    if action == "set" {
        context
            .flock
            .require_superuser(context.self_pane_id(), "lane_set", context.env)?;
        let result = lane_set(context, &args)?;
        context.flock.invalidate_view();
        return Ok(result);
    }
    if action == "planner" {
        context
            .flock
            .require_superuser(context.self_pane_id(), "planner scope", context.env)?;
        return context
            .flock
            .planner_scope_admin(context.env, &Value::Object(args.clone()));
    }
    if action == "project" {
        let pane = args
            .get("pane")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| DunkError::Refused("lane project needs a pane id".into()))?;
        let pane = if pane == "self" {
            context
                .self_pane_id()
                .ok_or_else(|| DunkError::Refused("caller pane is unknown".into()))?
        } else {
            pane
        };
        let project = match args.get("project") {
            None | Some(Value::Null) => None,
            Some(Value::String(value)) => Some(value.as_str()),
            Some(_) => return Err(DunkError::Refused("project must be a string".into())),
        };
        let global = python_bool_arg(&args, "global", false);
        if project.is_some() || global {
            context
                .flock
                .require_superuser(context.self_pane_id(), "lane project", context.env)?;
            if global && project.is_some() {
                return Err(DunkError::Refused(
                    "choose either a project or global scope".into(),
                ));
            }
            let scope = if global {
                crate::tasks::lane_project::ProjectScope::Global
            } else {
                let name = project.unwrap_or_default();
                if name.trim().is_empty() || name.trim() != name {
                    return Err(DunkError::Refused(
                        "project must be a non-empty name without surrounding whitespace".into(),
                    ));
                }
                crate::tasks::lane_project::ProjectScope::Project(name.to_owned())
            };
            return context.flock.lane_project_set(context.env, pane, scope);
        }
        let mut result = context.flock.lane_project_view(context.env, pane);
        result["pane_id"] = json!(pane);
        return Ok(result);
    }
    if action != "list" {
        return Err(DunkError::Refused(
            "lanes action must be list, set, project or planner".into(),
        ));
    }
    let mut rows = context.flock.lanes();
    if context.flock.stores().tasks.is_some() {
        for row in &mut rows {
            if let Some(pane) = row
                .get("pane_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
            {
                row["membership"] = context.flock.lane_project_view(context.env, &pane);
            }
        }
    }
    Ok(json!({"lanes": rows, "count": rows.len()}))
}

/// `dunk_api.h_lane_set`: missing/invalid parameters are refused by Flock with Python parity.
pub fn h_lane_set(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let result = lane_set(context, &args)?;
    context.flock.invalidate_view();
    Ok(result)
}

fn lane_set(context: &HandlerContext<'_>, args: &Args) -> Result<Value, DunkError> {
    let terminal = context
        .env
        .terminal()
        .lock()
        .map_err(|_| DunkError::Refused("terminal adapter lock poisoned".into()))?;
    context.flock.lane_set(
        terminal.as_ref(),
        args.get("pane").and_then(Value::as_str).unwrap_or(""),
        args.get("mailbox").and_then(Value::as_str).unwrap_or(""),
    )
}

/// `dunk_api.h_lane_register`: role checks and owner bootstrap live in Flock.
pub fn h_lane_register(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let pane = match args.get("pane") {
        None | Some(Value::Null) => None,
        Some(Value::String(pane)) => Some(pane.as_str()),
        Some(_) => return Err(DunkError::Refused("pane must be a string".into())),
    };
    context.flock.lane_register(
        pane,
        args.get("role").and_then(Value::as_str),
        args.get("provider").and_then(Value::as_str),
        args.get("model").and_then(Value::as_str),
        args.get("label").and_then(Value::as_str),
        context.self_pane_id(),
        context.env,
    )
}

/// `dunk_api.h_lane_unregister`.
pub fn h_lane_unregister(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let pane = match args.get("pane") {
        None | Some(Value::Null) => {
            return Err(DunkError::Refused("pane is required".into()));
        }
        Some(Value::String(pane)) => pane.as_str(),
        Some(_) => return Err(DunkError::Refused("pane must be a string".into())),
    };
    context
        .flock
        .lane_unregister(pane, context.self_pane_id(), context.env)
}

fn context_store(args: &Args) -> crate::context::store::Store {
    let global_store = python_bool_arg(args, "global_store", true);
    let project_path = args
        .get("project_path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty());
    if global_store || project_path.is_none() {
        crate::context::store::Store::new(None, true)
    } else {
        crate::context::store::Store::new(project_path.map(std::path::Path::new), false)
    }
}

fn context_arg_string<'a>(args: &'a Args, key: &str) -> Result<&'a str, DunkError> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| DunkError::Refused(format!("'{key}' is required")))
}

fn memory_service(
    context: &HandlerContext<'_>,
) -> Result<std::sync::Arc<crate::memory::service::MemoryService>, DunkError> {
    context.flock.memory_service().ok_or_else(|| {
        DunkError::Refused(
            "memory service is not enabled; start the daemon with persistence".into(),
        )
    })
}

fn memory_arg_string<'a>(args: &'a Args, key: &str) -> Result<&'a str, DunkError> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| DunkError::Refused(format!("'{key}' is required")))
}

fn context_ledger() -> crate::context::ledger::Ledger {
    crate::context::ledger::Ledger::new(None)
}

fn added_context_value(added: crate::context::store::Added) -> Value {
    json!({
        "slug": added.slug,
        "path": added.path.to_string_lossy(),
        "class": added.cls,
    })
}

/// `dunk_api.h_context_supersede`.
pub fn h_context_supersede(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let slug = context_arg_string(&args, "slug")?;
    let body = context_arg_string(&args, "body")?;
    let store = context_store(&args);
    let Some(old) = store
        .resolve(slug)
        .map_err(|error| DunkError::Refused(error.0))?
    else {
        return Err(DunkError::Refused(format!("no live entry '{slug}'")));
    };
    let self_pane_id = context.self_pane_id();
    let role = context.flock.caller_role(self_pane_id, context.env)?;
    if (old.get("class") == Some("ground-truth") || old.get("authority") == Some("owner"))
        && role != "owner"
    {
        return Err(DunkError::Refused(
            "only the owner supersedes a rule".into(),
        ));
    }
    let by = self_pane_id.unwrap_or(&role);
    let mut ledger = context_ledger();
    let added = store
        .supersede(
            slug,
            body,
            Some(by),
            &crate::context::store::SupersedeMeta::default(),
            Some(&mut ledger),
        )
        .map_err(|error| DunkError::Refused(error.0))?;
    Ok(added_context_value(added))
}

/// `dunk_api.h_context_retire`.
pub fn h_context_retire(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let slug = context_arg_string(&args, "slug")?;
    let reason = args
        .get("reason")
        .and_then(Value::as_str)
        .filter(|reason| !reason.is_empty())
        .unwrap_or("");
    let self_pane_id = context.self_pane_id();
    context
        .flock
        .require_superuser(self_pane_id, "context retire", context.env)?;
    let store = context_store(&args);
    let mut ledger = context_ledger();
    let (slug, reason) = store
        .retire(slug, reason, self_pane_id, Some(&mut ledger))
        .map_err(|error| DunkError::Refused(error.0))?;
    Ok(json!({"slug": slug, "retired": true, "reason": reason}))
}

const MEMORY_RECALL_MAX_LIMIT: i64 = 50;
const MEMORY_CONTEXT_MAX_SESSIONS: i64 = 20;

fn memory_int_arg(args: &Args, key: &str, default: i64) -> Result<i64, DunkError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => py_int(value).ok_or_else(|| match value {
            Value::String(value) => DunkError::Refused(format!(
                "invalid literal for int() with base 10: '{}'",
                value
            )),
            Value::Array(_) => DunkError::Refused(
                "int() argument must be a string, a bytes-like object or a real number, not 'list'"
                    .into(),
            ),
            Value::Object(_) => DunkError::Refused(
                "int() argument must be a string, a bytes-like object or a real number, not 'dict'"
                    .into(),
            ),
            _ => DunkError::Refused("cannot convert value to int".into()),
        }),
    }
}

fn memory_int_arg_at_most(
    args: &Args,
    key: &str,
    default: i64,
    maximum: i64,
) -> Result<i64, DunkError> {
    let value = memory_int_arg(args, key, default)?;
    if value > maximum {
        return Err(DunkError::Refused(format!(
            "{key} must not exceed {maximum}"
        )));
    }
    Ok(value)
}

fn memory_filters(args: &Args) -> Result<crate::memory::recall::Filters, DunkError> {
    let harness = args
        .get("harness")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let project = args
        .get("project")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let since = args
        .get("since")
        .filter(|value| truthy(value))
        .map(python_float)
        .transpose()?;
    Ok(crate::memory::recall::Filters {
        harness,
        project,
        since,
    })
}

fn memory_recall(context: &HandlerContext<'_>, args: &Args) -> Result<Value, DunkError> {
    let query = memory_arg_string(args, "query")?;
    let limit = memory_int_arg_at_most(args, "limit", 10, MEMORY_RECALL_MAX_LIMIT)?.max(0) as usize;
    let service = memory_service(context)?;
    let filters = memory_filters(args)?;
    let input = crate::memory::service::RecallInput::typed(query, limit, &filters, 1200);
    let rows = service.recall(&input)?;
    serde_json::to_value(rows).map_err(|error| DunkError::Refused(error.to_string()))
}

/// `dunk_api.h_memory_recall`.
pub fn h_memory_recall(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    memory_recall(context, &args)
}

/// `dunk_api.h_memory_search`.
pub fn h_memory_search(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    memory_recall(context, &args)
}

/// `dunk_api.h_memory_context`.
pub fn h_memory_context(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let query = memory_arg_string(&args, "query")?;
    // Negative values retain Python's slice-from-the-end behavior; positive requests follow
    // the published API maximum, bounding the recall multiplier below.
    let sessions = memory_int_arg_at_most(&args, "sessions", 5, MEMORY_CONTEXT_MAX_SESSIONS)?;
    memory_service(context)?.context(query, sessions, 2)
}

/// `dunk_api.h_memory_stats`.
pub fn h_memory_stats(context: &HandlerContext<'_>, _args: Args) -> Result<Value, DunkError> {
    memory_service(context)?.stats()
}

/// `dunk_api.h_memory_status`.
pub fn h_memory_status(context: &HandlerContext<'_>, _args: Args) -> Result<Value, DunkError> {
    memory_service(context)?.status()
}

/// `dunk_api.h_memory_ingest`: enqueue only; lifecycle owns starting the worker.
pub fn h_memory_ingest(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let path = memory_arg_string(&args, "path")?;
    let harness = args.get("harness").and_then(Value::as_str);
    let project = args.get("project").and_then(Value::as_str);
    let service = memory_service(context)?;
    let receipt = service.enqueue_receipt(path, harness, project)?;
    Ok(json!({
        "source_id": receipt.source_id,
        "queue_id": receipt.queue_id,
        "path": receipt.path,
        "harness": receipt.harness,
        "project": receipt.project,
    }))
}

fn python_bool_arg(args: &Args, key: &str, default: bool) -> bool {
    match args.get(key) {
        None | Some(Value::Null) => default,
        Some(Value::Bool(value)) => *value,
        Some(Value::String(value)) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "y" | "on"
        ),
        Some(value) => matches!(
            value.to_string().trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "y" | "on"
        ),
    }
}

fn python_float(value: &Value) -> Result<f64, DunkError> {
    match value {
        Value::Number(number) => crate::github::parse_python_json_float(&number.to_string())
            .map_err(|message| DunkError::Refused(message.into())),
        Value::Bool(value) => Ok(if *value { 1.0 } else { 0.0 }),
        Value::String(value) => crate::github::parse_python_float(value).ok_or_else(|| {
            DunkError::Refused(format!("could not convert string to float: '{value}'"))
        }),
        Value::Null => Ok(0.0),
        Value::Array(_) => Err(DunkError::Refused(
            "float() argument must be a string or a real number, not 'list'".into(),
        )),
        Value::Object(_) => Err(DunkError::Refused(
            "float() argument must be a string or a real number, not 'dict'".into(),
        )),
    }
}

/// `dunk_api.h_status`.
pub fn h_status(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    crate::flock::status::h_status(
        context.env.status_env()?,
        context.env.view_cache()?,
        python_bool_arg(&args, "full", false),
        python_bool_arg(&args, "fresh", false),
        python_bool_arg(&args, "check_herdr", false),
        context.self_pane_id(),
    )
}

/// `dunk_api.h_turn_start`.
pub fn h_turn_start(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let self_pane_id = context.self_pane_id();
    let requested = args.get("pane");
    if requested.is_some_and(|value| truthy(value) && value.as_str().is_none()) {
        return Err(DunkError::Refused(
            "turn event must target the caller's own pane".into(),
        ));
    }
    let pane = requested.and_then(Value::as_str);
    let pane_pid = self_pane_id
        .and_then(|id| context.env.get_pane(id))
        .and_then(|info| info.get("pane_pid").and_then(Value::as_i64));
    context.flock.require_turn_caller(
        pane,
        self_pane_id,
        pane_pid,
        context.request.caller.as_ref(),
    )?;

    let ttl_s = args
        .get("ttl_s")
        .filter(|value| truthy(value))
        .map(python_float)
        .transpose()?;
    if ttl_s.is_some_and(|ttl| ttl == 0.0) {
        return Err(DunkError::Refused("ttl_s must be positive".into()));
    }
    let source = args.get("source").and_then(Value::as_str);
    let handoff_id = match args.get("handoff_id") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => Some(value.as_str()),
        Some(value) => {
            let python_type = match value {
                Value::Bool(_) => "bool",
                Value::Number(number) if number.is_i64() || number.is_u64() => "int",
                Value::Number(_) => "float",
                Value::Array(_) => "list",
                Value::Object(_) => "dict",
                Value::Null | Value::String(_) => unreachable!("matched above"),
            };
            return Err(DunkError::Refused(format!(
                "expected string or bytes-like object, got '{python_type}'"
            )));
        }
    };
    context.flock.turn_start(
        pane,
        ttl_s,
        source,
        self_pane_id,
        handoff_id,
        context.env.turn_hooks()?,
    )
}

/// `dunk_api.h_task_follow`.
pub fn h_task_follow(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let scope = python_scope_arg(args.get("scope"));
    let (scope, followers) = context
        .flock
        .task_follow(scope.as_deref(), context.self_pane_id())?;
    Ok(json!({
        "pane_id": context.self_pane_id(),
        "scope": scope,
        "following": true,
        "followers": followers,
    }))
}

/// `dunk_api.h_task_add`.
pub fn h_task_add(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let title = task_add_string(&args, "title")?.ok_or_else(|| {
        DunkError::Refused("task_add: missing required argument(s): title".into())
    })?;
    let scope = task_add_string(&args, "scope")?;
    let body = task_add_string(&args, "body")?;
    let assignee_values = task_values(args.get("assignees"));
    let assignees = assignee_values
        .as_ref()
        .map(|values| values.iter().map(String::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    let dependency_values = task_values(args.get("depends_on"));
    let depends_on = dependency_values
        .as_ref()
        .map(|values| values.iter().map(String::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    let pr = args
        .get("pr")
        .filter(|value| !value.is_null())
        .map(python_value_string);
    let options = TaskAddArgs {
        status: task_add_string(&args, "status")?,
        priority: task_add_string(&args, "priority")?,
        kind: task_add_string(&args, "kind")?,
        author: task_add_string(&args, "author")?,
        repo: task_add_string(&args, "repo")?,
        pr: pr.as_deref(),
        head: task_add_string(&args, "head")?,
        assignees: &assignees,
        parent_id: task_add_string(&args, "parent_id")?,
        depends_on: &depends_on,
        multi_assignee: python_bool_arg(&args, "multi_assignee", false),
    };
    let role = context
        .flock
        .caller_role(context.self_pane_id(), context.env)?;
    let planner = if role == "planner" && options.kind == Some("review") {
        Some(require_planner_assignment_binding(context)?.ok_or_else(|| {
            DunkError::Refused(
                "planner review creation requires an authenticated live pane binding".into(),
            )
        })?)
    } else {
        None
    };
    let task = context.block_on(context.flock.task_add_wire_with_planner(
        title,
        scope,
        body,
        options,
        context.self_pane_id(),
        context.env,
        planner,
    ))??;
    Ok(task.wire)
}

/// `dunk_api.h_task_read`.
pub fn h_task_read(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let id = args.get("id").map(python_value_string).unwrap_or_default();
    let task = context.block_on(
        context
            .flock
            .task_read_wire_with_notes(&id, context.self_pane_id()),
    )??;
    Ok(task.wire)
}

/// `dunk_api.h_task_update`.
pub fn h_task_update(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let pane = context.self_pane_id();
    let role = context.flock.caller_role(pane, context.env)?;
    if !matches!(role.as_str(), "owner" | "system" | "web") {
        let touched = ["status", "assignees", "multi_assignee", "block_reason"]
            .into_iter()
            .filter(|key| args.contains_key(*key))
            .collect::<Vec<_>>();
        if !touched.is_empty() {
            return Err(DunkError::Refused(format!(
                "task update cannot change {} for role {role}: status moves through next/complete/block, and assignment is the owner's",
                touched.join(", ")
            )));
        }
    }
    let assignee_values = task_values(args.get("assignees"));
    let assignees = assignee_values
        .as_ref()
        .map(|values| values.iter().map(String::as_str).collect::<Vec<_>>());
    let depends_values = task_values(args.get("depends_on"));
    let depends_on = depends_values
        .as_ref()
        .map(|values| values.iter().map(String::as_str).collect::<Vec<_>>());
    let parent_id_value = args.get("parent_id").map(|value| {
        if truthy(value) {
            Some(python_value_string(value))
        } else {
            None
        }
    });
    let parent_id = parent_id_value.as_ref().map(|value| value.as_deref());
    let raw_block_reason = args.get("block_reason").map(|value| {
        if value.is_null() {
            None
        } else {
            Some(value.clone())
        }
    });
    let title = python_optional_string_arg(&args, "title");
    let body = python_optional_string_arg(&args, "body");
    let status = python_optional_string_arg(&args, "status");
    let priority = python_optional_string_arg(&args, "priority");
    let update = crate::flock::commands_tasks::TaskUpdateArgs {
        title: title.as_deref(),
        body: body.as_deref(),
        status: status.as_deref(),
        priority: priority.as_deref(),
        parent_id,
        assignees: assignees.as_deref(),
        depends_on: depends_on.as_deref(),
        block_reason: None,
        multi_assignee: args
            .contains_key("multi_assignee")
            .then(|| python_bool_arg(&args, "multi_assignee", false)),
    };
    let id = args.get("id").map(python_value_string).unwrap_or_default();
    let dependency_editor = if depends_on.is_some()
        && !matches!(role.as_str(), "owner" | "system" | "web")
        && context.request.socket_peer.is_some()
    {
        if role == "planner" {
            let planner = require_planner_assignment_binding(context)?
                .ok_or_else(|| DunkError::Refused("planner identity is required".into()))?;
            Some(crate::tasks::planner_scope::DependencyEditor::Planner(
                planner,
            ))
        } else {
            Some(crate::tasks::planner_scope::DependencyEditor::Other)
        }
    } else {
        None
    };
    let task = context.block_on(context.flock.task_update_wire_with_raw_block_reason(
        &id,
        update,
        raw_block_reason,
        pane,
        dependency_editor,
    ))??;
    Ok(task.wire)
}

/// Require fresh kernel evidence for a planner assigning over a socket.
///
/// This is a point-in-time process identity check, not a project grant or an
/// atomic role-revocation guarantee. A process death overlapping the check may
/// occur after a successful check; old daemons do not enforce this check.
fn require_planner_assignment_binding(
    context: &HandlerContext<'_>,
) -> Result<Option<crate::tasks::planner_scope::PlannerIdentity>, DunkError> {
    let refused = || {
        DunkError::Refused(
            "planner task assignment requires an authenticated live pane binding".into(),
        )
    };
    match context.request.socket_peer {
        None => return Ok(None), // Existing trusted in-process callers have no socket identity.
        Some(true) if context.request.identity_bound == Some(true) => {}
        _ => return Err(refused()),
    }
    let pane = context
        .self_pane_id()
        .filter(|p| !p.is_empty())
        .ok_or_else(refused)?;
    let info = context.env.get_pane(pane).ok_or_else(refused)?;
    if info.get("pane_id").and_then(Value::as_str) != Some(pane) {
        return Err(refused());
    }
    let pid = info
        .get("pane_pid")
        .and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
        })
        .filter(|pid| *pid > 1)
        .ok_or_else(refused)?;
    let entry = context
        .flock
        .with_roles(|roles, _| roles.entry(pane))?
        .ok_or_else(refused)?;
    let stored_pid = entry.get("pane_pid").and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
    });
    let stored_start = entry
        .get("start")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(refused)?;
    if entry.get("role").and_then(Value::as_str) != Some("planner")
        || stored_pid != Some(pid)
        || crate::proc::process_start(pid).as_deref() != Some(stored_start)
        || crate::proc::caller_pane_auth_failure(context.request.caller.as_ref(), Some(pid))
            .is_some()
    {
        return Err(refused());
    }
    Ok(Some(crate::tasks::planner_scope::PlannerIdentity {
        pane: pane.to_owned(),
        pid,
        start: stored_start.to_owned(),
        recipients: Vec::new(),
        expected_kind: None,
    }))
}

/// `dunk_api.h_task_assign`.
pub fn h_task_assign(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let pane = context.self_pane_id();
    let role = context.flock.caller_role(pane, context.env)?;
    if !matches!(role.as_str(), "owner" | "system" | "web" | "planner") {
        return Err(DunkError::Refused(format!(
            "task assign requires the owner role (caller is {role}); lanes pull work with task next, they are not handed it"
        )));
    }
    let mut planner = if role == "planner" {
        require_planner_assignment_binding(context)?
    } else {
        None
    };
    let values = task_values(args.get("assignees"));
    let assignees = values
        .as_ref()
        .map(|values| values.iter().map(String::as_str).collect::<Vec<_>>())
        .unwrap_or_default();
    let mode_value = args.get("mode").filter(|value| truthy(value));
    let id = args.get("id").map(python_value_string).unwrap_or_default();
    if let Some(planner_identity) = planner.as_mut().filter(|_| {
        !mode_value
            .and_then(Value::as_str)
            .is_some_and(|m| matches!(m.trim().to_lowercase().as_str(), "remove" | "rm"))
    }) {
        let task = context.block_on(context.flock.task_read(&id, pane))??;
        planner_identity.expected_kind = Some(task.kind.clone());
        let expected = if task.kind == "review" {
            "reviewer"
        } else {
            "worker"
        };
        for target in crate::flock::Flock::task_assignees(&assignees, pane)? {
            let actual = context.flock.caller_role(Some(&target), context.env)?;
            if actual != expected {
                return Err(DunkError::Refused(format!(
                    "planner assignment requires a registered {expected} recipient"
                )));
            }
            let (pid, start) = crate::flock::lane_project::identity(context.env, &target)?;
            planner_identity
                .recipients
                .push(crate::tasks::planner_scope::RecipientIdentity {
                    pane: target,
                    pid,
                    start,
                });
        }
    }
    let task = context.block_on(context.flock.task_assign_wire_with_mode_parser(
        &id,
        &assignees,
        || match mode_value {
            None => Ok("add".to_string()),
            Some(Value::String(value)) => Ok(value.clone()),
            Some(value) => Err(DunkError::Refused(format!(
                "'{}' object has no attribute 'strip'",
                python_type_name(value)
            ))),
        },
        pane,
        role == "planner",
        planner,
    ))??;
    Ok(task.wire)
}

/// Read one owner question and its current blocked-event version.
pub fn h_task_owner_question(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let role = context
        .flock
        .caller_role(context.self_pane_id(), context.env)?;
    if !matches!(role.as_str(), "owner" | "web") {
        return Err(DunkError::Refused("owner role required".into()));
    }
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| DunkError::Refused("task id is required".into()))?;
    let tasks = context
        .flock
        .stores()
        .tasks
        .ok_or_else(|| DunkError::Refused("the task store is not attached".into()))?;
    context.block_on(tasks.owner_question(id, context.self_pane_id()))?
}

/// Record and unblock one specifically versioned owner question atomically.
pub fn h_task_owner_answer(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let role = context
        .flock
        .caller_role(context.self_pane_id(), context.env)?;
    if !matches!(role.as_str(), "owner" | "web") {
        return Err(DunkError::Refused("owner role required".into()));
    }
    let id = args
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| DunkError::Refused("task id is required".into()))?;
    let version = args
        .get("version")
        .and_then(Value::as_str)
        .and_then(|v| v.parse::<i64>().ok())
        .filter(|v| *v > 0)
        .ok_or_else(|| DunkError::Refused("question version is required".into()))?;
    let option = match args.get("option_number") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .filter(|v| *v > 0)
                .ok_or_else(|| DunkError::Refused("invalid owner option".into()))?,
        ),
    };
    let reply = match args.get("reply") {
        None | Some(Value::Null) => None,
        Some(Value::String(v)) => Some(v.as_str()),
        _ => return Err(DunkError::Refused("invalid owner reply".into())),
    };
    let tasks = context
        .flock
        .stores()
        .tasks
        .ok_or_else(|| DunkError::Refused("the task store is not attached".into()))?;
    context.block_on(tasks.owner_answer(
        id,
        version,
        option,
        reply,
        context.self_pane_id(),
        context.self_pane_id(),
    ))?
}

/// `dunk_api.h_task_note`.
pub fn h_task_note(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let id = args.get("id").map(python_value_string).unwrap_or_default();
    let event = args
        .get("event")
        .filter(|value| truthy(value))
        .cloned()
        .unwrap_or_else(|| json!("note"));
    let meta = match args.get("reason").filter(|value| truthy(value)) {
        Some(reason) => json!({"reason": reason}),
        None => json!({}),
    };
    // t4781: a note from the owner's detached tooling (no pane, owner token) is recorded with
    // the actor `owner-service` rather than no actor at all. Everything else is unchanged.
    let pane = context.self_pane_id().filter(|pane| !pane.is_empty());
    let actor = match pane {
        Some(pane) => Some(pane),
        None => context
            .flock
            .caller_role(None, context.env)
            .is_ok_and(|role| {
                crate::flock::commands_task_complete::owner_service_request(
                    false,
                    &role,
                    context.flock.socket_peer_ok(),
                )
            })
            .then_some(crate::tasks::advance::OWNER_SERVICE),
    };
    context.block_on(context.flock.task_note_wire(&id, &event, actor, &meta))?
}

/// `dunk_api.h_task_block`.
pub fn h_task_block(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let id = args.get("id").map(python_value_string).unwrap_or_default();
    let task = context.block_on(context.flock.task_block_wire(
        &id,
        args.get("reason"),
        context.self_pane_id(),
    ))??;
    Ok(task.wire)
}

/// `dunk_api.h_task_unblock`.
pub fn h_task_unblock(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let id = args.get("id").map(python_value_string).unwrap_or_default();
    let task = context.block_on(context.flock.task_unblock_wire(&id, context.self_pane_id()))??;
    Ok(task.wire)
}

/// `dunk_api.h_task_cancel`.
pub fn h_task_cancel(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let id = args.get("id").map(python_value_string).unwrap_or_default();
    context.block_on(context.flock.task_cancel_wire(
        &id,
        args.get("reason"),
        context.self_pane_id(),
    ))?
}

/// `dunk_api.h_task_next`.
pub fn h_task_next(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let pane = context.self_pane_id();
    let scope = python_scope_arg(args.get("scope"));
    // `current_caller` is request-thread state; capture it before this future may hop workers.
    let caller = context.flock.caller_identity();
    context.block_on(context.flock.task_next_wire_with_lease_parser(
        context.env,
        scope.as_deref(),
        || python_lease_seconds(&args),
        python_bool_arg(&args, "allow_busy", false),
        caller,
        pane,
    ))?
}

/// `dunk_api.h_task_batch_claim` (t3583): claim an explicit, ordered list of tasks, all or none.
pub fn h_task_batch_claim(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let pane = context.self_pane_id();
    let ids = batch_ids_arg(&args)?;
    let lease_seconds = python_lease_seconds(&args)?;
    // `current_caller` is request-thread state; capture it before this future may hop workers.
    let caller = context.flock.caller_identity();
    context.block_on(context.flock.task_batch_claim_wire(
        context.env,
        &ids,
        lease_seconds,
        python_bool_arg(&args, "allow_busy", false),
        caller,
        pane,
    ))?
}

/// The `ids` argument: a list of task ids, kept in the order given.
fn batch_ids_arg(args: &Args) -> Result<Vec<String>, DunkError> {
    match args.get("ids") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned).ok_or_else(ids_refusal))
            .collect(),
        _ => Err(ids_refusal()),
    }
}

fn ids_refusal() -> DunkError {
    DunkError::Refused("ids must be a list of task ids".into())
}

/// `dunk_api.h_task_heartbeat`.
pub fn h_task_heartbeat(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let pane = context.self_pane_id();
    let id = args.get("id").map(python_value_string).unwrap_or_default();
    let caller = context.flock.caller_identity();
    let (expires_at, lease_seconds) =
        context.block_on(context.flock.task_heartbeat_with_lease_parser(
            &id,
            || python_lease_seconds(&args),
            caller,
            pane,
        ))??;
    Ok(json!({
        "task_id": id,
        "pane_id": pane,
        "expires_at": crate::github::python_float_json_value(expires_at)
            .expect("successful heartbeat expiry cannot be NaN"),
        "lease_seconds": crate::github::python_float_json_value(lease_seconds)
            .expect("successful heartbeat lease cannot be NaN"),
    }))
}

fn python_lease_seconds(args: &Args) -> Result<Option<f64>, DunkError> {
    let lease_seconds = args
        .get("lease_seconds")
        .filter(|value| truthy(value))
        .map(python_float)
        .transpose()?;
    if lease_seconds.is_some_and(|lease| lease <= 0.0) {
        return Err(DunkError::Refused("lease_seconds must be positive".into()));
    }
    Ok(lease_seconds)
}

/// `dunk_api.h_task_release`.
pub fn h_task_release(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let id = args.get("id").map(python_value_string).unwrap_or_default();
    let task = context.block_on(context.flock.task_release_wire(&id, context.self_pane_id()))??;
    Ok(task.wire)
}

fn python_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(number) if number.is_i64() || number.is_u64() => "int",
        Value::Number(_) => "float",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

impl Dispatcher {
    /// Register only the T39 task-completion handler.
    pub fn register_task_complete(&mut self) {
        self.register("task_complete", h_task_complete);
    }
}

/// `dunk_api.h_task_complete`.
pub fn h_task_complete(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let id = args.get("id").map(python_value_string).unwrap_or_default();
    let by = task_string_arg(&args, "by", "task_complete")?;
    let verdict = task_string_arg(&args, "verdict", "task_complete")?;
    let landed = python_bool_arg(&args, "landed", true);
    context
        .flock
        .task_complete(context, &id, by, verdict, landed)
}

/// `dunk_api.h_task_delete`.
pub fn h_task_delete(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let id = args.get("id").map(python_value_string).unwrap_or_default();
    let deleted = context.block_on(context.flock.task_delete(&id, context.self_pane_id()))??;
    Ok(json!({"deleted": deleted}))
}

/// `dunk_api.h_task_list`.
pub fn h_task_list(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let scope = task_string_arg(&args, "scope", "task_list")?;
    let status = task_string_arg(&args, "status", "task_list")?;
    let assignee = task_string_arg(&args, "assignee", "task_list")?;
    let kind = task_string_arg(&args, "kind", "task_list")?;
    let limit = python_int_arg(&args, "limit", 200)?;
    let (scope, tasks) = context.block_on(context.flock.task_list_wire(
        scope,
        status,
        assignee,
        python_bool_arg(&args, "include_done", true),
        Some(limit),
        context.self_pane_id(),
        kind,
        python_bool_arg(&args, "needs_owner", false),
    ))??;
    Ok(json!({
        "scope": scope.unwrap_or_else(|| "visible".into()),
        "pane_id": context.self_pane_id(),
        "tasks": tasks.into_iter().map(|task| task.wire).collect::<Vec<_>>(),
    }))
}

/// `dunk_api.h_task_unfollow`.
pub fn h_task_unfollow(context: &HandlerContext<'_>, _: Args) -> Result<Value, DunkError> {
    let (was_following, followers) = context.flock.task_unfollow(context.self_pane_id())?;
    Ok(json!({
        "pane_id": context.self_pane_id(),
        "following": false,
        "was_following": was_following,
        "followers": followers,
    }))
}

/// `dunk_api.h_task_followers`.
pub fn h_task_followers(context: &HandlerContext<'_>, _: Args) -> Result<Value, DunkError> {
    let mut followers = context.flock.task_followers();
    followers.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(json!({
        "followers": followers
            .into_iter()
            .map(|(pane_id, scope)| json!({"pane_id": pane_id, "scope": scope}))
            .collect::<Vec<_>>(),
    }))
}

/// Python `repr(str)` shared with native recovery's Unicode-aware formatter.
fn py_repr(text: &str) -> String {
    crate::recovery_native::prompt_python_string_repr(text)
}

#[cfg(test)]
mod tests;

mod desk_nudge;

#[cfg(test)]
#[path = "api/owner_ingest_nudge_tests.rs"]
mod owner_ingest_nudge_tests;

#[cfg(test)]
#[path = "api/owner_page_route_tests.rs"]
mod owner_page_route_tests;

#[cfg(test)]
mod landed_parity_tests;

mod task_statistics;
