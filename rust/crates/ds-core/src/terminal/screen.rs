//! Reading a pane's screen (t1306, T33 prerequisite), slice 1: the ANSI helpers, the
//! banner/status/modal readers and the placeholder grammar. Ports, from `dunk_core.py`:
//! `ANSI_RE`, `_strip_ansi`, `_sgr_dim`, `_bright_text`, `delivery_rows`, `delivery_tail`,
//! `_banner_marker`, `_is_placeholder`, `_status_marker`, `_modal_open`, `_collapse`, and the
//! marker constants they read.
//!
//! Slice 2 adds `box_text`, `_typed_text`, `_is_our_send`, `_prompt_body`, `_is_border_line`.
//! Slice 3 adds `classify_delivery`.
//! NOT here yet: `_federated_composer`, the native-footer and styled-boundary
//! readers (`_composer_region_text`, `_styled_composer_boundary`, ...).
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

/// `COMPOSER_HINT_TAIL_ROWS`: rows at the bottom searched for the harness's empty-box hint.
pub const COMPOSER_HINT_TAIL_ROWS: usize = 5;
/// `BOX_REGION_ROWS`.
pub const BOX_REGION_ROWS: usize = 14;
/// Empty text with this detail is NOT empty-box evidence and must never authorise a send.
pub const AMBIGUOUS_COMPOSER: &str = "ambiguous composer/transcript:";
/// `MID`: the middle-dot separator that marks a status footer row.
pub const MID: &str = " \u{b7} ";
/// `CONSUMPTION_PREFIX_CHARS`.
pub const CONSUMPTION_PREFIX_CHARS: usize = 80;
/// `COLLAPSED_INPUT_MARKERS`: what an agent shows inside the box for a collapsed paste.
pub const COLLAPSED_INPUT_MARKERS: [&str; 5] = [
    "ctrl+o to expand",
    "paste again to expand",
    "pasted text",
    "pasted content",
    "[image",
];

fn decoration_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[\u{2500}-\u{259f}\u{2800}-\u{28ff}\s]+").expect("static regex"))
}

/// First `n` characters (Python `s[:n]`).
fn head(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}

/// Python `repr()` of a str, for the detail sentences tests assert on.
fn repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32))
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

fn blank(row: &str) -> bool {
    strip_ansi(row).trim().is_empty()
}

/// `_is_border_line`: a horizontal rule at least ten box-drawing dashes long.
pub fn is_border_line(line: &str) -> bool {
    let stripped = strip_ansi(line);
    let stripped = stripped.trim();
    stripped.starts_with('\u{2500}') && stripped.matches('\u{2500}').count() >= 10
}

/// `_prompt_body`: the text after the prompt sigil on one row, or `None` if the row has no
/// sigil. The sigil is detected from the PLAIN row and the text read from the BRIGHT part of it:
/// codex draws its sigil dim while the draft after it is bright, so each needs the other.
pub fn prompt_body(row: &str) -> Option<String> {
    let plain = strip_ansi(row);
    let plain = plain.trim_start_matches(|c| BOX_LEADING.contains(c));
    if !plain
        .chars()
        .next()
        .is_some_and(|c| INPUT_BOX_SIGILS.contains(&c))
    {
        return None;
    }
    let bright = decoration_re()
        .replace_all(&bright_text(row), " ")
        .into_owned();
    let body = bright.trim_start_matches(|c| BOX_LEADING.contains(c));
    let body = match body.chars().next() {
        Some(c) if INPUT_BOX_SIGILS.contains(&c) => &body[c.len_utf8()..],
        _ => body,
    };
    let body = collapse(body);
    if !body.is_empty() && is_placeholder(row) {
        // A hint drawn bright is still a hint: the box is empty.
        return Some(String::new());
    }
    Some(body)
}

