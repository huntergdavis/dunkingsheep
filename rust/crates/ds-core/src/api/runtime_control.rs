//! Runtime-control handler leaf. Production adapter composition belongs to t1605.
//!
//! Providers must borrow the process-owned queue/allocator. In particular status must
//! never run an allocation tick, and a successful mutation resets only last_tick.
use super::{python_value_string, truthy, Args, HandlerContext};
use crate::{error::DunkError, terminal::raw::equal};
use serde_json::{json, Value};
use std::collections::BTreeSet;

#[derive(Debug)]
pub enum ControlError {
    Backend(DunkError),
    /// Preserve exception identity until the handler's Python catch boundary.
    Python {
        kind: &'static str,
        message: String,
    },
}
pub type ControlResult<T> = Result<T, ControlError>;
impl From<DunkError> for ControlError {
    fn from(value: DunkError) -> Self {
        Self::Backend(value)
    }
}
fn python(kind: &'static str, message: impl Into<String>) -> ControlError {
    ControlError::Python {
        kind,
        message: message.into(),
    }
}
fn wire(error: ControlError, allocation: bool) -> DunkError {
    match error {
        ControlError::Backend(error) => error,
        ControlError::Python { kind, message }
            if allocation && matches!(kind, "TaskError" | "InboxError") =>
        {
            DunkError::Refused(message)
        }
        ControlError::Python { message, .. } => {
            DunkError::Refused(format!("internal error: {message}"))
        }
    }
}
fn arg<'a>(args: &'a Args, key: &str) -> &'a Value {
    args.get(key).unwrap_or(&Value::Null)
}
fn required<'a>(args: &'a Args, key: &str) -> ControlResult<&'a Value> {
    args.get(key)
        .ok_or_else(|| python("KeyError", format!("'{key}'")))
}
fn get<'a>(value: &'a Value, key: &str) -> ControlResult<&'a Value> {
    if !value.is_object() {
        return Err(python(
            "AttributeError",
            format!(
                "'{}' object has no attribute 'get'",
                super::python_type_name(value)
            ),
        ));
    }
    Ok(value.get(key).unwrap_or(&Value::Null))
}
fn key<'a>(value: &'a Value, name: &str) -> ControlResult<&'a Value> {
    get(value, name)?;
    value
        .get(name)
        .ok_or_else(|| python("KeyError", format!("'{name}'")))
}
fn rows(value: &Value) -> ControlResult<&[Value]> {
    value.as_array().map(Vec::as_slice).ok_or_else(|| {
        python(
            "TypeError",
            format!(
                "'{}' object is not iterable",
                super::python_type_name(value)
            ),
        )
    })
}
fn hashable(value: &Value) -> ControlResult<()> {
    if value.is_array() || value.is_object() {
        Err(python(
            "TypeError",
            format!("unhashable type: '{}'", super::python_type_name(value)),
        ))
    } else {
        Ok(())
    }
}

/// A borrowed queue, opened only after dispatch's action-specific role check.
/// Raw parameters retain Python/SQLite validation order; do not stringify them early.
pub trait DispatchQueue {
    fn add(&self, brief: &Value, task_id: &Value, lane: &Value) -> ControlResult<Value>;
    fn items(&self) -> ControlResult<Vec<Value>>;
    fn remove(&self, id: &Value) -> ControlResult<Value>;
}
/// Explicit inputs to the existing dispatch tick producer. The provider must use the
/// same queue, actual delivery probes, fresh-mail gate (600s and turn ended_at), and
/// inbox.send(subject="standby", sender=caller), collecting expired-only mail panes.
pub struct DispatchTick<'a> {
    pub queue: &'a dyn DispatchQueue,
    pub panes: &'a [Value],
    pub derived: &'a [Value],
    pub allow: &'a BTreeSet<String>,
    pub dry_run: bool,
    pub caller: Option<&'a str>,
}
pub struct DispatchTickResult {
    pub state: Value,
    pub expired: Vec<String>,
}

