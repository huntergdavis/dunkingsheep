//! Reading a pane's screen (t1306, T33 prerequisite), slice 1: the ANSI helpers, the
//! banner/status/modal readers and the placeholder grammar. Ports, from `dunk_core.py`:
//! `ANSI_RE`, `_strip_ansi`, `_sgr_dim`, `_bright_text`, `delivery_rows`, `delivery_tail`,
//! `_banner_marker`, `_is_placeholder`, `_status_marker`, `_modal_open`, `_collapse`, and the
//! marker constants they read.
//!
//! NOT here yet: `box_text`, `_typed_text`, `_is_our_send`, `classify_delivery`,
//! `_federated_composer` (the composer readers, next slices).
//!
//! The rule these keep: ONLY THE BOTTOM OF THE SCREEN IS THE LIVE INTERFACE. Scanning the
//! whole scrollback made a pane look dead because its transcript quoted the phrase, and
//! pressed Escape into working agents 1042 times.

use std::sync::OnceLock;

use regex::Regex;

pub const MODAL_TAIL_LINES: usize = 8;
pub const MODAL_MARKERS: [&str; 6] = [
    "esc to cancel",
    "esc to close",
    "esc to exit",
    "enter to confirm",
    "press enter to continue",
    "to go back",
];
pub const DEAD_SESSION_MARKERS: [&str; 4] = [
    "server connection could not be restored",
    "automatic reconnect could not restore this session",
    "reconnect failed",
    "session expired",
];
pub const DELIVERY_TAIL_LINES: usize = 12;
pub const STATUS_TAIL_LINES: usize = 3;
pub const WORKING_MARKERS: [&str; 3] = ["esc to interrupt", "esc to cancel", "ctrl+c to interrupt"];
pub const PAUSED_GOAL_MARKERS: [&str; 3] = ["goal paused", "paused (/goal resume)", "/goal resume"];
pub const CONTEXT_FULL_MARKERS: [&str; 3] =
    ["/clear to save", "context is full", "context limit reached"];
/// `INPUT_BOX_SIGILS`.
pub const INPUT_BOX_SIGILS: [char; 3] = ['\u{276f}', '\u{203a}', '>'];
/// `BOX_LEADING`: borders and padding that sit BEFORE the prompt sigil.
pub const BOX_LEADING: &str = " \t\u{2502}\u{2503}\u{2506}\u{250a}\u{2551}|";

fn ansi_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\x1b\[[0-9;?]*[ -/]*[@-~]").expect("static regex"))
}

fn placeholder_patterns() -> &'static [Regex] {
    static RE: OnceLock<Vec<Regex>> = OnceLock::new();
    RE.get_or_init(|| {
        [
            r"(?i)new task\?\s*/clear to save [\d.,]+[km]? tokens?",
            r"(?i)^\s*(new task\?|type your (request|message)\b)",
            r"(?i)^\s*ask (codex|claude|open ?code) to do anything",
            r"(?i)^\s*tab to (queue message|send|interrupt|search)\b",
            r"(?i)^\s*(send a message|type a message|ask anything)\b",
        ]
        .iter()
        .map(|p| Regex::new(p).expect("static regex"))
        .collect()
    })
}

/// Python `str.splitlines()`: also splits on \x0b \x0c \x1c-\x1e \x85     and a
/// lone \r. A pane read can carry any of them.
pub fn splitlines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let is_break = matches!(
            c,
            '\n' | '\r'
                | '\x0b'
                | '\x0c'
                | '\x1c'
                | '\x1d'
                | '\x1e'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        );
        if !is_break {
            continue;
        }
        lines.push(&text[start..i]);
        let mut end = i + c.len_utf8();
        if c == '\r' {
            if let Some(&(_, '\n')) = chars.peek() {
                chars.next();
                end += 1;
            }
        }
        start = end;
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

/// `_strip_ansi`: plain text of an ANSI-styled screen.
pub fn strip_ansi(text: &str) -> String {
    ansi_re().replace_all(text, "").into_owned()
}

/// `_collapse`: whitespace-insensitive form, so wrapped lines still match.
pub fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `_sgr_dim`: apply one SGR parameter list to the current dim state.
fn sgr_dim(params: &[&str], mut dim: bool) -> bool {
    if params.is_empty() {
        return false;
    }
    let mut index = 0;
    while index < params.len() {
        match params[index] {
            "" | "0" => dim = false,
            "2" => dim = true,
            "22" => dim = false,
            "38" | "48" | "58" => {
                // Extended colour: skip its sub-parameters so "38;2;r;g;b" is not dim.
                if params.get(index + 1) == Some(&"5") {
                    index += 1;
                } else if params.get(index + 1) == Some(&"2") {
                    index += 4;
                }
            }
            _ => {}
        }
        index += 1;
    }
    dim
}

