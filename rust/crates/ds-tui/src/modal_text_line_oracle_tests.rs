//! Python-oracle parity for the line modal and the multiline text modal (T61, PR 3).
//!
//! Expected screens, bold runs and results are what `DunkingSheepTui.line_modal` and
//! `text_modal` in dunking_sheep_tui.py produce, captured by running those methods
//! under a fake curses window (no TTY). Box glyphs are normalised before comparison,
//! because Python draws ASCII '|' and '-' and ratatui draws box glyphs. Regenerate
//! from the Python, not from this file.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::{Frame, Terminal};

use crate::model::{LineModal, LineModalResult, TextModal, TextModalResult};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn ctrl(character: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(character), KeyModifiers::CONTROL)
}

/// Map box-drawing glyphs to the ASCII the Python harness draws.
fn normalize(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\u{2502}' => '|',
            '\u{2500}' | '\u{250c}' | '\u{2510}' | '\u{2514}' | '\u{2518}' => '-',
            other => other,
        })
        .collect()
}

/// Render with `draw` and return normalised, right-trimmed rows plus the terminal.
fn screen(
    width: u16,
    height: u16,
    draw: impl FnOnce(&mut Frame<'_>, Rect),
) -> (Vec<String>, Terminal<TestBackend>) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            let area = frame.area();
            draw(frame, area);
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let rows = (0..height)
        .map(|y| {
            let row: String = (0..width)
                .map(|x| buffer.cell((x, y)).unwrap().symbol())
                .collect();
            normalize(&row).trim_end().to_string()
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

fn assert_screen(actual: &[String], expected: &[&str]) {
    let actual: Vec<&str> = actual.iter().map(String::as_str).collect();
    assert_eq!(actual, expected);
}

const LINE_80X24_TYPED_ROWS: &[&str] = &[
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "",
    "            --------------------------------------------------------",
    "            | Interval (minutes, or e.g. 90s / 1.5h)               |",
    "            | Current: 10                                          |",
    "            | Enter keeps current, Esc cancels                     |",
    "            |                                                      |",
    "            | abc                                                  |",
    "            --------------------------------------------------------",
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
const LINE_80X24_TYPED_BOLD: &[(u16, u16, u16)] = &[(9, 14, 65)];

const LINE_40X10_CLIP_ROWS: &[&str] = &[
    "",
    "  ------------------------------------",
    "  | Interval (minutes, or e.g. 90... |",
    "  | Current: a very long current ... |",
    "  | Enter keeps current, Esc cancels |",
    "  |                                  |",
    "  |                                  |",
    "  ------------------------------------",
    "",
    "",
];
const LINE_40X10_CLIP_BOLD: &[(u16, u16, u16)] = &[(2, 4, 35)];

const TEXT_80X24_EDIT_ROWS: &[&str] = &[
    "--------------------------------------------------------------------------------",
    "| Edit text to send                                                            |",
    "| Ctrl+G saves, Esc cancels  Line 3/3  Col 7  Placeholders: {id} {name} {co... |",
    "| line one                                                                     |",
    "| line two is longer                                                           |",
    "| threeX                                                                       |",
    "|                                                                              |",
    "|                                                                              |",
    "|                                                                              |",
    "|                                                                              |",
    "|                                                                              |",
    "|                                                                              |",
    "|                                                                              |",
    "|                                                                              |",
    "|                                                                              |",
    "|                                                                              |",
    "|                                                                              |",
    "|                                                                              |",
    "|                                                                              |",
    "|                                                                              |",
    "|                                                                              |",
    "|                                                                              |",
    "|                                                                              |",
    "--------------------------------------------------------------------------------",
];
const TEXT_80X24_EDIT_BOLD: &[(u16, u16, u16)] = &[(1, 2, 77)];

const TEXT_40X10_PGUP_CANCEL_ROWS: &[&str] = &[
    "----------------------------------------",
    "| Edit text to send                    |",
    "| Ctrl+G saves, Esc cancels  Line 1... |",
    "| l15                                  |",
    "| l16                                  |",
    "| l17                                  |",
    "| l18                                  |",
    "| l19                                  |",
    "|                                      |",
    "----------------------------------------",
];
const TEXT_40X10_PGUP_CANCEL_BOLD: &[(u16, u16, u16)] = &[(1, 2, 37)];

#[test]
fn python_oracle_line_80x24_typed() {
    let mut modal = LineModal::new("Interval (minutes, or e.g. 90s / 1.5h)", "10");
    modal.handle_key(key(KeyCode::Char('a')));
    modal.handle_key(key(KeyCode::Char('b')));
    modal.handle_key(key(KeyCode::Char('c')));
    let (actual, terminal) = screen(80, 24, |frame, area| modal.render(frame, area));
    assert_screen(&actual, LINE_80X24_TYPED_ROWS);
    assert_eq!(
        runs(&terminal, 80, 24, Modifier::BOLD),
        LINE_80X24_TYPED_BOLD
    );
    assert!(
        matches!(modal.handle_key(key(KeyCode::Enter)), LineModalResult::Submitted(ref value) if value == "abc")
    );
}

#[test]
fn python_oracle_line_40x10_clip() {
    let mut modal = LineModal::new(
        "Interval (minutes, or e.g. 90s / 1.5h) and more words here",
        "a very long current value that exceeds the box width for sure yes",
    );
    let (actual, terminal) = screen(40, 10, |frame, area| modal.render(frame, area));
    assert_screen(&actual, LINE_40X10_CLIP_ROWS);
    assert_eq!(
        runs(&terminal, 40, 10, Modifier::BOLD),
        LINE_40X10_CLIP_BOLD
    );
    assert!(
        matches!(modal.handle_key(key(KeyCode::Enter)), LineModalResult::Submitted(ref value) if value == "a very long current value that exceeds the box width for sure yes")
    );
}

#[test]
fn python_oracle_text_80x24_edit() {
    let mut modal = TextModal::new("line one\nline two is longer\nthree");
    let _ = screen(80, 24, |frame, area| modal.render(frame, area));
    modal.handle_key(key(KeyCode::Down), TextModal::text_rows(24));
    let _ = screen(80, 24, |frame, area| modal.render(frame, area));
    modal.handle_key(key(KeyCode::End), TextModal::text_rows(24));
    let _ = screen(80, 24, |frame, area| modal.render(frame, area));
    modal.handle_key(key(KeyCode::Char('X')), TextModal::text_rows(24));
    let (actual, terminal) = screen(80, 24, |frame, area| modal.render(frame, area));
    assert_screen(&actual, TEXT_80X24_EDIT_ROWS);
    assert_eq!(
        runs(&terminal, 80, 24, Modifier::BOLD),
        TEXT_80X24_EDIT_BOLD
    );
    assert!(
        matches!(modal.handle_key(ctrl('g'), TextModal::text_rows(24)), TextModalResult::Submitted(ref value) if value == "line one\nline two is longer\nthreeX")
    );
}

#[test]
fn python_oracle_text_40x10_pgup_cancel() {
    let mut modal = TextModal::new("l01\nl02\nl03\nl04\nl05\nl06\nl07\nl08\nl09\nl10\nl11\nl12\nl13\nl14\nl15\nl16\nl17\nl18\nl19\nl20");
    let _ = screen(40, 10, |frame, area| modal.render(frame, area));
    modal.handle_key(key(KeyCode::PageUp), TextModal::text_rows(10));
    let (actual, terminal) = screen(40, 10, |frame, area| modal.render(frame, area));
    assert_screen(&actual, TEXT_40X10_PGUP_CANCEL_ROWS);
    assert_eq!(
        runs(&terminal, 40, 10, Modifier::BOLD),
        TEXT_40X10_PGUP_CANCEL_BOLD
    );
    assert!(matches!(
        modal.handle_key(key(KeyCode::Esc), TextModal::text_rows(10)),
        TextModalResult::Cancelled
    ));
}
