//! Python-oracle parity for the dunk and task list screens (T61, main-screen slice).
//!
//! The expected rows are what `DunkingSheepTui.draw` and `draw_tasks` in
//! dunking_sheep_tui.py put on screen, captured by running those methods under a
//! fake curses window (no TTY in the lane that generated them). Each row is
//! right-trimmed on both sides. Regenerate from the Python, not from this file.

use std::collections::VecDeque;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;
use serde_json::Value;

use crate::app::{run, TuiLoopIo};
use crate::model::{RpcClient, RpcError, TuiModel, View};
use crate::view::{draw_dunks, draw_tasks};

#[derive(Default)]
struct NoRpc;

impl RpcClient for NoRpc {
    fn call(&mut self, _: &str, _: Value) -> Result<Value, RpcError> {
        Ok(Value::Null)
    }
}

fn from_json(text: &str) -> Vec<Value> {
    serde_json::from_str(text).unwrap()
}

fn screen_rows(model: &TuiModel<NoRpc>, width: u16, height: u16, tasks: bool) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            if tasks {
                draw_tasks(frame, model, 10.0);
            } else {
                draw_dunks(frame, model, 10.0);
            }
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer.cell((x, y)).unwrap().symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

fn assert_screen(actual: &[String], expected: &[&str]) {
    let actual: Vec<&str> = actual.iter().map(String::as_str).collect();
    assert_eq!(actual, expected);
}

const EMPTY_DUNKS_80X24: &[&str] = &[
    "Dunking Sheep TUI",
    "a add  d remove  c target  t test  i interval  e text  n name  o idle-gate  ...",
    "-------------------------------------------------------------------------------",
    "#    Status               Target                      Every   Sent Gate     ...",
    "(no dunkers - press a to add one, or let an agent add one)",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "-------------------------------------------------------------------------------",
    "Ready",
    "",
];

const TWO_DUNKS_80X24: &[&str] = &[
    "Dunking Sheep TUI",
    "a add  d remove  c target  t test  i interval  e text  n name  o idle-gate  ...",
    "-------------------------------------------------------------------------------",
    "#    Status               Target                      Every   Sent Gate     ...",
    "d1   running              w1:p1                           1      2 -        ...",
    "d2   stopped              w2:p2                         30s    3/5 idle+skip...",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "-------------------------------------------------------------------------------",
    "2 dunkers (1 running)  |  changed",
    "",
];

const FIVE_DUNKS_40X10: &[&str] = &[
    "Dunking Sheep TUI",
    "a add  d remove  c target  t test  i...",
    "---------------------------------------",
    "#    Status               Target    ...",
    "d2   running              w2:p1     ...",
    "d3   running              w3:p1     ...",
    "d4   running              w4:p1     ...",
    "---------------------------------------",
    "5 dunkers (2 running)  |  updated",
    "",
];

const EMPTY_DUNKS_40X10: &[&str] = &[
    "Dunking Sheep TUI",
    "a add  d remove  c target  t test  i...",
    "---------------------------------------",
    "#    Status               Target    ...",
    "(no dunkers - press a to add one, or...",
    "",
    "",
    "---------------------------------------",
    "",
    "",
];

const WIDE_DUNK_120X30: &[&str] = &[
    "Dunking Sheep TUI",
    "a add  d remove  c target  t test  i interval  e text  n name  o idle-gate  u skip-unconsumed  y typing-hold  m max ...",
    "-----------------------------------------------------------------------------------------------------------------------",
    "#    Status               Target                      Every   Sent Gate      Text",
    "d9   running              nudger: w9:p9                  90  12/50 idle+skip keep going with the refactor and report...",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "-----------------------------------------------------------------------------------------------------------------------",
    "1 dunker (1 running)",
    "",
];

const EMPTY_TASKS_80X24: &[&str] = &[
    "Dunking Sheep TUI - tasks",
    "a add  e title  c/space complete  s start  b block  u unblock  p priority  h...",
    "-------------------------------------------------------------------------------",
    "#     Status      Prio   Scope      Assignees          Title",
    "(no tasks - press a to add one, or n to take the next)",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "-------------------------------------------------------------------------------",
    "Ready",
    "",
];

const TWO_TASKS_80X24: &[&str] = &[
    "Dunking Sheep TUI - tasks",
    "a add  e title  c/space complete  s start  b block  u unblock  p priority  h...",
    "-------------------------------------------------------------------------------",
    "#     Status      Prio   Scope      Assignees          Title",
    "t1    open        high   project    %1                 First task",
    "t2    in_progress medium global     %2,%3              Second task",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "-------------------------------------------------------------------------------",
    "2 tasks (2 open/in-progress)  |  changed",
    "",
];

