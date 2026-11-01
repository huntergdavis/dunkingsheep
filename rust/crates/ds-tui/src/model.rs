//! Connection and RPC state for the terminal UI.
//!
//! This module owns the state transitions used by the TUI. Transport creation and the
//! in-process fallback are supplied by the caller, which keeps these rules testable
//! without starting a daemon.

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::prelude::{Frame, Line, Text};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};
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

    /// Close resources owned by an in-process fallback. Daemon clients leave the
    /// shared service running when the TUI exits.
    fn close_local(&mut self) {}
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

/// Result of handling one key in the line-input modal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineModalResult {
    Pending,
    Cancelled,
    Submitted(String),
}

/// State and Ratatui rendering for the Python TUI's seven-row line input modal.
///
/// The value starts empty while the existing value is displayed as a prompt. Enter
/// submits the typed text, or the displayed default when the input is empty.
pub struct LineModal {
    label: String,
    default: String,
    value: String,
    cursor: usize,
}

impl LineModal {
    pub fn new(label: impl Into<String>, default: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            default: default.into(),
            value: String::new(),
            cursor: 0,
        }
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Apply one crossterm key, retaining the Python modal's ASCII input and cursor
    /// behavior. Non-printable and non-ASCII characters do not alter the value.
    pub fn handle_key(&mut self, key: KeyEvent) -> LineModalResult {
        match key.code {
            KeyCode::Enter => LineModalResult::Submitted(if self.value.is_empty() {
                self.default.clone()
            } else {
                self.value.clone()
            }),
            KeyCode::Esc => LineModalResult::Cancelled,
            KeyCode::Backspace => {
                if self.cursor > 0 {
                    let before = char_boundary(&self.value, self.cursor - 1);
                    let after = char_boundary(&self.value, self.cursor);
                    self.value.replace_range(before..after, "");
                    self.cursor -= 1;
                }
                LineModalResult::Pending
            }
            KeyCode::Delete => {
                if self.cursor < self.value.chars().count() {
                    let before = char_boundary(&self.value, self.cursor);
                    let after = char_boundary(&self.value, self.cursor + 1);
                    self.value.replace_range(before..after, "");
                }
                LineModalResult::Pending
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                LineModalResult::Pending
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.value.chars().count());
                LineModalResult::Pending
            }
            KeyCode::Home => {
                self.cursor = 0;
                LineModalResult::Pending
            }
            KeyCode::End => {
                self.cursor = self.value.chars().count();
                LineModalResult::Pending
            }
            KeyCode::Char(character)
                if (key.modifiers.is_empty()
                    || key.modifiers == crossterm::event::KeyModifiers::SHIFT)
                    && character.is_ascii()
                    && !character.is_control() =>
            {
                let offset = char_boundary(&self.value, self.cursor);
                self.value.insert(offset, character);
                self.cursor += 1;
                LineModalResult::Pending
            }
            _ => LineModalResult::Pending,
        }
    }

    /// Render a centered modal. Its rectangle is clipped to the terminal bounds, so
    /// small terminals remain drawable instead of requesting an oversized window.
    pub fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let desired_width = self
            .label
            .chars()
            .count()
            .saturating_add(18)
            .max(48)
            .min(usize::from(area.width.saturating_sub(4).max(20)));
        let width = u16::try_from(desired_width)
            .unwrap_or(u16::MAX)
            .min(area.width);
        let height = 7_u16.min(area.height);
        let modal = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );

        frame.render_widget(Clear, modal);
        let block = Block::default()
            .borders(Borders::ALL)
            .padding(Padding::horizontal(1));
        let inner = block.inner(modal);
        frame.render_widget(block, modal);
        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let input_width = usize::from(inner.width);
        let visible = self.visible_input(input_width);
        let lines = Text::from(vec![
            Line::from(self.label.as_str()),
            Line::from(format!("Current: {}", self.default)),
            Line::from("Enter keeps current, Esc cancels"),
            Line::from(""),
            Line::from(visible),
        ]);
        frame.render_widget(Paragraph::new(lines), inner);

        if inner.height >= 5 && inner.width > 0 {
            let cursor_offset =
                u16::try_from(self.cursor.min(input_width.saturating_sub(1))).unwrap_or(u16::MAX);
            let cursor_x = inner.x + cursor_offset;
            frame.set_cursor_position((cursor_x, inner.y + 4));
        }
    }

    fn visible_input(&self, width: usize) -> String {
        let mut characters: Vec<char> = self.value.chars().collect();
        if characters.len() > width {
            characters.drain(..characters.len() - width);
        }
        characters.into_iter().collect()
    }
}

