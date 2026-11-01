//! Connection and RPC state for the terminal UI.
//!
//! This module owns the state transitions used by the TUI. Transport creation and the
//! in-process fallback are supplied by the caller, which keeps these rules testable
//! without starting a daemon.

use std::time::Duration;

use serde_json::Value;

/// Retry interval used by `DunkingSheepTui.connect` in the Python client.
pub const RECONNECT_INTERVAL: Duration = Duration::from_secs(2);

/// The UI-facing distinction between a normal command refusal and a transport error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcError {
    Refused(String),
    Transport(String),
}

/// The minimum client interface needed by the TUI state machine.
pub trait RpcClient {
    fn call(&mut self, command: &str, args: Value) -> Result<Value, RpcError>;
}

/// Which backend currently serves requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientMode {
    Daemon,
    Local,
}

/// Active collection shown by the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Dunks,
    Tasks,
}

/// Mutable UI model for connection status, flashes, and the last connection attempt.
pub struct TuiModel<C> {
    pub client: C,
    pub mode: ClientMode,
    pub last_connect_attempt: f64,
    pub message: String,
    pub message_until: f64,
    pub dunks: Vec<Value>,
    pub selected: usize,
    pub global_status: String,
    pub view: View,
    pub tasks: Vec<Value>,
    pub task_selected: usize,
}

impl<C: RpcClient> TuiModel<C> {
    /// Record a successful daemon connection.
    pub fn connected(client: C, now: f64) -> Self {
        Self {
            client,
            mode: ClientMode::Daemon,
            last_connect_attempt: now,
            message: String::new(),
            message_until: 0.0,
            dunks: Vec::new(),
            selected: 0,
            global_status: String::new(),
            view: View::Dunks,
            tasks: Vec::new(),
            task_selected: 0,
        }
    }

    /// Record daemon connection failure and select a caller-provided local fallback.
    pub fn local_fallback(client: C, now: f64, error: impl std::fmt::Display) -> Self {
        let mut model = Self {
            client,
            mode: ClientMode::Local,
            last_connect_attempt: now,
            message: String::new(),
            message_until: 0.0,
            dunks: Vec::new(),
            selected: 0,
            global_status: String::new(),
            view: View::Dunks,
            tasks: Vec::new(),
            task_selected: 0,
        };
        model.flash(
            format!("Daemon unavailable ({error}); running in-process"),
            now,
            4.0,
        );
        model
    }

    /// Perform an RPC, preserving refusal text and translating transport failures to
    /// the Python TUI's `Daemon error: ...` flash. Reconnect is requested only after
    /// the two second threshold and only while using the daemon.
    pub fn rpc(&mut self, command: &str, args: Value, now: f64) -> (Option<Value>, bool) {
        match self.client.call(command, args) {
            Ok(value) => (Some(value), false),
            Err(RpcError::Refused(error)) => {
                self.flash(error, now, 4.0);
                (None, false)
            }
            Err(RpcError::Transport(error)) => {
                self.flash(format!("Daemon error: {error}"), now, 4.0);
                let reconnect = self.mode == ClientMode::Daemon
                    && now - self.last_connect_attempt > RECONNECT_INTERVAL.as_secs_f64();
                (None, reconnect)
            }
        }
    }

