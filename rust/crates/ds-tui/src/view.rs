//! Ratatui rendering for the dunk list (`DunkingSheepTui.draw`).

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use serde_json::Value;

use crate::model::{RpcClient, TuiModel};
use crate::rows::{format_interval, format_row, format_task_row};

const TITLE: &str = "Dunking Sheep TUI";
const HELP: &str = "a add  d remove  c target  t test  i interval  e text  n name  o idle-gate  u skip-unconsumed  y typing-hold  m max  space start/stop  q quit  Q stop all+quit";
const TASK_TITLE: &str = "Dunking Sheep TUI - tasks";
const TASK_HELP: &str = "a add  e title  c/space complete  s start  b block  u unblock  p priority  h heartbeat  r release  n next  f follow  F unfollow  d delete  v dunks  q quit";
const EMPTY_DUNKS: &str = "(no dunkers - press a to add one, or let an agent add one)";
const EMPTY_TASKS: &str = "(no tasks - press a to add one, or n to take the next)";

/// Render the dunk-list view into the current frame.
pub fn draw_dunks<C: RpcClient>(frame: &mut Frame<'_>, model: &TuiModel<C>, now: f64) {
    let area = frame.area();
    let width = area.width.saturating_sub(1);
    let height = area.height;

    draw_line(
        frame,
        area,
        0,
        TITLE,
        Style::default().add_modifier(Modifier::BOLD),
    );
    draw_line(frame, area, 1, HELP, Style::default());
    draw_line(
        frame,
        area,
        2,
        &"-".repeat(width as usize),
        Style::default(),
    );

    let header = format_row(
        "#",
        "Status",
        "Target",
        "Every",
        "Sent",
        "Gate",
        "Text",
        width as i64,
    );
    draw_line(
        frame,
        area,
        3,
        &header,
        Style::default().add_modifier(Modifier::BOLD),
    );

    let visible_rows = height.saturating_sub(7) as usize;
    if model.dunks.is_empty() {
        draw_line(frame, area, 4, EMPTY_DUNKS, Style::default());
    }
    let start = if model.selected >= visible_rows {
        model
            .selected
            .saturating_sub(visible_rows)
            .saturating_add(1)
    } else {
        0
    };
    let end = model.dunks.len().min(start.saturating_add(visible_rows));
    for (offset, index) in (start..end).enumerate() {
        let dunk = &model.dunks[index];
        let row = dunk_row(dunk, width);
        let style = if index == model.selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        draw_line(frame, area, 4 + offset as u16, &row, style);
    }

    let divider_y = height.saturating_sub(3);
    draw_line(
        frame,
        area,
        divider_y,
        &"-".repeat(width as usize),
        Style::default(),
    );
    let mut bottom = model.global_status.clone();
    if !model.message.is_empty() && now < model.message_until {
        bottom.push_str("  |  ");
        bottom.push_str(&model.message);
    }
    draw_line(
        frame,
        area,
        height.saturating_sub(2),
        &bottom,
        Style::default(),
    );
}

