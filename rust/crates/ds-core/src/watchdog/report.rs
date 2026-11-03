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

    let mut vp = state["alerts"]
        .as_object()
        .into_iter()
        .flat_map(|alerts| alerts.iter())
        .filter(|(key, entry)| !entry["last_alerted"].is_null() && !current.contains(key.as_str()))
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();

    let mut owner = state["owner_pages"]
        .as_object()
        .into_iter()
        .flat_map(|pages| pages.iter())
        .filter(|(key, page)| !page["confirmed_at"].is_null() && !current.contains(key.as_str()))
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();

    vp.sort();
    owner.sort();
    json!({"vp": vp, "owner": owner})
}

/// Serialize evidence like Python's `json.dumps(..., sort_keys=True, separators=(",", ":"))`.
///
/// serde_json emits compact JSON and orders object keys by default; this additionally escapes
/// non-ASCII characters because Python's default `ensure_ascii=True` does.
pub fn evidence(item: &Value) -> String {
    let encoded = serde_json::to_string(&item["evidence"]).unwrap_or_else(|_| "null".into());
    let mut ascii = String::with_capacity(encoded.len());
    for ch in encoded.chars() {
        if ch.is_ascii() {
            ascii.push(ch);
        } else {
            let code = ch as u32;
            if code <= 0xffff {
                ascii.push_str(&format!("\\u{code:04x}"));
            } else {
                let scalar = code - 0x1_0000;
                let high = 0xd800 + (scalar >> 10);
                let low = 0xdc00 + (scalar & 0x3ff);
                ascii.push_str(&format!("\\u{high:04x}\\u{low:04x}"));
            }
        }
    }
    ascii
}

fn field_text(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

/// Format one finding line, including the held-run window when an owner hold applies.
pub fn line(
    prefix: &str,
    item: &Value,
    suppressed: &Value,
    streak: Option<&Value>,
    hold: Option<&Value>,
) -> String {
    let window = if let Some(hold) = hold {
        format!(
            "{}; held {}/{} runs",
            field_text(&item["window"]),
            streak.map(field_text).unwrap_or_else(|| "null".into()),
            field_text(hold)
        )
    } else {
        field_text(&item["window"])
    };

    format!(
        "{prefix} rule={} class={} severity={} source={} threshold={} measured={} window={} dedupe={}|{} suppressed={} evidence={}",
        field_text(&item["rule"]),
        field_text(&item["class"]),
        field_text(&item["severity"]),
        field_text(&item["source"]),
        field_text(&item["threshold"]),
        field_text(&item["measured"]),
        window,
        field_text(&item["rule"]),
        field_text(&item["key"]),
        field_text(suppressed),
        evidence(item),
    )
}

/// Render finding, skipped-rule, and recovered-finding lines without performing delivery.
pub fn render_lines(
    config: &Value,
    decisions: &[Value],
    skipped: &[Value],
    recovered: &Value,
    mode: &str,
) -> Vec<String> {
    let (vp_prefix, owner_prefix) = if mode == "live" {
        ("NOTIFY VP", "PAGE OWNER")
    } else {
        ("WOULD-NOTIFY VP", "WOULD-PAGE OWNER")
    };
    let mut output = Vec::new();

    for decision in decisions {
        let item = &decision["finding"];
        if !decision["owner"].is_null() {
            let hold = &config["rules"][field_text(&item["rule"])]["must_hold_runs"];
            output.push(line(
                owner_prefix,
                item,
                &decision["owner"],
                Some(&decision["streak"]),
                Some(hold),
            ));
        }
        output.push(line(vp_prefix, item, &decision["vp"], None, None));
    }

    for entry in skipped {
        output.push(format!(
            "SKIPPED rule={} error={}",
            field_text(&entry["rule"]),
            field_text(&entry["error"])
        ));
    }
    for key in recovered["vp"].as_array().into_iter().flatten() {
        output.push(format!("RESOLVED VP dedupe={}", field_text(key)));
    }
    for key in recovered["owner"].as_array().into_iter().flatten() {
        output.push(format!("RESOLVED OWNER dedupe={}", field_text(key)));
    }
    output
}

/// Build the deterministic watchdog digest body.
pub fn digest(title: &str, lines: &[String]) -> String {
    let mut output = format!("Watchdog {title}: deterministic rules; no model involved.");
    for line in lines {
        output.push('\n');
        output.push_str(line);
    }
    output
}

#[cfg(test)]
#[path = "report_tests.rs"]
mod report_tests;
