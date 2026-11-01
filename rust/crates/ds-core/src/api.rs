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
use std::sync::Mutex;

use serde_json::{json, Map, Value};
use tokio::runtime::Handle;

use crate::error::DunkError;
use crate::flock::panes::PaneInfoEnv;
use crate::flock::Flock;
use crate::proc::Caller;
use crate::server::RequestCtx;
use crate::terminal::Terminal;
use crate::{registry, roles};

/// A request's arguments (the Python `args` dict; insertion-ordered).
pub type Args = Map<String, Value>;

/// A synchronous adapter for command dependencies. All methods are object-safe so a daemon can
/// supply one per request without putting backend types in handler signatures.
pub trait CommandEnv: PaneInfoEnv {
    /// The daemon's terminal adapter. The mutex permits the existing mutable terminal API while
    /// keeping the request context shared with role and read-only adapters.
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>>;

    /// Wall-clock time, in epoch seconds, for request-scoped caches and command timestamps.
    fn now_s(&self) -> f64;

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
        Value::Number(n) => n.as_f64().map_or(true, |f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
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
                None | Some(Value::Null) => true,
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
    let limit = memory_int_arg(args, "limit", 10)?.max(0) as usize;
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
    let sessions = memory_int_arg(&args, "sessions", 5)?;
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
