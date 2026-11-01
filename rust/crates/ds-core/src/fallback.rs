//! `dunk_fallback.py` (T55, `fallback.rs`): t359's deterministic model-fallback planning.
//!
//! Given the SAME credit numbers the operations board reads (`dunkingsheep usage --json`),
//! decide which subscription a pane should be relaunched on when its own is exhausted.
//!
//! Two properties matter more than the feature. REPRODUCIBLE: the decision is a pure function
//! of the credit snapshot and the pane state: no clock, no randomness, no dict-iteration
//! order, no model in the loop. FAIL CLOSED: a pane is only swapped when the target is
//! positively known to have credit AND the pane is positively known to be between turns.
//! Unknown credit is not credit; unknown turn state is not idle.
//!
//! This module only PLANS. It relaunches nothing and touches no pane, config or daemon.
//!
//! Typed `Credit`/`Reading` replace the Python's dicts because the tests (and a real
//! `usage --json`) can carry a NaN percentage, which `serde_json::Value` cannot represent;
//! `Reading::from_json` converts the live JSON shape.

use std::collections::BTreeMap;
use std::fmt;

use serde_json::{json, Map, Value};

/// The owner's tier order. A pane walks FORWARD through this list only: a lane on codex may
/// fall back to pi or free, never back to claude, so a swap cannot oscillate.
pub const TIER_ORDER: [&str; 4] = ["claude", "codex", "pi", "free"];

/// Every quota window a provider is REQUIRED to report. A reading that omits one is not
/// partially good news: the missing window could be the empty one, so the whole provider is
/// "unknown" (CoS review i33219 finding 3).
pub fn required_windows(provider: &str) -> Option<&'static [&'static str]> {
    match provider {
        "claude" => Some(&["five_hour", "weekly_all"]),
        "codex" => Some(&["weekly"]),
        "opencode-go" => Some(&["five_hour", "weekly", "monthly"]),
        _ => None,
    }
}

/// Free rows are usable only when a snapshot positively says so; the planner never assumes
/// free capacity ("everything is spent, so move it to free" is the unsafe swap the owner forbade).
pub const FREE_PROVIDER: &str = "free";

/// Which usage provider each tier's credit is read from. The board's "pi" reading is a LOCAL
/// SESSION ESTIMATE, not plan quota, so the pi tier reads `opencode-go` rows only.
pub fn tier_providers(tier: &str) -> &'static [&'static str] {
    match tier {
        "claude" => &["claude"],
        "codex" => &["codex"],
        "pi" => &["opencode-go"],
        "free" => &[FREE_PROVIDER],
        _ => &[],
    }
}

/// Per-harness relaunch facts from the owner's spec, kept as data so the planner can report
/// them rather than a relauncher hard-coding them. Unknown tiers get pi's.
pub fn harness_wiring(tier: &str) -> Value {
    match tier {
        "codex" => json!({
            "args": ["-s", "danger-full-access", "-a", "never"],
            // t310/t264 recorded that codex could not emit turn events; the paired native
            // producers accepted since then supersede that, so a relauncher must wire it.
            "emits_turn_events": true,
            "note": "codex needs -s danger-full-access -a never; paired native turn producers now exist",
        }),
        "claude" => json!({
            "args": [],
            "emits_turn_events": true,
            // No static reset date: claude's quota comes from the credit snapshot.
            "note": "claude quota is read from the credit snapshot",
        }),
        _ => json!({
            "args": [],
            "emits_turn_events": true,
            "note": "pi needs its turn-generation producer and can emit turn events",
        }),
    }
}

/// A window at or below this percentage is spent. The board reports one decimal, so anything
/// below a tenth of a percent is indistinguishable from empty.
pub const SPENT_PERCENT: f64 = 0.1;

/// A TARGET tier must have MORE than this much credit left in every window (owner rule, VP
/// i33250: "above 5 reserve"). Deliberately different from `SPENT_PERCENT`: that decides whether
/// the pane's CURRENT tier is exhausted (what already happened), this decides whether a TARGET
/// is worth moving to (not landing on a subscription about to run dry).
pub const RESERVE_PERCENT: f64 = 5.0;

/// A caller error, not a decision: bad input shapes are refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FallbackError(pub String);

impl fmt::Display for FallbackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FallbackError {}

/// A window's `percent_remaining`: a number, or anything that is not one (string, null, bool).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Percent {
    Number(f64),
    NotANumber,
}

