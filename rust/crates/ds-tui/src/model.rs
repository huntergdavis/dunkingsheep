//! Connection and RPC state for the terminal UI.
//!
//! This module owns the state transitions used by the TUI. Transport creation and the
//! in-process fallback are supplied by the caller, which keeps these rules testable
//! without starting a daemon.

use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::prelude::{Frame, Line, Text};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Clear, Padding, Paragraph};
use serde_json::Value;

use crate::text_editor::TextEditorBuffer;

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

/// Supplies the result of the line modal for model actions that prompt the user.
/// Returning `None` represents Escape; a submitted value is returned as entered,
/// with the modal adapter responsible for applying its displayed default.
pub trait LineModalProvider {
    fn line_modal(&mut self, label: &str, default: &str) -> Option<String>;
}

/// Supplies a selected pane for model actions that open the target picker.
pub trait TargetModalProvider {
    fn target_modal(&mut self, panes: &[Value]) -> Option<Value>;
}

/// Supplies submitted or cancelled multiline text from the full-screen editor.
pub trait TextModalProvider {
    fn text_modal(&mut self, initial_text: &str) -> Option<String>;
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

/// Follow-up requested by the key dispatcher for behavior implemented in later
/// TUI leaves. Returning these keeps key ownership here without duplicating the
/// modal and target-picker implementations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyDispatch {
    Handled,
    Ignored,
    Quit,
    QuitAndShutdown,
    TaskView(KeyEvent),
    PickTarget,
    EditInterval,
    EditText,
}

/// Result of handling one key in the line-input modal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineModalResult {
    Pending,
    Cancelled,
    Submitted(String),
}

/// Result of handling one key in the multiline text modal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextModalResult {
    Pending,
    Cancelled,
    Submitted(String),
}

/// Result of handling one key in the pane target picker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetPickerResult {
    Pending,
    Cancelled,
    Selected(Value),
}

/// Result of one task-view key dispatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKeyDispatch {
    Handled,
    Ignored,
    Quit,
    QuitAndShutdown,
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

/// Full-screen editor state used by the multiline text modal.
pub struct TextModal {
    editor: TextEditorBuffer,
    top_row: usize,
    left_col: usize,
}

impl TextModal {
    pub fn new(initial_text: &str) -> Self {
        Self {
            editor: TextEditorBuffer::new(initial_text),
            top_row: 0,
            left_col: 0,
        }
    }

    pub fn editor(&self) -> &TextEditorBuffer {
        &self.editor
    }

    /// Match the Python editor's Ctrl+G save, Escape cancel, newline, tab-as-space,
    /// editing keys, control-key aliases, and printable Unicode input.
    pub fn handle_key(&mut self, key: KeyEvent, viewport_height: usize) -> TextModalResult {
        let control = key
            .modifiers
            .contains(crossterm::event::KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc | KeyCode::Char('\u{1b}') => TextModalResult::Cancelled,
            KeyCode::Char('\u{7}') | KeyCode::Char('g') if control => {
                TextModalResult::Submitted(self.editor.text())
            }
            KeyCode::Char('\u{7}') => TextModalResult::Submitted(self.editor.text()),
            KeyCode::Enter | KeyCode::Char('\n' | '\r') => {
                self.editor.newline();
                TextModalResult::Pending
            }
            KeyCode::Tab | KeyCode::Char('\t') => {
                self.editor.insert(" ");
                TextModalResult::Pending
            }
            KeyCode::Backspace | KeyCode::Char('\u{7f}' | '\u{8}') => {
                self.editor.backspace();
                TextModalResult::Pending
            }
            KeyCode::Delete => {
                self.editor.delete();
                TextModalResult::Pending
            }
            KeyCode::Left | KeyCode::Char('b') if control => {
                self.editor.move_left();
                TextModalResult::Pending
            }
            KeyCode::Left => {
                self.editor.move_left();
                TextModalResult::Pending
            }
            KeyCode::Right | KeyCode::Char('f') if control => {
                self.editor.move_right();
                TextModalResult::Pending
            }
            KeyCode::Right => {
                self.editor.move_right();
                TextModalResult::Pending
            }
            KeyCode::Up | KeyCode::Char('p') if control => {
                self.editor.move_up(1);
                TextModalResult::Pending
            }
            KeyCode::Up => {
                self.editor.move_up(1);
                TextModalResult::Pending
            }
            KeyCode::Down | KeyCode::Char('n') if control => {
                self.editor.move_down(1);
                TextModalResult::Pending
            }
            KeyCode::Down => {
                self.editor.move_down(1);
                TextModalResult::Pending
            }
            KeyCode::Home | KeyCode::Char('a') if control => {
                self.editor.move_home();
                TextModalResult::Pending
            }
            KeyCode::Home => {
                self.editor.move_home();
                TextModalResult::Pending
            }
            KeyCode::End | KeyCode::Char('e') if control => {
                self.editor.move_end();
                TextModalResult::Pending
            }
            KeyCode::End => {
                self.editor.move_end();
                TextModalResult::Pending
            }
            KeyCode::PageUp => {
                self.editor.move_up(viewport_height.max(1) as i64);
                TextModalResult::Pending
            }
            KeyCode::PageDown => {
                self.editor.move_down(viewport_height.max(1) as i64);
                TextModalResult::Pending
            }
            KeyCode::Char(character)
                if !control
                    && !key.modifiers.intersects(
                        crossterm::event::KeyModifiers::ALT | crossterm::event::KeyModifiers::SUPER,
                    )
                    && !character.is_control() =>
            {
                self.editor.insert(&character.to_string());
                TextModalResult::Pending
            }
            _ => TextModalResult::Pending,
        }
    }

    /// Render the editor in a bounded full-screen frame. Logical text lines never
    /// wrap or truncate; scrolling only changes which cells are visible.
    pub fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        frame.render_widget(Clear, area);
        frame.render_widget(Block::default().borders(Borders::ALL), area);
        let inner = Rect::new(
            area.x.saturating_add(1),
            area.y.saturating_add(1),
            area.width.saturating_sub(2),
            area.height.saturating_sub(2),
        );
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let text_height = usize::from(inner.height.saturating_sub(2)).max(1);
        let text_width = usize::from(inner.width.saturating_sub(2)).max(1);
        let visible_width = text_width.saturating_sub(1).max(1);
        if self.editor.row < self.top_row {
            self.top_row = self.editor.row;
        } else if self.editor.row >= self.top_row.saturating_add(text_height) {
            self.top_row = self.editor.row - text_height + 1;
        }
        if self.editor.col < self.left_col {
            self.left_col = self.editor.col;
        } else if self.editor.col >= self.left_col.saturating_add(visible_width) {
            self.left_col = self.editor.col - visible_width + 1;
        }

        let location = format!(
            "Ctrl+G saves, Esc cancels  Line {}/{}  Col {}  Placeholders: {{id}} {{name}} {{count}} {{target}} {{time}}",
            self.editor.row + 1,
            self.editor.lines.len(),
            self.editor.col + 1
        );
        let mut lines = vec![Line::from("Edit text to send"), Line::from(location)];
        for row in self.top_row..self.top_row.saturating_add(text_height) {
            let line = self.editor.lines.get(row).map_or("", String::as_str);
            let visible = line
                .chars()
                .skip(self.left_col)
                .take(visible_width)
                .map(|character| if character == '\t' { ' ' } else { character })
                .collect::<String>();
            lines.push(Line::from(visible));
        }
        let paragraph_area = Rect::new(
            inner.x.saturating_add(1),
            inner.y,
            inner.width.saturating_sub(2),
            inner.height,
        );
        frame.render_widget(Paragraph::new(Text::from(lines)), paragraph_area);
        let cursor_x = inner.x.saturating_add(usize_to_u16(
            1 + self.editor.col.saturating_sub(self.left_col),
        ));
        let cursor_y = inner
            .y
            .saturating_add(2 + usize_to_u16(self.editor.row.saturating_sub(self.top_row)));
        if cursor_y < area.y.saturating_add(area.height) {
            frame.set_cursor_position((
                cursor_x.min(area.x.saturating_add(area.width.saturating_sub(1))),
                cursor_y,
            ));
        }
    }
}

