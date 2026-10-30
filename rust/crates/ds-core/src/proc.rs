//! Who is asking? A process identity for task leases (T03, from `dunk_callers.py`).
//!
//! The pane id (`HERDR_PANE_ID` / `TMUX_PANE`) is the registry key. Liveness is a
//! pid PLUS its kernel start time, because a pid alone is reused. The stable process
//! is the **agent** (pi/claude/codex/...), not the short-lived CLI that opens the
//! socket: the CLI exits after every command while the agent keeps working. So the
//! daemon walks from the socket peer up its parent chain and takes the nearest agent
//! ancestor; with no agent (a human shell) it takes the session leader, the stable
//! terminal frame.
//!
//! `/proc/<pid>/stat` gives the parent, session and starttime. A zombie, or a
//! different starttime, means the caller is dead even if the pid now belongs to
//! somebody else. This is the liveness the task reaper uses when the shell pane is
//! still alive but the agent inside it crashed.
//!
//! Two rules are load-bearing and were both earned by an incident:
//!
//! * **A client-supplied pane label is not authentication.** [`caller_belongs_to_pane`]
//!   checks the caller's pid/start AND that the pane process appears in its ancestry.
//!   Missing `/proc` information fails CLOSED - "cannot see" is never "authenticated".
//! * **Identity matching uses `comm`, never an argv basename.** A `/bin/sleep` started
//!   with `argv[0] = claude` is not an agent (t215).
//!
//! The refusal reasons are byte-identical to the Python because tests and operators
//! read them: `caller_missing`, `caller_start_missing`, `pane_pid_missing`,
//! `caller_pid_invalid`, `pane_pid_invalid`, `caller_proc_missing`, `caller_dead`,
//! `caller_start_mismatch`, `ancestry_proc_missing`, `ancestry_dead`,
//! `pane_not_in_ancestry`, `ancestry_cycle`, `ancestry_depth_exceeded`.

#![forbid(unsafe_code)]

use std::fs;
use std::path::Path;

/// The agent executables the terminal backend knows, so this module stays
/// independent of herdr/tmux.
pub const AGENT_NAMES: &[&str] = &[
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
/// How far up the parent chain the walk may go before it gives up.
pub const MAX_DEPTH: usize = 12;
/// Process states that mean the process is gone for our purposes.
pub const DEAD_STATES: &[&str] = &["Z", "X", "x"];

/// The `/proc/<pid>/stat` fields this module needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcStat {
    pub pid: i64,
    /// The kernel `comm`: parentheses stripped, spaces and parens preserved.
    pub comm: String,
    pub state: String,
    pub ppid: i64,
    pub session: i64,
    /// The starttime field, as text: it is compared, never arithmetic'd.
    pub start: String,
}

/// A resolved caller: the wire shape the daemon stores and compares.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Caller {
    pub pane_id: Option<String>,
    pub pid: Option<i64>,
    pub start: Option<String>,
    pub name: Option<String>,
    /// `agent`, `session`, `peer` or `none` - why this pid was chosen.
    pub source: &'static str,
}

impl Caller {
    /// The Python's `_caller(...)`: one constructor so no site invents a shape.
    pub fn new(
        pid: Option<i64>,
        start: Option<String>,
        name: Option<String>,
        pane_id: Option<String>,
        source: &'static str,
    ) -> Self {
        Self {
            pane_id,
            pid,
            start,
            name,
            source,
        }
    }
}

/// Parse the fields we need from `/proc/<pid>/stat`, or `None`.
///
/// `comm` is parenthesised and may itself contain spaces or parentheses, so the text
/// is split on the LAST `)` rather than by whitespace (a comm of `(a) b` broke a
/// naive split; the same rule as the Python).
pub fn stat(pid: i64) -> Option<ProcStat> {
    if pid <= 0 {
        return None;
    }
    let text = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (head, tail) = text.rsplit_once(')')?;
    let comm = head.split_once('(')?.1.to_string();
    let fields: Vec<&str> = tail.split_whitespace().collect();
    // tail[0] is state; indices are shifted by one from the raw stat(5) fields.
    Some(ProcStat {
        pid,
        comm,
        state: fields.first()?.to_string(),
        ppid: fields.get(1)?.parse().ok()?,
        session: fields.get(3)?.parse().ok()?,
        start: fields.get(19)?.to_string(),
    })
}

/// The starttime of `pid`, or `None` when it cannot be read.
pub fn process_start(pid: i64) -> Option<String> {
    stat(pid).map(|info| info.start)
}

