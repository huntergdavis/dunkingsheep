//! The composer boundary readers (t1306, T33 prerequisite, slice 4): the native Codex footer
//! shapes, the styled (SGR-state) boundary, the composer region and the federated composer.
//! Ports, from `dunk_core.py`: `NATIVE_*_RE`, `_native_banner/status/hints/warning`,
//! `_native_footer_start`, `_styled_sigil_state`, `_styled_composer_boundary`,
//! `_trailing_prompt_region`, `_native_prefix_accounted`, `_composer_region_text` and
//! `Flock._federated_composer` (as a pure function over the `federated_input` flag).
//!
//! The rule these keep: FAIL CLOSED. A single frame cannot tell a transcript echo from edited
//! composer content, so anything that does not match an exact product shape keeps the
//! conservative behaviour (CoS i31543/i31555: never submit on an undelimited frame).

use std::sync::OnceLock;

use regex::Regex;

use super::screen::{is_placeholder, modal_open, prompt_body, splitlines, strip_ansi, MID};
use crate::stuck;

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("static regex"))
}

fn plain_trim(row: &str) -> String {
    strip_ansi(row).trim().to_string()
}

fn blank(row: &str) -> bool {
    plain_trim(row).is_empty()
}

/// `_native_banner`: the exact product banner `>_ OpenAI Codex (v0.165.0)`, not a prompt.
pub fn native_banner(row: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"^>_\s+OpenAI Codex(?:\s+\(v[0-9][0-9.]*\))?$").is_match(&plain_trim(row))
}

/// `_native_status`: `<model> · <~/cwd>`.
pub fn native_status(row: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"^\S[\w .+/-]{0,40}\s\u{b7}\s(?:~|/)\S*$").is_match(&plain_trim(row))
}

/// `_native_hints`: `? for shortcuts` / `← for agents`.
pub fn native_hints(row: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"^(?:\? for shortcuts|\u{2190} for agents)\b").is_match(&plain_trim(row))
}

/// `_native_warning`: the right-aligned warning counter, singular or plural, exact suffix.
pub fn native_warning(row: &str) -> bool {
    static R: OnceLock<Regex> = OnceLock::new();
    re(&R, r"^\u{26a0}\s+\d+\s+warnings?\s+\u{b7}\s+f2 to view$").is_match(&plain_trim(row))
}

/// `_native_footer_start`: the index of the native two-row footer's first row, or `None`.
/// Both rows must match their product shape AND be the last two non-blank rows, and a BLANK row
/// must separate the pair from the composer (a human continuation is contiguous).
pub fn native_footer_start(raw: &[&str], nonblank: &[usize]) -> Option<usize> {
    if nonblank.len() < 2 {
        return None;
    }
    let (first, second) = (nonblank[nonblank.len() - 2], nonblank[nonblank.len() - 1]);
    if !native_status(raw[first]) {
        return None;
    }
    if !(native_hints(raw[second]) || native_warning(raw[second])) {
        return None;
    }
    if first == 0 || !blank(raw[first - 1]) {
        return None;
    }
    Some(first)
}

/// `_styled_sigil_state`: classify a prompt-sigil row by the SGR state IN EFFECT AT THE SIGIL
/// POSITION. `"bold"` = composer candidate, `"dim"` = transcript echo (bold+dim), `"unknown"` =
/// fail closed. SGR parameters apply LEFT TO RIGHT; 22 cancels BOTH bold and dim; the colon form
/// is not matched (documented safe refusal).
pub fn styled_sigil_state(row: &str) -> &'static str {
    static R: OnceLock<Regex> = OnceLock::new();
    let sgr = re(&R, r"\x1b\[([0-9;]*)m");
    let (mut dim, mut bold) = (false, false);
    let mut pos = 0;
    let mut broke = false;
    for found in sgr.captures_iter(row) {
        let whole = found.get(0).expect("group 0");
        if row[pos..whole.start()].contains('\u{203a}') {
            broke = true; // sigil reached: the current state is decisive
            break;
        }
        for code in found[1].split(';') {
            match code {
                "0" | "22" => {
                    dim = false;
                    bold = false;
                }
                "2" => dim = true,
                "1" => bold = true,
                _ => {}
            }
        }
        pos = whole.end();
    }
    if !broke && !row[pos..].contains('\u{203a}') {
        return "unknown"; // no sigil anywhere in the row
    }
    if bold && dim {
        "dim"
    } else if bold {
        "bold"
    } else {
        "unknown"
    }
}