/// `box_text`: what is in the input box, and how we know, as `(text, detail)`.
///
/// ONE reader, because three components answered "is there text in the box?" from something
/// other than the box. Empty text with a detail starting [`AMBIGUOUS_COMPOSER`] is an unknown
/// boundary, not an empty-box assertion.
pub fn box_text(screen: &str) -> (String, String) {
    if screen.is_empty() {
        return (String::new(), "the pane could not be read".to_string());
    }
    let all_rows = splitlines(screen);
    let rows: Vec<&str> = all_rows.iter().copied().filter(|r| !blank(r)).collect();
    let mut region_start = rows.len().saturating_sub(BOX_REGION_ROWS);
    let mut region_end = rows.len();
    let sigils: Vec<(usize, String)> = rows
        .iter()
        .enumerate()
        .filter_map(|(i, row)| prompt_body(row).map(|_| (i, (*row).to_string())))
        .collect();
    let hint_from = rows.len().saturating_sub(COMPOSER_HINT_TAIL_ROWS);

    // Footer proximity identifies the contiguous composer. BLANK ROWS MATTER HERE, and `rows`
    // has them filtered out, so this rule reads the raw lines.
    let raw_prompts: Vec<(usize, &str)> = all_rows
        .iter()
        .enumerate()
        .filter(|(_, row)| prompt_body(row).is_some())
        .map(|(i, row)| (i, *row))
        .collect();
    let footers: Vec<usize> = all_rows
        .iter()
        .enumerate()
        .filter(|(_, row)| strip_ansi(row).contains(MID) && prompt_body(row).is_none())
        .map(|(i, _)| i)
        .collect();
    // BORDERED COMPOSERS ARE NOT THIS RULE'S BUSINESS (claude keeps an empty prompt row at the
    // bottom with the draft above it); codex draws no border.
    let tail_from = all_rows.len().saturating_sub(6);
    let bordered_tail = all_rows[tail_from..].iter().any(|row| is_border_line(row));
    if !bordered_tail && !raw_prompts.is_empty() && !footers.is_empty() {
        let (last_prompt_index, last_prompt_row) = *raw_prompts.last().expect("non-empty");
        let footer_index = *footers.last().expect("non-empty");
        if footer_index > last_prompt_index {
            let between = last_prompt_index + 1..footer_index;
            let blanks = between.clone().filter(|&i| blank(all_rows[i])).count();
            if blanks >= 2 {
                let other_content = between.clone().any(|i| !blank(all_rows[i]));
                if is_placeholder(last_prompt_row) && !other_content {
                    return (
                        String::new(),
                        format!(
                            "the composer at row {} holds only the explicit harness hint, so the box is empty",
                            last_prompt_index + 1
                        ),
                    );
                }
                return (
                    String::new(),
                    format!(
                        "{AMBIGUOUS_COMPOSER} prompt row {} is separated from footer row {} by {blanks} blank rows; visible prompt content may be pending input or transcript, so no keys are authorised",
                        last_prompt_index + 1,
                        footer_index + 1
                    ),
                );
            }
            let body = prompt_body(last_prompt_row).unwrap_or_default();
            if !body.is_empty() {
                return (
                    body,
                    format!(
                        "composer: the prompt row at {} is contiguous with the status footer ({})",
                        last_prompt_index + 1,
                        footer_index + 1
                    ),
                );
            }
            return (
                String::new(),
                format!(
                    "the composer at row {} holds only the harness hint, so the box is empty",
                    last_prompt_index + 1
                ),
            );
        }
    }
    // When the live composer has borders, use those boundaries rather than the last 14 rows.
    let borders: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| is_border_line(row))
        .map(|(i, _)| i)
        .collect();
    if borders.len() >= 2 {
        let (top, bottom) = (borders[borders.len() - 2], borders[borders.len() - 1]);
        if bottom + 5 >= rows.len() && sigils.iter().any(|(i, _)| top < *i && *i < bottom) {
            region_start = top + 1;
            region_end = bottom;
        }
    }
    let in_region: Vec<&(usize, String)> = sigils
        .iter()
        .filter(|(i, _)| region_start <= *i && *i < region_end)
        .collect();
    if in_region.is_empty() {
        return (
            String::new(),
            format!(
                "no input box found (no prompt sigil in the last {BOX_REGION_ROWS} rows of {})",
                rows.len()
            ),
        );
    }
    // A PLACEHOLDER ON THE LAST PROMPT ROW MEANS THE BOX IS EMPTY.
    let (last_index, last_row) = in_region[in_region.len() - 1];
    if is_placeholder(last_row) {
        return (
            String::new(),
            format!(
                "the last prompt row (row {} of {}) holds only the harness hint ('{}'), so the box is empty and the earlier prompt lines are transcript",
                last_index + 1,
                rows.len(),
                head(&collapse(&strip_ansi(last_row)), 40)
            ),
        );
    }
    let candidates: Vec<(usize, String)> = in_region
        .iter()
        .filter_map(|(i, row)| prompt_body(row).filter(|b| !b.is_empty()).map(|b| (*i, b)))
        .collect();
    if let Some(&(first_index, _)) = candidates.first() {
        // A HINT UNDER THE CANDIDATE, WITH NO PROMPT BELOW IT, MEANS THE TEXT ABOVE IS TRANSCRIPT.
        let hint_rows: Vec<usize> = (hint_from..rows.len())
            .filter(|&i| is_placeholder(rows[i]))
            .collect();
        // ...and the hint is not directly under it (it lives one row below the line being typed).
        let near_hint: Vec<usize> = hint_rows
            .into_iter()
            .filter(|&i| i >= first_index + 2)
            .collect();
        if !near_hint.is_empty() && !sigils.iter().any(|(i, _)| first_index < *i) {
            return (
                String::new(),
                format!(
                    "the composer hint on row {} of {} ('{}') sits below the text on row {} with no prompt after it, so that text is transcript, not a draft",
                    near_hint[0] + 1,
                    rows.len(),
                    head(&collapse(&strip_ansi(rows[near_hint[0]])), 40),
                    first_index + 1
                ),
            );
        }
        // The draft STARTS at the first prompt line that carries text; later ones are
        // continuations of a multi-line draft, not a new box.
        let (index, text) = candidates[0].clone();
        let continuations: Vec<&String> = candidates[1..].iter().map(|(_, b)| b).collect();
        let below = rows[index + 1..region_end.max(index + 1)]
            .iter()
            .filter(|row| !blank(row) && prompt_body(row).is_none())
            .count();
        let mut extra = Vec::new();
        if !continuations.is_empty() {
            extra.push(format!(
                "+{} prompt line(s) below it ({}...)",
                continuations.len(),
                repr(&head(continuations[0], 40))
            ));
        }
        if below > 0 {
            extra.push(format!("+{below} indented line(s)"));
        }
        let tail = if extra.is_empty() {
            String::new()
        } else {
            format!(" {}", extra.join(", "))
        };
        return (
            text.clone(),
            format!(
                "read {} prompt line(s); text starts on row {} of {}: {}{tail}",
                in_region.len(),
                index + 1,
                rows.len(),
                repr(&head(&text, 60))
            ),
        );
    }
    let (index, _) = in_region[in_region.len() - 1];
    let hint = if in_region.iter().any(|(_, row)| is_placeholder(row)) {
        " (only the harness's own hint was there)"
    } else {
        ""
    };
    (
        String::new(),
        format!(
            "read {} prompt line(s); the last is at row {} of {} with nothing after the sigil{hint}",
            in_region.len(),
            index + 1,
            rows.len()
        ),
    )
}