/// One quota window of a reading.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    /// `str(window.get("name") or "").strip()`; empty means unnamed.
    pub name: String,
    pub percent: Percent,
}

/// One provider reading, as `dunkingsheep usage --json` shapes it.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub provider: String,
    pub status: String,
    pub windows: Vec<Window>,
}

fn py_text(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Bool(true)) => "True".into(),
        Some(Value::Bool(false)) => "".into(),
        Some(other) => other.to_string(),
    }
}

impl Reading {
    /// Convert one live JSON reading. A boolean percentage is NOT a number (Python's
    /// `isinstance(value, bool)` exclusion), and neither is a string or null.
    pub fn from_json(value: &Value) -> Reading {
        let windows = value
            .get("windows")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .map(|w| Window {
                        name: py_text(w.get("name")).trim().to_string(),
                        percent: match w.get("percent_remaining") {
                            Some(Value::Number(n)) => n
                                .as_f64()
                                .map(Percent::Number)
                                .unwrap_or(Percent::NotANumber),
                            _ => Percent::NotANumber,
                        },
                    })
                    .collect()
            })
            .unwrap_or_default();
        Reading {
            provider: py_text(value.get("provider")).trim().to_string(),
            status: py_text(value.get("status")),
            windows,
        }
    }
}

/// The reduced credit state: each provider's `available` / `exhausted` / `unknown`, plus the
/// lowest remaining percentage of each provider that has numbers.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Credit {
    pub states: BTreeMap<String, String>,
    pub headroom: BTreeMap<String, f64>,
}

impl Credit {
    /// `credit.get(name, "unknown")`.
    pub fn state_of(&self, provider: &str) -> &str {
        self.states
            .get(provider)
            .map(String::as_str)
            .unwrap_or("unknown")
    }

    /// The Python dict form: `{provider: state, ..., "headroom": {provider: pct}}`.
    pub fn to_value(&self) -> Value {
        let mut out = Map::new();
        for (name, state) in &self.states {
            out.insert(name.clone(), json!(state));
        }
        let mut headroom = Map::new();
        for (name, pct) in &self.headroom {
            headroom.insert(name.clone(), json!(pct));
        }
        out.insert("headroom".into(), Value::Object(headroom));
        Value::Object(out)
    }
}

/// `credit_state`: reduce `usage --json` readings to `{provider: state}` plus headroom. A
/// provider is available only when the reading succeeded AND every window it reports has credit
/// left. A reading with no windows is `unknown`: "no numbers" never reads as "plenty".
pub fn credit_state(readings: &[Reading]) -> Credit {
    let mut credit = Credit::default();
    for reading in readings {
        let provider = reading.provider.trim();
        if provider.is_empty() {
            continue;
        }
        let unknown = |credit: &mut Credit| {
            credit.states.insert(provider.to_string(), "unknown".into());
        };
        if reading.status != "ok" {
            unknown(&mut credit);
            continue;
        }
        let mut by_name: Vec<(&str, Percent)> = Vec::new();
        let mut unidentified = false;
        for window in &reading.windows {
            let name = window.name.trim();
            // A window we cannot identify, or two windows claiming the same name, means we do
            // not know which numbers describe this subscription (reviewer %19's residual).
            if name.is_empty() || by_name.iter().any(|(n, _)| *n == name) {
                unidentified = true;
                continue;
            }
            by_name.push((name, window.percent));
        }
        if unidentified || by_name.is_empty() {
            // pi reports a local session estimate, not plan quota: unknown, not assumed good.
            unknown(&mut credit);
            continue;
        }
        if let Some(required) = required_windows(provider) {
            if required
                .iter()
                .any(|r| !by_name.iter().any(|(n, _)| n == r))
            {
                // One missing window is not "the others look fine": it could be the empty one.
                unknown(&mut credit);
                continue;
            }
        }
        // ALL-REPORTED-WINDOW CONTRACT (VP i33250): every reported window is evaluated.
        let mut numbers = Vec::new();
        let mut ok = true;
        for (_, percent) in &by_name {
            match percent {
                Percent::Number(v) if v.is_finite() => numbers.push(*v),
                _ => {
                    ok = false;
                    break;
                }
            }
        }
        if !ok || numbers.is_empty() {
            unknown(&mut credit);
            continue;
        }
        let lowest = numbers.iter().copied().fold(f64::INFINITY, f64::min);
        credit.states.insert(
            provider.to_string(),
            if lowest <= SPENT_PERCENT {
                "exhausted"
            } else {
                "available"
            }
            .into(),
        );
        credit.headroom.insert(provider.to_string(), lowest);
    }
    credit
}