/// True only for a non-zombie process whose starttime still matches.
///
/// `start` is compared as text, exactly as the Python does: the registry stores what
/// it read, and a reused pid carries a different starttime.
pub fn process_alive(pid: i64, start: Option<&str>) -> bool {
    let Some(info) = stat(pid) else { return false };
    if DEAD_STATES.contains(&info.state.as_str()) {
        return false;
    }
    match start {
        Some(expected) if !expected.is_empty() => info.start == expected,
        _ => true,
    }
}

/// The argv of `pid`, or an empty vector (a process with no cmdline is normal).
pub fn cmdline(pid: i64) -> Vec<String> {
    if pid <= 0 {
        return Vec::new();
    }
    let Ok(bytes) = fs::read(format!("/proc/{pid}/cmdline")) else {
        return Vec::new();
    };
    bytes
        .split(|byte| *byte == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).to_string())
        .collect()
}

/// One kernel entry for a pane process or one of its direct children (t215 REVISE).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneProcess {
    pub pid: i64,
    pub comm: String,
    pub state: String,
    pub start: Option<String>,
    pub argv: Vec<String>,
}

/// Kernel process entries for the pane process plus its direct children.
///
/// `None` means this platform offers no process table (no `/proc`) or the pane
/// reports no usable pid, so callers fall back to screen-only verification rather
/// than refusing blind. An EMPTY vector means the table worked and nothing is there:
/// evidence of a dead launch, not ignorance.
pub fn pane_agent_pids(pane_pid: Option<i64>) -> Option<Vec<PaneProcess>> {
    let pane = pane_pid?;
    if pane <= 0 || !Path::new("/proc").is_dir() {
        return None;
    }
    let entry = |pid: i64| -> Option<PaneProcess> {
        let info = stat(pid)?;
        Some(PaneProcess {
            pid,
            comm: info.comm,
            state: info.state,
            start: Some(info.start),
            argv: cmdline(pid),
        })
    };
    let mut found = Vec::new();
    // The agent may have REPLACED the shell (same pid) rather than forking from it.
    if let Some(own) = entry(pane) {
        found.push(own);
    }
    let entries = fs::read_dir("/proc").ok()?;
    for item in entries.flatten() {
        let name = item.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<i64>() else {
            continue;
        };
        if pid == pane {
            continue;
        }
        if stat(pid).map(|info| info.ppid) != Some(pane) {
            continue;
        }
        if let Some(child) = entry(pid) {
            found.push(child);
        }
    }
    Some(found)
}

/// The agent executable this process is, if any.
///
/// `comm` decides first (the kernel's name is not spoofable by argv); the cmdline is
/// only a fallback for an agent started through an interpreter or a wrapper.
pub fn agent_name(pid: i64) -> Option<String> {
    let info = stat(pid)?;
    let comm = base_name(&info.comm).to_lowercase();
    if AGENT_NAMES.contains(&comm.as_str()) {
        return Some(comm);
    }
    for part in cmdline(pid) {
        let base = base_name(&part).to_lowercase();
        if AGENT_NAMES.contains(&base.as_str()) {
            return Some(base);
        }
    }
    None
}

fn base_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string())
}

/// Resolve the stable caller behind a socket peer.
///
/// Walk parents to the nearest agent; if there is none, use the peer's session leader
/// (the terminal/shell frame). [`caller_alive`] is the liveness check.
pub fn caller_from_peer(peer_pid: Option<i64>, pane_id: Option<String>) -> Caller {
    caller_from_peer_depth(peer_pid, pane_id, MAX_DEPTH)
}

/// [`caller_from_peer`] with an explicit depth, so a test can bound the walk.
pub fn caller_from_peer_depth(
    peer_pid: Option<i64>,
    pane_id: Option<String>,
    max_depth: usize,
) -> Caller {
    let Some(mut pid) = peer_pid else {
        return Caller::new(None, None, None, pane_id, "none");
    };
    let mut chain: Vec<ProcStat> = Vec::new();
    let mut seen: Vec<i64> = Vec::new();
    for _ in 0..max_depth.max(1) {
        if pid <= 0 || seen.contains(&pid) {
            break;
        }
        seen.push(pid);
        let Some(info) = stat(pid) else { break };
        chain.push(info.clone());
        if let Some(name) = agent_name(pid) {
            return Caller::new(Some(pid), Some(info.start), Some(name), pane_id, "agent");
        }
        pid = info.ppid;
    }
    if let Some(first) = chain.first() {
        if let Some(leader) = stat(first.session) {
            if leader.pid != first.pid {
                return Caller::new(
                    Some(leader.pid),
                    Some(leader.start),
                    Some(leader.comm),
                    pane_id,
                    "session",
                );
            }
        }
        return Caller::new(
            Some(first.pid),
            Some(first.start.clone()),
            Some(first.comm.clone()),
            pane_id,
            "peer",
        );
    }
    Caller::new(None, None, None, pane_id, "none")
}