/// `_bright_text`: the characters of one ANSI-styled line that are not rendered dim.
pub fn bright_text(line: &str) -> String {
    let mut out = String::new();
    let mut dim = false;
    let mut position = 0;
    for found in ansi_re().find_iter(line) {
        if !dim {
            out.push_str(&line[position..found.start()]);
        }
        let sequence = found.as_str();
        if sequence.ends_with('m') {
            let params: Vec<&str> = sequence[2..sequence.len() - 1].split(';').collect();
            dim = sgr_dim(&params, dim);
        }
        position = found.end();
    }
    if !dim {
        out.push_str(&line[position..]);
    }
    out
}

/// `delivery_rows`: non-blank rows with colour codes removed (a banner starts with an escape
/// sequence before its glyph).
pub fn delivery_rows(screen: &str) -> Vec<String> {
    let plain = strip_ansi(screen);
    splitlines(&plain)
        .into_iter()
        .filter(|row| !row.trim().is_empty())
        .map(|row| row.trim_end().to_string())
        .collect()
}

/// `delivery_tail`: the last non-blank lines of a pane, lowercased - where state banners live.
pub fn delivery_tail(screen: &str, lines: usize) -> String {
    let rows: Vec<&str> = splitlines(screen)
        .into_iter()
        .filter(|row| !row.trim().is_empty())
        .collect();
    rows[rows.len().saturating_sub(lines)..]
        .join("\n")
        .to_lowercase()
}

/// `_banner_marker`: a marker that STARTS its own line is a banner; one mid-sentence is prose.
pub fn banner_marker(screen: &str, markers: &[&str]) -> Option<String> {
    const LEAD: &str = " \t\u{2500}\u{2502}\u{25a0}\u{25cf}\u{2022}*\u{b7}-\u{2014}>|#~[]";
    for row in delivery_rows(screen) {
        let stripped = row.trim_start_matches(|c| LEAD.contains(c)).to_lowercase();
        for marker in markers {
            if stripped.starts_with(marker) {
                return Some((*marker).to_string());
            }
        }
    }
    None
}

/// `_is_placeholder`: the harness's empty-box hint, not a state. The hint is drawn INSIDE the
/// composer, so the box border and prompt sigil are stripped first.
pub fn is_placeholder(text: &str) -> bool {
    let plain = strip_ansi(text);
    let plain = plain
        .trim()
        .trim_start_matches(|c| BOX_LEADING.contains(c))
        .trim();
    let plain = plain
        .trim_start_matches(|c| INPUT_BOX_SIGILS.contains(&c))
        .trim();
    placeholder_patterns().iter().any(|p| p.is_match(plain))
}

/// `_status_marker`: a status decoration on the composer line. Only the last `lines` non-blank
/// rows count, and only NON-DIM text unless `dim_ok` (a placeholder is drawn dim).
pub fn status_marker(
    screen: &str,
    markers: &[&str],
    lines: usize,
    placeholder_safe: bool,
    dim_ok: bool,
) -> Option<String> {
    // Raw rows, NOT delivery_rows: the dim/not-dim distinction is carried by the escapes.
    let raw: Vec<&str> = splitlines(screen)
        .into_iter()
        .filter(|row| !strip_ansi(row).trim().is_empty())
        .collect();
    let tail = &raw[raw.len().saturating_sub(lines)..];
    for marker in markers {
        for row in tail {
            let plain = if dim_ok {
                strip_ansi(row)
            } else {
                strip_ansi(&bright_text(row))
            }
            .to_lowercase();
            if !plain.contains(marker) {
                continue;
            }
            if placeholder_safe && is_placeholder(row) {
                continue;
            }
            return Some((*marker).to_string());
        }
    }
    None
}

/// `_modal_open`: is the pane showing a panel or dialog RIGHT NOW? Only the bottom counts.
pub fn modal_open(screen: &str) -> bool {
    if screen.is_empty() {
        return false;
    }
    let plain = strip_ansi(screen);
    let lines: Vec<&str> = splitlines(&plain)
        .into_iter()
        .filter(|line| !line.trim().is_empty())
        .collect();
    // t405: native model/effort pickers advertise these exact control rows; require the live
    // bottom row so an old footer echoed above the composer is not a modal.
    if let Some(last) = lines.last() {
        let last = collapse(last).to_lowercase();
        if last == "enter select \u{b7} esc back"
            || last == "enter default \u{b7} s session \u{b7} esc back"
        {
            return true;
        }
    }
    let low = lines[lines.len().saturating_sub(MODAL_TAIL_LINES)..]
        .join("\n")
        .to_lowercase();
    MODAL_MARKERS.iter().any(|marker| low.contains(marker))
}

#[cfg(test)]
#[path = "screen_tests.rs"]
mod screen_tests;
