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

/// Mutable UI model for connection status, flashes, and the last connection attempt.
pub struct TuiModel<C> {
    pub client: C,
    pub mode: ClientMode,
    pub last_connect_attempt: f64,
    pub message: String,
    pub message_until: f64,
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

    pub fn flash(&mut self, text: impl Into<String>, now: f64, seconds: f64) {
        self.message = text.into();
        self.message_until = now + seconds;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct FakeClient(VecDeque<Result<Value, RpcError>>);

    impl RpcClient for FakeClient {
        fn call(&mut self, _: &str, _: Value) -> Result<Value, RpcError> {
            self.0.pop_front().expect("fake response")
        }
    }

    #[test]
    fn unavailable_daemon_selects_local_fallback_and_flashes_reason() {
        let model = TuiModel::local_fallback(FakeClient(VecDeque::new()), 10.0, "socket missing");
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
            FakeClient(VecDeque::from([Err(RpcError::Refused(
                "no permission".into(),
            ))])),
            1.0,
        );
        let (result, reconnect) = model.rpc("list_dunks", Value::Null, 20.0);
        assert_eq!(result, None);
        assert!(!reconnect);
        assert_eq!(model.message, "no permission");
    }

    #[test]
    fn transport_reconnect_uses_strict_two_second_threshold() {
        let fake = || FakeClient(VecDeque::from([Err(RpcError::Transport("gone".into()))]));
        let mut at_threshold = TuiModel::connected(fake(), 1.0);
        assert!(!at_threshold.rpc("list_dunks", Value::Null, 3.0).1);
        let mut beyond_threshold = TuiModel::connected(fake(), 1.0);
        assert!(beyond_threshold.rpc("list_dunks", Value::Null, 3.001).1);
    }
}