fn char_boundary(value: &str, character_index: usize) -> usize {
    value
        .char_indices()
        .nth(character_index)
        .map_or(value.len(), |(offset, _)| offset)
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

    /// Release only an in-process fallback; daemon-owned dunks outlive the TUI.
    pub fn shutdown(&mut self) {
        if self.mode == ClientMode::Local {
            self.client.close_local();
        }
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
    use crossterm::event::KeyModifiers;
    use ratatui::{backend::TestBackend, Terminal};
    use std::collections::VecDeque;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn modal_snapshot(modal: &LineModal, width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| modal.render(frame, frame.area()))
            .expect("draw modal");
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                let mut line = String::new();
                for x in 0..width {
                    line.push_str(buffer[(x, y)].symbol());
                }
                line.trim_end().to_owned()
            })
            .collect()
    }

    fn bordered_line(content: &str, inner_width: usize) -> String {
        format!("  │ {content:<inner_width$} │")
    }

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

    #[test]
    fn line_modal_initial_value_snapshot_and_empty_submit_use_default() {
        let mut modal = LineModal::new("Name", "current");
        assert_eq!(
            modal_snapshot(&modal, 36, 11),
            vec![
                "".into(),
                "".into(),
                format!("  ┌{}┐", "─".repeat(30)),
                bordered_line("Name", 28),
                bordered_line("Current: current", 28),
                bordered_line("Enter keeps current, Esc can", 28),
                bordered_line("", 28),
                bordered_line("", 28),
                format!("  └{}┘", "─".repeat(30)),
                "".into(),
                "".into(),
            ]
        );
        assert_eq!(
            modal.handle_key(key(KeyCode::Enter)),
            LineModalResult::Submitted("current".into())
        );
    }

    #[test]
    fn line_modal_escape_cancels_without_changing_the_prompt_snapshot() {
        let mut modal = LineModal::new("Interval minutes", "5");
        let before = modal_snapshot(&modal, 36, 11);
        assert_eq!(
            modal.handle_key(key(KeyCode::Esc)),
            LineModalResult::Cancelled
        );
        assert_eq!(modal_snapshot(&modal, 36, 11), before);
    }

    #[test]
    fn line_modal_submit_snapshot_contains_typed_value() {
        let mut modal = LineModal::new("Name", "default");
        assert_eq!(
            modal.handle_key(key(KeyCode::Char('x'))),
            LineModalResult::Pending
        );
        let snapshot = modal_snapshot(&modal, 36, 11);
        assert_eq!(snapshot[7], bordered_line("x", 28));
        assert_eq!(
            modal.handle_key(key(KeyCode::Enter)),
            LineModalResult::Submitted("x".into())
        );
    }

    #[test]
    fn line_modal_narrow_terminal_snapshot_is_clipped_and_safe() {
        let modal = LineModal::new("Very Long Label", "long default");
        assert_eq!(
            modal_snapshot(&modal, 12, 5),
            vec![
                format!("┌{}┐", "─".repeat(10)),
                "│ Very Lon │".into(),
                "│ Current: │".into(),
                "│ Enter ke │".into(),
                format!("└{}┘", "─".repeat(10)),
            ]
        );
    }

    #[test]
    fn line_modal_accepts_shifted_uppercase_and_printable_ascii_keys() {
        let mut modal = LineModal::new("Name", "current");
        assert_eq!(
            modal.handle_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::SHIFT,)),
            LineModalResult::Pending
        );
        assert_eq!(modal.value(), "A");
        assert_eq!(
            modal.handle_key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::SHIFT,)),
            LineModalResult::Pending
        );
        assert_eq!(modal.value(), "A!");

        for modifiers in [
            KeyModifiers::CONTROL,
            KeyModifiers::ALT,
            KeyModifiers::SUPER,
            KeyModifiers::SHIFT | KeyModifiers::CONTROL,
        ] {
            assert_eq!(
                modal.handle_key(KeyEvent::new(KeyCode::Char('x'), modifiers)),
                LineModalResult::Pending
            );
        }
        assert_eq!(modal.value(), "A!");
    }
}
