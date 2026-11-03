//! Pure watchdog streak, cooldown, and escalation decisions ported from
//! `dunk_watchdog.py`'s `dedupe_key`, `advance_streaks`, `_cooling`, and `plan`.
//!
//! These functions consume the already validated config, loaded state, and evaluated findings.
//! They do not send, persist, or mutate any input.

use serde_json::{json, Map, Value};

use super::config::{OWNER_CLASSES, OWNER_ELIGIBLE_RULES};
use super::severity_rank;

/// Python `dunk_watchdog.SOURCE_SELF`; self findings bypass VP severity and VP cooldown checks.
pub const SOURCE_SELF: &str = "watchdog-self";

/// Python `dedupe_key`: stable identity shared by alert, owner page, and streak state.
pub fn dedupe_key(item: &Value) -> String {
    format!(
        "{}|{}",
        item["rule"].as_str().expect("finding rule is a string"),
        item["key"].as_str().expect("finding key is a string")
    )
}

/// Python `advance_streaks`: calculate this run's consecutive, spaced finding counts.
/// Findings absent from this run are omitted, which resets those streaks.
pub fn advance_streaks(config: &Value, state: &Value, findings: &[Value], now: f64) -> Value {
    let spacing = config["min_run_spacing_seconds"]
        .as_f64()
        .expect("validated spacing is numeric");
    let empty = Map::new();
    let old = match state.get("streaks") {
        Some(streaks) => streaks.as_object().expect("loaded streaks are an object"),
        None => &empty,
    };
    let mut new = Map::new();

    for item in findings {
        let key = dedupe_key(item);
        let next = match old.get(&key) {
            None => json!({"count": 1, "last_counted": now}),
            Some(prior) => {
                let last_counted = prior["last_counted"]
                    .as_f64()
                    .expect("streak last_counted is numeric");
                if now - last_counted >= spacing {
                    let count = prior["count"].as_i64().expect("streak count is an integer");
                    json!({"count": count + 1, "last_counted": now})
                } else {
                    // Clone the whole entry like Python's dict(prior); unknown fields survive.
                    prior.clone()
                }
            }
        };
        new.insert(key, next);
    }

    Value::Object(new)
}

/// Python `_cooling`: self findings bypass cooldown; once findings never cool down.
pub fn cooling(config: &Value, state: &Value, item: &Value, now: f64) -> bool {
    if item["source"].as_str() == Some(SOURCE_SELF) {
        return false;
    }
    let key = dedupe_key(item);
    let alerts = state
        .get("alerts")
        .and_then(Value::as_object)
        .expect("loaded alerts are an object");
    let last_alerted = alerts
        .get(&key)
        .and_then(|entry| entry.get("last_alerted"))
        .filter(|value| !value.is_null());
    let Some(last_alerted) = last_alerted else {
        return false;
    };
    if item
        .get("once")
        .is_some_and(|once| once.as_bool() == Some(true))
    {
        return true;
    }
    let last_alerted = last_alerted
        .as_f64()
        .expect("alert last_alerted is numeric");
    let rule = item["rule"].as_str().expect("finding rule is a string");
    let cooldown_minutes = config["rules"][rule]["cooldown_minutes"]
        .as_f64()
        .expect("validated cooldown is numeric");
    now - last_alerted < cooldown_minutes * 60.0
}