/// Adapter to the SAME allocator store as RuntimeServices. All methods are required:
/// an incomplete provider cannot silently manufacture success or a fresh cache.
pub trait AllocationStore {
    fn status(&self) -> ControlResult<Value>;
    // Mirrors the Python store boundary without changing argument order.
    #[allow(clippy::too_many_arguments)]
    fn register(
        &self,
        pane: &Value,
        pools: &Value,
        roles: &Value,
        skills: &Value,
        caller: Option<&str>,
        enabled: &Value,
    ) -> ControlResult<Value>;
    fn offer(
        &self,
        task: &Value,
        pool: &Value,
        requires: &Value,
        caller: Option<&str>,
    ) -> ControlResult<Value>;
    #[allow(clippy::too_many_arguments)]
    fn review(
        &self,
        task: &Value,
        revision: &Value,
        body: &Value,
        pool: &Value,
        requires: &Value,
        caller: Option<&str>,
    ) -> ControlResult<Value>;
}
pub trait RuntimeControlEnv {
    fn caller_role(&self, caller: Option<&str>) -> ControlResult<String>;
    fn dispatch_queue(&self) -> ControlResult<&dyn DispatchQueue>;
    fn dispatch_state(&self) -> ControlResult<Value>;
    fn resolve_lanes(&self) -> ControlResult<Vec<Value>>;
    /// Resolve args/config/environment against exactly this one lane snapshot.
    fn dispatch_allowlist(&self, args: &Args, lanes: &[Value]) -> ControlResult<BTreeSet<String>>;
    fn task_list(&self, include_done: bool, limit: usize) -> ControlResult<Value>;
    fn task_leases(&self, task: &Value) -> ControlResult<Vec<Value>>;
    fn is_retired(&self, pane: &Value) -> ControlResult<bool>;
    fn all_retired_count(&self) -> ControlResult<usize>;
    fn now_s(&self) -> f64;
    fn dispatch_tick(&self, input: DispatchTick<'_>) -> ControlResult<DispatchTickResult>;
    fn allocation_store(&self) -> ControlResult<&dyn AllocationStore>;
    fn allocation_last_result(&self) -> ControlResult<Value>;
    fn require_turn_caller(&self, caller: Option<&str>) -> ControlResult<()>;
    fn require_superuser(&self, caller: Option<&str>, action: &str) -> ControlResult<()>;
    fn task_get(&self, task: &Value) -> ControlResult<Value>;
    fn authorize_task_complete(
        &self,
        task: &Value,
        caller: Option<&str>,
        operator: bool,
    ) -> ControlResult<()>;
    fn allocation_reset_last_tick(&self) -> ControlResult<()>;
    fn next_job_wake(&self) -> ControlResult<()>;
    /// Actual stop-all timer cancellation and persistence, not a DunkList-only edit.
    fn stop_all(&self) -> ControlResult<()>;
}

pub fn h_dispatch(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let env = context.env.runtime_control_env()?;
    dispatch(env, &args, context.self_pane_id()).map_err(|e| wire(e, false))
}
fn dispatch(
    env: &dyn RuntimeControlEnv,
    args: &Args,
    caller: Option<&str>,
) -> ControlResult<Value> {
    let action = args
        .get("action")
        .filter(|v| truthy(v))
        .and_then(Value::as_str);
    // Truthy nonstrings are invalid actions, not the default status.
    let default = !arg(args, "action").is_null() && truthy(arg(args, "action"));
    let action = action.unwrap_or(if default { "" } else { "status" });
    if matches!(action, "add" | "remove" | "tick")
        && !matches!(env.caller_role(caller)?.as_str(), "owner" | "system")
    {
        return Err(DunkError::Refused(format!(
            "dispatch {action} requires the owner or system role; status and list are open"
        ))
        .into());
    }
    let queue = env.dispatch_queue()?;
    match action {
        "add" => queue.add(
            required(args, "brief")?,
            arg(args, "task_id"),
            arg(args, "lane"),
        ),
        "remove" => queue.remove(required(args, "id")?),
        "list" => {
            let items = queue.items()?;
            let pending = queue.items()?.len();
            Ok(json!({"items":items,"pending":pending}))
        }
        "status" => {
            let state = env.dispatch_state()?;
            let summary = if truthy(&state) {
                crate::dispatch::tick::summary(&state)
            } else {
                "no tick yet".into()
            };
            let pending = queue.items()?.len();
            Ok(json!({"state":state,"summary":summary,"pending":pending}))
        }
        "tick" => tick(env, queue, args, caller),
        _ => Err(python(
            "ValueError",
            "dispatch action must be add, list, remove, tick or status",
        )),
    }
}
fn tasks(env: &dyn RuntimeControlEnv) -> ControlResult<Vec<Value>> {
    let listed = env.task_list(false, 500)?;
    let value = get(&listed, "tasks")?;
    Ok(if truthy(value) {
        rows(value)?.to_vec()
    } else {
        vec![]
    })
}
fn tick(
    env: &dyn RuntimeControlEnv,
    queue: &dyn DispatchQueue,
    args: &Args,
    caller: Option<&str>,
) -> ControlResult<Value> {
    let lanes = env.resolve_lanes()?;
    let allow = env.dispatch_allowlist(args, &lanes)?;
    let mut leased = BTreeSet::new();
    for task in tasks(env)? {
        // Python catches every Exception only around task["id"] + leases_for.
        let leases = key(&task, "id")
            .and_then(|id| env.task_leases(id))
            .unwrap_or_default();
        for lease in leases {
            leased.insert(python_value_string(get(&lease, "pane_id")?));
        }
    }
    let mut panes = vec![];
    for pane in lanes {
        let status = get(&pane, "agent_status")?;
        if status.is_null() || status == "unknown" {
            continue;
        }
        let id = key(&pane, "pane_id")?;
        hashable(id)?;
        if !id.as_str().is_some_and(|id| allow.contains(id))
            || leased.contains(&python_value_string(id))
        {
            continue;
        }
        if !env.is_retired(id)? {
            panes.push(pane);
        }
    }
    let mut tasks = tasks(env)?;
    for task in &mut tasks {
        if !task
            .as_object()
            .ok_or_else(|| python("TypeError", "task is not a mapping"))?
            .contains_key("leases")
        {
            let leases = key(task, "id")
                .and_then(|id| env.task_leases(id))
                .unwrap_or_default();
            task["leases"] = json!(leases);
        }
    }
    let state = env.dispatch_state()?;
    let history = get(&state, "history")?;
    let mut dispatched = BTreeSet::new();
    if truthy(history) {
        for entry in rows(history)? {
            let id = get(entry, "task_id")?;
            if !truthy(id) {
                continue;
            }
            let at = entry.get("at").unwrap_or(&Value::Null);
            let at = if entry.get("at").is_none() {
                0.0
            } else {
                at.as_f64()
                    .or_else(|| at.as_bool().map(|b| if b { 1.0 } else { 0.0 }))
                    .ok_or_else(|| {
                        python(
                            "TypeError",
                            format!(
                                "'>=' not supported between instances of '{}' and 'float'",
                                super::python_type_name(at)
                            ),
                        )
                    })?
            };
            if at >= env.now_s() - 21600.0 {
                hashable(id)?;
                if let Some(id) = id.as_str() {
                    dispatched.insert(id.to_owned());
                }
            }
        }
    }
    for item in queue.items()? {
        let id = get(&item, "task_id")?;
        if truthy(id) {
            dispatched.insert(python_value_string(id));
        }
    }
    let (derived, coarse) =
        crate::dispatch::derive_board_items(&tasks, &dispatched, Some(env.now_s()), 90);
    let mut result = env.dispatch_tick(DispatchTick {
        queue,
        panes: &panes,
        derived: &derived,
        allow: &allow,
        dry_run: truthy(arg(args, "dry_run")),
        caller,
    })?;
    if !result.state.is_object() {
        return Err(python("TypeError", "tick state is not a mapping"));
    }
    result.expired.sort();
    result.expired.dedup();
    result.state["unread_mail_expired"] = json!(result.expired);
    result.state["retired"] = json!(env.all_retired_count()?);
    result.state["board_open"] = json!(tasks
        .iter()
        .filter(|t| t
            .get("status")
            .is_some_and(|s| python_value_string(s) == "open"))
        .count());
    result.state["coarse_tasks"] = json!(coarse);
    Ok(result.state)
}

pub fn h_allocation(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let env = context.env.runtime_control_env()?;
    allocation(env, &args, context.self_pane_id()).map_err(|e| wire(e, true))
}
fn allocation(
    env: &dyn RuntimeControlEnv,
    args: &Args,
    caller: Option<&str>,
) -> ControlResult<Value> {
    let action = args
        .get("action")
        .map_or(Some("status"), Value::as_str)
        .unwrap_or("");
    let store = env.allocation_store()?;
    if action == "status" {
        let mut status = store.status()?;
        let last = env.allocation_last_result()?;
        let map = status
            .as_object_mut()
            .ok_or_else(|| python("TypeError", "allocation status is not a mapping"))?;
        map.insert("last_tick".into(), last);
        return Ok(status);
    }
    if matches!(action, "register" | "offer") {
        env.require_turn_caller(caller)?;
        env.require_superuser(caller, &format!("allocation_{action}"))?;
    }
    let empty = json!([]);
    let result = match action {
        "register" => {
            let pane = arg(args, "pane_id");
            let lanes = env.resolve_lanes()?;
            let mut found = false;
            for p in lanes {
                if equal(key(&p, "pane_id")?, pane) {
                    found = true;
                    break;
                }
            }
            if !found {
                return Err(
                    DunkError::Refused("register requires an existing pane id".into()).into(),
                );
            }
            store.register(
                pane,
                args.get("pools").unwrap_or(&empty),
                args.get("roles").unwrap_or(&empty),
                args.get("skills").unwrap_or(&empty),
                caller,
                args.get("enabled").unwrap_or(&Value::Bool(true)),
            )?
        }
        "offer" => store.offer(
            arg(args, "task_id"),
            arg(args, "pool"),
            args.get("requires").unwrap_or(&empty),
            caller,
        )?,
        "review" => {
            let task = env.task_get(arg(args, "task_id"))?;
            env.authorize_task_complete(&task, caller, false)?;
            store.review(
                key(&task, "id")?,
                arg(args, "revision"),
                arg(args, "body"),
                arg(args, "pool"),
                args.get("requires").unwrap_or(&empty),
                caller,
            )?
        }
        _ => {
            return Err(DunkError::Refused(
                "allocation action must be status, register, offer or review".into(),
            )
            .into())
        }
    };
    env.allocation_reset_last_tick()?;
    env.next_job_wake()?;
    Ok(result)
}
pub fn h_shutdown(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let stop = match arg(&args, "stop_all") {
        Value::Null => false,
        Value::Bool(value) => *value,
        value => matches!(
            python_value_string(value)
                .trim_matches(|c: char| c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}'))
                .to_lowercase()
                .as_str(),
            "1" | "true" | "yes" | "y" | "on"
        ),
    };
    if stop {
        context
            .env
            .runtime_control_env()?
            .stop_all()
            .map_err(|e| wire(e, false))?;
    }
    // No stop flag or service join here: the server replies before stopping.
    Ok(json!({"shutting_down":true,"stopped_all":stop}))
}

#[cfg(test)]
#[path = "runtime_control_tests.rs"]
mod tests;
