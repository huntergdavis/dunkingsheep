//! T1400 delivery rule slice: report queued messages held on an ambiguous composer.
//!
//! The rule is deliberately read-only. It reports the fixed refusal class and documented row
//! numbers, never a queued payload or arbitrary suffix from the metrics reason.

use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{json, Value};

use super::{finding, minutes, py_float, py_number, SOURCE_RULE};
use crate::error::DunkError;

pub const AMBIGUOUS_READER_MARKER: &str = "ambiguous composer/transcript";

/// The newest held-message observation within a pane/message time window.
#[derive(Debug, Clone, PartialEq)]
pub struct HeldObservation {
    pub reason: String,
    pub at: f64,
}

/// The read-only query used by `rule_delivery_ambiguous_queued`.
pub trait HeldObservations {
    fn latest_held(
        &self,
        pane: &str,
        since: f64,
        now: f64,
    ) -> Result<Option<HeldObservation>, DunkError>;
}

/// `metrics.db`, opened read-only for each query like `Inputs.metrics`.
pub struct MetricsDb<'a> {
    pub path: &'a Path,
}

impl HeldObservations for MetricsDb<'_> {
    fn latest_held(
        &self,
        pane: &str,
        since: f64,
        now: f64,
    ) -> Result<Option<HeldObservation>, DunkError> {
        let conn = Connection::open_with_flags(self.path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(crate::db::map_sqlite)?;
        conn.query_row(
            "SELECT reason, at FROM events WHERE kind='message.held' AND pane_id=? \
             AND at>=? AND at<=? ORDER BY at DESC LIMIT 1",
            rusqlite::params![pane, since, now],
            |row| {
                Ok(HeldObservation {
                    reason: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    at: row.get(1)?,
                })
            },
        )
        .optional()
        .map_err(crate::db::map_sqlite)
    }
}

/// Parse only the documented shape; an arbitrary suffix is intentionally discarded.
pub fn ambiguous_shape(reason: &str) -> Value {
    let pattern = regex::Regex::new(
        r"^ambiguous composer/transcript: prompt row (\d+) is separated from footer row (\d+) by (\d+) blank rows",
    )
    .expect("the ambiguous composer pattern is static");
    let Some(captures) = pattern.captures(reason) else {
        return json!({});
    };
    let number = |index| {
        captures
            .get(index)
            .and_then(|capture| capture.as_str().parse::<i64>().ok())
            .unwrap_or(0)
    };
    json!({
        "prompt_row": number(1),
        "footer_row": number(2),
        "blank_rows": number(3),
    })
}

/// Report queued messages whose latest in-lifetime held event is an ambiguous composer refusal.
/// Metrics errors are ignored as in Python: an unreadable metrics store produces no finding.
pub fn rule_delivery_ambiguous_queued(
    outbox: &[Value],
    events: &dyn HeldObservations,
    rule: &Value,
    now: f64,
) -> Vec<Value> {
    let min_holds = rule["min_holds"].as_f64().unwrap_or(0.0);
    let min_age = rule["min_age_minutes"].as_f64().unwrap_or(0.0) * 60.0;
    let mut findings = Vec::new();

    for row in outbox {
        let holds = py_int(row.get("holds").unwrap_or(&Value::Null)).unwrap_or(0);
        if (holds as f64) < min_holds {
            continue;
        }
        let created = row
            .get("created_at")
            .filter(|value| truthy(value))
            .and_then(py_f64)
            .unwrap_or(now);
        let age_s = (now - created).max(0.0);
        if age_s < min_age {
            continue;
        }
        let pane = row
            .get("pane_id")
            .filter(|value| truthy(value))
            .map(py_text)
            .unwrap_or_default();
        let message_id = row
            .get("id")
            .filter(|value| truthy(value))
            .map(py_text)
            .unwrap_or_default();
        if pane.is_empty() || message_id.is_empty() {
            continue;
        }

        let observation = events.latest_held(&pane, created - 5.0, now).ok().flatten();
        let reason = observation
            .as_ref()
            .map(|event| event.reason.as_str())
            .unwrap_or("");
        if !reason.starts_with(AMBIGUOUS_READER_MARKER) {
            continue;
        }

        let age_minutes = minutes(age_s);
        let observed_age = observation.as_ref().and_then(|event| {
            (event.at != 0.0).then(|| ((now - event.at).max(0.0)).round_ties_even() as i64)
        });
        let next_attempt = row
            .get("next_attempt_at")
            .filter(|value| truthy(value))
            .and_then(py_f64)
            .map(|at| ((at - now).max(0.0)).round_ties_even() as i64);

        findings.push(finding(
            "delivery.ambiguous_queued",
            rule,
            &message_id,
            format!(
                "message {message_id} has been queued {} min on {pane} with {holds} refused attempts: the reader could not delimit that composer (message retained, not terminal, recipient not assumed dead)",
                py_float(age_minutes)
            ),
            format!(
                "{} refused attempts and {} min queued",
                py_number(&rule["min_holds"]),
                py_number(&rule["min_age_minutes"])
            ),
            format!("{holds} attempts, {} queued", py_float(age_minutes)),
            "queue age and refused attempts".into(),
            json!({
                "message": message_id,
                "pane": pane,
                "age_minutes": age_minutes,
                "holds": holds,
                "reason_class": "ambiguous_reader",
                "shape": ambiguous_shape(reason),
                "last_refusal_age_s": observed_age,
                "next_attempt_in_s": next_attempt,
            }),
            SOURCE_RULE,
            false,
        ));
    }
    findings
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn py_int(value: &Value) -> Option<i64> {
    match value {
        Value::Bool(value) => Some(i64::from(*value)),
        Value::Number(value) => value
            .as_i64()
            .or_else(|| value.as_f64().map(|number| number as i64)),
        Value::String(value) => value.parse().ok(),
        _ => None,
    }
}

fn py_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Bool(value) => Some(if *value { 1.0 } else { 0.0 }),
        Value::Number(value) => value.as_f64(),
        Value::String(value) => value.parse().ok(),
        _ => None,
    }
}

fn py_text(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::String(value) => value.clone(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        other => other.to_string(),
    }
}

#[cfg(test)]
#[path = "rules_delivery_tests.rs"]
mod ambiguous_queued_tests;
