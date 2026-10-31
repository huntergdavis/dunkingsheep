//! The push sweep's pure decisions (`dunk_core.py::Flock`): `_unread_mail`,
//! `_lane_agent_alive`, `_push_targets`.
//!
//! `Flock` itself (T30+, `flock/mod.rs`) is not in Rust yet, so the methods cannot be
//! ported as methods. What CAN be ported now is the decision each one makes, as a
//! function over the values the method reads from `self` (inbox rows, the process
//! table, the role registry). The Flock port wires these to the real stores.

use std::collections::{BTreeMap, BTreeSet};

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
        !(now - created > grace_s)
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
    Registered,
}

/// `_push_targets`: pane id -> (scope, why): explicit followers plus registered lanes.
///
/// `followers` is `task_waiters` (pane -> scope). `lanes` is the registry's lane panes, or
/// `None` when the registry is unavailable (followers only). `role_of` is `caller_role`:
/// the RESOLVED role, not the registry string (t975: a reused pane id must not become a
/// target). Only `worker` and `reviewer` pull work; a desk/planner/owner pane never does.
pub fn push_targets<'a>(
    followers: &BTreeMap<String, String>,
    lanes: Option<&'a BTreeSet<String>>,
    role_of: impl Fn(&str) -> String,
) -> BTreeMap<String, (String, Via)> {
    let mut targets: BTreeMap<String, (String, Via)> = followers
        .iter()
        .map(|(pane, scope)| (pane.clone(), (scope.clone(), Via::Follower)))
        .collect();
    let Some(lanes) = lanes else {
        return targets;
    };
    for pane in lanes {
        if targets.contains_key(pane) {
            continue;
        }
        let role = role_of(pane);
        if role != "worker" && role != "reviewer" {
            continue;
        }
        targets.insert(pane.clone(), (TASK_GLOBAL.to_string(), Via::Registered));
    }
    targets
}

#[cfg(test)]
#[path = "push_tests.rs"]
mod push_tests;
