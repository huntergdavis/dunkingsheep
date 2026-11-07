//! The production `RecoverEnv` (t1605): `doctor` and `recover` over this request's real
//! terminal, the guard status from the captured startup environment, and the attached memory
//! service. `recover --apply` types through the backend's `send_text_and_enter`, the call
//! `Flock.recover` makes in Python; the producer re-checks the pane first and that stays its job.

use serde_json::{json, Value};

use super::RequestRuntimeAdapter;
use crate::error::{refusal, DunkError};
use crate::flock::mailbox::MailboxEnv;
use crate::flock::panes::PaneInfoEnv;
use crate::flock::recovery_commands::{
    type_name, RecoverEnv, RecoveryCommandError, RecoveryResult,
};
use crate::flock::truthy;

impl RequestRuntimeAdapter<'_> {
    /// This request's recovery providers. A detached adapter has no captured guard paths, so
    /// it refuses rather than reading the process environment.
    pub(super) fn recovery_env(&self) -> Result<&dyn RecoverEnv, DunkError> {
        if self.dunk_runtime.is_none() {
            return Err(refusal("request runtime services are not attached"));
        }
        Ok(self)
    }
}

/// A backend call with a non-string pane id: `subprocess` refuses the argument in Python.
fn backend_pane_id(value: &Value) -> RecoveryResult<&str> {
    value.as_str().ok_or_else(|| {
        python_exception(
            "TypeError",
            format!(
                "expected str, bytes or os.PathLike object, not {}",
                type_name(value)
            ),
        )
    })
}

fn python_exception(kind: &'static str, message: String) -> RecoveryCommandError {
    RecoveryCommandError::Python { kind, message }
}

impl RecoverEnv for RequestRuntimeAdapter<'_> {
    /// `self.herdr.list_panes()`: the backend's pane dicts; a backend failure is an empty list.
    fn list_panes(&self) -> RecoveryResult<Vec<Value>> {
        Ok(MailboxEnv::list_panes(&self.terminal))
    }

    /// `self.herdr.get_pane(pane_id)`.
    fn get_pane(&self, pane_id: &Value) -> RecoveryResult<Option<Value>> {
        let pane_id = backend_pane_id(pane_id)?;
        Ok(PaneInfoEnv::get_pane(&self.terminal, pane_id))
    }

    /// `self.herdr.read_pane(pane_id, lines=3, source="visible")`: None when the backend cannot
    /// read the pane. The terminal reads the newest `lines` lines rather than the visible
    /// screen; `doctor` only asks whether the read succeeded.
    fn read_visible(&self, pane_id: &Value, lines: usize) -> RecoveryResult<Option<Value>> {
        let pane_id = backend_pane_id(pane_id)?;
        let lines = i64::try_from(lines).unwrap_or(i64::MAX);
        let text = self
            .terminal
            .with_terminal(|terminal| terminal.read_pane(pane_id, lines, false));
        Ok(text.ok().flatten().map(Value::String))
    }

    /// `self.herdr.version()`: `text or None`.
    fn version(&self) -> RecoveryResult<Value> {
        let version = self.terminal.with_terminal(|terminal| terminal.version());
        Ok(if version.is_empty() {
            Value::Null
        } else {
            Value::String(version)
        })
    }

    /// `dunk_guard.status()` over the captured startup paths. `Flock._guard_status` turns the
    /// exception into `{"error": ...}`; the producer does that with this error's text.
    fn guard_status(&self) -> RecoveryResult<Value> {
        let services = self
            .dunk_runtime
            .ok_or_else(|| refusal("request runtime services are not attached"))?;
        crate::guard::status(&services.guard_paths)
            .map_err(|message| python_exception("OSError", message))
    }

    /// `self.memory is not None`.
    fn has_memory(&self) -> bool {
        self.terminal.flock.memory_service().is_some()
    }

    /// `self.memory.latest_session(harness, cwd)`: the newest indexed session row, or None.
    /// A falsy cwd is None before any query. sqlite3 binds a str as itself and a number or
    /// bool as its digits (the TEXT column's affinity converts the parameter); it refuses any
    /// other type.
    fn latest_session(&self, harness: &str, cwd: &Value) -> RecoveryResult<Option<Value>> {
        let service = self.terminal.flock.memory_service().ok_or_else(|| {
            python_exception(
                "AttributeError",
                "'NoneType' object has no attribute 'latest_session'".into(),
            )
        })?;
        if !truthy(cwd) {
            return Ok(None);
        }
        let cwd = match cwd {
            Value::String(text) => text.clone(),
            Value::Number(number) => number.to_string(),
            Value::Bool(_) => "1".to_string(),
            other => {
                return Err(python_exception(
                    "ProgrammingError",
                    format!(
                        "Error binding parameter 2: type '{}' is not supported",
                        type_name(other)
                    ),
                ))
            }
        };
        crate::memory::queries::latest_session(service.read_pool(), harness, &cwd).map_err(
            |error| match error {
                DunkError::Busy => RecoveryCommandError::Backend(DunkError::Busy),
                other => python_exception("OperationalError", other.to_string()),
            },
        )
    }

    /// `self.herdr.send_text_and_enter(pane_id, command)`: `(ok, detail)`.
    fn submit_command(&self, pane_id: &Value, command: &str) -> RecoveryResult<(Value, Value)> {
        let pane_id = backend_pane_id(pane_id)?;
        let outcome = {
            let mut terminal = self
                .terminal
                .terminal
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            terminal.send_text_and_enter(pane_id, command)
        };
        Ok(match outcome {
            Ok(()) => (json!(true), json!("sent")),
            Err(error) => (json!(false), json!(error.message)),
        })
    }
}

#[cfg(test)]
#[path = "recovery_env_tests.rs"]
mod tests;
