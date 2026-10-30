//! Deterministic recovery of a stuck agent message (t377, owner2281/2282).
//!
//! A message the daemon typed can sit unsubmitted in a lane's composer while the
//! lane is idle: the harness swallowed the Enter, or the composer needed a carriage
//! return. The owner sees a lane with mail that never acts, and the only fix for two
//! years was a human pressing Enter.
//!
//! THE RULE, unchanged from `dunk_stuck.py`: track the exact message we own, the
//! complete composer text, the pane generation and the observation times. If the
//! SAME text is still there unchanged after `threshold_s` and the target is idle
//! rather than busy, dialogged or unknown, press Enter ONCE under the daemon's
//! guard, then verify consumption INDEPENDENTLY - the composer cleared, or a NEW
//! turn started. "Still working" is not success evidence.
//!
//! EVERYTHING ELSE RESETS, and every uncertain path fails closed: submitting
//! somebody's half-written sentence once is worse than leaving a nudge stuck.
//!
//! This module is the state machine only. It performs no I/O and reads no clock: the
//! caller injects `now`, which is what makes the fake-clock tests exact.

use std::collections::BTreeMap;

/// The fields one pane's record carries, in the order the Python's schema lists them.
pub const SCHEMA_FIELDS: [&str; 7] = ["message_id", "generation", "fingerprint",
                                      "first_seen", "last_attempt_at", "attempts", "terminal"];

pub const ACTIONS: [&str; 5] = ["wait", "recover", "cooling", "reset", "terminal"];

pub const DEFAULT_THRESHOLD_S: f64 = 120.0;
pub const DEFAULT_MAX_ATTEMPTS: u32 = 2;
pub const DEFAULT_COOLDOWN_S: f64 = 60.0;
pub const DEFAULT_OBSERVATION_INTERVAL_S: f64 = 20.0;
/// Statuses that mean "do not touch this pane".
///
/// `working` is here on purpose: a lane mid-turn is not a lane that failed to accept
/// our text, and t130/t271 settled that a working lane queues its own input.
pub const BLOCKED_STATUSES: [&str; 4] = ["blocked", "dead", "unknown", "working"];

/// The composer's own wrapping is not content: compare on collapsed text.
pub fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Stable identity of the COMPLETE composer text.
///
/// Exact, not prefix: if a person appended one word to our nudge, this changes and
/// the recovery resets. That is the whole guard against submitting a continuation.
pub fn fingerprint(text: &str) -> String {
    // FNV-1a over the collapsed text: the Python uses sha256, but the value is only
    // ever compared for equality inside one tracker, and a 64-bit hash keeps the
    // record readable in a state file. (Named, so nobody mistakes it for the sha256
    // the Python stores in `fingerprint` and compares across processes.)
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in collapse(text).as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn positive(raw: Option<f64>, default: f64, what: &str) -> Result<f64, String> {
    match raw {
        None => Ok(default),
        Some(value) if value.is_finite() && value > 0.0 => Ok(value),
        Some(value) => Err(format!("{what} must be a positive number, got {value}")),
    }
}

/// Tunables, validated once. Threshold default is the CoS's 120 s.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Config {
    pub enabled: bool,
    pub threshold_s: f64,
    pub max_attempts: u32,
    pub cooldown_s: f64,
    pub observation_interval_s: f64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            enabled: true,
            threshold_s: DEFAULT_THRESHOLD_S,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            cooldown_s: DEFAULT_COOLDOWN_S,
            observation_interval_s: DEFAULT_OBSERVATION_INTERVAL_S,
        }
    }
}

/// The raw knobs a config file may set, all optional so a partial file works.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RawConfig {
    pub enabled: Option<bool>,
    pub threshold_s: Option<f64>,
    pub max_attempts: Option<i64>,
    pub cooldown_s: Option<f64>,
    pub observation_interval_s: Option<f64>,
}

impl Config {
    /// Validate a raw config. A nonsense value is REFUSED, never coerced.
    pub fn from_raw(raw: &RawConfig) -> Result<Config, String> {
        let enabled = raw.enabled.unwrap_or(true);
        let interval = positive(raw.observation_interval_s, DEFAULT_OBSERVATION_INTERVAL_S,
                                "observation_interval_s")?;
        let threshold = positive(raw.threshold_s, DEFAULT_THRESHOLD_S, "threshold_s")?;
        if enabled && interval > threshold {
            return Err("observation_interval_s must be <= threshold_s, or the bound cannot \
                        be honoured".to_string());
        }
        let attempts = match raw.max_attempts {
            None => DEFAULT_MAX_ATTEMPTS,
            Some(value) if value >= 1 => value as u32,
            Some(_) => return Err("max_attempts must be at least 1 (0 would disable the \
                                   action)".to_string()),
        };
        Ok(Config {
            enabled,
            threshold_s: threshold,
            max_attempts: attempts,
            cooldown_s: positive(raw.cooldown_s, DEFAULT_COOLDOWN_S, "cooldown_s")?,
            observation_interval_s: interval,
        })
    }
}