/// Python `plan`: ordered pure VP and owner decisions, one result per input finding.
/// A nonempty `owner_blocked` reason suppresses owner pages run-wide.
pub fn plan(
    config: &Value,
    findings: &[Value],
    state: &Value,
    streaks: &Value,
    now: f64,
    owner_blocked: Option<&str>,
) -> Vec<Value> {
    let vp_rule = &config["escalation"]["vp"];
    let owner = &config["escalation"]["owner"];
    let min_vp_severity = severity_rank(
        vp_rule["min_severity"]
            .as_str()
            .expect("validated VP severity is a string"),
    );
    let owner_min_severity = severity_rank(
        owner["min_severity"]
            .as_str()
            .expect("validated owner severity is a string"),
    );
    let max_alerts = config["max_alerts_per_run"]
        .as_i64()
        .expect("validated VP cap is an integer");
    let hour_ago = now - 3600.0;
    let empty_times = Vec::new();
    let owner_times = state
        .get("owner_page_times")
        .map(|times| {
            times
                .as_array()
                .expect("loaded owner_page_times is an array")
        })
        .unwrap_or(&empty_times);
    let recent_owner_count = owner_times
        .iter()
        .filter(|time| time.as_f64().expect("owner page time is numeric") > hour_ago)
        .count() as i64;
    let mut owner_budget = owner["max_pages_per_hour"]
        .as_i64()
        .expect("validated owner cap is an integer")
        - recent_owner_count;
    let mut vp_count = 0_i64;
    let owner_pages = state["owner_pages"]
        .as_object()
        .expect("loaded owner_pages is an object");
    let streak_map = streaks.as_object().expect("advanced streaks are an object");
    let owner_classes = owner["classes"]
        .as_array()
        .expect("validated owner classes are an array");
    let mut decisions = Vec::with_capacity(findings.len());

    for item in findings {
        let key = dedupe_key(item);
        let source = item["source"].as_str().expect("finding source is a string");
        let is_self = source == SOURCE_SELF;
        let class = item["class"].as_str().expect("finding class is a string");
        let rule_id = item["rule"].as_str().expect("finding rule is a string");
        let severity = severity_rank(
            item["severity"]
                .as_str()
                .expect("finding severity is a string"),
        );
        let streak = streak_map
            .get(&key)
            .and_then(|entry| entry.get("count"))
            .cloned()
            .unwrap_or_else(|| json!(1));
        let streak_count = streak.as_i64().expect("finding streak count is an integer");
        let mut decision = json!({
            "finding": item,
            "vp": Value::Null,
            "owner": Value::Null,
            "streak": streak,
        });

        // Keep Python's short-circuit order: each suppression has a distinct reason.
        let vp_reason = if !is_self && severity < min_vp_severity {
            "below-min-severity"
        } else if rule_id == "fleet.assignment_starvation"
            && streak_count
                < config["rules"][rule_id]["must_hold_runs"]
                    .as_i64()
                    .expect("validated hold count is an integer")
        {
            "hold"
        } else if cooling(config, state, item, now) {
            "dedupe"
        } else if vp_count >= max_alerts {
            "rate"
        } else {
            vp_count += 1;
            "no"
        };
        decision["vp"] = json!(vp_reason);

        let owner_eligible =
            !is_self && OWNER_CLASSES.contains(&class) && OWNER_ELIGIBLE_RULES.contains(&rule_id);
        if owner_eligible {
            let rule = &config["rules"][rule_id];
            let pending = owner_pages.get(&key);
            let owner_reason = if owner["enabled"].as_bool() != Some(true)
                || !owner_classes
                    .iter()
                    .any(|configured| configured.as_str() == Some(class))
            {
                "disabled"
            } else if severity < owner_min_severity {
                "below-min-severity"
            } else if let Some(reason) = owner_blocked.filter(|reason| !reason.is_empty()) {
                reason
            } else if streak_count
                < rule["must_hold_runs"]
                    .as_i64()
                    .expect("validated hold count is an integer")
            {
                "hold"
            } else if pending.is_some_and(|page| {
                let confirmed_at = page.get("confirmed_at").filter(|value| !value.is_null());
                match confirmed_at {
                    None => true,
                    Some(confirmed_at) => {
                        now - confirmed_at
                            .as_f64()
                            .expect("owner confirmation time is numeric")
                            < rule["cooldown_minutes"]
                                .as_f64()
                                .expect("validated cooldown is numeric")
                                * 60.0
                    }
                }
            }) {
                "dedupe"
            } else if owner_budget <= 0 {
                "global-cap"
            } else {
                owner_budget -= 1;
                "no"
            };
            decision["owner"] = json!(owner_reason);
        }

        decisions.push(decision);
    }

    decisions
}

#[cfg(test)]
#[path = "planner_tests.rs"]
mod tests;
