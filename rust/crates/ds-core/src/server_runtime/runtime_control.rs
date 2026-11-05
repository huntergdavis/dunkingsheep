//! Production provider for the accepted dispatch, allocation, and shutdown handlers.
//!
//! The handler API owns Python argument/refusal order. This layer binds those operations to the
//! request's one Flock and the process-owned Tasks, inbox, queue, allocator, and dunk runtime.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use crate::api::runtime_control::{
    AllocationStore, ControlError, ControlResult, DispatchQueue, DispatchTick, DispatchTickResult,
    RuntimeControlEnv,
};
use crate::dispatch::queue::{DispatchError, Queue};
use crate::dispatch::tick::{Tick, TickQueue};
use crate::error::DunkError;
use crate::flock::panes::PaneInfoEnv;
use crate::inbox_store::Outgoing;
use crate::server_runtime::{RequestRuntimeAdapter, RuntimeServices};

/// Startup-captured paths and a retryable, process-owned lazy standby queue.
pub(super) struct RuntimeControlState {
    config_dir: PathBuf,
    dispatch_lanes_env: Option<String>,
    queue: OnceLock<Queue>,
    queue_open: Mutex<()>,
}

impl RuntimeControlState {
    pub(super) fn new(config_dir: PathBuf, dispatch_lanes_env: Option<String>) -> Self {
        Self {
            config_dir,
            dispatch_lanes_env,
            queue: OnceLock::new(),
            queue_open: Mutex::new(()),
        }
    }

    fn queue(&self) -> ControlResult<&Queue> {
        if let Some(queue) = self.queue.get() {
            return Ok(queue);
        }
        let _open = self
            .queue_open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(queue) = self.queue.get() {
            return Ok(queue);
        }
        let queue = Queue::open(self.config_dir.join("dispatch.db")).map_err(|error| {
            ControlError::Backend(DunkError::Unavailable(format!(
                "could not open dispatch queue: {error}"
            )))
        })?;
        let _ = self.queue.set(queue);
        Ok(self
            .queue
            .get()
            .expect("queue was installed under its open lock"))
    }

    fn state_path(&self) -> PathBuf {
        self.config_dir.join("dispatch-state.json")
    }

    fn lanes_path(&self) -> PathBuf {
        self.config_dir.join("dispatch_lanes.json")
    }

    fn retired_path(&self) -> PathBuf {
        self.config_dir.join("retired-panes.json")
    }
}

fn backend(error: DunkError) -> ControlError {
    ControlError::Backend(error)
}

fn dispatch_error(error: DispatchError) -> ControlError {
    ControlError::Backend(DunkError::Refused(error.to_string()))
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_none_or(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn python_lane_text(value: &Value) -> String {
    crate::api::python_value_string(value)
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

impl DispatchQueue for Queue {
    fn add(&self, brief: &Value, task_id: &Value, lane: &Value) -> ControlResult<Value> {
        self.add_values(brief, task_id, lane)
            .map_err(dispatch_error)
    }

    fn items(&self) -> ControlResult<Vec<Value>> {
        Queue::items(self, true).map_err(dispatch_error)
    }

    fn remove(&self, id: &Value) -> ControlResult<Value> {
        self.remove_value(id).map_err(dispatch_error)
    }

    fn next_for(&self, lane: &str) -> ControlResult<Option<Value>> {
        Queue::next_for(self, lane).map_err(dispatch_error)
    }

    fn take(
        &self,
        id: &Value,
        lane: &str,
        sent_message: Option<&str>,
        now: f64,
    ) -> ControlResult<Value> {
        let Some(id) = id.as_i64() else {
            return Err(dispatch_error(DispatchError(format!(
                "no standby item {id}"
            ))));
        };
        Queue::take(self, id, lane, sent_message, Some(now)).map_err(dispatch_error)
    }
}

struct TickQueueAdapter<'a>(&'a dyn DispatchQueue);

impl TickQueue for TickQueueAdapter<'_> {
    type Error = ControlError;

    fn next_for(&self, lane: &str) -> Result<Option<Value>, Self::Error> {
        self.0.next_for(lane)
    }

    fn take(
        &self,
        item_id: &Value,
        lane: &str,
        sent_message: Option<&str>,
        now: f64,
    ) -> Result<Value, Self::Error> {
        self.0.take(item_id, lane, sent_message, now)
    }

    fn items(&self) -> Result<Vec<Value>, Self::Error> {
        self.0.items()
    }
}

impl RequestRuntimeAdapter<'_> {
    fn services(&self) -> ControlResult<&RuntimeServices> {
        self.dunk_runtime.ok_or_else(|| {
            backend(DunkError::Refused(
                "request runtime services are not attached".into(),
            ))
        })
    }

    fn tasks(&self) -> ControlResult<std::sync::Arc<crate::tasks::Tasks>> {
        self.terminal
            .flock
            .stores()
            .tasks
            .ok_or_else(|| backend(DunkError::Refused("task store unavailable".into())))
    }

    fn caller(&self) -> Option<crate::proc::Caller> {
        self.request.caller.clone()
    }
}