/// Liveness for a stored caller; a missing identity means not alive.
pub fn caller_alive(caller: Option<&Caller>) -> bool {
    let Some(caller) = caller else { return false };
    let Some(pid) = caller.pid else { return false };
    process_alive(pid, caller.start.as_deref())
}

/// The first failed binding condition, or `None` when the caller is authenticated.
///
/// KEEP THE ORDER: the tests and the daemon log read these names, and the walk is
/// fail-closed - a missing `/proc` entry is a refusal, not a pass.
pub fn caller_pane_auth_failure(
    caller: Option<&Caller>,
    pane_pid: Option<i64>,
) -> Option<&'static str> {
    let Some(caller) = caller else {
        return Some("caller_missing");
    };
    let Some(start) = caller.start.as_deref().filter(|text| !text.is_empty()) else {
        return Some("caller_start_missing");
    };
    let Some(root) = pane_pid else {
        return Some("pane_pid_missing");
    };
    let Some(mut pid) = caller.pid else {
        return Some("caller_pid_invalid");
    };
    if root <= 0 {
        return Some("pane_pid_invalid");
    }
    let Some(info) = stat(pid) else {
        return Some("caller_proc_missing");
    };
    if DEAD_STATES.contains(&info.state.as_str()) {
        return Some("caller_dead");
    }
    if info.start != start {
        return Some("caller_start_mismatch");
    }
    let mut seen: Vec<i64> = Vec::new();
    let mut current = Some(info);
    for _ in 0..64 {
        let Some(step) = current else {
            return Some("ancestry_proc_missing");
        };
        if DEAD_STATES.contains(&step.state.as_str()) {
            return Some("ancestry_dead");
        }
        if pid <= 1 {
            return Some("pane_not_in_ancestry");
        }
        if seen.contains(&pid) {
            return Some("ancestry_cycle");
        }
        if pid == root {
            return None;
        }
        seen.push(pid);
        pid = step.ppid;
        current = stat(pid);
    }
    Some("ancestry_depth_exceeded")
}

/// Bind a kernel-attributed caller to the backend's pane process ancestry.
pub fn caller_belongs_to_pane(caller: Option<&Caller>, pane_pid: Option<i64>) -> bool {
    caller_pane_auth_failure(caller, pane_pid).is_none()
}

#[cfg(test)]
mod tests {
    //! Ported 1:1 from `tests/test_dunk_callers.py` (T03), same names and order.
    //! `test_a_system_send_does_not_carry_a_pane_identity` is NOT here: it pins the
    //! CLIENT's request shape (`FlockClient.call`), which arrives with T41.
    use super::*;

    fn this_pid() -> i64 {
        std::process::id() as i64
    }

    fn parent_pid() -> i64 {
        stat(this_pid())
            .expect("this test process must be in /proc")
            .ppid
    }

    #[test]
    fn test_pane_binding_requires_process_ancestry_and_current_start_time() {
        let caller = Caller::new(
            Some(this_pid()),
            process_start(this_pid()),
            None,
            None,
            "peer",
        );
        assert!(caller_belongs_to_pane(Some(&caller), Some(this_pid())));
        assert!(caller_belongs_to_pane(Some(&caller), Some(parent_pid())));
        assert!(!caller_belongs_to_pane(Some(&caller), Some(99_999_999)));
        let wrong = Caller::new(
            Some(this_pid()),
            Some("wrong".to_string()),
            None,
            None,
            "peer",
        );
        assert!(!caller_belongs_to_pane(Some(&wrong), Some(this_pid())));
        assert!(!caller_belongs_to_pane(Some(&caller), None));
        assert!(!caller_belongs_to_pane(None, Some(this_pid())));
    }

    #[test]
    fn test_self_is_alive_with_its_own_start_time() {
        let start = process_start(this_pid());
        assert!(start.is_some());
        assert!(process_alive(this_pid(), start.as_deref()));
    }

    #[test]
    fn test_start_time_mismatch_is_dead() {
        let start = process_start(this_pid()).expect("starttime");
        assert!(!process_alive(this_pid(), Some(&format!("{start}0"))));
    }

    #[test]
    fn test_missing_pid_is_dead() {
        assert!(!process_alive(99_999_999, None));
        let caller = Caller::new(None, None, None, None, "none");
        assert!(!caller_alive(Some(&caller)));
        assert!(!caller_alive(None));
    }

    #[test]
    fn test_agent_name_of_this_test_process_is_none() {
        assert_eq!(None, agent_name(this_pid()));
    }