/// Interactive terminal-pane picker grouped under workspace headings.
pub struct TargetPicker {
    panes: Vec<Value>,
    selected: usize,
}

impl TargetPicker {
    pub fn new(panes: Vec<Value>) -> Self {
        let selected = panes
            .iter()
            .position(|pane| python_truthy(pane.get("agent").unwrap_or(&Value::Null)))
            .unwrap_or(0);
        Self { panes, selected }
    }

    pub fn selected(&self) -> Option<&Value> {
        self.panes.get(self.selected)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> TargetPickerResult {
        match key.code {
            KeyCode::Esc => TargetPickerResult::Cancelled,
            KeyCode::Enter => self
                .selected()
                .cloned()
                .map_or(TargetPickerResult::Cancelled, TargetPickerResult::Selected),
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                TargetPickerResult::Pending
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = self
                    .selected
                    .saturating_add(1)
                    .min(self.panes.len().saturating_sub(1));
                TargetPickerResult::Pending
            }
            _ => TargetPickerResult::Pending,
        }
    }

    pub fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        frame.render_widget(Clear, area);
        frame.render_widget(Block::default().borders(Borders::ALL), area);
        // Python: text at column 2 clipped to w - 4; tab rows at column 2 + indent clipped
        // to w - 4 - indent. Titles and headers are bold; the selected pane is reversed.
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let text_width = area.width.saturating_sub(4);
        let row_column = MODAL_COLUMN + TARGET_INDENT;
        let row_width = area.width.saturating_sub(4 + TARGET_INDENT);
        draw_modal_cells(
            frame,
            area,
            1,
            MODAL_COLUMN,
            text_width,
            "Select target pane",
            bold,
        );
        draw_modal_cells(
            frame,
            area,
            2,
            MODAL_COLUMN,
            text_width,
            "j/k or arrows move, Enter selects, Esc cancels",
            Style::default(),
        );
        let header = crate::rows::format_target_row(
            "Tab",
            "Agent",
            "Status",
            "Directory",
            i64::from(area.width),
            i64::from(TARGET_INDENT),
        );
        draw_modal_cells(frame, area, 3, row_column, row_width, &header, bold);