impl RuntimeControlEnv for RequestRuntimeAdapter<'_> {
    fn caller_role(&self, caller: Option<&str>) -> ControlResult<String> {
        self.terminal
            .flock
            .caller_role(caller, self)
            .map_err(backend)
    }

    fn dispatch_queue(&self) -> ControlResult<&dyn DispatchQueue> {
        Ok(self.services()?.runtime_control.queue()?)
    }

    fn dispatch_state(&self) -> ControlResult<Value> {
        Ok(crate::dispatch::load_state(
            &self.services()?.runtime_control.state_path(),
        ))
    }

    fn resolve_lanes(&self) -> ControlResult<Vec<Value>> {
        self.terminal
            .flock
            .resolve_lanes(&self.terminal, crate::clock::now_s())
            .map_err(backend)
    }

    fn dispatch_allowlist(
        &self,
        args: &crate::api::Args,
        lanes: &[Value],
    ) -> ControlResult<BTreeSet<String>> {
        let control = &self.services()?.runtime_control;
        let mut wanted = Vec::new();
        if let Some(raw) = args.get("lanes").filter(|value| truthy(value)) {
            match raw {
                Value::String(value) => {
                    wanted.extend(
                        value
                            .split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_owned),
                    );
                }
                Value::Array(values) => {
                    wanted.extend(
                        values
                            .iter()
                            .map(python_lane_text)
                            .map(|s| s.trim().to_owned())
                            .filter(|s| !s.is_empty()),
                    );
                }
                Value::Object(values) => {
                    wanted.extend(
                        values
                            .keys()
                            .map(|s| s.trim().to_owned())
                            .filter(|s| !s.is_empty()),
                    );
                }
                value => {
                    return Err(ControlError::Python {
                        kind: "TypeError",
                        message: format!("'{}' object is not iterable", python_type_name(value)),
                    });
                }
            }
        }
        if wanted.is_empty() {
            match std::fs::read_to_string(control.lanes_path()) {
                Ok(text) => {
                    let loaded: Value = match serde_json::from_str(&text) {
                        Ok(value) => value,
                        Err(error) => {
                            eprintln!("dispatch: dispatch_lanes.json unreadable ({error}); feeding nothing");
                            return Ok(BTreeSet::new());
                        }
                    };
                    let listed = if loaded.is_object() {
                        loaded.get("lanes").cloned().unwrap_or(Value::Null)
                    } else {
                        loaded.clone()
                    };
                    let listed = match listed {
                        Value::String(value) => value
                            .split(',')
                            .map(str::to_owned)
                            .map(Value::String)
                            .collect::<Vec<_>>(),
                        Value::Array(values) => values,
                        Value::Null | Value::Bool(false) => Vec::new(),
                        Value::Number(number) if number.as_f64() == Some(0.0) => Vec::new(),
                        value if !truthy(&value) => Vec::new(),
                        value => {
                            eprintln!("dispatch: dispatch_lanes.json unreadable (lanes is not iterable); feeding nothing");
                            let _ = value;
                            return Ok(BTreeSet::new());
                        }
                    };
                    if truthy(&loaded) && listed.is_empty() {
                        eprintln!(
                            "dispatch: dispatch_lanes.json has no lanes list; feeding nothing"
                        );
                    }
                    for entry in listed {
                        wanted.extend(
                            python_lane_text(&entry)
                                .split(',')
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(str::to_owned),
                        );
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    eprintln!(
                        "dispatch: dispatch_lanes.json unreadable ({error}); feeding nothing"
                    );
                    return Ok(BTreeSet::new());
                }
            }
        }
        if wanted.is_empty() {
            wanted.extend(
                control
                    .dispatch_lanes_env
                    .as_deref()
                    .unwrap_or("")
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
            );
        }
        let mut by_name: std::collections::BTreeMap<String, BTreeSet<String>> =
            std::collections::BTreeMap::new();
        for lane in lanes {
            let pane_id = lane
                .get("pane_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !pane_id.is_empty() {
                by_name
                    .entry(pane_id.to_lowercase())
                    .or_default()
                    .insert(pane_id.to_owned());
            }
            if let Some(name) = lane.get("agent_name").and_then(Value::as_str) {
                by_name
                    .entry(name.to_lowercase())
                    .or_default()
                    .insert(pane_id.to_owned());
            }
        }
        let mut resolved = BTreeSet::new();
        for entry in wanted {
            let matches = by_name.get(&entry.to_lowercase());
            if let Some(matches) = matches {
                resolved.extend(matches.iter().cloned());
            } else {
                resolved.insert(entry);
            }
        }
        Ok(resolved)
    }

    fn task_list(&self, include_done: bool, limit: usize) -> ControlResult<Value> {
        let (scope, tasks) = self
            .runtime
            .block_on(self.terminal.flock.task_list_wire(
                None,
                None,
                None,
                include_done,
                Some(i64::try_from(limit).unwrap_or(i64::MAX)),
                None,
                None,
                false,
            ))
            .map_err(backend)?;
        let tasks = tasks.into_iter().map(|task| task.wire).collect::<Vec<_>>();
        Ok(json!({"scope": scope.unwrap_or_else(|| "visible".into()), "tasks": tasks}))
    }

    fn task_leases(&self, task: &Value) -> ControlResult<Vec<Value>> {
        let id = task.as_str().ok_or_else(|| ControlError::Python {
            kind: "TypeError",
            message: "task id must be a string".into(),
        })?;
        let wire = self
            .runtime
            .block_on(self.tasks()?.get_full_wire(id, None))
            .map_err(backend)?
            .wire;
        wire.get("leases")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| backend(DunkError::Refused("task lease wire unavailable".into())))
    }

    fn is_retired(&self, pane: &Value) -> ControlResult<bool> {
        let Some(pane) = pane.as_str() else {
            return Ok(false);
        };
        Ok(crate::panes::is_retired(
            pane,
            &self.services()?.runtime_control.retired_path(),
        ))
    }

    fn all_retired_count(&self) -> ControlResult<usize> {
        Ok(crate::panes::all_retired(&self.services()?.runtime_control.retired_path()).len())
    }

    fn now_s(&self) -> f64 {
        crate::clock::now_s()
    }

    fn dispatch_tick(&self, input: DispatchTick<'_>) -> ControlResult<DispatchTickResult> {
        let services = self.services()?;
        let queue = TickQueueAdapter(input.queue);
        let expired = RefCell::new(Vec::new());
        let delivery = |pane: &Value| -> ControlResult<Value> {
            let pane_id = pane.get("pane_id").and_then(Value::as_str).ok_or_else(|| {
                ControlError::Python {
                    kind: "KeyError",
                    message: "'pane_id'".into(),
                }
            })?;
            let status = pane
                .get("agent_status")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if matches!(status.to_ascii_lowercase().as_str(), "working" | "busy") {
                return Ok(json!({"state":"working","reason":status}));
            }
            Ok(self.terminal.flock.delivery_state(
                &self.terminal,
                pane_id,
                Some(status),
                Some(pane),
                None,
            ))
        };
        let unread =
            |pane: &Value| -> ControlResult<i64> {
                let pane_id = pane.get("pane_id").and_then(Value::as_str).ok_or_else(|| {
                    ControlError::Python {
                        kind: "KeyError",
                        message: "'pane_id'".into(),
                    }
                })?;
                let inbox =
                    self.terminal.flock.stores().inbox.ok_or_else(|| {
                        backend(DunkError::Refused("inbox store unavailable".into()))
                    })?;
                let (fresh, expired_only) = inbox
                    .sweep_gate(
                        pane_id,
                        self.terminal.flock.turn_ended_at(pane_id),
                        self.now_s(),
                    )
                    .map_err(backend)?;
                if expired_only {
                    expired.borrow_mut().push(pane_id.to_owned());
                }
                Ok(fresh)
            };
        let inbox = self
            .terminal
            .flock
            .stores()
            .inbox
            .ok_or_else(|| backend(DunkError::Refused("inbox store unavailable".into())))?;
        let caller = input.caller;
        let mut send = |pane_id: &str, text: &str| {
            inbox
                .send(&Outgoing {
                    mailbox: pane_id,
                    body: Some(text),
                    subject: Some("standby"),
                    sender: caller,
                    ..Outgoing::default()
                })
                .map(|id| json!(id))
                .map_err(|error| error.to_string())
        };
        let state_path = services.runtime_control.state_path();
        let options = Tick {
            now: Some(self.now_s()),
            dry_run: input.dry_run,
            lanes: Some(input.allow),
            state_path: Some(&state_path),
            derived: input.derived,
        };
        let state = crate::dispatch::tick::tick_fallible(
            &queue,
            input.panes,
            &delivery,
            &unread,
            &mut send,
            &options,
        )?;
        let mut expired = expired.into_inner();
        expired.sort();
        expired.dedup();
        Ok(DispatchTickResult { state, expired })
    }

    fn allocation_store(&self) -> ControlResult<&dyn AllocationStore> {
        Ok(self)
    }

    fn allocation_last_result(&self) -> ControlResult<Value> {
        Ok(self.services()?.allocator.last_result())
    }

    fn require_turn_caller(&self, caller: Option<&str>) -> ControlResult<()> {
        let pane_pid = caller
            .and_then(|pane| PaneInfoEnv::get_pane(&self.terminal, pane))
            .and_then(|pane| pane.get("pane_pid").and_then(Value::as_i64));
        self.terminal
            .flock
            .require_turn_caller(caller, caller, pane_pid, self.caller().as_ref())
            .map_err(backend)
    }

    fn require_superuser(&self, caller: Option<&str>, action: &str) -> ControlResult<()> {
        self.terminal
            .flock
            .require_superuser(caller, action, self)
            .map_err(backend)
    }

    fn task_get(&self, task: &Value) -> ControlResult<Value> {
        let id = task.as_str().ok_or_else(|| ControlError::Python {
            kind: "TypeError",
            message: "task id must be a string".into(),
        })?;
        let task = self
            .runtime
            .block_on(self.tasks()?.get(id))
            .map_err(backend)?;
        // h_allocation only passes this row to authorize_task_complete, which keys it by id;
        // Tasks::get intentionally has no visibility projection, matching the Python store.
        Ok(json!({"id": task.id}))
    }

    fn authorize_task_complete(
        &self,
        task: &Value,
        caller: Option<&str>,
        operator: bool,
    ) -> ControlResult<()> {
        let id = task
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| ControlError::Python {
                kind: "KeyError",
                message: "'id'".into(),
            })?;
        let decorated = self
            .runtime
            .block_on(self.tasks()?.decorate(id, None))
            .map_err(backend)?;
        self.terminal
            .flock
            .authorize_task_complete(
                &decorated,
                caller,
                operator,
                &self.terminal,
                self.caller().as_ref(),
            )
            .map_err(backend)
    }

    fn allocation_reset_last_tick(&self) -> ControlResult<()> {
        self.services()?.allocator.reset_last_tick();
        Ok(())
    }

    fn next_job_wake(&self) -> ControlResult<()> {
        self.terminal.flock.wake_push();
        Ok(())
    }

    fn stop_all(&self) -> ControlResult<()> {
        self.services()?
            .stop_all_dunks()
            .map(|_| ())
            .map_err(backend)
    }
}

