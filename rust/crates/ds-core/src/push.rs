//! The push sweep's pure decisions (`dunk_core.py::Flock`): `_unread_mail`,
//! `_lane_agent_alive`, `_push_targets`.
//!
//! `Flock` itself (T30+, `flock/mod.rs`) is not in Rust yet, so the methods cannot be
//! ported as methods. What CAN be ported now is the decision each one makes, as a
//! function over the values the method reads from `self` (inbox rows, the process
//! table, the role registry). The Flock port wires these to the real stores.

use std::collections::BTreeMap;

/// `BUSY_STATUSES`: an agent in one of these is never fed.
pub const BUSY_STATUSES: [&str; 2] = ["working", "blocked"];

/// `UNREAD_MAIL_GRACE_S` default (`DUNKINGSHEEP_UNREAD_GRACE_S`, 600): mail older than
/// this no longer holds a lane back from the push sweep (t1053).
pub const UNREAD_MAIL_GRACE_S: f64 = 600.0;

/// `dunk_callers.AGENT_NAMES` and `DEAD_STATES`.
pub const AGENT_NAMES: [&str; 24] = [
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
pub const DEAD_STATES: [&str; 3] = ["Z", "X", "x"];

/// `dunk_tasks.GLOBAL`.
pub const TASK_GLOBAL: &str = "global";

/// The fields of an inbox row `_unread_mail` reads.
#[derive(Debug, Clone, Default)]
pub struct Mail {
    pub subject: Option<String>,
    pub body: Option<String>,
    pub created_at: Option<f64>,
}

/// `_unread_mail`: does this lane have unread mail it must ACT on?
///
/// `unread` is `inbox.count(mailbox)["unread"]`; `messages` is the peeked read (limit 50).
/// The daemon's own "TASK ..." announcements never gate (they are the sweep's output, so
/// counting them made every claim block the next one), and mail older than `grace_s`
/// stops gating. A missing or zero `created_at` reads as "now" (Python: `created_at or
/// now`), i.e. fresh. The Python returns False when the inbox read raises; the caller
/// maps its error to `false` the same way.
pub fn unread_mail(unread: i64, messages: &[Mail], now: f64, grace_s: f64) -> bool {
    if unread == 0 {
        return false;
    }
    messages.iter().any(|message| {
        let subject = message.subject.as_deref().unwrap_or("");
        let body = message.body.as_deref().unwrap_or("");
        if subject.starts_with("TASK ") || body.starts_with("TASK ") {
            return false;
        }
        let created = message.created_at.filter(|at| *at != 0.0).unwrap_or(now);
        (now - created)
            .partial_cmp(&grace_s)
            .is_none_or(|ordering| ordering != std::cmp::Ordering::Greater)
    })
}

/// One row of the pane's process-table answer (`herdr.agent_child_pids`).
#[derive(Debug, Clone)]
pub struct ChildProc {
    pub comm: String,
    pub state: String,
}

/// `_lane_agent_alive`: `Some(true)` when an agent process runs in the pane, `Some(false)`
/// when the process table answered and there is none (a bare shell), `None` when it cannot
/// say. Unknown never blocks a push; only evidence of a dead lane does.
///
/// `entries` is `None` when there is no process table or no pid (or the lookup failed).
/// `descendants` is only asked when the direct children show no agent: it walks the whole
/// descendant tree (`npx claude` -> node -> claude, review t1029) and returns `None` when
/// the walk fails, which is "cannot say", never "dead".
pub fn lane_agent_alive(
    entries: Option<&[ChildProc]>,
    descendants: impl FnOnce() -> Option<bool>,
) -> Option<bool> {
    let entries = entries?;
    if entries.iter().any(|entry| {
        AGENT_NAMES.contains(&entry.comm.as_str()) && !DEAD_STATES.contains(&entry.state.as_str())
    }) {
        return Some(true);
    }
    descendants()
}

/// `why` a pane is a push target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Via {
    Follower,
}

/// `_push_targets`: pane id -> (scope, why): the explicit followers only.
///
/// A pane is fed when it subscribed (`task follow`, or an unclaimed `task next`) or when it
/// claimed work (commands_tasks subscribes the claimant). A registered lane that did neither is
/// never pushed, so reading mail never hands it a task. This replaces the Python rule that fed every
/// registered worker and reviewer (owner decision on t3554, 2026-11-07).
pub fn push_targets(followers: &BTreeMap<String, String>) -> BTreeMap<String, (String, Via)> {
    followers
        .iter()
        .map(|(pane, scope)| (pane.clone(), (scope.clone(), Via::Follower)))
        .collect()
}

#[cfg(test)]
#[path = "push_tests.rs"]
mod push_tests;

#[cfg(test)]
#[path = "push_parity_tests.rs"]
mod push_parity_tests;