        let list_top = 5_usize;
        let visible = usize::from(area.height.saturating_sub(7)).max(1);
        let display = self.display_rows();
        let selected_row = display
            .iter()
            .position(|row| matches!(row, TargetDisplayRow::Pane(index) if *index == self.selected))
            .unwrap_or(0);
        let start = selected_row.saturating_sub(visible.saturating_sub(1));
        for (screen_index, row) in display.iter().skip(start).take(visible).enumerate() {
            let y = list_top + screen_index;
            match row {
                TargetDisplayRow::Workspace(name) => draw_modal_cells(
                    frame,
                    area,
                    usize_to_u16(y),
                    MODAL_COLUMN,
                    text_width,
                    name,
                    bold,
                ),
                TargetDisplayRow::Pane(index) => {
                    let pane = &self.panes[*index];
                    let tab = pane
                        .get("tab_label")
                        .or_else(|| pane.get("tab_id"))
                        .filter(|value| python_truthy(value))
                        .map(python_string)
                        .unwrap_or_else(|| "?".to_owned());
                    let tab = if python_truthy(pane.get("is_self").unwrap_or(&Value::Null)) {
                        format!("{tab} (this)")
                    } else {
                        tab
                    };
                    let row = crate::rows::format_target_row(
                        &tab,
                        &target_string(pane.get("agent")),
                        &target_string(pane.get("agent_status")),
                        &target_string(pane.get("cwd")),
                        i64::from(area.width),
                        i64::from(TARGET_INDENT),
                    );
                    let style = if *index == self.selected {
                        Style::default().add_modifier(Modifier::REVERSED)
                    } else {
                        Style::default()
                    };
                    draw_modal_cells(
                        frame,
                        area,
                        usize_to_u16(y),
                        row_column,
                        row_width,
                        &row,
                        style,
                    );
                }
            }
        }
    }

    fn display_rows(&self) -> Vec<TargetDisplayRow> {
        let mut rows = Vec::new();
        let mut last_workspace: Option<String> = None;
        for (index, pane) in self.panes.iter().enumerate() {
            let workspace = pane
                .get("workspace_label")
                .or_else(|| pane.get("workspace_id"))
                .filter(|value| python_truthy(value))
                .map(python_string)
                .unwrap_or_else(|| "?".to_owned());
            if last_workspace.as_deref() != Some(workspace.as_str()) {
                rows.push(TargetDisplayRow::Workspace(workspace.clone()));
                last_workspace = Some(workspace);
            }
            rows.push(TargetDisplayRow::Pane(index));
        }
        rows
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TargetDisplayRow {
    Workspace(String),
    Pane(usize),
}

fn target_string(value: Option<&Value>) -> String {
    value
        .filter(|value| python_truthy(value))
        .map(python_string)
        .unwrap_or_else(|| "-".to_owned())
}

/// Column where the Python modal text starts: the border is column 0 and `safe_addstr`
/// is called with x = 2.
const MODAL_COLUMN: u16 = 2;

/// Indent of the target-picker tab rows beneath the text column (Python `TARGET_INDENT`).
const TARGET_INDENT: u16 = 4;

/// Draw one modal line the way Python's `safe_addstr` does: at `(row, column)`, at most
/// `width` cells, `...` when the text overflows, and the style covers the whole width
/// (Python pads the line with spaces, so reverse video spans the row).
fn draw_modal_cells(
    frame: &mut Frame<'_>,
    area: Rect,
    row: u16,
    column: u16,
    width: u16,
    text: &str,
    style: Style,
) {
    if row >= area.height {
        return;
    }
    let x = area.x.saturating_add(column);
    let right = area.x.saturating_add(area.width);
    if x >= right {
        return;
    }
    // Python caps the usable width at max_x - x - 1, keeping the last column free.
    let width = width.min(right.saturating_sub(x).saturating_sub(1));
    if width == 0 {
        return;
    }
    let text = crate::rows::clip(text, i64::from(width));
    frame.render_widget(
        Paragraph::new(text).style(style),
        Rect::new(x, area.y.saturating_add(row), width, 1),
    );
}

fn usize_to_u16(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
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

    /// Return the currently selected dunk, if the selection is in range.
    pub fn current(&self) -> Option<&Value> {
        self.dunks.get(self.selected)
    }

    /// Return the selected dunk's id, flashing when there is no selected dunk.
    pub fn current_id(&mut self, now: f64) -> Option<&str> {
        if self.current().is_none() {
            self.flash("No dunker selected - press a", now, 4.0);
            return None;
        }
        self.current()
            .and_then(|dunk| dunk.get("id"))
            .and_then(Value::as_str)
    }

    /// Return the currently selected task, flashing when the task list is empty.
    pub fn current_task(&mut self, now: f64) -> Option<&Value> {
        if self.tasks.is_empty() {
            self.flash("No task selected", now, 4.0);
            return None;
        }
        self.tasks.get(self.task_selected)
    }

    /// List available terminal panes and update the selected dunk's target after
    /// a caller-provided modal chooses one.
    pub fn pick_target<M: TargetModalProvider>(&mut self, modal: &mut M, now: f64) -> bool {
        let Some(dunk) = self.current().cloned() else {
            self.current_id(now);
            return false;
        };
        let id = dunk.get("id").cloned().unwrap_or(Value::Null);
        let (result, reconnect) = self.rpc("list_panes", Value::Null, now);
        let panes = result
            .as_ref()
            .and_then(|result| result.get("panes"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if panes.is_empty() {
            let (status, status_reconnect) = self.rpc("status", Value::Null, now);
            let hint = status
                .as_ref()
                .and_then(|status| status.get("herdr_hint"))
                .filter(|hint| python_truthy(hint))
                .map(python_string)
                .unwrap_or_else(|| "No panes (is the backend reachable?)".into());
            self.flash(hint, now, 4.0);
            return reconnect || status_reconnect;
        }
        let Some(chosen) = modal.target_modal(&panes) else {
            return reconnect;
        };
        let target = chosen.get("pane_id").cloned().unwrap_or(Value::Null);
        self.rpc(
            "update_dunk",
            serde_json::json!({"id": id, "target": target}),
            now,
        )
        .1
    }

    /// Dispatch every task-view key from the Python TUI through fakeable modal and
    /// RPC interfaces. Modifier filtering mirrors the dunk-view dispatcher.
    pub fn handle_task_key<M: LineModalProvider>(
        &mut self,
        key: KeyEvent,
        modal: &mut M,
        now: f64,
    ) -> TaskKeyDispatch {
        let plain_or_shift =
            key.modifiers.is_empty() || key.modifiers == crossterm::event::KeyModifiers::SHIFT;
        if !plain_or_shift {
            return TaskKeyDispatch::Ignored;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => TaskKeyDispatch::Quit,
            KeyCode::Char('Q') => {
                self.quit_and_shutdown(now);
                TaskKeyDispatch::QuitAndShutdown
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.task_selected = self.task_selected.saturating_sub(1);
                TaskKeyDispatch::Handled
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.task_selected = self
                    .task_selected
                    .saturating_add(1)
                    .min(self.tasks.len().saturating_sub(1));
                TaskKeyDispatch::Handled
            }
            KeyCode::Char(' ') | KeyCode::Char('c') => {
                if let Some(task) = self.current_task(now).cloned() {
                    let id = task.get("id").cloned().unwrap_or(Value::Null);
                    self.rpc("task_complete", serde_json::json!({"id": id}), now);
                }
                TaskKeyDispatch::Handled
            }
            KeyCode::Char('s') => {
                if let Some(task) = self.current_task(now).cloned() {
                    let id = task.get("id").cloned().unwrap_or(Value::Null);
                    self.rpc(
                        "task_update",
                        serde_json::json!({"id": id, "status": "in_progress"}),
                        now,
                    );
                }
                TaskKeyDispatch::Handled
            }
            KeyCode::Char('b') => {
                if let Some(task) = self.current_task(now).cloned() {
                    let default = task
                        .get("block_reason")
                        .filter(|value| python_truthy(value))
                        .map(python_string)
                        .unwrap_or_default();
                    if let Some(reason) = modal.line_modal("Why is it blocked?", &default) {
                        let reason = reason.trim();
                        let id = task.get("id").cloned().unwrap_or(Value::Null);
                        self.rpc(
                            "task_block",
                            serde_json::json!({
                                "id": id,
                                "reason": if reason.is_empty() { Value::Null } else { Value::String(reason.into()) }
                            }),
                            now,
                        );
                    }
                }
                TaskKeyDispatch::Handled
            }
            KeyCode::Char('u') => {
                if let Some(task) = self.current_task(now).cloned() {
                    let id = task.get("id").cloned().unwrap_or(Value::Null);
                    self.rpc("task_unblock", serde_json::json!({"id": id}), now);
                }
                TaskKeyDispatch::Handled
            }
            KeyCode::Char('p') => {
                if let Some(task) = self.current_task(now).cloned() {
                    const PRIORITIES: [&str; 4] = ["low", "medium", "high", "urgent"];
                    let current = task
                        .get("priority")
                        .and_then(Value::as_str)
                        .filter(|priority| !priority.is_empty())
                        .unwrap_or("low");
                    if let Some(index) = PRIORITIES.iter().position(|priority| *priority == current)
                    {
                        let priority = PRIORITIES[(index + 1) % PRIORITIES.len()];
                        let id = task.get("id").cloned().unwrap_or(Value::Null);
                        self.rpc(
                            "task_update",
                            serde_json::json!({"id": id, "priority": priority}),
                            now,
                        );
                    }
                }
                TaskKeyDispatch::Handled
            }
            KeyCode::Char('d') => {
                if let Some(task) = self.current_task(now).cloned() {
                    let id = task.get("id").cloned().unwrap_or(Value::Null);
                    self.rpc("task_delete", serde_json::json!({"id": id}), now);
                }
                TaskKeyDispatch::Handled
            }
            KeyCode::Char('e') => {
                if let Some(task) = self.current_task(now).cloned() {
                    let default = task
                        .get("title")
                        .filter(|value| python_truthy(value))
                        .map(python_string)
                        .unwrap_or_default();
                    if let Some(title) = modal.line_modal("Task title", &default) {
                        let title = title.trim();
                        if !title.is_empty() {
                            let id = task.get("id").cloned().unwrap_or(Value::Null);
                            self.rpc(
                                "task_update",
                                serde_json::json!({"id": id, "title": title}),
                                now,
                            );
                        }
                    }
                }
                TaskKeyDispatch::Handled
            }
            KeyCode::Char('h') => {
                if let Some(task) = self.current_task(now).cloned() {
                    let id = task.get("id").cloned().unwrap_or(Value::Null);
                    self.rpc("task_heartbeat", serde_json::json!({"id": id}), now);
                }
                TaskKeyDispatch::Handled
            }
            KeyCode::Char('r') => {
                if let Some(task) = self.current_task(now).cloned() {
                    let id = task.get("id").cloned().unwrap_or(Value::Null);
                    self.rpc("task_release", serde_json::json!({"id": id}), now);
                }
                TaskKeyDispatch::Handled
            }
            KeyCode::Char('n') => {
                let (result, _) = self.rpc("task_next", serde_json::json!({"scope": "auto"}), now);
                if let Some(result) = result.filter(python_truthy) {
                    if let Some(task) = result.get("task").filter(|task| python_truthy(task)) {
                        let id = task.get("id").map(python_string).unwrap_or_default();
                        self.flash(format!("claimed {id}"), now, 4.0);
                    } else {
                        self.flash("no task ready; following the queue", now, 4.0);
                    }
                }
                TaskKeyDispatch::Handled
            }
            KeyCode::Char('f') => {
                let (result, _) = self.rpc("task_follow", Value::Null, now);
                if let Some(result) = result.filter(python_truthy) {
                    let pane_id = result.get("pane_id").map(python_string).unwrap_or_default();
                    self.flash(format!("receiving pushed tasks as {pane_id}"), now, 4.0);
                }
                TaskKeyDispatch::Handled
            }
            KeyCode::Char('F') => {
                self.rpc("task_unfollow", Value::Null, now);
                TaskKeyDispatch::Handled
            }
            KeyCode::Char('a') => {
                if let Some(title) = modal.line_modal("New task title", "") {
                    let title = title.trim();
                    if !title.is_empty() {
                        self.rpc("task_add", serde_json::json!({"title": title}), now);
                    }
                }
                TaskKeyDispatch::Handled
            }
            _ => TaskKeyDispatch::Ignored,
        }
    }

    /// Prompt for and update the selected dunk's interval using the Python label.
    pub fn edit_interval<M: LineModalProvider>(&mut self, modal: &mut M, now: f64) -> bool {
        let Some(dunk) = self.current().cloned() else {
            self.current_id(now);
            return false;
        };
        let id = dunk.get("id").cloned().unwrap_or(Value::Null);
        let minutes = dunk
            .get("interval_minutes")
            .and_then(Value::as_f64)
            .unwrap_or_default();
        let default = crate::rows::format_interval(minutes);
        let Some(value) = modal.line_modal("Interval (minutes, or e.g. 90s / 1.5h)", &default)
        else {
            return false;
        };
        let value = value.trim();
        if value.is_empty() {
            return false;
        }
        self.rpc(
            "update_dunk",
            serde_json::json!({"id": id, "interval_minutes": value}),
            now,
        )
        .1
    }

    /// Prompt with the selected dunk's current text and update it verbatim after
    /// Python-compatible outer whitespace trimming.
    pub fn edit_text<M: TextModalProvider>(&mut self, modal: &mut M, now: f64) -> bool {
        let Some(dunk) = self.current().cloned() else {
            self.current_id(now);
            return false;
        };
        let id = dunk.get("id").cloned().unwrap_or(Value::Null);
        let initial_text = dunk
            .get("text")
            .filter(|value| python_truthy(value))
            .map(python_string)
            .unwrap_or_default();
        let Some(text) = modal.text_modal(&initial_text) else {
            return false;
        };
        self.rpc(
            "update_dunk",
            serde_json::json!({"id": id, "text": text.trim()}),
            now,
        )
        .1
    }

    /// Apply the Python dunk-view key map. Actions whose implementations live in
    /// later TUI leaves are returned to the caller; all actions already represented
    /// by this model execute through the fakeable RPC and modal interfaces.
    pub fn handle_key<M: LineModalProvider>(
        &mut self,
        key: KeyEvent,
        modal: &mut M,
        now: f64,
    ) -> KeyDispatch {
        let plain_or_shift =
            key.modifiers.is_empty() || key.modifiers == crossterm::event::KeyModifiers::SHIFT;
        if key.code == KeyCode::Char('v') && plain_or_shift {
            self.toggle_view(now);
            self.refresh(now);
            return KeyDispatch::Handled;
        }
        if self.view == View::Tasks {
            return KeyDispatch::TaskView(key);
        }
        if !plain_or_shift {
            return KeyDispatch::Ignored;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => KeyDispatch::Quit,
            KeyCode::Char('Q') => {
                self.quit_and_shutdown(now);
                KeyDispatch::QuitAndShutdown
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                KeyDispatch::Handled
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selected = self
                    .selected
                    .saturating_add(1)
                    .min(self.dunks.len().saturating_sub(1));
                KeyDispatch::Handled
            }
            KeyCode::Char('a') => {
                let (result, _) = self.rpc("add_dunk", serde_json::json!({"start": false}), now);
                if let Some(result) = result.filter(python_truthy) {
                    self.refresh(now);
                    if let Some(id) = result.get("id") {
                        if let Some(index) = self
                            .dunks
                            .iter()
                            .position(|dunk| dunk.get("id") == Some(id))
                        {
                            self.selected = index;
                        }
                    }
                }
                KeyDispatch::Handled
            }
            KeyCode::Char('d') => {
                if let Some(id) = self.current_id(now).map(str::to_owned) {
                    self.rpc("remove_dunk", serde_json::json!({"id": id}), now);
                }
                KeyDispatch::Handled
            }
            KeyCode::Char(' ') | KeyCode::Char('s') => {
                if let Some(id) = self.current_id(now).map(str::to_owned) {
                    self.rpc("toggle_dunk", serde_json::json!({"id": id}), now);
                }
                KeyDispatch::Handled
            }
            KeyCode::Char('c') => KeyDispatch::PickTarget,
            KeyCode::Char('t') => {
                if let Some(id) = self.current_id(now).map(str::to_owned) {
                    self.rpc(
                        "fire_dunk",
                        serde_json::json!({"id": id, "countdown_s": 2, "wait": false}),
                        now,
                    );
                }
                KeyDispatch::Handled
            }
            KeyCode::Char('i') => KeyDispatch::EditInterval,
            KeyCode::Char('e') => KeyDispatch::EditText,
            KeyCode::Char('n') => {
                self.edit_name(modal, now);
                KeyDispatch::Handled
            }
            KeyCode::Char('o') => {
                self.toggle_only_idle(now);
                KeyDispatch::Handled
            }
            KeyCode::Char('u') => {
                self.toggle_skip_if_unconsumed(now);
                KeyDispatch::Handled
            }
            KeyCode::Char('y') => {
                self.toggle_hold_while_typing(now);
                KeyDispatch::Handled
            }
            KeyCode::Char('m') => {
                self.edit_max_sends(modal, now);
                KeyDispatch::Handled
            }
            _ => KeyDispatch::Ignored,
        }
    }

    /// Prompt for a dunker's display name and update it through the daemon.
    pub fn edit_name<M: LineModalProvider>(&mut self, modal: &mut M, now: f64) -> bool {
        let Some(dunk) = self.current().cloned() else {
            self.current_id(now);
            return false;
        };
        let id = dunk.get("id").cloned().unwrap_or(Value::Null);
        let default = dunk
            .get("name")
            .filter(|value| python_truthy(value))
            .map(python_string)
            .unwrap_or_default();
        let Some(value) = modal.line_modal("Dunker name", &default) else {
            return false;
        };
        self.rpc(
            "update_dunk",
            serde_json::json!({"id": id, "name": value.trim()}),
            now,
        )
        .1
    }

    /// Prompt for the maximum number of sends. Blank and zero both mean forever.
    pub fn edit_max_sends<M: LineModalProvider>(&mut self, modal: &mut M, now: f64) -> bool {
        let Some(dunk) = self.current().cloned() else {
            self.current_id(now);
            return false;
        };
        let id = dunk.get("id").cloned().unwrap_or(Value::Null);
        let default = dunk
            .get("max_sends")
            .filter(|value| python_truthy(value))
            .map(python_string)
            .unwrap_or_default();
        let Some(value) = modal.line_modal("Max sends (blank = forever)", &default) else {
            return false;
        };
        let value = value.trim();
        let max_sends = if value.is_empty() || value == "0" {
            Some(serde_json::json!(0))
        } else {
            normalize_unicode_decimal_integer(value)
                .and_then(|digits| serde_json::from_str::<Value>(&digits).ok())
        };
        let Some(max_sends) = max_sends else {
            self.flash("Max sends must be a whole number", now, 4.0);
            return false;
        };
        self.rpc(
            "update_dunk",
            serde_json::json!({"id": id, "max_sends": max_sends}),
            now,
        )
        .1
    }

    /// Toggle the send gate between scheduled delivery and idle-only delivery.
    pub fn toggle_only_idle(&mut self, now: f64) -> bool {
        let Some(dunk) = self.current().cloned() else {
            self.current_id(now);
            return false;
        };
        let id = dunk.get("id").cloned().unwrap_or(Value::Null);
        let new_value = if dunk.get("only_when").and_then(Value::as_str) == Some("idle") {
            "any"
        } else {
            "idle"
        };
        let reconnect = self
            .rpc(
                "update_dunk",
                serde_json::json!({"id": id, "only_when": new_value}),
                now,
            )
            .1;
        self.flash(
            if new_value == "idle" {
                "Sends wait for idle agent"
            } else {
                "Sends on schedule"
            },
            now,
            4.0,
        );
        reconnect
    }

    /// Toggle whether missed, unconsumed sends are skipped.
    pub fn toggle_skip_if_unconsumed(&mut self, now: f64) -> bool {
        let Some(dunk) = self.current().cloned() else {
            self.current_id(now);
            return false;
        };
        let id = dunk.get("id").cloned().unwrap_or(Value::Null);
        let new_value = !dunk.get("skip_if_unconsumed").is_some_and(python_truthy);
        let reconnect = self
            .rpc(
                "update_dunk",
                serde_json::json!({"id": id, "skip_if_unconsumed": new_value}),
                now,
            )
            .1;
        self.flash(
            if new_value {
                "Skip sends the target never consumed"
            } else {
                "Always send on schedule"
            },
            now,
            4.0,
        );
        reconnect
    }

    /// Toggle whether a send waits while someone is typing in its target.
    pub fn toggle_hold_while_typing(&mut self, now: f64) -> bool {
        let Some(dunk) = self.current().cloned() else {
            self.current_id(now);
            return false;
        };
        let id = dunk.get("id").cloned().unwrap_or(Value::Null);
        let current_value = dunk.get("hold_while_typing").is_none_or(python_truthy);
        let new_value = !current_value;
        let reconnect = self
            .rpc(
                "update_dunk",
                serde_json::json!({"id": id, "hold_while_typing": new_value}),
                now,
            )
            .1;
        self.flash(
            if new_value {
                "Hold while someone is typing in the target"
            } else {
                "Send even while someone is typing"
            },
            now,
            4.0,
        );
        reconnect
    }

    pub fn flash(&mut self, text: impl Into<String>, now: f64, seconds: f64) {
        self.message = text.into();
        self.message_until = now + seconds;
    }

    /// Switch between the dunk and task views and flash the new view's name, as the
    /// Python TUI does on `v`. Shared by the key handler and the run loop, which
    /// consumes `v` before forwarding keys.
    pub fn toggle_view(&mut self, now: f64) {
        self.view = match self.view {
            View::Dunks => View::Tasks,
            View::Tasks => View::Dunks,
        };
        self.flash(
            match self.view {
                View::Dunks => "Dunks view",
                View::Tasks => "Tasks view",
            },
            now,
            4.0,
        );
    }

    /// Stop daemon-owned dunks only for the explicit uppercase-Q action.
    pub fn quit_and_shutdown(&mut self, now: f64) {
        if self.mode == ClientMode::Daemon {
            self.rpc("shutdown", serde_json::json!({"stop_all": true}), now);
        }
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

fn python_string(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Number(number) => number.to_string(),
        Value::String(string) => string.clone(),
        Value::Array(_) | Value::Object(_) => value.to_string(),
    }
}

fn normalize_unicode_decimal_integer(value: &str) -> Option<String> {
    value.chars().try_fold(
        String::with_capacity(value.len()),
        |mut digits, character| {
            let digit = unicode_decimal_digit(character)?;
            digits.push(char::from_digit(digit, 10)?);
            Some(digits)
        },
    )
}

fn unicode_decimal_digit(character: char) -> Option<u32> {
    // Zero code points for Unicode 16.0 decimal digit blocks (the table used by
    // Python 3.14's `str.isdigit`/`int` for decimal digits). Rust's `char` API
    // exposes ASCII radix digits only, so retain the block starts here.
    const ZEROES: &[u32] = &[
        0x0030, 0x0660, 0x06F0, 0x07C0, 0x0966, 0x09E6, 0x0A66, 0x0AE6, 0x0B66, 0x0BE6, 0x0C66,
        0x0CE6, 0x0D66, 0x0DE6, 0x0E50, 0x0ED0, 0x0F20, 0x1040, 0x1090, 0x17E0, 0x1810, 0x1946,
        0x19D0, 0x1A80, 0x1A90, 0x1B50, 0x1BB0, 0x1C40, 0x1C50, 0xA620, 0xA8D0, 0xA900, 0xA9D0,
        0xA9F0, 0xAA50, 0xABF0, 0xFF10, 0x104A0, 0x10D30, 0x10D40, 0x11066, 0x110F0, 0x11136,
        0x111D0, 0x112F0, 0x11450, 0x114D0, 0x11650, 0x116C0, 0x116D0, 0x116DA, 0x11730, 0x118E0,
        0x11950, 0x11BF0, 0x11C50, 0x11D50, 0x11DA0, 0x11F50, 0x16130, 0x16A60, 0x16AC0, 0x16B50,
        0x16D70, 0x1CCF0, 0x1D7CE, 0x1D7D8, 0x1D7E2, 0x1D7EC, 0x1D7F6, 0x1E140, 0x1E2F0, 0x1E4F0,
        0x1E5F1, 0x1E950, 0x1FBF0,
    ];
    let codepoint = u32::from(character);
    let block_index = ZEROES.partition_point(|zero| *zero <= codepoint);
    let zero = *ZEROES.get(block_index.checked_sub(1)?)?;
    let digit = codepoint - zero;
    (digit < 10).then_some(digit)
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

    fn text_modal_snapshot(modal: &mut TextModal, width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| modal.render(frame, frame.area()))
            .expect("draw text modal");
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

    fn target_picker_snapshot(picker: &TargetPicker, width: u16, height: u16) -> Vec<String> {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("test terminal");
        terminal
            .draw(|frame| picker.render(frame, frame.area()))
            .expect("draw target picker");
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

    #[derive(Default)]
    struct FakeModal {
        responses: VecDeque<Option<String>>,
        prompts: Vec<(String, String)>,
        target_responses: VecDeque<Option<Value>>,
        target_panes: Vec<Vec<Value>>,
        text_responses: VecDeque<Option<String>>,
        text_inputs: Vec<String>,
    }

    impl LineModalProvider for FakeModal {
        fn line_modal(&mut self, label: &str, default: &str) -> Option<String> {
            self.prompts.push((label.to_owned(), default.to_owned()));
            self.responses.pop_front().expect("fake modal response")
        }
    }

    impl TargetModalProvider for FakeModal {
        fn target_modal(&mut self, panes: &[Value]) -> Option<Value> {
            self.target_panes.push(panes.to_vec());
            self.target_responses
                .pop_front()
                .expect("fake target modal response")
        }
    }

    impl TextModalProvider for FakeModal {
        fn text_modal(&mut self, initial_text: &str) -> Option<String> {
            self.text_inputs.push(initial_text.to_owned());
            self.text_responses
                .pop_front()
                .expect("fake text modal response")
        }
    }

    fn action_model(dunk: Value, responses: Vec<Result<Value, RpcError>>) -> TuiModel<FakeClient> {
        let client = FakeClient {
            responses: VecDeque::from(responses),
            ..FakeClient::default()
        };
        let mut model = TuiModel::connected(client, 0.0);
        model.dunks.push(dunk);
        model
    }

    #[test]
    fn key_dispatch_toggles_views_refreshes_and_routes_task_keys() {
        let client = FakeClient {
            responses: VecDeque::from([
                Ok(serde_json::json!({"dunks": []})),
                Ok(serde_json::json!({"tasks": []})),
                Ok(serde_json::json!({"dunks": []})),
            ]),
            ..FakeClient::default()
        };
        let mut model = TuiModel::connected(client, 0.0);
        let mut modal = FakeModal::default();

        assert_eq!(
            model.handle_key(key(KeyCode::Char('v')), &mut modal, 1.0),
            KeyDispatch::Handled
        );
        assert_eq!(model.view, View::Tasks);
        assert_eq!(model.message, "Tasks view");
        assert_eq!(
            model.client.calls,
            vec![
                ("list_dunks".into(), Value::Null),
                (
                    "task_list".into(),
                    serde_json::json!({"include_done": true, "limit": 500})
                )
            ]
        );

        assert_eq!(
            model.handle_key(key(KeyCode::Char('q')), &mut modal, 2.0),
            KeyDispatch::TaskView(key(KeyCode::Char('q')))
        );
        let control_q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL);
        assert_eq!(
            model.handle_key(control_q, &mut modal, 2.5),
            KeyDispatch::TaskView(control_q)
        );
        assert_eq!(
            model.handle_key(key(KeyCode::Char('v')), &mut modal, 3.0),
            KeyDispatch::Handled
        );
        assert_eq!(model.view, View::Dunks);
        assert_eq!(
            model.client.calls.last(),
            Some(&("list_dunks".into(), Value::Null))
        );
    }

    #[test]
    fn key_dispatch_adds_and_selects_the_created_dunker() {
        let client = FakeClient {
            responses: VecDeque::from([
                Ok(serde_json::json!({"id": "d2"})),
                Ok(serde_json::json!({"dunks": [{"id": "d1"}, {"id": "d2"}]})),
            ]),
            ..FakeClient::default()
        };
        let mut model = TuiModel::connected(client, 0.0);
        let mut modal = FakeModal::default();

        assert_eq!(
            model.handle_key(key(KeyCode::Char('a')), &mut modal, 1.0),
            KeyDispatch::Handled
        );
        assert_eq!(model.selected, 1);
        assert_eq!(
            model.client.calls,
            vec![
                ("add_dunk".into(), serde_json::json!({"start": false})),
                ("list_dunks".into(), Value::Null)
            ]
        );
    }

    #[test]
    fn key_dispatch_routes_remove_toggle_and_fire_to_the_selected_dunker() {
        let mut model = action_model(
            serde_json::json!({"id": "d1"}),
            vec![Ok(Value::Null), Ok(Value::Null), Ok(Value::Null)],
        );
        let mut modal = FakeModal::default();

        for code in [KeyCode::Char('d'), KeyCode::Char('s'), KeyCode::Char('t')] {
            assert_eq!(
                model.handle_key(key(code), &mut modal, 1.0),
                KeyDispatch::Handled
            );
        }
        assert_eq!(
            model.client.calls,
            vec![
                ("remove_dunk".into(), serde_json::json!({"id": "d1"})),
                ("toggle_dunk".into(), serde_json::json!({"id": "d1"})),
                (
                    "fire_dunk".into(),
                    serde_json::json!({"id": "d1", "countdown_s": 2, "wait": false})
                )
            ]
        );
    }

    #[test]
    fn key_dispatch_calls_existing_edits_and_returns_unported_edit_actions() {
        let mut model = action_model(
            serde_json::json!({"id": "d1", "name": "Old", "max_sends": 4}),
            vec![Ok(Value::Null), Ok(Value::Null)],
        );
        let mut modal = FakeModal {
            responses: VecDeque::from([Some("New".into()), Some("8".into())]),
            ..FakeModal::default()
        };

        assert_eq!(
            model.handle_key(key(KeyCode::Char('n')), &mut modal, 1.0),
            KeyDispatch::Handled
        );
        assert_eq!(
            model.handle_key(key(KeyCode::Char('m')), &mut modal, 2.0),
            KeyDispatch::Handled
        );
        assert_eq!(
            model.handle_key(key(KeyCode::Char('i')), &mut modal, 3.0),
            KeyDispatch::EditInterval
        );
        assert_eq!(
            model.handle_key(key(KeyCode::Char('e')), &mut modal, 3.0),
            KeyDispatch::EditText
        );
        assert_eq!(
            model.handle_key(key(KeyCode::Char('c')), &mut modal, 3.0),
            KeyDispatch::PickTarget
        );
        assert_eq!(
            modal.prompts,
            vec![
                ("Dunker name".into(), "Old".into()),
                ("Max sends (blank = forever)".into(), "4".into())
            ]
        );
        assert_eq!(
            model.client.calls,
            vec![
                (
                    "update_dunk".into(),
                    serde_json::json!({"id": "d1", "name": "New"})
                ),
                (
                    "update_dunk".into(),
                    serde_json::json!({"id": "d1", "max_sends": 8})
                )
            ]
        );
    }

    #[test]
    fn key_dispatch_navigates_quits_and_shutdowns_like_the_python_map() {
        let mut model = action_model(serde_json::json!({"id": "d1"}), vec![Ok(Value::Null)]);
        model.dunks.push(serde_json::json!({"id": "d2"}));
        let mut modal = FakeModal::default();

        let control_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        let control_a = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL);
        assert_eq!(
            model.handle_key(control_c, &mut modal, 1.0),
            KeyDispatch::Ignored
        );
        assert_eq!(
            model.handle_key(control_a, &mut modal, 1.0),
            KeyDispatch::Ignored
        );
        assert!(model.client.calls.is_empty());

        assert_eq!(
            model.handle_key(key(KeyCode::Down), &mut modal, 1.0),
            KeyDispatch::Handled
        );
        assert_eq!(model.selected, 1);
        assert_eq!(
            model.handle_key(key(KeyCode::Char('k')), &mut modal, 1.0),
            KeyDispatch::Handled
        );
        assert_eq!(model.selected, 0);
        assert_eq!(
            model.handle_key(key(KeyCode::Esc), &mut modal, 1.0),
            KeyDispatch::Quit
        );
        assert_eq!(
            model.handle_key(
                KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::SHIFT),
                &mut modal,
                1.0,
            ),
            KeyDispatch::QuitAndShutdown
        );
        assert_eq!(
            model.client.calls,
            vec![("shutdown".into(), serde_json::json!({"stop_all": true}))]
        );
    }

    #[test]
    fn edit_name_prompts_with_current_name_and_sends_trimmed_value() {
        let mut model = action_model(
            serde_json::json!({"id": "d1", "name": "Current"}),
            vec![Ok(Value::Null)],
        );
        let mut modal = FakeModal {
            responses: VecDeque::from([Some("  New name  ".into())]),
            ..FakeModal::default()
        };

        assert!(!model.edit_name(&mut modal, 1.0));
        assert_eq!(
            modal.prompts,
            vec![("Dunker name".into(), "Current".into())]
        );
        assert_eq!(
            model.client.calls,
            vec![(
                "update_dunk".into(),
                serde_json::json!({"id": "d1", "name": "New name"})
            )]
        );
    }

    #[test]
    fn edit_name_cancel_does_not_send_an_update() {
        let mut model = action_model(serde_json::json!({"id": "d1", "name": "Current"}), vec![]);
        let mut modal = FakeModal {
            responses: VecDeque::from([None]),
            ..FakeModal::default()
        };

        assert!(!model.edit_name(&mut modal, 1.0));
        assert_eq!(
            modal.prompts,
            vec![("Dunker name".into(), "Current".into())]
        );
        assert!(model.client.calls.is_empty());
    }

    #[test]
    fn edit_max_sends_handles_blank_zero_valid_integer_and_invalid_text() {
        for (input, expected) in [
            ("", Some(0)),
            ("0", Some(0)),
            (" 12 ", Some(12)),
            ("١٢", Some(12)),
            ("1.5", None),
        ] {
            let responses = expected.map_or_else(Vec::new, |_| vec![Ok(Value::Null)]);
            let mut model = action_model(serde_json::json!({"id": "d1"}), responses);
            let mut modal = FakeModal {
                responses: VecDeque::from([Some(input.into())]),
                ..FakeModal::default()
            };

            assert!(!model.edit_max_sends(&mut modal, 2.0));
            assert_eq!(
                modal.prompts,
                vec![("Max sends (blank = forever)".into(), String::new())]
            );
            if let Some(max_sends) = expected {
                assert_eq!(
                    model.client.calls,
                    vec![(
                        "update_dunk".into(),
                        serde_json::json!({"id": "d1", "max_sends": max_sends})
                    )]
                );
            } else {
                assert!(model.client.calls.is_empty());
                assert_eq!(model.message, "Max sends must be a whole number");
                assert_eq!(model.message_until, 6.0);
            }
        }
    }

    #[test]
    fn edit_max_sends_preserves_integers_larger_than_u64() {
        let input = "18446744073709551616";
        let mut model = action_model(serde_json::json!({"id": "d1"}), vec![Ok(Value::Null)]);
        let mut modal = FakeModal {
            responses: VecDeque::from([Some(input.into())]),
            ..FakeModal::default()
        };
        let expected_max_sends: Value = serde_json::from_str(input).expect("large JSON integer");

        assert!(!model.edit_max_sends(&mut modal, 2.0));
        assert_eq!(
            model.client.calls,
            vec![(
                "update_dunk".into(),
                serde_json::json!({"id": "d1", "max_sends": expected_max_sends})
            )]
        );
    }

    #[test]
    fn send_gate_toggles_submit_exact_fields_values_and_flashes() {
        let mut model = action_model(
            serde_json::json!({
                "id": "d1",
                "only_when": "idle",
                "skip_if_unconsumed": true,
                "hold_while_typing": true
            }),
            vec![Ok(Value::Null); 6],
        );

        assert!(!model.toggle_only_idle(1.0));
        assert_eq!(model.message, "Sends on schedule");
        model.dunks[0]["only_when"] = serde_json::json!("any");
        assert!(!model.toggle_only_idle(2.0));
        assert_eq!(model.message, "Sends wait for idle agent");

        assert!(!model.toggle_skip_if_unconsumed(3.0));
        assert_eq!(model.message, "Always send on schedule");
        model.dunks[0]["skip_if_unconsumed"] = serde_json::json!(false);
        assert!(!model.toggle_skip_if_unconsumed(4.0));
        assert_eq!(model.message, "Skip sends the target never consumed");

        assert!(!model.toggle_hold_while_typing(5.0));
        assert_eq!(model.message, "Send even while someone is typing");
        model.dunks[0]["hold_while_typing"] = serde_json::json!(false);
        assert!(!model.toggle_hold_while_typing(6.0));
        assert_eq!(model.message, "Hold while someone is typing in the target");

        assert_eq!(
            model.client.calls,
            vec![
                (
                    "update_dunk".into(),
                    serde_json::json!({"id": "d1", "only_when": "any"})
                ),
                (
                    "update_dunk".into(),
                    serde_json::json!({"id": "d1", "only_when": "idle"})
                ),
                (
                    "update_dunk".into(),
                    serde_json::json!({"id": "d1", "skip_if_unconsumed": false})
                ),
                (
                    "update_dunk".into(),
                    serde_json::json!({"id": "d1", "skip_if_unconsumed": true})
                ),
                (
                    "update_dunk".into(),
                    serde_json::json!({"id": "d1", "hold_while_typing": false})
                ),
                (
                    "update_dunk".into(),
                    serde_json::json!({"id": "d1", "hold_while_typing": true})
                ),
            ]
        );
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
    fn current_and_current_task_return_the_selected_items() {
        let mut model = TuiModel::connected(FakeClient::default(), 0.0);
        model.dunks = vec![
            serde_json::json!({"id": "d1"}),
            serde_json::json!({"id": "d2"}),
        ];
        model.selected = 1;
        assert_eq!(Some(&serde_json::json!({"id": "d2"})), model.current());
        assert_eq!(Some("d2"), model.current_id(1.0));

        model.tasks = vec![
            serde_json::json!({"id": "t1"}),
            serde_json::json!({"id": "t2"}),
        ];
        model.task_selected = 1;
        assert_eq!(
            Some(&serde_json::json!({"id": "t2"})),
            model.current_task(2.0)
        );
    }

    #[test]
    fn current_id_and_current_task_flash_when_their_lists_are_empty() {
        let mut model = TuiModel::connected(FakeClient::default(), 0.0);
        assert_eq!(None, model.current());
        assert_eq!(None, model.current_id(1.0));
        assert_eq!(model.message, "No dunker selected - press a");
        assert_eq!(model.message_until, 5.0);

        assert_eq!(None, model.current_task(3.0));
        assert_eq!(model.message, "No task selected");
        assert_eq!(model.message_until, 7.0);
    }

    #[test]
    fn task_key_dispatch_routes_every_task_write_and_modal_call() {
        let mut model = TuiModel::connected(
            FakeClient {
                responses: VecDeque::from([
                    Ok(Value::Null),
                    Ok(Value::Null),
                    Ok(Value::Null),
                    Ok(Value::Null),
                    Ok(Value::Null),
                    Ok(Value::Null),
                    Ok(Value::Null),
                    Ok(Value::Null),
                    Ok(Value::Null),
                    Ok(serde_json::json!({"task": {"id": "t2"}})),
                    Ok(serde_json::json!({"pane_id": "%3"})),
                    Ok(Value::Null),
                    Ok(Value::Null),
                ]),
                ..FakeClient::default()
            },
            0.0,
        );
        model.tasks.push(serde_json::json!({
            "id": "t1",
            "title": "Old title",
            "block_reason": "old reason",
            "priority": "medium"
        }));
        let mut modal = FakeModal {
            responses: VecDeque::from([
                Some("  blocked reason  ".into()),
                Some("  New title  ".into()),
                Some("  New task  ".into()),
            ]),
            ..FakeModal::default()
        };

        for code in [
            KeyCode::Char('c'),
            KeyCode::Char('s'),
            KeyCode::Char('b'),
            KeyCode::Char('u'),
            KeyCode::Char('p'),
            KeyCode::Char('d'),
            KeyCode::Char('e'),
            KeyCode::Char('h'),
            KeyCode::Char('r'),
            KeyCode::Char('n'),
            KeyCode::Char('f'),
            KeyCode::Char('F'),
            KeyCode::Char('a'),
        ] {
            assert_eq!(
                model.handle_task_key(key(code), &mut modal, 1.0),
                TaskKeyDispatch::Handled
            );
        }

        assert_eq!(
            modal.prompts,
            vec![
                ("Why is it blocked?".into(), "old reason".into()),
                ("Task title".into(), "Old title".into()),
                ("New task title".into(), String::new())
            ]
        );
        assert_eq!(
            model.client.calls,
            vec![
                ("task_complete".into(), serde_json::json!({"id": "t1"})),
                (
                    "task_update".into(),
                    serde_json::json!({"id": "t1", "status": "in_progress"})
                ),
                (
                    "task_block".into(),
                    serde_json::json!({"id": "t1", "reason": "blocked reason"})
                ),
                ("task_unblock".into(), serde_json::json!({"id": "t1"})),
                (
                    "task_update".into(),
                    serde_json::json!({"id": "t1", "priority": "high"})
                ),
                ("task_delete".into(), serde_json::json!({"id": "t1"})),
                (
                    "task_update".into(),
                    serde_json::json!({"id": "t1", "title": "New title"})
                ),
                ("task_heartbeat".into(), serde_json::json!({"id": "t1"})),
                ("task_release".into(), serde_json::json!({"id": "t1"})),
                ("task_next".into(), serde_json::json!({"scope": "auto"})),
                ("task_follow".into(), Value::Null),
                ("task_unfollow".into(), Value::Null),
                ("task_add".into(), serde_json::json!({"title": "New task"}))
            ]
        );
        assert_eq!(model.message, "receiving pushed tasks as %3");
    }

    #[test]
    fn task_key_dispatch_ignores_control_shortcuts_and_preserves_quit_behavior() {
        let mut model = TuiModel::connected(
            FakeClient {
                responses: VecDeque::from([Ok(Value::Null)]),
                ..FakeClient::default()
            },
            0.0,
        );
        model.tasks.push(serde_json::json!({"id": "t1"}));
        let mut modal = FakeModal::default();
        assert_eq!(
            model.handle_task_key(
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                &mut modal,
                1.0,
            ),
            TaskKeyDispatch::Ignored
        );
        assert_eq!(
            model.handle_task_key(key(KeyCode::Down), &mut modal, 1.0),
            TaskKeyDispatch::Handled
        );
        assert_eq!(model.task_selected, 0);
        assert_eq!(
            model.handle_task_key(key(KeyCode::Char('q')), &mut modal, 1.0),
            TaskKeyDispatch::Quit
        );
        assert_eq!(
            model.handle_task_key(
                KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::SHIFT),
                &mut modal,
                1.0,
            ),
            TaskKeyDispatch::QuitAndShutdown
        );
        assert_eq!(
            model.client.calls,
            vec![("shutdown".into(), serde_json::json!({"stop_all": true}))]
        );
    }

    #[test]
    fn interval_and_text_edits_use_python_defaults_and_preserve_multiline_text() {
        let mut model = action_model(
            serde_json::json!({"id": "d1", "interval_minutes": 0.5, "text": "old\ntext"}),
            vec![Ok(Value::Null), Ok(Value::Null)],
        );
        let mut interval_modal = FakeModal {
            responses: VecDeque::from([Some(" 1.5h ".into())]),
            ..FakeModal::default()
        };
        assert!(!model.edit_interval(&mut interval_modal, 1.0));
        assert_eq!(
            interval_modal.prompts,
            vec![(
                "Interval (minutes, or e.g. 90s / 1.5h)".into(),
                "30s".into()
            )]
        );

        let mut text_modal = FakeModal {
            text_responses: VecDeque::from([Some("  new\ntext  ".into())]),
            ..FakeModal::default()
        };
        assert!(!model.edit_text(&mut text_modal, 2.0));
        assert_eq!(text_modal.text_inputs, vec!["old\ntext"]);
        assert_eq!(
            model.client.calls,
            vec![
                (
                    "update_dunk".into(),
                    serde_json::json!({"id": "d1", "interval_minutes": "1.5h"})
                ),
                (
                    "update_dunk".into(),
                    serde_json::json!({"id": "d1", "text": "new\ntext"})
                )
            ]
        );
    }

    #[test]
    fn pick_target_lists_panes_and_updates_the_selected_dunker() {
        let pane = serde_json::json!({"pane_id": "%42", "agent": "codex"});
        let mut model = action_model(
            serde_json::json!({"id": "d1"}),
            vec![
                Ok(serde_json::json!({"panes": [pane.clone()]})),
                Ok(Value::Null),
            ],
        );
        let mut modal = FakeModal {
            target_responses: VecDeque::from([Some(pane.clone())]),
            ..FakeModal::default()
        };

        assert!(!model.pick_target(&mut modal, 1.0));
        assert_eq!(modal.target_panes, vec![vec![pane.clone()]]);
        assert_eq!(
            model.client.calls,
            vec![
                ("list_panes".into(), Value::Null),
                (
                    "update_dunk".into(),
                    serde_json::json!({"id": "d1", "target": "%42"})
                )
            ]
        );
    }

    #[test]
    fn pick_target_empty_panes_show_status_hint_without_opening_picker() {
        let mut model = action_model(
            serde_json::json!({"id": "d1"}),
            vec![
                Ok(serde_json::json!({"panes": []})),
                Ok(serde_json::json!({"herdr_hint": "Open a managed pane first"})),
            ],
        );
        let mut modal = FakeModal::default();

        assert!(!model.pick_target(&mut modal, 2.0));
        assert_eq!(model.message, "Open a managed pane first");
        assert!(modal.target_panes.is_empty());
        assert_eq!(
            model.client.calls,
            vec![
                ("list_panes".into(), Value::Null),
                ("status".into(), Value::Null)
            ]
        );
    }

    #[test]
    fn target_picker_prefers_agent_and_supports_selection_cancel_and_rows() {
        let panes = vec![
            serde_json::json!({
                "pane_id": "%1",
                "workspace_label": "Work",
                "tab_label": "shell",
                "cwd": "/tmp",
                "agent_status": "idle"
            }),
            serde_json::json!({
                "pane_id": "%2",
                "workspace_label": "Work",
                "tab_label": "codex",
                "agent": "codex",
                "agent_status": "working",
                "cwd": "/workspace"
            }),
            serde_json::json!({
                "pane_id": "%3",
                "workspace_id": "ws-2",
                "tab_id": "tab-2",
                "is_self": true,
                "cwd": "/home"
            }),
        ];
        let mut picker = TargetPicker::new(panes.clone());
        assert_eq!(picker.selected(), Some(&panes[1]));
        let snapshot = target_picker_snapshot(&picker, 80, 12);
        assert!(snapshot
            .iter()
            .any(|line| line.contains("Select target pane")));
        assert!(snapshot.iter().any(|line| line.contains("Work")));
        assert!(snapshot.iter().any(|line| line.contains("codex")));

        assert_eq!(
            picker.handle_key(key(KeyCode::Down)),
            TargetPickerResult::Pending
        );
        assert_eq!(picker.selected(), Some(&panes[2]));
        assert_eq!(
            picker.handle_key(key(KeyCode::Up)),
            TargetPickerResult::Pending
        );
        assert_eq!(
            picker.handle_key(key(KeyCode::Enter)),
            TargetPickerResult::Selected(panes[1].clone())
        );
        assert_eq!(
            picker.handle_key(key(KeyCode::Esc)),
            TargetPickerResult::Cancelled
        );
    }

    #[test]
    fn text_modal_key_map_edits_logical_text_and_handles_escape_submit() {
        let mut modal = TextModal::new("ab\ncd");
        assert_eq!(
            modal.handle_key(key(KeyCode::Left), 3),
            TextModalResult::Pending
        );
        assert_eq!(
            modal.handle_key(key(KeyCode::Backspace), 3),
            TextModalResult::Pending
        );
        assert_eq!(modal.editor().text(), "ab\nd");
        assert_eq!(
            modal.handle_key(key(KeyCode::Backspace), 3),
            TextModalResult::Pending
        );
        assert_eq!(modal.editor().text(), "abd");
        assert_eq!(
            modal.handle_key(key(KeyCode::Delete), 3),
            TextModalResult::Pending
        );
        assert_eq!(modal.editor().text(), "ab");
        assert_eq!(
            modal.handle_key(key(KeyCode::Char('X')), 3),
            TextModalResult::Pending
        );
        assert_eq!(
            modal.handle_key(key(KeyCode::Enter), 3),
            TextModalResult::Pending
        );
        assert_eq!(
            modal.handle_key(key(KeyCode::Tab), 3),
            TextModalResult::Pending
        );
        assert_eq!(
            modal.handle_key(key(KeyCode::Char('é')), 3),
            TextModalResult::Pending
        );
        let submitted =
            modal.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL), 3);
        assert_eq!(submitted, TextModalResult::Submitted("abX\n é".into()));

        let mut cancelled = TextModal::new("unchanged");
        assert_eq!(
            cancelled.handle_key(key(KeyCode::Esc), 3),
            TextModalResult::Cancelled
        );
    }

    #[test]
    fn text_modal_snapshot_shows_title_help_and_visible_logical_lines() {
        let mut modal = TextModal::new("first\nsecond");
        let snapshot = text_modal_snapshot(&mut modal, 36, 10);
        assert!(snapshot
            .iter()
            .any(|line| line.contains("Edit text to send")));
        assert!(snapshot
            .iter()
            .any(|line| line.contains("Ctrl+G saves, Esc cancels")));
        assert!(snapshot.iter().any(|line| line.contains("first")));
        assert!(snapshot.iter().any(|line| line.contains("second")));
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