/// `_typed_text`: what a human has typed into the target's input box. `""` when the box is empty
/// or shows only a hint; `None` when no input box can be found in the last
/// [`BOX_REGION_ROWS`] rows.
pub fn typed_text(screen: &str) -> Option<String> {
    if screen.is_empty() {
        return None;
    }
    let rows: Vec<&str> = splitlines(screen)
        .into_iter()
        .filter(|r| !blank(r))
        .collect();
    if !rows[rows.len().saturating_sub(BOX_REGION_ROWS)..]
        .iter()
        .any(|row| prompt_body(row).is_some())
    {
        return None;
    }
    Some(box_text(screen).0)
}

/// `_is_our_send`: is the text in the box our own previous send rather than a human's draft?
pub fn is_our_send(typed: &str, last_sent_text: Option<&str>, awaiting_consumption: bool) -> bool {
    let low = collapse(typed).to_lowercase();
    if let Some(sent) = last_sent_text.filter(|s| !s.is_empty()) {
        let full = collapse(sent).to_lowercase();
        let needle = head(&full, CONSUMPTION_PREFIX_CHARS);
        if !low.is_empty() && (full.starts_with(&low) || low.starts_with(&needle)) {
            return true;
        }
    }
    awaiting_consumption && COLLAPSED_INPUT_MARKERS.iter().any(|m| low.contains(m))
}

/// `classify_delivery`: why a pane can or cannot receive a message, as `(state, reason)`.
///
/// States: `dead`, `blocked`, `working`, `ready`. `blocked` is the one that matters: the pane
/// is alive and idle but a send would be held, so a dispatcher must count it as unavailable and
/// say why. Pure, so a canned screen can be asserted without a terminal.
pub fn classify_delivery(
    screen: &str,
    status: Option<&str>,
    typed: Option<&str>,
    our_text: Option<&str>,
    awaiting_consumption: bool,
) -> (&'static str, Option<String>) {
    if let Some(marker) = banner_marker(screen, &DEAD_SESSION_MARKERS) {
        return ("dead", Some(format!("dead session ({marker})")));
    }
    if let Some(marker) = status_marker(
        screen,
        &PAUSED_GOAL_MARKERS,
        STATUS_TAIL_LINES,
        false,
        false,
    ) {
        return (
            "blocked",
            Some(format!(
                "paused goal ({marker}) - it will not act on a message"
            )),
        );
    }
    if let Some(marker) = status_marker(
        screen,
        &CONTEXT_FULL_MARKERS,
        STATUS_TAIL_LINES,
        true,
        false,
    ) {
        return (
            "blocked",
            Some(format!("context full ({marker}) - needs a /clear first")),
        );
    }
    if let Some(progress) = status_marker(screen, &WORKING_MARKERS, STATUS_TAIL_LINES, true, false)
    {
        // The pane is mid-work: it offers an interrupt, so something is running.
        return ("working", Some(format!("mid-work ({progress})")));
    }
    if typed.is_some_and(|t| !t.is_empty())
        && !is_our_send(typed.unwrap_or(""), our_text, awaiting_consumption)
    {
        // OWNER POLICY (2026-10-24): 'a human is typing' is not a real state on this path.
        // Text we did not type is PRESERVED at send time, never a reason to refuse delivery.
        return ("ready", None);
    }
    let status_text = status.unwrap_or("").to_lowercase();
    if matches!(
        status_text.as_str(),
        "working" | "thinking" | "running" | "busy"
    ) {
        return ("working", Some(status_text));
    }
    ("ready", None)
}

#[cfg(test)]
#[path = "screen_tests.rs"]
mod screen_tests;
