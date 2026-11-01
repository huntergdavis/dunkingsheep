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

fn python_scope_arg(value: Option<&Value>) -> Option<String> {
    let value = value.filter(|value| truthy(value))?;
    Some(match value {
        Value::Null => unreachable!("falsey null is filtered out"),
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

fn python_value_repr(value: &Value) -> String {
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
        Value::String(_) => unreachable!("matched above"),
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

    /// Register only the T39 task-following API leaf.
    pub fn register_task_follow_unfollow_followers(&mut self) {
        self.register("task_follow", h_task_follow);
        self.register("task_unfollow", h_task_unfollow);
        self.register("task_followers", h_task_followers);
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
