//! Parsing tmux's `list-panes -F PANE_FORMAT` output (T21).
//!
//! Ported from `TmuxBackend._parse_panes` plus `_agent_kind` and `_safe_name`. The
//! fields come back `\x1f`-separated on one line per pane, positionally.
//!
//! The rule this file exists to keep: EVERY FIELD IS PARSED ON ITS OWN. The Python's
//! old single try/except meant a blank `session_index` (tmux leaves it empty when a
//! session has no index set) raised on the first `int()` and silently nulled
//! `pane_pid` with it - which is why capture could not identify a lane's agent
//! process, and therefore why `session_id` was null for every live pane on
//! 2026-10-24. A blank field is that field's absence, never another field's.

use crate::terminal::{Pane, Tab, Workspace};

/// tmux's field separator (ASCII unit separator): it cannot appear in a pane title.
pub const PANE_SEP: char = '\x1f';

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
    let first = command.trim().split_whitespace().next().unwrap_or("");
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

/// Parse `list-panes -F PANE_FORMAT` output.
///
/// A line with fewer than 12 fields is SKIPPED (a partial write, a fixture from
/// before geometry was appended); a 12-field line parses with cols/rows absent rather
/// than guessing a size. `label_of` is the registry lookup the Python does
/// (`dunks.json`'s recorded start_agent name), which is why it is a parameter.
pub fn parse_panes(output: &str, label_of: &dyn Fn(&str) -> Option<String>) -> Vec<Pane> {
    let mut panes = Vec::new();
    for line in output.lines() {
        let fields: Vec<&str> = line.split(PANE_SEP).collect();
        if fields.len() < 12 {
            continue;
        }
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
            window_index: optional_int(window_index),
            workspace_index: optional_int(session_index),
            pane_pid: optional_int(pane_pid),
            width: fields.get(12).and_then(|value| optional_int(value)),
            height: fields.get(13).and_then(|value| optional_int(value)),
        });
    }
    panes
}

/// Parse `list-windows -F TAB_FORMAT` output (`window_id`, session, name, index, active).
pub fn parse_tabs(output: &str) -> Vec<Tab> {
    let mut tabs = Vec::new();
    for line in output.lines() {
        let fields: Vec<&str> = line.split(PANE_SEP).collect();
        if fields.len() < 5 {
            continue;
        }
        tabs.push(Tab {
            tab_id: fields[0].to_string(),
            workspace_id: fields[1].to_string(),
            label: fields[2].to_string(),
            index: int_or(fields[3], 0),
            active: fields[4] == "1",
        });
    }
    tabs
}

/// Parse `list-sessions -F WORKSPACE_FORMAT` output (`session_id`, name, index).
pub fn parse_workspaces(output: &str) -> Vec<Workspace> {
    let mut workspaces = Vec::new();
    for line in output.lines() {
        let fields: Vec<&str> = line.split(PANE_SEP).collect();
        if fields.len() < 3 {
            continue;
        }
        workspaces.push(Workspace {
            workspace_id: fields[0].to_string(),
            label: fields[1].to_string(),
            index: int_or(fields[2], 0),
        });
    }
    workspaces
}