impl AllocationStore for RequestRuntimeAdapter<'_> {
    fn status(&self) -> ControlResult<Value> {
        self.runtime
            .block_on(self.tasks()?.allocation_status())
            .map_err(backend)
    }

    fn register(
        &self,
        pane: &Value,
        pools: &Value,
        roles: &Value,
        skills: &Value,
        caller: Option<&str>,
        enabled: &Value,
    ) -> ControlResult<Value> {
        let pane = pane.as_str().ok_or_else(|| ControlError::Python {
            kind: "TypeError",
            message: "pane id must be a string".into(),
        })?;
        let caller = caller.ok_or_else(|| {
            backend(DunkError::Refused(
                "registration requires a caller pane".into(),
            ))
        })?;
        self.runtime
            .block_on(
                self.tasks()?
                    .allocation_register(pane, pools, roles, skills, caller, enabled),
            )
            .map_err(backend)
    }

    fn offer(
        &self,
        task: &Value,
        pool: &Value,
        requires: &Value,
        caller: Option<&str>,
    ) -> ControlResult<Value> {
        let task = task.as_str().ok_or_else(|| ControlError::Python {
            kind: "TypeError",
            message: "task id must be a string".into(),
        })?;
        let pool = pool.as_str().ok_or_else(|| ControlError::Python {
            kind: "TypeError",
            message: "pool must be a string".into(),
        })?;
        let caller = caller
            .ok_or_else(|| backend(DunkError::Refused("offer requires a caller pane".into())))?;
        let task = self
            .runtime
            .block_on(self.tasks()?.allocation_offer(task, pool, requires, caller))
            .map_err(backend)?;
        self.runtime
            .block_on(self.tasks()?.get_full_wire(&task.id, None))
            .map(|task| task.wire)
            .map_err(backend)
    }

    fn review(
        &self,
        task: &Value,
        revision: &Value,
        body: &Value,
        pool: &Value,
        requires: &Value,
        caller: Option<&str>,
    ) -> ControlResult<Value> {
        fn text<'a>(value: &'a Value, field: &str) -> ControlResult<&'a str> {
            value.as_str().ok_or_else(|| ControlError::Python {
                kind: "TypeError",
                message: format!("{field} must be a string"),
            })
        }
        let task = text(task, "task id")?;
        let revision = text(revision, "revision")?;
        let body = text(body, "body")?;
        let pool = text(pool, "pool")?;
        let caller = caller
            .ok_or_else(|| backend(DunkError::Refused("review requires a caller pane".into())))?;
        let task = self
            .runtime
            .block_on(
                self.tasks()?
                    .allocation_review(task, revision, body, pool, requires, caller),
            )
            .map_err(backend)?;
        self.task_get(&json!(task.id))
    }
}

#[cfg(test)]
#[path = "runtime_control_tests.rs"]
mod tests;
