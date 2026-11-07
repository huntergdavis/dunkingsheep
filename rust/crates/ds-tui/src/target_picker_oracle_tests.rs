//! Python-oracle parity for the target picker (T61, PR 2).
//!
//! Expected rows, bold runs and reverse-video runs are what
//! `DunkingSheepTui.target_modal` in dunking_sheep_tui.py draws, captured by running
//! that method under a fake curses window (no TTY). Runs are (row, first column,
//! last column), inclusive. Regenerate from the Python, not from this file.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::style::Modifier;
use ratatui::Terminal;
use serde_json::Value;

use crate::model::{TargetPicker, TargetPickerResult};

fn from_json(text: &str) -> Vec<Value> {
    serde_json::from_str(text).unwrap()
}

/// Render the picker and return the trimmed interior rows (the outer border is
/// excluded: Python draws ASCII border glyphs, ratatui draws box glyphs).
fn rows(picker: &TargetPicker, width: u16, height: u16) -> (Vec<String>, Terminal<TestBackend>) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            let area = frame.area();
            picker.render(frame, area);
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let rows = (1..height - 1)
        .map(|y| {
            (1..width - 1)
                .map(|x| buffer.cell((x, y)).unwrap().symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect();
    (rows, terminal)
}

/// Maximal runs of cells carrying `modifier`, as (row, first column, last column).
fn runs(
    terminal: &Terminal<TestBackend>,
    width: u16,
    height: u16,
    modifier: Modifier,
) -> Vec<(u16, u16, u16)> {
    let buffer = terminal.backend().buffer();
    let mut out = Vec::new();
    for y in 0..height {
        let mut start: Option<u16> = None;
        for x in 0..width {
            let on = buffer
                .cell((x, y))
                .unwrap()
                .style()
                .add_modifier
                .contains(modifier);
            match (on, start) {
                (true, None) => start = Some(x),
                (false, Some(first)) => {
                    out.push((y, first, x - 1));
                    start = None;
                }
                _ => {}
            }
        }
        if let Some(first) = start {
            out.push((y, first, width - 1));
        }
    }
    out
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn assert_screen(
    picker: &TargetPicker,
    width: u16,
    height: u16,
    expected_rows: &[&str],
    bold: &[(u16, u16, u16)],
    reversed: &[(u16, u16, u16)],
) {
    let (actual, terminal) = rows(picker, width, height);
    let actual: Vec<&str> = actual.iter().map(String::as_str).collect();
    assert_eq!(actual, expected_rows);
    assert_eq!(runs(&terminal, width, height, Modifier::BOLD), bold);
    assert_eq!(runs(&terminal, width, height, Modifier::REVERSED), reversed);
}

const PICKER_80X24_FIRST_ROWS: &[&str] = &[
    " Select target pane",
    " j/k or arrows move, Enter selects, Esc cancels",
    "     Tab                  Agent      Status    Directory",
    "",
    " ws-one",
    "     main (this)          claude     idle      /home/hunter/workspace/dunk...",
    "     logs                 -          -         /tmp/x",
    " ws-two",
    "     review               codex      working   /home/hunter/workspace/othe...",
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
];
const PICKER_80X24_FIRST_BOLD: &[(u16, u16, u16)] =
    &[(1, 2, 77), (3, 6, 77), (5, 2, 77), (8, 2, 77)];
const PICKER_80X24_FIRST_REVERSED: &[(u16, u16, u16)] = &[(6, 6, 77)];

const PICKER_80X24_DOWN_ENTER_ROWS: &[&str] = &[
    " Select target pane",
    " j/k or arrows move, Enter selects, Esc cancels",
    "     Tab                  Agent      Status    Directory",
    "",
    " ws-one",
    "     main (this)          claude     idle      /home/hunter/workspace/dunk...",
    "     logs                 -          -         /tmp/x",
    " ws-two",
    "     review               codex      working   /home/hunter/workspace/othe...",
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
];
const PICKER_80X24_DOWN_ENTER_BOLD: &[(u16, u16, u16)] =
    &[(1, 2, 77), (3, 6, 77), (5, 2, 77), (8, 2, 77)];
const PICKER_80X24_DOWN_ENTER_REVERSED: &[(u16, u16, u16)] = &[(9, 6, 77)];

const PICKER_40X10_SCROLL_ROWS: &[&str] = &[
    " Select target pane",
    " j/k or arrows move, Enter selects...",
    "     Tab                  Agent   ...",
    "",
    "     five                 -       ...",
    " ws-three",
    "     six                  -       ...",
    "",
];
const PICKER_40X10_SCROLL_BOLD: &[(u16, u16, u16)] = &[(1, 2, 37), (3, 6, 37), (6, 2, 37)];
const PICKER_40X10_SCROLL_REVERSED: &[(u16, u16, u16)] = &[(7, 6, 37)];

#[test]
fn python_oracle_picker_80x24_first() {
    let panes = from_json(
        r#"[{"pane_id": "w1:p1", "workspace_label": "ws-one", "tab_label": "main", "agent": "claude", "agent_status": "idle", "cwd": "/home/hunter/workspace/dunkingsheep", "is_self": true}, {"pane_id": "w1:p2", "workspace_label": "ws-one", "tab_label": "logs", "agent": null, "agent_status": null, "cwd": "/tmp/x"}, {"pane_id": "w2:p1", "workspace_label": "ws-two", "tab_label": "review", "agent": "codex", "agent_status": "working", "cwd": "/home/hunter/workspace/other-long-directory-name-to-clip-at-the-edge-of-the-screen/sub"}]"#,
    );
    let mut picker = TargetPicker::new(panes);
    assert_screen(
        &picker,
        80,
        24,
        PICKER_80X24_FIRST_ROWS,
        PICKER_80X24_FIRST_BOLD,
        PICKER_80X24_FIRST_REVERSED,
    );
    assert!(matches!(
        picker.handle_key(key(KeyCode::Esc)),
        TargetPickerResult::Cancelled
    ));
}

#[test]
fn python_oracle_picker_80x24_down_enter() {
    let panes = from_json(
        r#"[{"pane_id": "w1:p1", "workspace_label": "ws-one", "tab_label": "main", "agent": "claude", "agent_status": "idle", "cwd": "/home/hunter/workspace/dunkingsheep", "is_self": true}, {"pane_id": "w1:p2", "workspace_label": "ws-one", "tab_label": "logs", "agent": null, "agent_status": null, "cwd": "/tmp/x"}, {"pane_id": "w2:p1", "workspace_label": "ws-two", "tab_label": "review", "agent": "codex", "agent_status": "working", "cwd": "/home/hunter/workspace/other-long-directory-name-to-clip-at-the-edge-of-the-screen/sub"}]"#,
    );
    let mut picker = TargetPicker::new(panes);
    picker.handle_key(key(KeyCode::Down));
    picker.handle_key(key(KeyCode::Down));
    assert_screen(
        &picker,
        80,
        24,
        PICKER_80X24_DOWN_ENTER_ROWS,
        PICKER_80X24_DOWN_ENTER_BOLD,
        PICKER_80X24_DOWN_ENTER_REVERSED,
    );
    assert!(
        matches!(picker.handle_key(key(KeyCode::Enter)), TargetPickerResult::Selected(ref pane) if pane["pane_id"] == "w2:p1")
    );
}

#[test]
fn python_oracle_picker_40x10_scroll() {
    let panes = from_json(
        r#"[{"pane_id": "w1:p1", "workspace_label": "ws-one", "tab_label": "main", "agent": null, "agent_status": null, "cwd": "/a"}, {"pane_id": "w1:p2", "workspace_label": "ws-one", "tab_label": "two", "agent": null, "agent_status": null, "cwd": "/b"}, {"pane_id": "w1:p3", "workspace_label": "ws-one", "tab_label": "three", "agent": null, "agent_status": null, "cwd": "/c"}, {"pane_id": "w2:p1", "workspace_label": "ws-two", "tab_label": "four", "agent": "claude", "agent_status": "idle", "cwd": "/d"}, {"pane_id": "w2:p2", "workspace_label": "ws-two", "tab_label": "five", "agent": null, "agent_status": null, "cwd": "/e"}, {"pane_id": "w3:p1", "workspace_label": "ws-three", "tab_label": "six", "agent": null, "agent_status": null, "cwd": "/f"}]"#,
    );
    let mut picker = TargetPicker::new(panes);
    picker.handle_key(key(KeyCode::Down));
    picker.handle_key(key(KeyCode::Down));
    picker.handle_key(key(KeyCode::Down));
    picker.handle_key(key(KeyCode::Down));
    picker.handle_key(key(KeyCode::Down));
    picker.handle_key(key(KeyCode::Down));
    picker.handle_key(key(KeyCode::Down));
    assert_screen(
        &picker,
        40,
        10,
        PICKER_40X10_SCROLL_ROWS,
        PICKER_40X10_SCROLL_BOLD,
        PICKER_40X10_SCROLL_REVERSED,
    );
    assert!(
        matches!(picker.handle_key(key(KeyCode::Enter)), TargetPickerResult::Selected(ref pane) if pane["pane_id"] == "w3:p1")
    );
}
