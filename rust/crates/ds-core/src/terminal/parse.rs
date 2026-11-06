//! Parsing tmux's `list-panes -F PANE_FORMAT` output (T21).
//!
//! Ported from `TmuxBackend._parse_panes` plus `_agent_kind` and `_safe_name`. The
//! fields come back separated by [`pane_separator`] on one line per pane, positionally.
//!
//! The rule this file exists to keep: EVERY FIELD IS PARSED ON ITS OWN. The Python's
//! old single try/except meant a blank `session_index` (tmux leaves it empty when a
//! session has no index set) raised on the first `int()` and silently nulled
//! `pane_pid` with it - which is why capture could not identify a lane's agent
//! process, and therefore why `session_id` was null for every live pane on
//! 2026-10-24. A blank field is that field's absence, never another field's.

use crate::terminal::{Pane, Tab, Workspace};

/// Fields in one pane line, as `pane_format` defines them (the Python's `PANE_FORMAT`).
pub const PANE_FIELD_COUNT: usize = 14;
/// Fields in one tab line: window id, session id, name, index, active.
pub const TAB_FIELD_COUNT: usize = 5;
/// Fields in one workspace line: session id, name, index.
pub const WORKSPACE_FIELD_COUNT: usize = 3;

/// The field separator for tmux format output: a printable token, random for each process.
///
/// tmux 3.4 writes a control character in format output as a backslash escape (PR 752), so a
/// raw 0x1f split nothing and every pane line was dropped. A printable token is not escaped on
/// any tmux version. It is random per process, so a pane title does not carry it by chance. A
/// title that does carry it adds fields, and the listing is refused rather than misread.
pub fn pane_separator() -> &'static str {
    static TOKEN: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TOKEN.get_or_init(random_token).as_str()
}

/// Twenty-four ASCII letters and digits from the OS random source. The fallback hashes a
/// per-process `RandomState`, which is itself seeded from the OS, so it stays unpredictable.
fn random_token() -> String {
    use std::io::Read;
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = [0u8; 24];
    let from_os =
        std::fs::File::open("/dev/urandom").and_then(|mut file| file.read_exact(&mut bytes));
    if from_os.is_err() {
        use std::hash::{BuildHasher, Hasher};
        let state = std::collections::hash_map::RandomState::new();
        for (index, byte) in bytes.iter_mut().enumerate() {
            let mut hasher = state.build_hasher();
            hasher.write_usize(index);
            *byte = hasher.finish().to_le_bytes()[0];
        }
    }
    bytes
        .iter()
        .map(|byte| char::from(ALPHABET[usize::from(*byte) % ALPHABET.len()]))
        .collect()
}

/// A listing refused because one of its lines did not have the format's field count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListingError {
    /// The 1-based line number of the first line that did not parse.
    pub line: usize,
    /// The fields that line split into.
    pub fields: usize,
    /// The fields the format defines.
    pub expected: usize,
}

impl std::fmt::Display for ListingError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "tmux listing refused: line {} has {} fields, expected {}",
            self.line, self.fields, self.expected
        )
    }
}

impl std::error::Error for ListingError {}

/// Split a listing into rows of exactly `expected` fields. Blank lines are skipped. Any other
/// line with a different field count refuses the whole listing: a partial row, or a row a title
/// split with the separator, cannot be read as the rows that were asked for.
fn split_listing(output: &str, expected: usize) -> Result<Vec<Vec<&str>>, ListingError> {
    let separator = pane_separator();
    let mut rows = Vec::new();
    for (index, line) in output.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(separator).collect();
        if fields.len() != expected {
            return Err(ListingError {
                line: index + 1,
                fields: fields.len(),
                expected,
            });
        }
        rows.push(fields);
    }
    Ok(rows)
}

/// The agent kinds a start_agent request may name (herdr 0.9.5's list, which is
/// also the set `_agent_kind` matches against).
pub const AGENT_KINDS: [&str; 24] = [
    "pi",
    "claude",
    "codex",
    "gemini",
    "cursor",
    "devin",
    "agy",
    "cline",
    "omp",
    "mastracode",
    "opencode",
    "copilot",
    "kimi",
    "kiro",
    "droid",
    "amp",
    "grok",
    "hermes",
    "kilo",
    "qodercli",
    "qwen",
    "letta",
    "maki",
    "muse",
];

fn int_or(value: &str, default: i64) -> i64 {
    value.trim().parse::<i64>().unwrap_or(default)
}

fn optional_int(value: &str) -> Option<i64> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "None" {
        return None;
    }
    trimmed.parse::<i64>().ok()
}

