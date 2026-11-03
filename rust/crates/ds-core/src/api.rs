//! `api.rs` (T39 slice 1): the dispatch CORE. Ports `dunk_api.dispatch`: unknown-command
//! refusal, the owner gate / per-role gate, the desk argument clamp, the required and unknown
//! argument checks, the handler call, and the desk result clamp, in exactly that order.
//!
//! Handlers live in a table keyed by registry command name. Every command not yet ported
//! resolves to a NAMED refusal ("command <x> is not ported yet"), never a guess, so the later
//! command-group slices (T34..T38) only call [`Dispatcher::register`].
//!
//! NOT in this slice: the handlers themselves.

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

/// A synchronous adapter for command dependencies. All methods are object-safe so a daemon can
/// supply one per request without putting backend types in handler signatures.
pub trait CommandEnv: PaneInfoEnv {
    /// The daemon's terminal adapter. The mutex permits the existing mutable terminal API while
    /// keeping the request context shared with role and read-only adapters.
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>>;

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

    /// Status sources for the `status` handler. Existing fixtures may stage this adapter, but
    /// production dispatch must supply the real daemon snapshot.
    fn status_env(&self) -> Result<&dyn crate::flock::status::StatusEnv, DunkError> {
        Err(DunkError::Refused(
            "status command adapter is not wired".into(),
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
        crate::panes::default_path(&crate::config::Env::from_process())
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

    /// Fallible PM routing and digest services. Runtime composition is assigned to t1605.
    fn pm_env(&self) -> Result<&dyn crate::pm::PmEnv, DunkError> {
        Err(DunkError::Refused("PM adapter is not wired".into()))
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
fn truthy(value: &Value) -> bool {
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
        Value::Number(value) => value.to_string(),
    }
}

fn python_value_string(value: &Value) -> String {
    match value {
        Value::String(value) => value.clone(),
        Value::Array(_) | Value::Object(_) => python_value_repr(value),
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(value) => value.to_string(),
    }
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
}

impl Dispatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register the handler for a registry command (later slices call this).
    pub fn register(&mut self, name: &'static str, handler: Handler) {
        self.handlers.insert(name, handler);
    }

    /// Register only the T39 status, identity and turn-start handler leaf.
    pub fn register_status_whoami_turn_start(&mut self) {
        self.register("status", h_status);
        self.register("whoami", h_whoami);
        self.register("turn_start", h_turn_start);
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
        self.register("review_sync", h_review_sync);
        self.register("review_list", h_review_list);
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

    /// Register the T39 owner statistics, receipts and conversation views.
    pub fn register_owner_stats_receipts_conversation(&mut self) {
        self.register("owner_stats", h_owner_stats);
        self.register("owner_receipts", h_owner_receipts);
        self.register("owner_conversation", h_owner_conversation);
    }

    /// Register the T39 owner delivery and mailbox view handlers.
    pub fn register_owner_delivered_message_state_reply_stats(&mut self) {
        self.register("owner_delivered", h_owner_delivered);
        self.register("message_state", h_message_state);
        self.register("reply_stats", h_reply_stats);
    }

    /// Register only the T39 owner reply, ingest and outbox handlers.
    pub fn register_owner_reply_ingest_outbox(&mut self) {
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

    /// Register only the T39 direct-send handlers.
    pub fn register_send_text_keys_flush_box(&mut self) {
        self.register("send_text", h_send_text);
        self.register("send_keys", h_send_keys);
        self.register("flush_box", h_flush_box);
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
        let role = gate.caller_role(self_pane_id)?;
        if command.owner_only && role != "system" && role != "web" {
            // The owner gate: passing it IS the permission for these verbs.
            gate.require_superuser(self_pane_id, name)?;
        } else if !roles::allowed(&role, name) {
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
            .required
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
            return Err(DunkError::Refused(format!(
                "{name}: unknown argument(s): {}",
                unknown.join(", ")
            )));
        }
        let result = match self.handlers.get(command.name) {
            Some(handler) => handler(context, args)?,
            None => return Err(not_ported(command.name)),
        };
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
        .and_then(Value::as_str)
        .ok_or_else(|| DunkError::Refused("send_keys: keys must be a string".into()))?;
    let hold_while_typing = python_bool_arg(&args, "hold_while_typing", true);
    let wait_s = send_wait_s(&args, "send_keys")?;
    context.env.send_commands()?.send_keys(
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
    let env = context.env.pm_env()?;
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
    context.flock.pm_digest(
        context.env.pm_env()?,
        &project,
        payload,
        target.as_deref(),
        context.self_pane_id(),
    )
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
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::Bool(value)) => Some(if *value { 1.0 } else { 0.0 }),
        Some(Value::String(text)) => Some(text.trim().parse::<f64>().map_err(|_| {
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
    Ok(json!({
        "message": owner_message_value(&ingested.message),
        "duplicate": ingested.duplicate,
        "transport": ingested.transport,
    }))
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

fn poolrad_limit(args: &Args) -> Result<usize, DunkError> {
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

fn poolrad_dry_run(value: Option<&Value>) -> bool {
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
    let tokens = context.env.github_tokens()?;
    let transport = context.env.review_transport()?;
    let policy = crate::review::ConfiguredReviewPolicy {
        config_dir: tokens.config_dir().to_path_buf(),
    };
    let result = context.block_on(context.flock.review_sync(
        tokens,
        policy,
        tokens.api_root(),
        crate::flock::commands_review::BorrowedGitHubTransport(transport),
        python_bool_arg(&args, "dry_run", false),
    ))?;
    result.map_err(DunkError::Refused)
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
    if kind == "author" {
        if let Some(pane_id) = context.self_pane_id().filter(|pane| !pane.is_empty()) {
            let now = context.env.now_s();
            context.flock.record_gh_token_mint(pane_id, now);
            let _ =
                crate::github::record_author_mint(pane_id, Some(now), None, tokens.config_dir());
        }
    }
    Ok(json!({
        "token": token,
        "kind": kind,
        "role": role,
        "slug": entry.get("slug").cloned().unwrap_or(Value::Null),
        "expires_at": entry.get("expires_at").cloned().unwrap_or(Value::Null),
        "cached": entry.get("cached").cloned().unwrap_or(Value::Bool(false)),
    }))
}

/// `dunk_api.h_poolrad_sync` through the accepted T52 Poolrad service.
pub fn h_poolrad_sync(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let limit = poolrad_limit(&args)?;
    let dry_run = poolrad_dry_run(args.get("dry_run"));
    context.env.poolrad_sync(limit, dry_run)
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
    if action != "list" {
        return Err(DunkError::Refused(
            "lanes action must be list or set".into(),
        ));
    }
    let rows = context.flock.lanes();
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

fn memory_pool(
    context: &HandlerContext<'_>,
) -> Result<std::sync::Arc<crate::db::ReadPool>, DunkError> {
    context.flock.stores().memory.ok_or_else(|| {
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
    let pool = memory_pool(context)?;
    let rows = crate::memory::recall::recall(
        pool.as_ref(),
        query,
        limit,
        &memory_filters(args)?,
        context.env.now_s(),
        1200,
    )?;
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
    let pool = memory_pool(context)?;
    crate::memory::queries::context(pool.as_ref(), query, sessions, 2, context.env.now_s())
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
        Value::Number(number) => number
            .as_f64()
            .ok_or_else(|| DunkError::Refused("number is outside float range".into())),
        Value::Bool(value) => Ok(if *value { 1.0 } else { 0.0 }),
        Value::String(value) => value.parse::<f64>().map_err(|_| {
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
    let task = context.block_on(context.flock.task_add_wire(
        title,
        scope,
        body,
        options,
        context.self_pane_id(),
        context.env,
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

/// The named refusal for a registered command with no Rust handler yet.
pub fn not_ported(name: &str) -> DunkError {
    DunkError::Refused(format!("command {name} is not ported yet"))
}

/// Python `repr(str)` for the names a client can send (single quotes unless the text has a
/// single quote and no double quote; backslash and control characters escaped).
fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

#[cfg(test)]
mod tests;