    /// Refresh dunks and, when the task view is active, refresh tasks afterward.
    /// Failed RPCs leave the last successful collection intact, matching the Python
    /// UI's early return behavior.
    pub fn refresh(&mut self, now: f64) -> bool {
        let (result, reconnect) = self.rpc("list_dunks", Value::Null, now);
        let Some(result) = result else {
            return reconnect;
        };
        self.dunks = result
            .get("dunks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        self.selected = self.selected.min(self.dunks.len().saturating_sub(1));

        let running = self
            .dunks
            .iter()
            .filter(|dunk| dunk.get("running").is_some_and(python_truthy))
            .count();
        let count = self.dunks.len();
        let mut label = format!("{count} dunker{}", if count != 1 { "s" } else { "" });
        if running > 0 {
            label.push_str(&format!(" ({running} running)"));
        }
        if self.mode == ClientMode::Local {
            label.push_str("  [in-process, not shared]");
        }
        self.global_status = label;

        if self.view == View::Tasks {
            return self.refresh_tasks(now);
        }
        false
    }

    /// Fetch task rows and replace the dunk summary with the task count summary.
    pub fn refresh_tasks(&mut self, now: f64) -> bool {
        let args = serde_json::json!({"include_done": true, "limit": 500});
        let (result, reconnect) = self.rpc("task_list", args, now);
        let Some(result) = result else {
            return reconnect;
        };
        self.tasks = result
            .get("tasks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        self.task_selected = self.task_selected.min(self.tasks.len().saturating_sub(1));
        let open_count = self
            .tasks
            .iter()
            .filter(|task| {
                matches!(
                    task.get("status").and_then(Value::as_str),
                    Some("open" | "in_progress")
                )
            })
            .count();
        self.global_status = format!("{} tasks ({open_count} open/in-progress)", self.tasks.len());
        false
    }

    pub fn flash(&mut self, text: impl Into<String>, now: f64, seconds: f64) {
        self.message = text.into();
        self.message_until = now + seconds;
    }
}

fn python_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[derive(Default)]
    struct FakeClient {
        responses: VecDeque<Result<Value, RpcError>>,
        calls: Vec<(String, Value)>,
    }

    impl RpcClient for FakeClient {
        fn call(&mut self, command: &str, args: Value) -> Result<Value, RpcError> {
            self.calls.push((command.to_owned(), args));
            self.responses.pop_front().expect("fake response")
        }
    }

    #[test]
    fn unavailable_daemon_selects_local_fallback_and_flashes_reason() {
        let model = TuiModel::local_fallback(FakeClient::default(), 10.0, "socket missing");
        assert_eq!(model.mode, ClientMode::Local);
        assert_eq!(
            model.message,
            "Daemon unavailable (socket missing); running in-process"
        );
        assert_eq!(model.message_until, 14.0);
    }

    #[test]
    fn refused_rpc_flashes_error_without_reconnect() {
        let mut model = TuiModel::connected(
            FakeClient {
                responses: VecDeque::from([Err(RpcError::Refused("no permission".into()))]),
                ..FakeClient::default()
            },
            1.0,
        );
        let (result, reconnect) = model.rpc("list_dunks", Value::Null, 20.0);
        assert_eq!(result, None);
        assert!(!reconnect);
        assert_eq!(model.message, "no permission");
    }

    #[test]
    fn transport_reconnect_uses_strict_two_second_threshold() {
        let fake = || FakeClient {
            responses: VecDeque::from([Err(RpcError::Transport("gone".into()))]),
            ..FakeClient::default()
        };
        let mut at_threshold = TuiModel::connected(fake(), 1.0);
        assert!(!at_threshold.rpc("list_dunks", Value::Null, 3.0).1);
        let mut beyond_threshold = TuiModel::connected(fake(), 1.0);
        assert!(beyond_threshold.rpc("list_dunks", Value::Null, 3.001).1);
    }

    #[test]
    fn refresh_pins_dunks_clamps_selection_and_only_fetches_tasks_in_task_view() {
        let client = FakeClient {
            responses: VecDeque::from([
                Ok(serde_json::json!({"dunks": [
                    {"id": "d1", "running": true},
                    {"id": "d2", "running": false},
                ]})),
                Ok(serde_json::json!({"dunks": [
                    {"id": "d1", "running": true},
                    {"id": "d2", "running": false},
                ]})),
                Ok(serde_json::json!({"tasks": [
                    {"id": "t1", "status": "open"},
                    {"id": "t2", "status": "done"},
                    {"id": "t3", "status": "in_progress"},
                ]})),
            ]),
            ..FakeClient::default()
        };
        let mut model = TuiModel::connected(client, 0.0);
        model.selected = 9;
        model.refresh(1.0);
        assert_eq!(
            model.dunks,
            vec![
                serde_json::json!({"id": "d1", "running": true}),
                serde_json::json!({"id": "d2", "running": false}),
            ]
        );
        assert_eq!(model.selected, 1);
        assert_eq!(model.global_status, "2 dunkers (1 running)");
        assert_eq!(model.client.calls.len(), 1);
        assert_eq!(model.client.calls[0].0, "list_dunks");

        model.view = View::Tasks;
        model.task_selected = 9;
        model.refresh(2.0);
        assert_eq!(
            model.tasks,
            vec![
                serde_json::json!({"id": "t1", "status": "open"}),
                serde_json::json!({"id": "t2", "status": "done"}),
                serde_json::json!({"id": "t3", "status": "in_progress"}),
            ]
        );
        assert_eq!(model.task_selected, 2);
        assert_eq!(model.global_status, "3 tasks (2 open/in-progress)");
        assert_eq!(model.client.calls.len(), 3);
        assert_eq!(model.client.calls[1], ("list_dunks".into(), Value::Null));
        assert_eq!(
            model.client.calls[2],
            (
                "task_list".into(),
                serde_json::json!({"include_done": true, "limit": 500})
            )
        );
    }

    #[test]
    fn refresh_empty_result_clamps_selection_to_zero_and_formats_singular_count() {
        let client = FakeClient {
            responses: VecDeque::from([Ok(serde_json::json!({"dunks": []}))]),
            ..FakeClient::default()
        };
        let mut model = TuiModel::connected(client, 0.0);
        model.selected = 3;
        model.refresh(1.0);
        assert_eq!(model.selected, 0);
        assert_eq!(model.global_status, "0 dunkers");
    }

    #[test]
    fn refresh_propagates_reconnect_request_for_list_dunks_transport_error() {
        let client = FakeClient {
            responses: VecDeque::from([Err(RpcError::Transport("gone".into()))]),
            ..FakeClient::default()
        };
        let mut model = TuiModel::connected(client, 1.0);

        assert!(model.refresh(3.001));
        assert_eq!(model.message, "Daemon error: gone");
        assert_eq!(model.client.calls[0].0, "list_dunks");
    }

    #[test]
    fn refresh_propagates_reconnect_request_for_task_list_transport_error() {
        let client = FakeClient {
            responses: VecDeque::from([
                Ok(serde_json::json!({"dunks": []})),
                Err(RpcError::Transport("gone".into())),
            ]),
            ..FakeClient::default()
        };
        let mut model = TuiModel::connected(client, 1.0);
        model.view = View::Tasks;

        assert!(model.refresh(3.001));
        assert_eq!(model.message, "Daemon error: gone");
        assert_eq!(model.client.calls[0].0, "list_dunks");
        assert_eq!(model.client.calls[1].0, "task_list");
    }
}
