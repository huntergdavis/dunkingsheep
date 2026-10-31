//! The TUI's state and key handling, minus drawing (`DunkingSheepTui` in
//! `dunking_sheep_tui.py`: `rpc`, `refresh`, `refresh_tasks`, `flash`, `current_id`,
//! `handle_key`, `handle_task_key`, `quit_and_shutdown`).
//!
//! NOT ported here: `connect` (needs the daemon client wiring), the modals' drawing,
//! the dunk key `c` (`pick_target`), and every `draw*`. The task-view keys that open a modal (`b e a`) go through [`LineModal`].

use serde_json::{json, Map, Value};

pub const RECONNECT_S: f64 = 2.0;

/// The two failure families `rpc()` tells apart: a `DunkError` is flashed as-is, a
/// transport failure is flashed as "Daemon error: ..." and may trigger a reconnect.
#[derive(Debug, Clone, PartialEq)]
pub enum RpcError {
    Dunk(String),
    Daemon(String),
}

/// `client.call(cmd, **args)`: a `FlockClient` or the in-process `LocalFlock`.
pub trait Backend {
    fn call(&mut self, cmd: &str, args: Map<String, Value>) -> Result<Value, RpcError>;
    /// `connect()`: try the daemon again; true when it came back.
    fn reconnect(&mut self) -> bool;
}

/// `line_modal(label, default)`: `None` means the user cancelled.
pub trait LineModal {
    fn line_modal(&mut self, label: &str, default: &str) -> Option<String>;
    /// `text_modal(initial_text)`: the multiline editor; `None` means cancelled.
    fn text_modal(&mut self, initial_text: &str) -> Option<String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Escape,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Dunks,
    Tasks,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Daemon,
    Local,
}

pub struct Model<B: Backend> {
    pub backend: B,
    pub dunks: Vec<Value>,
    pub selected: usize,
    pub global_status: String,
    pub message: String,
    pub message_until: f64,
    pub quit_requested: bool,
    pub view: View,
    pub tasks: Vec<Value>,
    pub task_selected: usize,
    pub mode: Mode,
    pub last_connect_attempt: f64,
}