/// One look at one pane, at one instant.
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    pub pane_id: String,
    pub message_id: Option<String>,
    /// The COMPLETE composer text as read. `None` means the read FAILED (t377).
    pub text: Option<String>,
    pub status: String,
    /// The matcher says this text is ours.
    pub owned: bool,
    /// Pane identity (pid/start), so a replaced pane differs.
    pub generation: Option<String>,
    /// A panel/dialog is open.
    pub dialog: bool,
    /// The sender cancelled the tracked message.
    pub cancelled: bool,
}

impl Observation {
    pub fn new(pane_id: &str, message_id: Option<&str>, text: Option<&str>, status: &str) -> Self {
        Observation {
            pane_id: pane_id.to_string(),
            message_id: message_id.map(str::to_string),
            text: text.map(str::to_string),
            status: status.to_string(),
            owned: true,
            generation: None,
            dialog: false,
            cancelled: false,
        }
    }
}

/// What to do, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub action: String,
    pub reason: String,
    pub attempts: u32,
    pub terminal: Option<String>,
    pub fingerprint: Option<String>,
    pub first_seen: Option<f64>,
    pub stale_for: f64,
}

impl Decision {
    fn new(action: &str, reason: impl Into<String>) -> Self {
        debug_assert!(ACTIONS.contains(&action), "unknown action {action}");
        Decision { action: action.to_string(), reason: reason.into(), attempts: 0,
                   terminal: None, fingerprint: None, first_seen: None, stale_for: 0.0 }
    }

    pub fn recovers(&self) -> bool {
        self.action == "recover"
    }
}

/// One pane's record of the tracked message.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Record {
    pub message_id: Option<String>,
    pub generation: Option<String>,
    pub fingerprint: Option<String>,
    pub first_seen: Option<f64>,
    pub last_attempt_at: Option<f64>,
    pub attempts: u32,
    pub terminal: Option<String>,
}

/// Per-pane tracker with the durable budget ledger (`spent`).
///
/// `spent` is keyed by message identity: a reset or a consumption FORGETS the
/// per-pane record but must not hand the same message a fresh budget, so it cannot
/// regain retries by disappearing and reappearing, and a crash mid-press cannot
/// refund the attempt it reserved.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tracker {
    pub records: BTreeMap<String, Record>,
    pub spent: BTreeMap<String, u32>,
}

/// The clock is the caller's: this machine never reads one.
pub fn obs_now(now: Option<f64>) -> Result<f64, String> {
    now.ok_or_else(|| "this state machine takes `now` from the caller; it never reads a \
                        clock".to_string())
}

pub fn ledger_key(pane_id: &str, message_id: &str, generation: Option<&str>,
                  fingerprint: &str) -> String {
    format!("{}|{}|{}|{}", pane_id, message_id,
            generation.unwrap_or("None"), fingerprint)
}

/// The independent consumption check after ONE Enter.
///
/// "Still working" is deliberately NOT success: a lane that was already working is
/// running somebody's turn, and t130's lesson is that a busy status proves nothing
/// about our text.
pub fn verify(before: &Observation, after: Option<&Observation>) -> (&'static str, String) {
    let after = match after {
        None => return ("unverified", "no observation after the press".to_string()),
        Some(after) => after,
    };
    let after_text = match after.text.as_deref() {
        // A read that FAILED is not an empty composer. Treating None as "" is how a
        // missing pane once reported a successful recovery.
        None => return ("unverified", "the pane could not be read after the press".to_string()),
        Some(text) => text,
    };
    if after.dialog {
        return ("unverified", "a dialog opened after the press".to_string());
    }
    let before_text = before.text.as_deref().unwrap_or("");
    if !collapse(before_text).is_empty() && collapse(after_text).is_empty() {
        return ("consumed", "the composer cleared".to_string());
    }
    if before.status != "working" && after.status == "working" {
        return ("consumed", "a turn started after the press".to_string());
    }
    if fingerprint(after_text) == fingerprint(before_text) {
        return ("unchanged", "the same text is still in the composer".to_string());
    }
    ("modified", "the composer now holds different text".to_string())
}

