//! What an agent is DOING - from the tool that knows, not from the screen (T16 slice).
//!
//! Screen scraping answered this for a year and lied in both directions: a working pi lane
//! read "ready" because pi reports no pane status, and a live pane read "dead" because its
//! transcript quoted the marker. Each harness exposes its own truth:
//!
//! > claude: `claude agents --json --cwd <dir>` -> busy / waiting / idle (+ waitingFor)
//! > pi: its extension's turn events -> a declared turn with a TTL
//! > codex/opencode: declared turns when supplied; automatic producers NOT WIRED YET
//!
//! THE RULE, and the reason this module exists: **an ABSENT signal is UNKNOWN, never idle**.
//! "Assume idle" has cost us four stalls in one day; "unknown" costs a lane one question.

use serde::Serialize;
use std::collections::BTreeMap;

pub const STATES: [&str; 4] = ["working", "waiting", "idle", "unknown"];

/// How long an ENDED turn keeps supporting an "idle" claim: six hours.
///
/// "Idle" is a claim about NOW; past this window the lane is silent, and silence is
/// unknown rather than idle. The Python reads `DUNKINGSHEEP_IDLE_CLAIM_S` for this, which
/// is the caller's knob here (`turn_state(.., idle_claim_s)`).
pub const IDLE_CLAIM_S: f64 = 6.0 * 3600.0;

/// A declared turn, as the extension records it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Turn {
    pub started_at: Option<f64>,
    pub ends_at: Option<f64>,
    pub ended_at: Option<f64>,
}

/// One lane's activity: the state, since when, where the answer came from, and - only
/// when the state could NOT be established - why.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Activity {
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<f64>,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inferred: Option<bool>,
    /// The caller's clock at the moment of the answer, so `say()` can render "since Nh"
    /// without reading a clock of its own (the Python's `say` calls time.time()).
    #[serde(skip)]
    pub since_hours: i64,
}

/// THE one rule for a declared turn: `(state_or_None, since, reason)`.
///
/// There must be exactly one derivation of this, because the desk and the dispatcher read
/// the same turn and must not disagree about it (reviewer on t227: the roster said IDLE for
/// a turn that had EXPIRED without ever ending, which is the case the TTL exists to prevent
/// us from calling idle).
pub fn turn_state(
    state: Option<&Turn>,
    now: f64,
    idle_claim_s: f64,
    harness: &str,
) -> (Option<&'static str>, Option<f64>, Option<String>) {
    let state = match state {
        None => {
            return (None, None, Some(format!(
                "no activity source: no {harness} turn has ever been recorded for this pane                  (extension not loaded, or the lane has not taken a turn since it started)")))
        }
        Some(state) => state,
    };
    if state.ends_at.unwrap_or(0.0) > now {
        return (Some("working"), state.started_at, None);
    }
    if let Some(ended) = state.ended_at {
        if now - ended <= idle_claim_s {
            return (Some("idle"), Some(ended), None);
        }
        return (None, Some(ended), Some(format!(
            "the last turn ended {}h ago: idle is a claim about now, and this lane has been              silent since", ((now - ended) / 3600.0) as i64)));
    }
    // A turn that EXPIRED without an end is neither: the agent declared work and never said
    // it finished. The TTL exists so a crashed agent cannot hold mail for ever - not so that
    // we can call it idle.
    (None, state.ends_at, Some(
        "the last turn expired without an end: the agent declared work and never reported          finishing".to_string()))
}

/// The best available answer about one lane, with its source.
pub fn activity(
    pane_id: &str,
    harness: &str,
    claude_status: Option<&str>,
    turns: Option<&BTreeMap<String, Turn>>,
    now: f64,
) -> Activity {
    let lowered = harness.to_lowercase();
    match lowered.as_str() {
        "claude" => claude_activity(claude_status, None),
        "pi" => match turns {
            None => Activity {
                state: "unknown".into(),
                since: None,
                source: "pi turn events".into(),
                reason: Some("no turn state available".into()),
                ..Activity::default()
            },
            Some(turns) => {
                let (state, since, reason) =
                    turn_state(turns.get(pane_id), now, IDLE_CLAIM_S, "pi");
                Activity {
                    state: state.unwrap_or("unknown").to_string(),
                    since,
                    source: "pi turn events".into(),
                    reason,
                    ..Activity::default()
                }
            }
        },
        "codex" | "opencode" => {
            let (state, since, reason) = turn_state(
                turns.and_then(|turns| turns.get(pane_id)),
                now,
                IDLE_CLAIM_S,
                &lowered,
            );
            Activity {
                state: state.unwrap_or("unknown").to_string(),
                since,
                source: format!("{lowered} declared turns"),
                reason,
                inferred: Some(true),
                ..Activity::default()
            }
        }
        other => Activity {
            state: "unknown".into(),
            since: None,
            source: if other.is_empty() {
                "none".into()
            } else {
                other.to_string()
            },
            reason: Some(format!("no activity source for harness {other:?}")),
            ..Activity::default()
        },
    }
}

/// The claude half: the CLI's own answer, never a guess.
///
/// `claude_status` is what `claude agents --json` said for this pane's cwd - `None` means
/// the probe could not run, and a missing entry is UNKNOWN rather than idle (the session may
/// be starting, or in another cwd).
pub fn claude_activity(status: Option<&str>, waiting_for: Option<&str>) -> Activity {
    let status = match status {
        None => {
            return Activity {
                state: "unknown".into(),
                since: None,
                source: "claude".into(),
                reason: Some("no claude session matches this pane's cwd".into()),
                ..Activity::default()
            }
        }
        Some(status) => status,
    };
    match status {
        "busy" => Activity {
            state: "working".into(),
            since: None,
            source: "claude agents --json".into(),
            waiting_for: waiting_for.map(str::to_string),
            ..Activity::default()
        },
        "waiting" => Activity {
            state: "waiting".into(),
            since: None,
            source: "claude agents --json".into(),
            waiting_for: waiting_for.map(str::to_string),
            ..Activity::default()
        },
        "idle" => Activity {
            state: "idle".into(),
            since: None,
            source: "claude agents --json".into(),
            waiting_for: waiting_for.map(str::to_string),
            ..Activity::default()
        },
        other => Activity {
            state: "unknown".into(),
            since: None,
            source: "claude agents --json".into(),
            reason: Some(format!("claude reported status {other:?}")),
            ..Activity::default()
        },
    }
}

/// Seconds-ago as hours, never negative (`now_hours` in the Python).
pub fn now_hours(at: Option<f64>, now: f64) -> f64 {
    match at {
        None => 0.0,
        Some(at) if at == 0.0 => 0.0,
        Some(at) => ((now - at) / 3600.0).max(0.0),
    }
}

/// One line for a log or a table.
pub fn say(record: &Activity) -> String {
    if record.state == "unknown" {
        return format!(
            "unknown ({}: {})",
            record.source,
            record.reason.clone().unwrap_or_default()
        );
    }
    match record.since {
        Some(_) => format!("{} since {}h ago", record.state, record.since_hours),
        None => record.state.clone(),
    }
}