const FIVE_TASKS_40X10: &[&str] = &[
    "Dunking Sheep TUI - tasks",
    "a add  e title  c/space complete  s ...",
    "---------------------------------------",
    "#     Status      Prio   Scope      ...",
    "t2    open        low    project    ...",
    "t3    open        low    project    ...",
    "t4    open        low    project    ...",
    "---------------------------------------",
    "5 tasks (5 open/in-progress)  |  upd...",
    "",
];

const EMPTY_TASKS_40X10: &[&str] = &[
    "Dunking Sheep TUI - tasks",
    "a add  e title  c/space complete  s ...",
    "---------------------------------------",
    "#     Status      Prio   Scope      ...",
    "(no tasks - press a to add one, or n...",
    "",
    "",
    "---------------------------------------",
    "",
    "",
];

const WIDE_TASK_120X30: &[&str] = &[
    "Dunking Sheep TUI - tasks",
    "a add  e title  c/space complete  s start  b block  u unblock  p priority  h heartbeat  r release  n next  f follow ...",
    "-----------------------------------------------------------------------------------------------------------------------",
    "#     Status      Prio   Scope      Assignees          Title",
    "t77   open        urgent project    %7,%8              Ship the ds-tui parity fixes and keep the snapshot oracle cur...",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "-----------------------------------------------------------------------------------------------------------------------",
    "1 tasks (1 open/in-progress)",
    "",
];

const V_TO_TASKS_80X24: &[&str] = &[
    "Dunking Sheep TUI - tasks",
    "a add  e title  c/space complete  s start  b block  u unblock  p priority  h...",
    "-------------------------------------------------------------------------------",
    "#     Status      Prio   Scope      Assignees          Title",
    "(no tasks - press a to add one, or n to take the next)",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "-------------------------------------------------------------------------------",
    "0 tasks (0 open/in-progress)  |  Tasks view",
    "",
];

#[test]
fn python_oracle_empty_dunk_80x24() {
    let mut model = TuiModel::connected(NoRpc, 0.0);
    model.global_status = "Ready".into();
    assert_screen(&screen_rows(&model, 80, 24, false), EMPTY_DUNKS_80X24);
}

#[test]
fn python_oracle_selected_dunk_80x24() {
    let mut model = TuiModel::connected(NoRpc, 0.0);
    model.dunks = from_json(
        r#"[{"id": "d1", "status": "running", "target_label": "w1:p1", "interval_minutes": 1.0, "send_count": 2, "text": "one"}, {"id": "d2", "status": "stopped", "target_label": "w2:p2", "interval_minutes": 0.5, "send_count": 3, "max_sends": 5, "only_when": "idle", "skip_if_unconsumed": true, "text": "two\nlines"}]"#,
    );
    model.selected = 1;
    model.global_status = "2 dunkers (1 running)".into();
    model.flash("changed", 8.0, 4.0);
    assert_screen(&screen_rows(&model, 80, 24, false), TWO_DUNKS_80X24);
}

#[test]
fn python_oracle_scrolling_dunk_40x10() {
    let mut model = TuiModel::connected(NoRpc, 0.0);
    model.dunks = from_json(
        r#"[{"id": "d0", "status": "running", "target_label": "w0:p1", "interval_minutes": 1.0, "send_count": 0, "text": "item 0"}, {"id": "d1", "status": "running", "target_label": "w1:p1", "interval_minutes": 1.0, "send_count": 1, "text": "item 1"}, {"id": "d2", "status": "running", "target_label": "w2:p1", "interval_minutes": 1.0, "send_count": 2, "text": "item 2"}, {"id": "d3", "status": "running", "target_label": "w3:p1", "interval_minutes": 1.0, "send_count": 3, "text": "item 3"}, {"id": "d4", "status": "running", "target_label": "w4:p1", "interval_minutes": 1.0, "send_count": 4, "text": "item 4"}]"#,
    );
    model.selected = 4;
    model.global_status = "5 dunkers (2 running)".into();
    model.flash("updated", 8.0, 4.0);
    assert_screen(&screen_rows(&model, 40, 10, false), FIVE_DUNKS_40X10);
}

#[test]
fn python_oracle_empty_dunk_40x10() {
    let model = TuiModel::connected(NoRpc, 0.0);
    assert_screen(&screen_rows(&model, 40, 10, false), EMPTY_DUNKS_40X10);
}

#[test]
fn python_oracle_wide_dunk_120x30() {
    let mut model = TuiModel::connected(NoRpc, 0.0);
    model.dunks = from_json(
        r#"[{"id": "d9", "name": "nudger", "status": "running", "target_label": "w9:p9", "interval_minutes": 90, "send_count": 12, "max_sends": 50, "only_when": "idle", "skip_if_unconsumed": true, "text": "keep going with the refactor and report back when the tests are green"}]"#,
    );
    model.global_status = "1 dunker (1 running)".into();
    assert_screen(&screen_rows(&model, 120, 30, false), WIDE_DUNK_120X30);
}