/// Best-effort agent kind from tmux's foreground command and pane title.
///
/// The basename of the command wins (exact or prefix); otherwise a whole-word match
/// in the title, longest kind first so `mastracode` beats `code`. Returns `""` when
/// nothing matches, which is the Python's "not an agent" value.
pub fn agent_kind(command: &str, title: &str) -> String {
    let first = command.split_whitespace().next().unwrap_or("");
    let base = first.rsplit('/').next().unwrap_or("").to_lowercase();
    for kind in AGENT_KINDS {
        if base == kind || base.starts_with(kind) {
            return kind.to_string();
        }
    }
    let lowered = title.to_lowercase();
    let mut kinds: Vec<&str> = AGENT_KINDS.to_vec();
    kinds.sort_by_key(|kind| std::cmp::Reverse(kind.len()));
    for kind in kinds {
        let bytes = lowered.as_bytes();
        let mut start = 0;
        while let Some(found) = lowered[start..].find(kind) {
            let at = start + found;
            let before_ok = at == 0 || !bytes[at - 1].is_ascii_alphanumeric();
            let after = at + kind.len();
            let after_ok = after >= bytes.len() || !bytes[after].is_ascii_alphanumeric();
            if before_ok && after_ok {
                return kind.to_string();
            }
            start = at + 1;
        }
    }
    String::new()
}

/// tmux session/window names cannot contain `:`, `.` or whitespace.
pub fn safe_name(label: &str, fallback: &str) -> String {
    let mut name = String::new();
    let mut last_dash = false;
    for character in label.trim().chars() {
        let keep = character.is_ascii_alphanumeric() || matches!(character, '_' | '@' | '+' | '-');
        if keep {
            name.push(character);
            last_dash = character == '-';
        } else if !last_dash && !name.is_empty() {
            name.push('-');
            last_dash = true;
        }
    }
    let trimmed = name.trim_matches('-').to_string();
    let chosen = if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed
    };
    chosen.chars().take(60).collect()
}

/// Parse `list-panes -F PANE_FORMAT` output: fourteen fields per line, as the format defines.
///
/// A listing whose lines do not all have fourteen fields is refused whole ([`ListingError`]).
/// A short line is a partial write, and a long one means a field carried the separator, so the
/// fields after it cannot be trusted. `label_of` is the registry lookup the Python does
/// (`dunks.json`'s recorded start_agent name), which is why it is a parameter.
pub fn parse_panes(
    output: &str,
    label_of: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<Pane>, ListingError> {
    let mut panes = Vec::new();
    for fields in split_listing(output, PANE_FIELD_COUNT)? {
        let (pane_id, session_id, window_id, cwd, command, title, active) = (
            fields[0], fields[1], fields[2], fields[3], fields[4], fields[5], fields[6],
        );
        let (session_name, window_name) = (fields[7], fields[8]);
        let (window_index, session_index, pane_pid) = (fields[9], fields[10], fields[11]);
        let kind = agent_kind(command, title);
        let label = label_of(pane_id)
            .filter(|name| !name.trim().is_empty())
            .or_else(|| {
                let trimmed = title.trim().to_string();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                }
            })
            .unwrap_or_else(|| window_name.to_string());
        panes.push(Pane {
            pane_id: pane_id.to_string(),
            session_id: session_id.to_string(),
            window_id: window_id.to_string(),
            cwd: cwd.to_string(),
            command: if kind.is_empty() {
                command.to_string()
            } else {
                kind.clone()
            },
            label,
            title: title.to_string(),
            active: active == "1",
            workspace: session_name.to_string(),
            tab: window_name.to_string(),
            window_index: int_or(window_index, 0),
            workspace_index: int_or(session_index, 0),
            pane_pid: optional_int(pane_pid),
            width: int_or(fields[12], 0),
            height: int_or(fields[13], 0),
        });
    }
    Ok(panes)
}

/// Parse `list-windows -F TAB_FORMAT` output (`window_id`, session, name, index, active).
pub fn parse_tabs(output: &str) -> Result<Vec<Tab>, ListingError> {
    Ok(split_listing(output, TAB_FIELD_COUNT)?
        .into_iter()
        .map(|fields| Tab {
            tab_id: fields[0].to_string(),
            workspace_id: fields[1].to_string(),
            label: fields[2].to_string(),
            index: int_or(fields[3], 0),
            active: fields[4] == "1",
        })
        .collect())
}

/// Parse `list-sessions -F WORKSPACE_FORMAT` output (`session_id`, name, index).
pub fn parse_workspaces(output: &str) -> Result<Vec<Workspace>, ListingError> {
    Ok(split_listing(output, WORKSPACE_FIELD_COUNT)?
        .into_iter()
        .map(|fields| Workspace {
            workspace_id: fields[0].to_string(),
            label: fields[1].to_string(),
            index: int_or(fields[2], 0),
        })
        .collect())
}