/// Render the task-list view into the current frame.
pub fn draw_tasks<C: RpcClient>(frame: &mut Frame<'_>, model: &TuiModel<C>, now: f64) {
    let area = frame.area();
    let width = area.width.saturating_sub(1);
    let height = area.height;

    draw_line(
        frame,
        area,
        0,
        TASK_TITLE,
        Style::default().add_modifier(Modifier::BOLD),
    );
    draw_line(frame, area, 1, TASK_HELP, Style::default());
    draw_line(
        frame,
        area,
        2,
        &"-".repeat(width as usize),
        Style::default(),
    );
    let header = format_task_row(
        "#",
        "Status",
        "Prio",
        "Scope",
        "Assignees",
        "Title",
        width as i64,
    );
    draw_line(
        frame,
        area,
        3,
        &header,
        Style::default().add_modifier(Modifier::BOLD),
    );

    let visible_rows = height.saturating_sub(7) as usize;
    if model.tasks.is_empty() {
        draw_line(frame, area, 4, EMPTY_TASKS, Style::default());
    }
    let start = if model.task_selected >= visible_rows {
        model
            .task_selected
            .saturating_sub(visible_rows)
            .saturating_add(1)
    } else {
        0
    };
    let end = model.tasks.len().min(start.saturating_add(visible_rows));
    for (offset, index) in (start..end).enumerate() {
        let row = task_row(&model.tasks[index], width);
        let style = if index == model.task_selected {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        draw_line(frame, area, 4 + offset as u16, &row, style);
    }

    let divider_y = height.saturating_sub(3);
    draw_line(
        frame,
        area,
        divider_y,
        &"-".repeat(width as usize),
        Style::default(),
    );
    let mut bottom = model.global_status.clone();
    if !model.message.is_empty() && now < model.message_until {
        bottom.push_str("  |  ");
        bottom.push_str(&model.message);
    }
    draw_line(
        frame,
        area,
        height.saturating_sub(2),
        &bottom,
        Style::default(),
    );
}

fn task_row(task: &Value, width: u16) -> String {
    let id = display(task.get("id").unwrap_or(&Value::Null));
    let status = display_or(task.get("status"), "");
    let priority = display_or(task.get("priority"), "");
    let scope = if truthy(task.get("is_global")) {
        "global".to_string()
    } else {
        display_or(task.get("scope"), "")
    };
    let assignees = task
        .get("assignees")
        .and_then(Value::as_array)
        .map(|values| values.iter().map(display).collect::<Vec<_>>().join(","))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "-".to_string());
    let title = display_or(task.get("title"), "");

    format_task_row(
        &id,
        &status,
        &priority,
        &scope,
        &assignees,
        &title,
        width as i64,
    )
}

fn draw_line(frame: &mut Frame<'_>, area: Rect, y: u16, text: &str, style: Style) {
    if y >= area.height {
        return;
    }
    let width = area.width.saturating_sub(1);
    frame.render_widget(
        Paragraph::new(Line::raw(text.to_owned())).style(style),
        Rect::new(area.x, area.y + y, width, 1),
    );
}

fn dunk_row(dunk: &Value, width: u16) -> String {
    let id = display(dunk.get("id").unwrap_or(&Value::Null));
    let status = display_or(dunk.get("status"), "");
    let mut target = display_or(dunk.get("target_label"), "(no target)");
    if truthy(dunk.get("name")) {
        target = format!("{}: {target}", display(dunk.get("name").unwrap()));
    }

    let minutes = dunk
        .get("interval_minutes")
        .filter(|value| truthy(Some(value)))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let mut sent = display_or(dunk.get("send_count"), "0");
    if truthy(dunk.get("max_sends")) {
        sent.push('/');
        sent.push_str(&display(dunk.get("max_sends").unwrap()));
    }
    let mut gates = Vec::new();
    if dunk.get("only_when").and_then(Value::as_str) == Some("idle") {
        gates.push("idle");
    }
    if truthy(dunk.get("skip_if_unconsumed")) {
        gates.push("skip");
    }
    let gate = if gates.is_empty() {
        "-".to_string()
    } else {
        gates.join("+")
    };
    let text = display_or(dunk.get("text"), "").replace('\n', " ");

    format_row(
        &id,
        &status,
        &target,
        &format_interval(minutes),
        &sent,
        &gate,
        &text,
        width as i64,
    )
}

fn display_or(value: Option<&Value>, fallback: &str) -> String {
    value
        .filter(|value| truthy(Some(value)))
        .map(display)
        .unwrap_or_else(|| fallback.to_string())
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(value)) => value.as_f64().is_some_and(|number| number != 0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(_)) | Some(Value::Object(_)) => true,
    }
}