#[test]
fn python_oracle_empty_task_80x24() {
    let mut model = TuiModel::connected(NoRpc, 0.0);
    model.global_status = "Ready".into();
    assert_screen(&screen_rows(&model, 80, 24, true), EMPTY_TASKS_80X24);
}

#[test]
fn python_oracle_selected_task_80x24() {
    let mut model = TuiModel::connected(NoRpc, 0.0);
    model.tasks = from_json(
        r#"[{"id": "t1", "status": "open", "priority": "high", "scope": "project", "assignees": ["%1"], "title": "First task"}, {"id": "t2", "status": "in_progress", "priority": "medium", "is_global": true, "assignees": ["%2", "%3"], "title": "Second task"}]"#,
    );
    model.task_selected = 1;
    model.global_status = "2 tasks (2 open/in-progress)".into();
    model.flash("changed", 8.0, 4.0);
    assert_screen(&screen_rows(&model, 80, 24, true), TWO_TASKS_80X24);
}

#[test]
fn python_oracle_scrolling_task_40x10() {
    let mut model = TuiModel::connected(NoRpc, 0.0);
    model.tasks = from_json(
        r#"[{"id": "t0", "status": "open", "priority": "low", "scope": "project", "assignees": ["%0"], "title": "Task 0"}, {"id": "t1", "status": "open", "priority": "low", "scope": "project", "assignees": ["%1"], "title": "Task 1"}, {"id": "t2", "status": "open", "priority": "low", "scope": "project", "assignees": ["%2"], "title": "Task 2"}, {"id": "t3", "status": "open", "priority": "low", "scope": "project", "assignees": ["%3"], "title": "Task 3"}, {"id": "t4", "status": "open", "priority": "low", "scope": "project", "assignees": ["%4"], "title": "Task 4"}]"#,
    );
    model.task_selected = 4;
    model.global_status = "5 tasks (5 open/in-progress)".into();
    model.flash("updated", 8.0, 4.0);
    assert_screen(&screen_rows(&model, 40, 10, true), FIVE_TASKS_40X10);
}

#[test]
fn python_oracle_empty_task_40x10() {
    let model = TuiModel::connected(NoRpc, 0.0);
    assert_screen(&screen_rows(&model, 40, 10, true), EMPTY_TASKS_40X10);
}

#[test]
fn python_oracle_wide_task_120x30() {
    let mut model = TuiModel::connected(NoRpc, 0.0);
    model.tasks = from_json(
        r#"[{"id": "t77", "status": "open", "priority": "urgent", "scope": "project", "assignees": ["%7", "%8"], "title": "Ship the ds-tui parity fixes and keep the snapshot oracle current across every screen"}]"#,
    );
    model.global_status = "1 tasks (1 open/in-progress)".into();
    assert_screen(&screen_rows(&model, 120, 30, true), WIDE_TASK_120X30);
}

#[test]
fn v_key_flashes_tasks_view_and_renders_like_python() {
    // Python: `v` flips the view, flashes "Tasks view", then refreshes.
    let mut model = TuiModel::connected(NoRpc, 0.0);
    model.toggle_view(10.0);
    assert_eq!(model.view, View::Tasks);
    assert_eq!(model.message, "Tasks view");
    model.global_status = "0 tasks (0 open/in-progress)".into();
    assert_screen(&screen_rows(&model, 80, 24, true), V_TO_TASKS_80X24);
}

struct ScriptedIo {
    keys: VecDeque<Option<KeyEvent>>,
}

impl TuiLoopIo<NoRpc> for ScriptedIo {
    type Error = std::convert::Infallible;

    fn now(&self) -> f64 {
        10.0
    }

    fn draw(&mut self, _: &TuiModel<NoRpc>, _: f64) -> Result<(), Self::Error> {
        Ok(())
    }

    fn read_key(&mut self, _: Duration) -> Result<Option<KeyEvent>, Self::Error> {
        Ok(self.keys.pop_front().unwrap_or(None))
    }

    fn handle_key(&mut self, _: &mut TuiModel<NoRpc>, _: KeyEvent) -> Result<(), Self::Error> {
        Ok(())
    }
}

#[test]
fn live_loop_v_key_shows_the_python_view_flash() {
    // Regression: run_loop used to consume `v` before model.handle_key, so the Python
    // "Tasks view" flash never appeared in the real TUI.
    let mut model = TuiModel::connected(NoRpc, 0.0);
    let mut io = ScriptedIo {
        keys: VecDeque::from([
            Some(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE)),
            Some(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)),
        ]),
    };
    run(&mut model, &mut io).unwrap();
    assert_eq!(model.view, View::Tasks);
    assert_eq!(model.message, "Tasks view");
}