/// `tier_headroom`: the lowest FINITE remaining percentage for a tier, or None. None means "no
/// usable number", which is not "plenty": such a tier cannot be a target.
pub fn tier_headroom(credit: &Credit, tier: &str) -> Option<f64> {
    tier_providers(tier)
        .iter()
        .filter_map(|name| credit.headroom.get(*name).copied())
        .filter(|v| v.is_finite())
        .fold(None, |acc: Option<f64>, v| {
            Some(acc.map_or(v, |a| a.min(v)))
        })
}

/// `tier_credit`: the headroom keyed by the TIER name (as the Python reads it); None is never
/// treated as headroom.
pub fn tier_credit(credit: &Credit, tier: &str) -> Option<f64> {
    if tier_providers(tier).is_empty() {
        return None;
    }
    credit.headroom.get(tier).copied()
}

/// `tier_state`: availability of one tier from the reduced credit. Available if ANY of its
/// providers has credit, exhausted only if ALL are exhausted, unknown otherwise; a tier with no
/// provider mapped is unknown, so it can never be free capacity.
pub fn tier_state(credit: &Credit, tier: &str) -> &'static str {
    let providers = tier_providers(tier);
    if providers.is_empty() {
        return "unknown";
    }
    let states: Vec<&str> = providers.iter().map(|p| credit.state_of(p)).collect();
    if states.contains(&"available") {
        "available"
    } else if states.iter().all(|s| *s == "exhausted") {
        "exhausted"
    } else {
        "unknown"
    }
}

fn provider_label(tier: &str) -> String {
    let providers = tier_providers(tier);
    if providers.is_empty() {
        tier.to_string()
    } else {
        providers.join("+")
    }
}

fn credit_label(credit: &Credit, tier: &str) -> String {
    let providers = tier_providers(tier);
    if providers.is_empty() {
        return "unknown".into();
    }
    providers
        .iter()
        .map(|p| credit.state_of(p))
        .collect::<Vec<_>>()
        .join("+")
}

/// Python `repr(float)` for the percentages a reading carries (`42.0`, `96.9`, `0.05`).
fn py_float(v: f64) -> String {
    // `{:?}` keeps the ".0" on whole numbers and the shortest round-trip digits, as repr does.
    // (Exponent forms for values below 1e-4 differ from Python's `1e-05`; none is a percentage
    // a reading carries.)
    format!("{v:?}")
}

