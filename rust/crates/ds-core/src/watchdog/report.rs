//! Recovery selection from `dunk_watchdog.py::recoveries`.
//!
//! This module only selects disappeared findings from persisted alert/page state. It has no
//! transport or state mutation behavior.

use std::collections::BTreeSet;

use serde_json::{json, Value};

/// Select VP recoveries for prior alerts and owner recoveries for confirmed owner pages.
///
/// Both outputs are sorted dedupe keys, matching Python's `sorted(...)` result. A still-current
/// finding suppresses both recovery lines; an alert never sent to the VP and a page not yet
/// confirmed are not reported as recovered.
pub fn recoveries(state: &Value, findings: &[Value]) -> Value {
    let current: BTreeSet<String> = findings
        .iter()
        .map(|finding| {
            format!(
                "{}|{}",
                finding["rule"].as_str().unwrap_or(""),
                finding["key"].as_str().unwrap_or("")
            )
        })
        .collect();

    let vp = state["alerts"]
        .as_object()
        .into_iter()
        .flat_map(|alerts| alerts.iter())
        .filter(|(key, entry)| !entry["last_alerted"].is_null() && !current.contains(key.as_str()))
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();

    let owner = state["owner_pages"]
        .as_object()
        .into_iter()
        .flat_map(|pages| pages.iter())
        .filter(|(key, page)| !page["confirmed_at"].is_null() && !current.contains(key.as_str()))
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();

    json!({"vp": vp, "owner": owner})
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod report_tests;
