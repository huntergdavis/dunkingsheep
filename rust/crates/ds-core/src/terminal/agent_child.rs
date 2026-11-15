//! t5331: a registered agent pane whose foreground process is the shell that started it.
//!
//! start-agent launches `sh -c 'PATH=... codex ...'`. When the shell does not exec the agent,
//! tmux reports `sh` as the pane's current command and the pane was not detected as an agent:
//! without an explicit project it then dropped out of the roster and showed as "without a desk
//! route" while its agent ran on. Here such a pane counts as the agent kind the pane registry
//! recorded for it, but only when a live direct child of the pane's shell IS that kind, checked
//! twice by pid and start time so a child that exited or was replaced does not count.

use std::path::Path;

/// Shells start-agent's command can leave in front of the agent.
const SHELLS: [&str; 4] = ["sh", "bash", "dash", "zsh"];

/// `(comm, ppid, start)` of `pid` from `<proc_root>/<pid>/stat`, or `None`.
fn stat(proc_root: &Path, pid: i64) -> Option<(String, i64, String)> {
    let text = std::fs::read_to_string(proc_root.join(pid.to_string()).join("stat")).ok()?;
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    let comm = text.get(open + 1..close)?.to_string();
    let fields: Vec<&str> = text.get(close + 1..)?.split_whitespace().collect();
    // After the comm: state, ppid, ... starttime is field 22 of the line, 20th after the comm.
    let state = *fields.first()?;
    if matches!(state, "Z" | "X" | "x") {
        return None;
    }
    let ppid = fields.get(1)?.parse().ok()?;
    let start = fields.get(19)?.to_string();
    Some((comm, ppid, start))
}

/// Whether a live direct child of `pane_pid` runs `kind` (by its kernel name), confirmed by a
/// second read of the same pid with the same start time.
pub fn has_live_child(proc_root: &Path, pane_pid: i64, kind: &str) -> bool {
    let Ok(entries) = std::fs::read_dir(proc_root) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i64>().ok())
        else {
            return false;
        };
        match stat(proc_root, pid) {
            Some((comm, ppid, start)) if ppid == pane_pid && comm == kind => stat(proc_root, pid)
                .is_some_and(|(_, ppid_again, start_again)| {
                    ppid_again == pane_pid && start_again == start
                }),
            _ => false,
        }
    })
}

/// The agent kind for a pane tmux reports as `command` (not already an agent): the registered
/// kind, when the pane's foreground is a shell and a live child of that kind runs under it.
pub fn registered_kind(
    proc_root: &Path,
    command: &str,
    pane_pid: Option<i64>,
    registered: Option<&str>,
) -> Option<String> {
    let base = command.rsplit('/').next().unwrap_or(command);
    if !SHELLS.contains(&base) {
        return None;
    }
    let kind = registered.filter(|kind| crate::state::AGENT_EXES.contains(kind))?;
    has_live_child(proc_root, pane_pid?, kind).then(|| kind.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(root: &Path, pid: i64, comm: &str, state: &str, ppid: i64, start: u64) {
        let dir = root.join(pid.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        // pid (comm) state ppid pgrp session tty tpgid flags minflt cminflt majflt cmajflt
        // utime stime cutime cstime priority nice threads itrealvalue starttime ...
        let line = format!(
            "{pid} ({comm}) {state} {ppid} 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 {start} 0 0\n"
        );
        std::fs::write(dir.join("stat"), line).unwrap();
    }

    #[test]
    fn a_shell_pane_with_a_live_child_of_the_registered_kind_is_that_kind() {
        let root = tempfile::tempdir().unwrap();
        process(root.path(), 100, "sh", "S", 1, 10);
        process(root.path(), 101, "codex", "S", 100, 11);
        assert_eq!(
            registered_kind(root.path(), "sh", Some(100), Some("codex")),
            Some("codex".into())
        );
        // Another kind registered than runs: not detected.
        assert_eq!(
            registered_kind(root.path(), "sh", Some(100), Some("claude")),
            None
        );
        // Not a shell in front, no registered kind, unknown kind, or no pane pid: nothing.
        assert_eq!(
            registered_kind(root.path(), "vim", Some(100), Some("codex")),
            None
        );
        assert_eq!(registered_kind(root.path(), "sh", Some(100), None), None);
        assert_eq!(
            registered_kind(root.path(), "sh", Some(100), Some("gemini")),
            None
        );
        assert_eq!(
            registered_kind(root.path(), "sh", None, Some("codex")),
            None
        );
    }

    /// The listing step: a registered codex pane fronted by `sh` becomes a codex pane (and so
    /// a roster lane, which `pane_to_json` reads from `command`); one whose child exited, or
    /// that has no registry entry, keeps `sh`; an already detected agent is left alone.
    #[test]
    fn the_listing_takes_the_registered_kind_only_with_a_live_child() {
        let root = tempfile::tempdir().unwrap();
        process(root.path(), 100, "sh", "S", 1, 10);
        process(root.path(), 101, "codex", "S", 100, 11);
        process(root.path(), 200, "sh", "S", 1, 10);
        process(root.path(), 201, "codex", "Z", 200, 11);
        process(root.path(), 300, "sh", "S", 1, 10);
        process(root.path(), 301, "codex", "S", 300, 11);
        let pane = |id: &str, command: &str, pid: i64| crate::terminal::Pane {
            pane_id: id.into(),
            command: command.into(),
            pane_pid: Some(pid),
            ..Default::default()
        };
        let mut panes = vec![
            pane("%259", "sh", 100),
            pane("%260", "sh", 200),
            pane("%261", "sh", 300),
            pane("%262", "claude", 400),
        ];
        let registry: serde_json::Map<String, serde_json::Value> =
            serde_json::from_value(serde_json::json!({
                "panes": {
                    "%259": {"agent": "sn", "agent_kind": "codex", "name": "sn"},
                    "%260": {"agent": "w", "agent_kind": "codex", "name": "w"},
                    "%262": {"agent": "c", "agent_kind": "codex", "name": "c"},
                }
            }))
            .unwrap();
        crate::terminal::tmux::apply_registered_kinds(root.path(), &mut panes, &registry);
        let commands: Vec<&str> = panes.iter().map(|pane| pane.command.as_str()).collect();
        assert_eq!(commands, ["codex", "sh", "sh", "claude"]);
        assert_eq!(
            crate::flock::lanes::pane_to_json(&panes[0], None)["agent"],
            "codex"
        );
    }

    #[test]
    fn an_exited_or_zombie_child_or_a_grandchild_does_not_count() {
        let root = tempfile::tempdir().unwrap();
        process(root.path(), 200, "sh", "S", 1, 10);
        // The agent exited and is a zombie: not a live child.
        process(root.path(), 201, "codex", "Z", 200, 11);
        assert!(!has_live_child(root.path(), 200, "codex"));
        // A codex under some other process of the pane: only direct children count.
        process(root.path(), 202, "node", "S", 200, 12);
        process(root.path(), 203, "codex", "S", 202, 13);
        assert!(!has_live_child(root.path(), 200, "codex"));
        // A child that is gone entirely: nothing to find.
        assert!(!has_live_child(root.path(), 300, "codex"));
    }
}