fn args(pairs: &[(&str, Value)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn clamp_selected(selected: usize, len: usize) -> usize {
    selected.min(len.saturating_sub(1))
}

impl<B: Backend> Model<B> {
    pub fn new(backend: B, mode: Mode, now: f64) -> Self {
        Model {
            backend,
            dunks: Vec::new(),
            selected: 0,
            global_status: String::new(),
            message: String::new(),
            message_until: 0.0,
            quit_requested: false,
            view: View::Dunks,
            tasks: Vec::new(),
            task_selected: 0,
            mode,
            last_connect_attempt: now,
        }
    }

    pub fn flash(&mut self, text: &str, seconds: f64, now: f64) {
        self.message = text.to_string();
        self.message_until = now + seconds;
    }

    /// Run a command; errors become a status flash and `None`.
    pub fn rpc(&mut self, cmd: &str, call_args: Map<String, Value>, now: f64) -> Option<Value> {
        match self.backend.call(cmd, call_args) {
            Ok(value) => Some(value),
            Err(RpcError::Dunk(text)) => {
                self.flash(&text, 4.0, now);
                None
            }
            Err(RpcError::Daemon(text)) => {
                self.flash(&format!("Daemon error: {text}"), 4.0, now);
                if self.mode == Mode::Daemon && now - self.last_connect_attempt > RECONNECT_S {
                    self.last_connect_attempt = now;
                    self.mode = if self.backend.reconnect() {
                        Mode::Daemon
                    } else {
                        Mode::Local
                    };
                }
                None
            }
        }
    }

    pub fn refresh(&mut self, now: f64) {
        let Some(result) = self.rpc("list_dunks", Map::new(), now) else {
            return;
        };
        self.dunks = result
            .get("dunks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        self.selected = clamp_selected(self.selected, self.dunks.len());
        let running = self
            .dunks
            .iter()
            .filter(|d| d.get("running").is_some_and(truthy))
            .count();
        let n = self.dunks.len();
        let mut label = format!("{n} dunker{}", if n != 1 { "s" } else { "" });
        if running > 0 {
            label.push_str(&format!(" ({running} running)"));
        }
        if self.mode == Mode::Local {
            label.push_str("  [in-process, not shared]");
        }
        self.global_status = label;
        if self.view == View::Tasks {
            self.refresh_tasks(now);
        }
    }

    pub fn refresh_tasks(&mut self, now: f64) {
        let call = args(&[("include_done", json!(true)), ("limit", json!(500))]);
        let Some(result) = self.rpc("task_list", call, now) else {
            return;
        };
        self.tasks = result
            .get("tasks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        self.task_selected = clamp_selected(self.task_selected, self.tasks.len());
        let open = self
            .tasks
            .iter()
            .filter(|t| {
                matches!(
                    t.get("status").and_then(Value::as_str),
                    Some("open" | "in_progress")
                )
            })
            .count();
        self.global_status = format!("{} tasks ({open} open/in-progress)", self.tasks.len());
    }

    pub fn current_id(&mut self, now: f64) -> Option<String> {
        match self.dunks.get(self.selected) {
            None => {
                self.flash("No dunker selected - press a", 4.0, now);
                None
            }
            Some(dunk) => id_of(dunk),
        }
    }

    fn current_task_id(&mut self, now: f64) -> Option<(String, Value)> {
        match self.tasks.get(self.task_selected).cloned() {
            None => {
                self.flash("No task selected", 4.0, now);
                None
            }
            Some(task) => id_of(&task).map(|id| (id, task)),
        }
    }

    pub fn quit_and_shutdown(&mut self, now: f64) {
        if self.mode == Mode::Daemon {
            self.rpc("shutdown", args(&[("stop_all", json!(true))]), now);
        }
        self.quit_requested = true;
    }

    pub fn handle_key(&mut self, key: Key, modal: &mut dyn LineModal, now: f64) {
        if key == Key::Char('v') {
            self.view = if self.view == View::Dunks {
                View::Tasks
            } else {
                View::Dunks
            };
            let title = match self.view {
                View::Dunks => "Dunks",
                View::Tasks => "Tasks",
            };
            self.flash(&format!("{title} view"), 4.0, now);
            self.refresh(now);
            return;
        }
        if self.view == View::Tasks {
            self.handle_task_key(key, modal, now);
            return;
        }
        match key {
            Key::Char('q') | Key::Escape => self.quit_requested = true,
            Key::Char('Q') => self.quit_and_shutdown(now),
            Key::Up | Key::Char('k') => self.selected = self.selected.saturating_sub(1),
            Key::Down | Key::Char('j') => {
                self.selected = (self.selected + 1).min(self.dunks.len().saturating_sub(1))
            }
            Key::Char('a') => {
                if let Some(result) = self.rpc("add_dunk", args(&[("start", json!(false))]), now) {
                    self.refresh(now);
                    if let Some(index) = self
                        .dunks
                        .iter()
                        .position(|d| d.get("id") == result.get("id"))
                    {
                        self.selected = index;
                    }
                }
            }
            Key::Char('d') => {
                if let Some(id) = self.current_id(now) {
                    self.rpc("remove_dunk", args(&[("id", json!(id))]), now);
                }
            }
            Key::Char(' ') | Key::Char('s') => {
                if let Some(id) = self.current_id(now) {
                    self.rpc("toggle_dunk", args(&[("id", json!(id))]), now);
                }
            }
            Key::Char('i') => self.edit_interval(modal, now),
            Key::Char('e') => self.edit_text(modal, now),
            Key::Char('n') => self.edit_name(modal, now),
            Key::Char('o') => self.toggle_only_idle(now),
            Key::Char('u') => self.toggle_skip_if_unconsumed(now),
            Key::Char('y') => self.toggle_hold_while_typing(now),
            Key::Char('m') => self.edit_max_sends(modal, now),
            Key::Char('t') => {
                if let Some(id) = self.current_id(now) {
                    let call = args(&[
                        ("id", json!(id)),
                        ("countdown_s", json!(2)),
                        ("wait", json!(false)),
                    ]);
                    self.rpc("fire_dunk", call, now);
                }
            }
            _ => {}
        }
    }

    /// The selected dunk, or the "No dunker selected" flash the Python's `current_id()` gives.
    fn current_dunk(&mut self, now: f64) -> Option<Value> {
        match self.dunks.get(self.selected).cloned() {
            Some(dunk) => Some(dunk),
            None => {
                self.current_id(now);
                None
            }
        }
    }

    fn update_dunk(&mut self, id: &Value, field: &str, value: Value, now: f64) {
        let mut call = Map::new();
        call.insert("id".into(), id.clone());
        call.insert(field.into(), value);
        self.rpc("update_dunk", call, now);
    }

    pub fn edit_interval(&mut self, modal: &mut dyn LineModal, now: f64) {
        let Some(dunk) = self.current_dunk(now) else {
            return;
        };
        let minutes = dunk
            .get("interval_minutes")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let shown = crate::rows::format_interval(minutes);
        if let Some(value) = modal.line_modal("Interval (minutes, or e.g. 90s / 1.5h)", &shown) {
            if !value.trim().is_empty() {
                self.update_dunk(&dunk["id"], "interval_minutes", json!(value.trim()), now);
            }
        }
    }

    pub fn edit_text(&mut self, modal: &mut dyn LineModal, now: f64) {
        let Some(dunk) = self.current_dunk(now) else {
            return;
        };
        let initial = dunk.get("text").and_then(Value::as_str).unwrap_or("");
        if let Some(new_text) = modal.text_modal(initial) {
            self.update_dunk(&dunk["id"], "text", json!(new_text.trim()), now);
        }
    }

    pub fn edit_name(&mut self, modal: &mut dyn LineModal, now: f64) {
        let Some(dunk) = self.current_dunk(now) else {
            return;
        };
        let initial = dunk.get("name").and_then(Value::as_str).unwrap_or("");
        if let Some(value) = modal.line_modal("Dunker name", initial) {
            self.update_dunk(&dunk["id"], "name", json!(value.trim()), now);
        }
    }

    pub fn toggle_only_idle(&mut self, now: f64) {
        let Some(dunk) = self.current_dunk(now) else {
            return;
        };
        let idle = dunk.get("only_when").and_then(Value::as_str) != Some("idle");
        self.update_dunk(
            &dunk["id"],
            "only_when",
            json!(if idle { "idle" } else { "any" }),
            now,
        );
        let text = if idle {
            "Sends wait for idle agent"
        } else {
            "Sends on schedule"
        };
        self.flash(text, 4.0, now);
    }

    pub fn toggle_skip_if_unconsumed(&mut self, now: f64) {
        let Some(dunk) = self.current_dunk(now) else {
            return;
        };
        let new_value = !dunk.get("skip_if_unconsumed").is_some_and(truthy);
        self.update_dunk(&dunk["id"], "skip_if_unconsumed", json!(new_value), now);
        let text = if new_value {
            "Skip sends the target never consumed"
        } else {
            "Always send on schedule"
        };
        self.flash(text, 4.0, now);
    }

    pub fn toggle_hold_while_typing(&mut self, now: f64) {
        let Some(dunk) = self.current_dunk(now) else {
            return;
        };
        // `dunk.get("hold_while_typing", True)`: absent means true.
        let current = dunk.get("hold_while_typing").is_none_or(truthy);
        self.update_dunk(&dunk["id"], "hold_while_typing", json!(!current), now);
        let text = if !current {
            "Hold while someone is typing in the target"
        } else {
            "Send even while someone is typing"
        };
        self.flash(text, 4.0, now);
    }

    pub fn edit_max_sends(&mut self, modal: &mut dyn LineModal, now: f64) {
        let Some(dunk) = self.current_dunk(now) else {
            return;
        };
        // `str(dunk.get("max_sends") or "")`: absent, null and 0 all show blank.
        let current = match dunk.get("max_sends") {
            Some(v) if truthy(v) => plain(v),
            _ => String::new(),
        };
        let Some(value) = modal.line_modal("Max sends (blank = forever)", &current) else {
            return;
        };
        let value = value.trim();
        if value.is_empty() || value == "0" {
            self.update_dunk(&dunk["id"], "max_sends", json!(0), now);
        } else if value.chars().all(|c| c.is_ascii_digit()) {
            match value.parse::<u64>() {
                Ok(n) => self.update_dunk(&dunk["id"], "max_sends", json!(n), now),
                Err(_) => self.flash("Max sends must be a whole number", 4.0, now),
            }
        } else {
            self.flash("Max sends must be a whole number", 4.0, now);
        }
    }

    pub fn handle_task_key(&mut self, key: Key, modal: &mut dyn LineModal, now: f64) {
        let by_id = |id: &str| args(&[("id", json!(id))]);
        match key {
            Key::Char('q') | Key::Escape => self.quit_requested = true,
            Key::Char('Q') => self.quit_and_shutdown(now),
            Key::Up | Key::Char('k') => self.task_selected = self.task_selected.saturating_sub(1),
            Key::Down | Key::Char('j') => {
                self.task_selected =
                    (self.task_selected + 1).min(self.tasks.len().saturating_sub(1))
            }
            Key::Char(' ') | Key::Char('c') => {
                if let Some((id, _)) = self.current_task_id(now) {
                    self.rpc("task_complete", by_id(&id), now);
                }
            }
            Key::Char('s') => {
                if let Some((id, _)) = self.current_task_id(now) {
                    let mut call = by_id(&id);
                    call.insert("status".into(), json!("in_progress"));
                    self.rpc("task_update", call, now);
                }
            }
            Key::Char('b') => {
                if let Some((id, task)) = self.current_task_id(now) {
                    let default = task
                        .get("block_reason")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if let Some(reason) = modal.line_modal("Why is it blocked?", default) {
                        let mut call = by_id(&id);
                        let reason = reason.trim();
                        call.insert(
                            "reason".into(),
                            if reason.is_empty() {
                                Value::Null
                            } else {
                                json!(reason)
                            },
                        );
                        self.rpc("task_block", call, now);
                    }
                }
            }
            Key::Char('u') => {
                if let Some((id, _)) = self.current_task_id(now) {
                    self.rpc("task_unblock", by_id(&id), now);
                }
            }
            Key::Char('p') => {
                if let Some((id, task)) = self.current_task_id(now) {
                    const ORDER: [&str; 4] = ["low", "medium", "high", "urgent"];
                    let current = task
                        .get("priority")
                        .and_then(Value::as_str)
                        .filter(|p| !p.is_empty())
                        .unwrap_or("low");
                    // Python's `order.index(current)` raises on an unknown priority.
                    if let Some(at) = ORDER.iter().position(|p| *p == current) {
                        let mut call = by_id(&id);
                        call.insert("priority".into(), json!(ORDER[(at + 1) % ORDER.len()]));
                        self.rpc("task_update", call, now);
                    }
                }
            }
            Key::Char('d') => {
                if let Some((id, _)) = self.current_task_id(now) {
                    self.rpc("task_delete", by_id(&id), now);
                }
            }
            Key::Char('e') => {
                if let Some((id, task)) = self.current_task_id(now) {
                    let default = task.get("title").and_then(Value::as_str).unwrap_or("");
                    if let Some(title) = modal.line_modal("Task title", default) {
                        if !title.trim().is_empty() {
                            let mut call = by_id(&id);
                            call.insert("title".into(), json!(title.trim()));
                            self.rpc("task_update", call, now);
                        }
                    }
                }
            }
            Key::Char('h') => {
                if let Some((id, _)) = self.current_task_id(now) {
                    self.rpc("task_heartbeat", by_id(&id), now);
                }
            }
            Key::Char('r') => {
                if let Some((id, _)) = self.current_task_id(now) {
                    self.rpc("task_release", by_id(&id), now);
                }
            }
            Key::Char('n') => {
                if let Some(result) = self.rpc("task_next", args(&[("scope", json!("auto"))]), now)
                {
                    match result.get("task").filter(|t| truthy(t)) {
                        Some(task) => {
                            let id = task.get("id").map(plain).unwrap_or_default();
                            self.flash(&format!("claimed {id}"), 4.0, now);
                        }
                        None => self.flash("no task ready; following the queue", 4.0, now),
                    }
                }
            }
            Key::Char('f') => {
                if let Some(result) = self.rpc("task_follow", Map::new(), now) {
                    let pane = result.get("pane_id").map(plain).unwrap_or_default();
                    self.flash(&format!("receiving pushed tasks as {pane}"), 4.0, now);
                }
            }
            Key::Char('F') => {
                self.rpc("task_unfollow", Map::new(), now);
            }
            Key::Char('a') => {
                if let Some(title) = modal.line_modal("New task title", "") {
                    if !title.trim().is_empty() {
                        self.rpc("task_add", args(&[("title", json!(title.trim()))]), now);
                    }
                }
            }
            _ => {}
        }
    }
}

/// Python truthiness for a JSON value (`if d.get("running")`).
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// `str(value)` for an id: a bare string stays bare, anything else is its JSON text.
fn plain(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn id_of(value: &Value) -> Option<String> {
    value.get("id").filter(|id| truthy(id)).map(plain)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        calls: Vec<(String, Map<String, Value>)>,
        replies: Vec<Result<Value, RpcError>>,
        reconnects: u32,
    }
    impl Fake {
        fn new(replies: Vec<Result<Value, RpcError>>) -> Self {
            Fake {
                calls: vec![],
                replies,
                reconnects: 0,
            }
        }
    }
    impl Backend for Fake {
        fn call(&mut self, cmd: &str, a: Map<String, Value>) -> Result<Value, RpcError> {
            self.calls.push((cmd.to_string(), a));
            if self.replies.is_empty() {
                Ok(json!({}))
            } else {
                self.replies.remove(0)
            }
        }
        fn reconnect(&mut self) -> bool {
            self.reconnects += 1;
            false
        }
    }
    struct Says(Option<String>);
    impl LineModal for Says {
        fn line_modal(&mut self, _: &str, _: &str) -> Option<String> {
            self.0.clone()
        }
        fn text_modal(&mut self, _: &str) -> Option<String> {
            self.0.clone()
        }
    }

    fn model(replies: Vec<Result<Value, RpcError>>) -> Model<Fake> {
        Model::new(Fake::new(replies), Mode::Daemon, 100.0)
    }

    #[test]
    fn refresh_labels_and_clamps_selection() {
        let mut m = model(vec![Ok(
            json!({"dunks": [{"id": "a", "running": true}, {"id": "b"}]}),
        )]);
        m.selected = 9;
        m.refresh(100.0);
        assert_eq!(m.global_status, "2 dunkers (1 running)");
        assert_eq!(m.selected, 1);
        let mut one = model(vec![Ok(json!({"dunks": [{"id": "a"}]}))]);
        one.mode = Mode::Local;
        one.refresh(100.0);
        assert_eq!(one.global_status, "1 dunker  [in-process, not shared]");
    }

    #[test]
    fn refresh_tasks_counts_open_and_in_progress() {
        let mut m = model(vec![Ok(json!({"tasks": [
            {"id": "t1", "status": "open"}, {"id": "t2", "status": "in_progress"},
            {"id": "t3", "status": "done"}]}))]);
        m.refresh_tasks(0.0);
        assert_eq!(m.global_status, "3 tasks (2 open/in-progress)");
        let (cmd, a) = &m.backend.calls[0];
        assert_eq!(cmd, "task_list");
        assert_eq!(a["limit"], json!(500));
        assert_eq!(a["include_done"], json!(true));
    }

    #[test]
    fn dunk_errors_flash_and_daemon_errors_reconnect_after_the_gap() {
        let mut m = model(vec![
            Err(RpcError::Dunk("nope".into())),
            Err(RpcError::Daemon("gone".into())),
            Err(RpcError::Daemon("gone".into())),
        ]);
        assert!(m.rpc("x", Map::new(), 100.5).is_none());
        assert_eq!((m.message.as_str(), m.message_until), ("nope", 104.5));
        m.rpc("x", Map::new(), 101.0);
        assert_eq!(m.message, "Daemon error: gone");
        assert_eq!(
            m.backend.reconnects, 0,
            "within RECONNECT_S of the last attempt"
        );
        m.rpc("x", Map::new(), 103.0);
        assert_eq!(m.backend.reconnects, 1);
    }

    #[test]
    fn keys_move_the_selection_and_send_the_agent_commands() {
        let mut m = model(vec![Ok(json!({"dunks": [{"id": "a"}, {"id": "b"}]}))]);
        m.refresh(0.0);
        let mut modal = Says(None);
        m.handle_key(Key::Char('j'), &mut modal, 0.0);
        m.handle_key(Key::Char('j'), &mut modal, 0.0);
        assert_eq!(m.selected, 1);
        m.handle_key(Key::Char('t'), &mut modal, 0.0);
        let (cmd, a) = m.backend.calls.last().unwrap();
        assert_eq!(cmd, "fire_dunk");
        assert_eq!(a["id"], json!("b"));
        assert_eq!(a["countdown_s"], json!(2));
        m.handle_key(Key::Escape, &mut modal, 0.0);
        assert!(m.quit_requested);
    }

    #[test]
    fn no_dunk_selected_flashes_the_hint() {
        let mut m = model(vec![]);
        m.handle_key(Key::Char('d'), &mut Says(None), 5.0);
        assert_eq!(m.message, "No dunker selected - press a");
        assert!(m.backend.calls.is_empty());
    }

    #[test]
    fn v_toggles_views_and_task_keys_use_the_tasks() {
        let mut m = model(vec![
            Ok(json!({"dunks": []})),
            Ok(json!({"tasks": [{"id": "t1", "status": "open", "priority": "urgent"}]})),
        ]);
        m.handle_key(Key::Char('v'), &mut Says(None), 0.0);
        assert_eq!((m.view, m.message.as_str()), (View::Tasks, "Tasks view"));
        assert_eq!(m.tasks.len(), 1);
        m.handle_key(Key::Char('p'), &mut Says(None), 0.0);
        let (cmd, a) = m.backend.calls.last().unwrap();
        assert_eq!(cmd, "task_update");
        assert_eq!(a["priority"], json!("low"), "urgent wraps to low");
    }

    #[test]
    fn block_modal_cancel_sends_nothing_and_blank_reason_is_null() {
        let mut m = model(vec![Ok(json!({"tasks": [{"id": "t1"}]}))]);
        m.view = View::Tasks;
        m.refresh_tasks(0.0);
        let before = m.backend.calls.len();
        m.handle_task_key(Key::Char('b'), &mut Says(None), 0.0);
        assert_eq!(m.backend.calls.len(), before);
        m.handle_task_key(Key::Char('b'), &mut Says(Some("  ".into())), 0.0);
        let (cmd, a) = m.backend.calls.last().unwrap();
        assert_eq!(cmd, "task_block");
        assert_eq!(a["reason"], Value::Null);
    }

    #[test]
    fn task_next_flashes_claimed_or_following() {
        let mut m = model(vec![
            Ok(json!({"task": {"id": "t9"}})),
            Ok(json!({"task": null})),
        ]);
        m.handle_task_key(Key::Char('n'), &mut Says(None), 0.0);
        assert_eq!(m.message, "claimed t9");
        m.handle_task_key(Key::Char('n'), &mut Says(None), 0.0);
        assert_eq!(m.message, "no task ready; following the queue");
    }

    fn with_dunk(d: Value) -> Model<Fake> {
        let mut m = model(vec![Ok(json!({"dunks": [d]}))]);
        m.refresh(0.0);
        m
    }

    #[test]
    fn toggles_send_the_flipped_value_and_flash() {
        let mut m = with_dunk(json!({"id": "a"}));
        m.handle_key(Key::Char('o'), &mut Says(None), 0.0);
        let (cmd, a) = m.backend.calls.last().unwrap();
        assert_eq!(
            (cmd.as_str(), &a["only_when"]),
            ("update_dunk", &json!("idle"))
        );
        assert_eq!(m.message, "Sends wait for idle agent");
        m.handle_key(Key::Char('u'), &mut Says(None), 0.0);
        assert_eq!(
            m.backend.calls.last().unwrap().1["skip_if_unconsumed"],
            json!(true)
        );
        // hold_while_typing defaults to true when absent, so the first press turns it off
        m.handle_key(Key::Char('y'), &mut Says(None), 0.0);
        assert_eq!(
            m.backend.calls.last().unwrap().1["hold_while_typing"],
            json!(false)
        );
        assert_eq!(m.message, "Send even while someone is typing");
    }

    #[test]
    fn max_sends_blank_and_zero_mean_forever_and_junk_flashes() {
        let mut m = with_dunk(json!({"id": "a", "max_sends": 3}));
        m.handle_key(Key::Char('m'), &mut Says(Some("".into())), 0.0);
        assert_eq!(m.backend.calls.last().unwrap().1["max_sends"], json!(0));
        m.handle_key(Key::Char('m'), &mut Says(Some(" 12 ".into())), 0.0);
        assert_eq!(m.backend.calls.last().unwrap().1["max_sends"], json!(12));
        let calls = m.backend.calls.len();
        m.handle_key(Key::Char('m'), &mut Says(Some("x1".into())), 0.0);
        assert_eq!(m.backend.calls.len(), calls);
        assert_eq!(m.message, "Max sends must be a whole number");
    }

    #[test]
    fn edit_interval_sends_the_stripped_string_and_blank_sends_nothing() {
        let mut m = with_dunk(json!({"id": "a", "interval_minutes": 5}));
        let calls = m.backend.calls.len();
        m.handle_key(Key::Char('i'), &mut Says(Some("  ".into())), 0.0);
        assert_eq!(m.backend.calls.len(), calls);
        m.handle_key(Key::Char('i'), &mut Says(Some(" 90s ".into())), 0.0);
        assert_eq!(
            m.backend.calls.last().unwrap().1["interval_minutes"],
            json!("90s")
        );
    }

    #[test]
    fn edit_text_and_name_strip_and_cancel_sends_nothing() {
        let mut m = with_dunk(json!({"id": "a"}));
        let calls = m.backend.calls.len();
        m.handle_key(Key::Char('e'), &mut Says(None), 0.0);
        assert_eq!(m.backend.calls.len(), calls);
        m.handle_key(Key::Char('e'), &mut Says(Some(" go on\n".into())), 0.0);
        assert_eq!(m.backend.calls.last().unwrap().1["text"], json!("go on"));
        m.handle_key(Key::Char('n'), &mut Says(Some("".into())), 0.0);
        assert_eq!(m.backend.calls.last().unwrap().1["name"], json!(""));
    }

    #[test]
    fn capital_q_stops_the_daemon_only_in_daemon_mode() {
        let mut m = model(vec![]);
        m.handle_key(Key::Char('Q'), &mut Says(None), 0.0);
        let (cmd, a) = m.backend.calls.last().unwrap();
        assert_eq!((cmd.as_str(), &a["stop_all"]), ("shutdown", &json!(true)));
        let mut local = model(vec![]);
        local.mode = Mode::Local;
        local.handle_key(Key::Char('Q'), &mut Says(None), 0.0);
        assert!(local.backend.calls.is_empty() && local.quit_requested);
    }
}