fn skip(reason: &str, extra: &[(&str, Value)]) -> Value {
    let mut out = Map::new();
    out.insert("action".into(), json!("skip"));
    out.insert("reason".into(), json!(reason));
    for (key, value) in extra {
        out.insert((*key).to_string(), value.clone());
    }
    Value::Object(out)
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `plan_swap`: decide whether `pane` should be relaunched on another tier.
///
/// `pane` is a mapping with `pane_id` (stable; the relaunch must preserve it), `tier` (the tier
/// it runs now), `harness`, `turn_state` (`working` | `idle` | unknown) and
/// `owned_by_mechanism`. Returns a decision: `action` is `swap` or `skip`; a skip always carries
/// a `reason` so "nothing to do" and "I refused" never look alike. No side effects.
pub fn plan_swap(
    pane: &Value,
    credit: &Credit,
    allowed_panes: Option<&[String]>,
) -> Result<Value, FallbackError> {
    if !pane.is_object() {
        return Err(FallbackError("pane must be a mapping".into()));
    }
    let pane_id = py_text(pane.get("pane_id")).trim().to_string();
    if pane_id.is_empty() {
        return Err(FallbackError("pane needs a pane_id".into()));
    }
    let owned = match allowed_panes {
        Some(allowed) => allowed.iter().any(|p| *p == pane_id),
        None => truthy(pane.get("owned_by_mechanism")),
    };
    if !owned {
        return Ok(skip(
            "not_owned_by_mechanism",
            &[("pane_id", json!(pane_id))],
        ));
    }
    let turn = pane.get("turn_state").cloned().unwrap_or(Value::Null);
    if turn != json!("idle") {
        // "working" is mid-turn; None is unknown. Both refuse: never swap mid-turn, and an
        // unknown turn is not evidence of a gap.
        let reason = if turn == json!("working") {
            "mid_turn"
        } else {
            "turn_unknown"
        };
        return Ok(skip(reason, &[("pane_id", json!(pane_id))]));
    }
    let current_value = pane.get("tier").cloned().unwrap_or(Value::Null);
    let current = current_value.as_str().filter(|t| TIER_ORDER.contains(t));
    let Some(current) = current else {
        return Ok(skip(
            "unknown_current_tier",
            &[("pane_id", json!(pane_id)), ("current", current_value)],
        ));
    };
    if current == "free" {
        // The end of the line: nothing cheaper behind it.
        return Ok(skip(
            "on_last_tier",
            &[("pane_id", json!(pane_id)), ("current", json!(current))],
        ));
    }
    match tier_state(credit, current) {
        "available" => {
            return Ok(skip(
                "current_has_credit",
                &[("pane_id", json!(pane_id)), ("current", json!(current))],
            ))
        }
        "exhausted" => {}
        // Unknown is not exhausted: swapping on unproven exhaustion would move a working lane
        // off a subscription that may still have credit.
        _ => {
            return Ok(skip(
                "current_credit_unknown",
                &[("pane_id", json!(pane_id)), ("current", json!(current))],
            ))
        }
    }
    let start = TIER_ORDER.iter().position(|t| *t == current).unwrap_or(0);
    for tier in &TIER_ORDER[start + 1..] {
        // "free" is a tier like any other: its state comes from the snapshot.
        if tier_state(credit, tier) != "available" {
            continue;
        }
        // No positive finite number means the reserve is UNPROVEN, not satisfied.
        let Some(left) = tier_headroom(credit, tier) else {
            continue;
        };
        // Above-5 reserve: a tier that still has credit but is nearly dry is not a landing place.
        if left <= RESERVE_PERCENT {
            continue;
        }
        return Ok(json!({
            "action": "swap",
            "pane_id": pane_id,
            "from": current,
            "to": tier,
            "harness": pane.get("harness").cloned().unwrap_or(Value::Null),
            "wiring": harness_wiring(tier),
            "record": format!(
                "pane {pane_id}: {current} -> {tier} because {} credit {} ({tier} {}% left, above 5 reserve)",
                provider_label(current), credit_label(credit, current), py_float(left)
            ),
        }));
    }
    // Every later tier is exhausted or unknown: leave the pane alone and say exactly why.
    let mut remaining = Map::new();
    let mut texts = Vec::new();
    for tier in &TIER_ORDER[start + 1..] {
        let state = tier_state(credit, tier);
        let text = match tier_headroom(credit, tier) {
            None => format!("{state} (headroom unknown)"),
            Some(left) => format!("{state} ({}% left)", py_float(left)),
        };
        texts.push(text.clone());
        remaining.insert((*tier).to_string(), json!(text));
    }
    let mut reason = "no_tier_has_credit";
    if texts.iter().any(|t| t.starts_with("available")) {
        // A later tier has credit but not enough of it: a reserve refusal, not an exhaustion one.
        reason = "targets_below_reserve";
    }
    if texts
        .iter()
        .any(|t| t.starts_with("available (headroom unknown"))
    {
        // Available but with no number behind it: the reserve is unproven.
        reason = "target_headroom_unknown";
    }
    Ok(skip(
        reason,
        &[
            ("pane_id", json!(pane_id)),
            ("current", json!(current)),
            ("tiers", Value::Object(remaining)),
        ],
    ))
}

/// `plan_all`: plan every pane, in stable pane-id order so two readers agree.
pub fn plan_all(
    panes: &[Value],
    credit: &Credit,
    allowed_panes: Option<&[String]>,
) -> Result<Value, FallbackError> {
    let mut ordered: Vec<&Value> = panes.iter().collect();
    ordered.sort_by_key(|p| match p.get("pane_id") {
        None | Some(Value::Null) => "None".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    });
    let mut decisions = Vec::new();
    for pane in ordered {
        decisions.push(plan_swap(pane, credit, allowed_panes)?);
    }
    let swapped: Vec<Value> = decisions
        .iter()
        .filter(|d| d["action"] == "swap")
        .cloned()
        .collect();
    let skipped: Vec<Value> = decisions
        .iter()
        .filter(|d| d["action"] != "swap")
        .cloned()
        .collect();
    let summary = format!(
        "{} swap(s), {} skip(s)",
        swapped.len(),
        decisions.len() - swapped.len()
    );
    Ok(json!({"decisions": decisions, "swapped": swapped, "skipped": skipped, "summary": summary}))
}

#[cfg(test)]
#[path = "fallback_tests.rs"]
mod fallback_tests;