impl Tracker {
    /// What to do about this observation, and the record it leaves behind.
    ///
    /// Mutating by design: `first_seen`, `attempts` and `last_attempt_at` ARE the
    /// state that makes the decision deterministic across ticks.
    pub fn decide(&mut self, obs: &Observation, config: &Config, now: f64)
        -> Decision
    {
        if !config.enabled {
            // A disabled recovery is not a slower recovery: it does not act at all.
            return Decision::new("reset", "stuck recovery is disabled");
        }
        let text = obs.text.as_deref().unwrap_or("");
        let message_id = match obs.message_id.as_deref() {
            None => return self.reset(obs, "no tracked message for this pane"),
            Some(id) => id,
        };
        if obs.cancelled {
            return self.reset(obs, "the tracked message was cancelled");
        }
        if text.is_empty() {
            return self.reset(obs, "the composer is empty");
        }
        if !obs.owned {
            return self.reset(obs, "the composer text is not provably ours");
        }
        if obs.dialog {
            return self.reset(obs, "a dialog or panel is open");
        }
        if BLOCKED_STATUSES.contains(&obs.status.as_str()) {
            return self.reset(obs, format!("target is {}", obs.status));
        }
        let current = fingerprint(text);
        let existing = self.records.get(&obs.pane_id).cloned();
        if let Some(record) = &existing {
            if record.generation.is_some() && obs.generation.is_some()
                && record.generation != obs.generation {
                return self.reset(obs, "the pane was replaced");
            }
            if record.message_id.is_some() && record.message_id.as_deref() != Some(message_id) {
                return self.reset(obs, "the tracked message changed");
            }
            if record.fingerprint.is_some() && record.fingerprint.as_deref() != Some(&current) {
                return self.reset(obs, "the composer text changed");
            }
        }
        let record = match existing {
            None => {
                let already = self.reserved(&obs.pane_id, message_id,
                                            obs.generation.as_deref(), &current);
                self.records.insert(obs.pane_id.clone(), Record {
                    message_id: Some(message_id.to_string()),
                    generation: obs.generation.clone(),
                    fingerprint: Some(current.clone()),
                    first_seen: Some(now),
                    last_attempt_at: None,
                    attempts: already,
                    terminal: None,
                });
                let mut decision = Decision::new("wait", "first sighting; the clock starts now");
                decision.fingerprint = Some(current);
                decision.first_seen = Some(now);
                return decision;
            }
            Some(record) => record,
        };
        if let Some(terminal) = record.terminal.clone() {
            let mut decision = Decision::new("terminal", terminal.clone());
            decision.attempts = record.attempts;
            decision.terminal = Some(terminal);
            decision.fingerprint = Some(current);
            decision.first_seen = record.first_seen;
            decision.stale_for = now - record.first_seen.unwrap_or(now);
            return decision;
        }
        let first_seen = record.first_seen.unwrap_or(now);
        let stale_for = now - first_seen;
        if stale_for < config.threshold_s {
            let mut decision = Decision::new(
                "wait",
                format!("unchanged for {stale_for:.0}s of {:.0}s", config.threshold_s));
            decision.attempts = record.attempts;
            decision.fingerprint = Some(current);
            decision.first_seen = record.first_seen;
            decision.stale_for = stale_for;
            return decision;
        }
        if record.attempts >= config.max_attempts {
            let terminal = format!("{} recovery attempt(s) did not submit the text",
                                   record.attempts);
            if let Some(stored) = self.records.get_mut(&obs.pane_id) {
                stored.terminal = Some(terminal.clone());
            }
            self.spent.insert(
                ledger_key(&obs.pane_id, message_id, obs.generation.as_deref(), &current),
                record.attempts);
            let mut decision = Decision::new("terminal", terminal.clone());
            decision.attempts = record.attempts;
            decision.terminal = Some(terminal);
            decision.fingerprint = Some(current);
            decision.first_seen = Some(first_seen);
            decision.stale_for = stale_for;
            return decision;
        }
        if let Some(last) = record.last_attempt_at {
            if now - last < config.cooldown_s {
                let mut decision = Decision::new(
                    "cooling",
                    format!("attempted {:.0}s ago; cooldown is {:.0}s", now - last,
                            config.cooldown_s));
                decision.attempts = record.attempts;
                decision.fingerprint = Some(current);
                decision.first_seen = record.first_seen;
                decision.stale_for = stale_for;
                return decision;
            }
        }
        let attempts = record.attempts + 1;
        if let Some(stored) = self.records.get_mut(&obs.pane_id) {
            stored.attempts = attempts;
            stored.last_attempt_at = Some(now);
        }
        // RESERVED BEFORE THE PRESS: the caller must persist this before it sends a
        // key, so a crash between the key and the result cannot refund the attempt.
        self.spent.insert(
            ledger_key(&obs.pane_id, message_id, obs.generation.as_deref(), &current),
            attempts);
        let mut decision = Decision::new(
            "recover",
            format!("unchanged for {stale_for:.0}s and idle; attempt {attempts} of {}",
                    config.max_attempts));
        decision.attempts = attempts;
        decision.fingerprint = Some(current);
        decision.first_seen = record.first_seen;
        decision.stale_for = stale_for;
        decision
    }

    fn reset(&mut self, obs: &Observation, reason: impl Into<String>) -> Decision {
        self.records.remove(&obs.pane_id);
        Decision::new("reset", reason)
    }

    pub fn reserved(&self, pane_id: &str, message_id: &str, generation: Option<&str>,
                     fingerprint: &str) -> u32 {
        *self.spent.get(&ledger_key(pane_id, message_id, generation, fingerprint))
            .unwrap_or(&0)
    }

    pub fn mark_terminal(&mut self, pane_id: &str, reason: &str) {
        if let Some(record) = self.records.get_mut(pane_id) {
            record.terminal = Some(reason.to_string());
        }
    }

    pub fn forget(&mut self, pane_id: &str) {
        self.records.remove(pane_id);
    }
}
