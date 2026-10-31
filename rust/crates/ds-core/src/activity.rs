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

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const STATES: [&str; 4] = ["working", "waiting", "idle", "unknown"];

/// How long an ENDED turn keeps supporting an "idle" claim: six hours.
///
/// "Idle" is a claim about NOW; past this window the lane is silent, and silence is
/// unknown rather than idle. The Python reads `DUNKINGSHEEP_IDLE_CLAIM_S` for this, which
/// is the caller's knob here (`turn_state(.., idle_claim_s)`).
pub const IDLE_CLAIM_S: f64 = 6.0 * 3600.0;

/// A declared turn, as the extension records it.
///
/// The LIVE field is `expires_at`, which is what every producer writes
/// (`dunk_core.turn_start`, integrations/claude/turn-generation.py:327,
/// integrations/codex/turn-generation.py:267). This port first called it `ends_at`, and
/// since the type had no Deserialize at all, the obvious `#[derive(Deserialize)]` would
/// have SILENTLY ignored `expires_at` and sent every live turn down the "expired without
/// an end" branch - a working lane read as unknown, the exact failure this module exists
/// to prevent (t984 finding 2). `ends_at` stays as an alias so an older fixture still
/// parses.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    #[serde(default)]
    pub started_at: Option<f64>,
    #[serde(default, alias = "ends_at")]
    pub expires_at: Option<f64>,
    #[serde(default)]
    pub ended_at: Option<f64>,
}

impl Turn {
    /// A record with no fields at all: the Python's `{}`, which is FALSY there and
    /// therefore means "no activity source" rather than "an expired turn" (t984 note).
    pub fn is_empty(&self) -> bool {
        self.started_at.is_none() && self.expires_at.is_none() && self.ended_at.is_none()
    }
}

/// One lane's activity: the state, since when, where the answer came from, and - only
/// when the state could NOT be established - why.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Activity {
    pub state: String,
    pub since: Option<f64>,
    pub source: String,
    /// Always present, like the Python's dict: a consumer doing `record["reason"]` must
    /// not KeyError on a real answer (t984 note).
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_for: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inferred: Option<bool>,
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
                "no activity source: no {harness} turn has ever been recorded for this pane (extension not loaded, or the lane has not taken a turn since it started)")))
        }
        Some(state) if state.is_empty() => {
            return (None, None, Some(format!(
                "no activity source: no {harness} turn has ever been recorded for this pane (extension not loaded, or the lane has not taken a turn since it started)")))
        }
        Some(state) => state,
    };
    if state.expires_at.unwrap_or(0.0) > now {
        return (Some("working"), state.started_at, None);
    }
    if let Some(ended) = state.ended_at {
        if now - ended <= idle_claim_s {
            return (Some("idle"), Some(ended), None);
        }
        return (None, Some(ended), Some(format!(
            "the last turn ended {}h ago: idle is a claim about now, and this lane has been silent since", ((now - ended) / 3600.0) as i64)));
    }
    // A turn that EXPIRED without an end is neither: the agent declared work and never said
    // it finished. The TTL exists so a crashed agent cannot hold mail for ever - not so that
    // we can call it idle.
    (None, state.expires_at, Some(
        "the last turn expired without an end: the agent declared work and never reported          finishing".to_string()))
}

/// The best available answer about one lane, with its source.
pub fn activity(
    pane_id: &str,
    harness: &str,
    probe: ClaudeProbe<'_>,
    turns: Option<&BTreeMap<String, Turn>>,
    now: f64,
    idle_claim_s: f64,
) -> Activity {
    let lowered = harness.to_lowercase();
    match lowered.as_str() {
        "claude" => claude_activity(probe, None),
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
                    turn_state(turns.get(pane_id), now, idle_claim_s, "pi");
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

/// What the claude probe actually did, so the REASON names the right cause.
///
/// The Python distinguishes three failures - no client, the command failed with an
/// error, and no session matching the pane's cwd - and the first version of this port
/// collapsed them into the third, which is the module's diagnostic naming the wrong
/// cause (t984 finding 3). The state is "unknown" in all three, so the rule held.
#[derive(Debug, Clone, PartialEq)]
pub enum ClaudeProbe<'a> {
    /// No status client was available to ask.
    NoClient,
    /// The probe ran and failed; the message is the Python's `{error}`.
    Failed(String),
    /// The probe ran and answered: the session status, or None when no session matched.
    Session(Option<&'a str>),
}

/// The claude half: the CLI's own answer, never a guess.
///
/// `claude_status` is what `claude agents --json` said for this pane's cwd - `None` means
/// the probe could not run, and a missing entry is UNKNOWN rather than idle (the session may
/// be starting, or in another cwd).
pub fn claude_activity(probe: ClaudeProbe<'_>, waiting_for: Option<&str>) -> Activity {
    let status = match probe {
        ClaudeProbe::NoClient => {
            return Activity {
                state: "unknown".into(),
                since: None,
                reason: Some("no claude status client available".into()),
                source: "claude".into(),
                ..Activity::default()
            }
        }
        ClaudeProbe::Failed(error) => {
            return Activity {
                state: "unknown".into(),
                since: None,
                reason: Some(format!("claude agents --json failed: {error}")),
                source: "claude".into(),
                ..Activity::default()
            }
        }
        ClaudeProbe::Session(None) => {
            // A missing entry is NOT idle: the session may be starting, or in another cwd.
            return Activity {
                state: "unknown".into(),
                since: None,
                reason: Some("no claude session matches this pane's cwd".into()),
                source: "claude agents --json".into(),
                ..Activity::default()
            };
        }
        ClaudeProbe::Session(Some(status)) => status,
    };
    let state = match status {
        "busy" => "working",
        "waiting" => "waiting",
        "idle" => "idle",
        other => {
            return Activity {
                state: "unknown".into(),
                since: None,
                reason: Some(format!("claude reported status {other:?}")),
                source: "claude agents --json".into(),
                ..Activity::default()
            }
        }
    };
    Activity {
        state: state.to_string(),
        since: None,
        source: "claude agents --json".into(),
        waiting_for: waiting_for.map(str::to_string),
        reason: None,
        ..Activity::default()
    }
}

/// `DUNKINGSHEEP_IDLE_CLAIM_S`, the one env knob this module has.
///
/// The Python reads it at import and `dunk_core` aliases it for the age bound. The first
/// version of this port dropped the knob entirely - `activity()` always passed the
/// constant - which quietly killed the only tunable (t984 finding 4). The caller now
/// passes `idle_claim_s` and reads the default from here.
pub fn idle_claim_s_from(env: &crate::config::Env) -> f64 {
    env.float("DUNKINGSHEEP_IDLE_CLAIM_S", IDLE_CLAIM_S)
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
pub fn say(record: &Activity, now: f64) -> String {
    if record.state == "unknown" {
        return format!(
            "unknown ({}: {})",
            record.source,
            record.reason.clone().unwrap_or_default()
        );
    }
    match record.since {
        Some(since) => format!(
            "{} since {}h ago",
            record.state,
            now_hours(Some(since), now) as i64
        ),
        None => record.state.clone(),
    }
}