/// `_styled_composer_boundary`: `("delimited", region)` or `("undelimitable", "")` using CAPTURED
/// STYLE STATE, for the multi-sigil frames the plain boundary fails closed on.
pub fn styled_composer_boundary(screen: &str) -> (&'static str, String) {
    const NO: (&str, String) = ("undelimitable", String::new());
    if screen.is_empty() {
        return NO;
    }
    let raw = splitlines(screen);
    let nonblank: Vec<usize> = (0..raw.len()).filter(|&i| !blank(raw[i])).collect();
    let Some(&last_row) = nonblank.last() else {
        return NO;
    };
    if !strip_ansi(raw[last_row]).contains(MID) || prompt_body(raw[last_row]).is_some() {
        return NO;
    }
    let sigils: Vec<(usize, &'static str)> = (0..last_row)
        .filter(|&i| prompt_body(raw[i]).is_some())
        .map(|i| (i, styled_sigil_state(raw[i])))
        .collect();
    if sigils.is_empty() {
        return NO;
    }
    let candidates: Vec<usize> = sigils
        .iter()
        .filter(|(_, s)| *s == "bold")
        .map(|(i, _)| *i)
        .collect();
    if candidates.len() != 1 || candidates[0] != sigils[sigils.len() - 1].0 {
        return NO;
    }
    let composer_row = candidates[0];
    let footer_rows: Vec<usize> = nonblank
        .iter()
        .copied()
        .filter(|&i| {
            i > composer_row && strip_ansi(raw[i]).contains(MID) && prompt_body(raw[i]).is_none()
        })
        .collect();
    if footer_rows.len() != 1 {
        return NO;
    }
    let footer = footer_rows[0];
    let mut parts: Vec<String> = Vec::new();
    if let Some(body) = prompt_body(raw[composer_row]).filter(|b| !b.is_empty()) {
        parts.push(body);
    }
    for row in &raw[composer_row + 1..footer] {
        let text = plain_trim(row);
        if !text.is_empty() {
            parts.push(text);
        }
    }
    ("delimited", parts.join(" "))
}

/// `_trailing_prompt_region`: the complete trailing region for the native-bound recovery path
/// only (earlier rows must additionally be accounted by the exact native transcript).
pub fn trailing_prompt_region(screen: &str) -> String {
    let rows = splitlines(screen);
    let nonblank: Vec<usize> = (0..rows.len()).filter(|&i| !blank(rows[i])).collect();
    let Some(&last) = nonblank.last() else {
        return String::new();
    };
    let end = match native_footer_start(&rows, &nonblank) {
        Some(end) => end,
        None => {
            if !strip_ansi(rows[last]).contains(MID) || prompt_body(rows[last]).is_some() {
                return String::new();
            }
            last
        }
    };
    let sigils: Vec<usize> = (0..end)
        .filter(|&i| prompt_body(rows[i]).is_some() && !native_banner(rows[i]))
        .collect();
    let Some(&composer) = sigils.last() else {
        return String::new();
    };
    let mut parts = vec![prompt_body(rows[composer]).unwrap_or_default()];
    parts.extend(rows[composer + 1..end].iter().map(|row| plain_trim(row)));
    parts.join(" ")
}

/// `_native_prefix_accounted`: an earlier prompt or human continuation cannot disappear at
/// extraction.
pub fn native_prefix_accounted(screen: &str, native_text: &str) -> bool {
    static WORKED: OnceLock<Regex> = OnceLock::new();
    let worked = re(
        &WORKED,
        r"^Worked for (?:\d+[hms] ?)+ \u{2022} \d{1,2}:\d{2} [AP]M$",
    );
    let rows = splitlines(screen);
    let sigils: Vec<usize> = (0..rows.len())
        .filter(|&i| prompt_body(rows[i]).is_some() && !native_banner(rows[i]))
        .collect();
    let Some(&composer) = sigils.last() else {
        return false;
    };
    let transcript = stuck::collapse(native_text);
    let mut cursor = 0;
    let mut worked_rows = 0;
    for row in &rows[..composer] {
        let mut text = plain_trim(row);
        if text.is_empty() {
            continue;
        }
        if worked.is_match(&text) {
            worked_rows += 1;
            if worked_rows > 1 {
                return false;
            }
            continue;
        }
        if let Some(body) = prompt_body(row) {
            text = body
                .strip_suffix('\u{2026}')
                .unwrap_or(&body)
                .trim()
                .to_string();
        } else if let Some(rest) = text.strip_prefix("\u{2022} ") {
            text = rest.to_string();
        }
        if text.is_empty() {
            return false;
        }
        let text = stuck::collapse(&text);
        match transcript[cursor..].find(&text) {
            Some(found) => cursor += found + text.len(),
            None => return false,
        }
    }
    true
}

/// `_composer_region_text`: the live composer's visible content, or `""` when it cannot be
/// delimited (FAIL CLOSED).
pub fn composer_region_text(screen: &str) -> String {
    if screen.is_empty() {
        return String::new();
    }
    let raw = splitlines(screen);
    let nonblank: Vec<usize> = (0..raw.len()).filter(|&i| !blank(raw[i])).collect();
    let Some(&last) = nonblank.last() else {
        return String::new();
    };
    let end = match native_footer_start(&raw, &nonblank) {
        Some(end) => end,
        None => {
            // the last visible row must be a status footer
            if !strip_ansi(raw[last]).contains(MID) || prompt_body(raw[last]).is_some() {
                return String::new();
            }
            last
        }
    };
    // The native banner is not a prompt; every OTHER sigil-shaped row still counts.
    let sigils: Vec<usize> = (0..raw.len())
        .filter(|&i| prompt_body(raw[i]).is_some() && i < end && !native_banner(raw[i]))
        .collect();
    if sigils.len() != 1 {
        return String::new(); // zero or several sigil candidates: fail closed
    }
    let composer = sigils[0];
    let mut parts: Vec<String> = Vec::new();
    if let Some(body) = prompt_body(raw[composer]).filter(|b| !b.is_empty()) {
        parts.push(body);
    }
    for row in &raw[composer + 1..end] {
        let text = plain_trim(row);
        if !text.is_empty() {
            parts.push(text);
        }
    }
    parts.join(" ")
}

/// `Flock._federated_composer`: authorized live input under the explicit all-input-via-DS
/// policy (`federated_input` is `DUNKINGSHEEP_FEDERATED_INPUT=1`). NOT an ownership assertion:
/// the owner authorizes preserving and submitting the existing composer, even when scrolled.
/// `None` = not a federated frame.
pub fn federated_composer(federated_input: bool, screen: &str) -> Option<String> {
    if !federated_input || screen.is_empty() || modal_open(screen) {
        return None;
    }
    let rows = splitlines(screen);
    let nonblank: Vec<usize> = (0..rows.len()).filter(|&i| !blank(rows[i])).collect();
    let mut end = *nonblank.last()?;
    // Installed native Codex appends its session title after cwd.
    let status_of = |row: &str| {
        let plain = plain_trim(row);
        let two: Vec<&str> = plain.split(" \u{b7} ").take(2).collect();
        native_status(&two.join(" \u{b7} "))
    };
    if (native_hints(rows[end]) || native_warning(rows[end])) && nonblank.len() > 1 {
        end = nonblank[nonblank.len() - 2];
    }
    if !status_of(rows[end]) {
        return None;
    }
    let sigils: Vec<(usize, &'static str)> = (0..end)
        .filter(|&i| prompt_body(rows[i]).is_some() && !native_banner(rows[i]))
        .map(|i| (i, styled_sigil_state(rows[i])))
        .collect();
    let (start, last_state) = *sigils.last()?;
    if last_state != "bold" || sigils[..sigils.len() - 1].iter().any(|(_, s)| *s != "dim") {
        return None;
    }
    let mut parts = vec![prompt_body(rows[start]).unwrap_or_default()];
    parts.extend(rows[start + 1..end].iter().map(|row| plain_trim(row)));
    let text = parts
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    Some(if is_placeholder(&text) {
        String::new()
    } else {
        text
    })
}

#[cfg(test)]
#[path = "composer_tests.rs"]
mod composer_tests;