fn display(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        Value::Array(_) | Value::Object(_) => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use serde_json::json;

    use super::*;
    use crate::model::RpcError;

    #[derive(Default)]
    struct FakeClient;

    impl RpcClient for FakeClient {
        fn call(&mut self, _: &str, _: Value) -> Result<Value, RpcError> {
            Ok(Value::Null)
        }
    }

    fn snapshot<C: RpcClient>(
        model: &TuiModel<C>,
        width: u16,
        height: u16,
        now: f64,
    ) -> (String, Terminal<TestBackend>) {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_dunks(frame, model, now))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text = (0..height)
            .filter_map(|y| {
                let row = (0..width)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string();
                (!row.is_empty()).then(|| format!("{y:02} {row}"))
            })
            .collect::<Vec<_>>()
            .join("\n");
        (text, terminal)
    }

    fn snapshot_tasks<C: RpcClient>(
        model: &TuiModel<C>,
        width: u16,
        height: u16,
        now: f64,
    ) -> (String, Terminal<TestBackend>) {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw_tasks(frame, model, now))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text = (0..height)
            .filter_map(|y| {
                let row = (0..width)
                    .map(|x| buffer.cell((x, y)).unwrap().symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string();
                (!row.is_empty()).then(|| format!("{y:02} {row}"))
            })
            .collect::<Vec<_>>()
            .join("\n");
        (text, terminal)
    }

    fn row_is_selected(terminal: &Terminal<TestBackend>, y: u16) -> bool {
        terminal
            .backend()
            .buffer()
            .cell((0, y))
            .is_some_and(|cell| cell.style().add_modifier.contains(Modifier::REVERSED))
    }

    fn model() -> TuiModel<FakeClient> {
        TuiModel::connected(FakeClient, 0.0)
    }

    #[test]
    fn empty_dunk_view_80x24_snapshot() {
        let mut model = model();
        model.global_status = "Ready".to_string();
        let (snapshot, terminal) = snapshot(&model, 80, 24, 10.0);
        assert!(!row_is_selected(&terminal, 4));
        assert_eq!(
            snapshot,
            "00 Dunking Sheep TUI\n01 a add  d remove  c target  t test  i interval  e text  n name  o idle-gate  u s\n02 -------------------------------------------------------------------------------\n03 #    Status               Target                      Every   Sent Gate    ...\n04 (no dunkers - press a to add one, or let an agent add one)\n21 -------------------------------------------------------------------------------\n22 Ready"
        );
    }

    #[test]
    fn selected_dunk_and_status_message_80x24_snapshot() {
        let mut model = model();
        model.dunks = vec![
            json!({"id":"d1","status":"running","target_label":"w1:p1","interval_minutes":1.0,"send_count":2,"text":"one"}),
            json!({"id":"d2","status":"stopped","target_label":"w2:p2","interval_minutes":0.5,"send_count":3,"max_sends":5,"only_when":"idle","skip_if_unconsumed":true,"text":"two\nlines"}),
        ];
        model.selected = 1;
        model.global_status = "2 dunkers (1 running)".to_string();
        model.flash("changed", 8.0, 4.0);
        let (snapshot, terminal) = snapshot(&model, 80, 24, 10.0);
        assert!(!row_is_selected(&terminal, 4));
        assert!(row_is_selected(&terminal, 5));
        assert_eq!(
            snapshot,
            "00 Dunking Sheep TUI\n01 a add  d remove  c target  t test  i interval  e text  n name  o idle-gate  u s\n02 -------------------------------------------------------------------------------\n03 #    Status               Target                      Every   Sent Gate    ...\n04 d1   running              w1:p1                           1      2 -       ...\n05 d2   stopped              w2:p2                         30s    3/5 idle+ski...\n21 -------------------------------------------------------------------------------\n22 2 dunkers (1 running)  |  changed"
        );
    }

    #[test]
    fn scrolling_dunk_view_40x10_snapshot_keeps_status_message() {
        let mut model = model();
        model.dunks = (0..5)
            .map(|index| json!({"id":format!("d{index}"),"status":"running","target_label":format!("w{index}:p1"),"interval_minutes":1.0,"send_count":index,"text":format!("item {index}")}))
            .collect();
        model.selected = 4;
        model.global_status = "5 dunkers (2 running)".to_string();
        model.flash("updated", 8.0, 4.0);
        let (snapshot, terminal) = snapshot(&model, 40, 10, 10.0);
        assert!(!row_is_selected(&terminal, 4));
        assert!(row_is_selected(&terminal, 6));
        assert_eq!(
            snapshot,
            "00 Dunking Sheep TUI\n01 a add  d remove  c target  t test  i in\n02 ---------------------------------------\n03 #    Status               Target   ...\n04 d2   running              w2:p1    ...\n05 d3   running              w3:p1    ...\n06 d4   running              w4:p1    ...\n07 ---------------------------------------\n08 5 dunkers (2 running)  |  updated"
        );
    }

    #[test]
    fn empty_dunk_view_40x10_snapshot() {
        let model = model();
        let (snapshot, _) = snapshot(&model, 40, 10, 10.0);
        assert_eq!(
            snapshot,
            "00 Dunking Sheep TUI\n01 a add  d remove  c target  t test  i in\n02 ---------------------------------------\n03 #    Status               Target   ...\n04 (no dunkers - press a to add one, or le\n07 ---------------------------------------"
        );
    }

    #[test]
    fn empty_task_view_80x24_snapshot() {
        let mut model = model();
        model.global_status = "Ready".to_string();
        let (snapshot, terminal) = snapshot_tasks(&model, 80, 24, 10.0);
        assert!(!row_is_selected(&terminal, 4));
        assert_eq!(
            snapshot,
            "00 Dunking Sheep TUI - tasks\n01 a add  e title  c/space complete  s start  b block  u unblock  p priority  h he\n02 -------------------------------------------------------------------------------\n03 #     Status      Prio   Scope      Assignees          Title\n04 (no tasks - press a to add one, or n to take the next)\n21 -------------------------------------------------------------------------------\n22 Ready"
        );
    }

    #[test]
    fn selected_task_and_status_message_80x24_snapshot() {
        let mut model = model();
        model.tasks = vec![
            json!({"id":"t1","status":"open","priority":"high","scope":"project","assignees":["%1"],"title":"First task"}),
            json!({"id":"t2","status":"in_progress","priority":"medium","is_global":true,"assignees":["%2","%3"],"title":"Second task"}),
        ];
        model.task_selected = 1;
        model.global_status = "2 tasks (2 open/in-progress)".to_string();
        model.flash("changed", 8.0, 4.0);
        let (snapshot, terminal) = snapshot_tasks(&model, 80, 24, 10.0);
        assert!(!row_is_selected(&terminal, 4));
        assert!(row_is_selected(&terminal, 5));
        assert_eq!(
            snapshot,
            "00 Dunking Sheep TUI - tasks\n01 a add  e title  c/space complete  s start  b block  u unblock  p priority  h he\n02 -------------------------------------------------------------------------------\n03 #     Status      Prio   Scope      Assignees          Title\n04 t1    open        high   project    %1                 First task\n05 t2    in_progress medium global     %2,%3              Second task\n21 -------------------------------------------------------------------------------\n22 2 tasks (2 open/in-progress)  |  changed"
        );
    }

    #[test]
    fn scrolling_task_view_40x10_snapshot_keeps_status_message() {
        let mut model = model();
        model.tasks = (0..5)
            .map(|index| json!({"id":format!("t{index}"),"status":"open","priority":"low","scope":"project","assignees":[format!("%{index}")],"title":format!("Task {index}")}))
            .collect();
        model.task_selected = 4;
        model.global_status = "5 tasks (5 open/in-progress)".to_string();
        model.flash("updated", 8.0, 4.0);
        let (snapshot, terminal) = snapshot_tasks(&model, 40, 10, 10.0);
        assert!(!row_is_selected(&terminal, 4));
        assert!(row_is_selected(&terminal, 6));
        assert_eq!(
            snapshot,
            "00 Dunking Sheep TUI - tasks\n01 a add  e title  c/space complete  s sta\n02 ---------------------------------------\n03 #     Status      Prio   Scope     ...\n04 t2    open        low    project   ...\n05 t3    open        low    project   ...\n06 t4    open        low    project   ...\n07 ---------------------------------------\n08 5 tasks (5 open/in-progress)  |  update"
        );
    }

    #[test]
    fn empty_task_view_40x10_snapshot() {
        let model = model();
        let (snapshot, _) = snapshot_tasks(&model, 40, 10, 10.0);
        assert_eq!(
            snapshot,
            "00 Dunking Sheep TUI - tasks\n01 a add  e title  c/space complete  s sta\n02 ---------------------------------------\n03 #     Status      Prio   Scope     ...\n04 (no tasks - press a to add one, or n to\n07 ---------------------------------------"
        );
    }
}