    #[test]
    fn test_caller_walks_to_a_live_ancestor_and_keeps_the_pane() {
        let caller = caller_from_peer(Some(this_pid()), Some("w8:p8".to_string()));
        assert_eq!(Some("w8:p8".to_string()), caller.pane_id);
        assert!(caller.pid.is_some());
        assert!(caller.start.is_some());
        assert!(matches!(caller.source, "agent" | "session" | "peer"));
        assert!(caller_alive(Some(&caller)));
    }

    #[test]
    fn test_the_agent_ancestor_is_preferred_over_the_short_lived_peer() {
        let caller = caller_from_peer(Some(this_pid()), Some("w8:p8".to_string()));
        if caller.source != "agent" {
            // The Python skips the same way: not every host runs the suite under an agent.
            eprintln!("skipped: this test process is not running under an agent");
            return;
        }
        assert_ne!(Some(this_pid()), caller.pid);
        let name = caller.name.unwrap_or_default();
        assert!(
            ["pi", "claude", "codex", "opencode"].contains(&name.as_str()),
            "unexpected agent name {name}"
        );
    }

    #[test]
    fn test_invalid_peer_is_a_named_none_caller() {
        let caller = caller_from_peer(None, Some("w8:p8".to_string()));
        assert_eq!("none", caller.source);
        assert_eq!(None, caller.pid);
        assert!(!caller_alive(Some(&caller)));
    }

    // -- the refusal reasons: identical strings, identical order (T03 acceptance) --

    #[test]
    fn test_each_refusal_names_its_failing_condition() {
        let live = Caller::new(
            Some(this_pid()),
            process_start(this_pid()),
            None,
            None,
            "peer",
        );
        let missing_start = Caller::new(Some(this_pid()), None, None, None, "peer");
        let bad_pid = Caller::new(Some(-1), Some("1".to_string()), None, None, "peer");

        assert_eq!(
            Some("caller_missing"),
            caller_pane_auth_failure(None, Some(parent_pid()))
        );
        assert_eq!(
            Some("caller_start_missing"),
            caller_pane_auth_failure(Some(&missing_start), Some(parent_pid()))
        );
        assert_eq!(
            Some("pane_pid_missing"),
            caller_pane_auth_failure(Some(&live), None)
        );
        assert_eq!(
            Some("pane_pid_invalid"),
            caller_pane_auth_failure(Some(&live), Some(0))
        );
        assert_eq!(
            Some("caller_proc_missing"),
            caller_pane_auth_failure(Some(&bad_pid), Some(parent_pid()))
        );
        let wrong_start = Caller::new(Some(this_pid()), Some("0".to_string()), None, None, "peer");
        assert_eq!(
            Some("caller_start_mismatch"),
            caller_pane_auth_failure(Some(&wrong_start), Some(parent_pid()))
        );
        // 1 is not a pane process: the walk ends there and says so.
        assert_eq!(
            Some("pane_not_in_ancestry"),
            caller_pane_auth_failure(Some(&live), Some(1))
        );
        // An authenticated caller against its own pid is the only `None`.
        assert_eq!(
            None,
            caller_pane_auth_failure(Some(&live), Some(this_pid()))
        );
    }

    #[test]
    fn test_the_walk_is_bounded_and_reports_its_depth_limit() {
        // A depth of 1 starts at a pid that is not the pane root: the loop ends before
        // reaching it, so the failure is named rather than guessed.
        let caller = Caller::new(
            Some(this_pid()),
            process_start(this_pid()),
            None,
            None,
            "peer",
        );
        let shallow = caller_from_peer_depth(Some(this_pid()), None, 1);
        assert!(shallow.pid.is_some());
        assert!(caller_pane_auth_failure(Some(&caller), Some(parent_pid())).is_none());
    }

    #[test]
    fn test_pane_agent_pids_includes_the_pane_process_itself() {
        let found = pane_agent_pids(Some(parent_pid())).expect("this host has /proc");
        assert!(
            found.iter().any(|entry| entry.pid == parent_pid()),
            "the pane process itself must be in the list (the agent may have \
                 replaced the shell rather than forked)"
        );
        assert!(found.iter().all(|entry| !entry.comm.is_empty()));
        assert_eq!(None, pane_agent_pids(None));
        assert_eq!(None, pane_agent_pids(Some(0)));
    }

    #[test]
    fn test_identity_uses_comm_not_an_argv_basename() {
        // /bin/sleep started with argv[0]=claude is NOT an agent (t215): comm decides
        // first, and this process is neither.
        let mine = agent_name(this_pid());
        assert!(mine.is_none() || AGENT_NAMES.contains(&mine.unwrap().as_str()));
        let comm = stat(this_pid()).expect("stat").comm;
        assert_eq!(base_name(&comm), comm);
    }
}
