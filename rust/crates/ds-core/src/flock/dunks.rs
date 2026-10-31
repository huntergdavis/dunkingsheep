//! `flock/dunks.rs` (T32, slice 1): the `Dunker` data model and the pure helpers around it.
//! Ports `dunk_core.Dunker` (fields, `__init__` defaults, `target_label`, `set_target`,
//! `to_dict`, `from_dict`), `format_interval`, `parse_interval`, `expand_template`.
//!
//! NOT in this slice (tracked on t800): the `Flock` dunk methods (add/update/remove/start/
//! stop/toggle/stop_all/fire/list/get), `_save`/`_load` of `dunks.json`, the per-dunk tokio
//! timer, idle gating and the skip/hold backpressure. Those need the outbox and stuck state
//! that have no Rust owner yet.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::clock;
use crate::error::DunkError;

/// `DEFAULT_TEXT`.
pub const DEFAULT_TEXT: &str = "continue";
/// `DEFAULT_INTERVAL_MINUTES`.
pub const DEFAULT_INTERVAL_MINUTES: f64 = 10.0;

/// `Dunker.FIELDS`, in the Python's order: this is the persisted key order of `dunks.json`.
pub const FIELDS: [&str; 28] = [
    "id",
    "name",
    "target_pane_id",
    "target_agent",
    "target_tab",
    "target_workspace",
    "target_cwd",
    "interval_minutes",
    "text",
    "running",
    "only_when",
    "max_sends",
    "send_count",
    "created_at",
    "started_at",
    "last_sent_at",
    "next_send_at",
    "last_error",
    "skip_if_unconsumed",
    "skip_count",
    "total_skips",
    "last_skipped_at",
    "awaiting_consumption",
    "last_sent_text",
    "unknown_skips",
    "hold_while_typing",
    "hold_count",
    "last_held_at",
];

/// `TEMPLATE_RE`.
fn template_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\{(id|name|count|target|interval|time|date)\}").expect("static regex")
    })
}

/// Python's `format(x, "g")`: six significant digits, no trailing zeros. As in C, the
/// exponent that picks fixed or scientific is the one AFTER rounding to six digits
/// (999999.5 -> `1e+06`), and scientific is used below 1e-4 (`5e-05`).
fn fmt_g(value: f64) -> String {
    if value == 0.0 {
        return "0".into();
    }
    let sci = format!("{value:.5e}");
    let (mantissa, exponent) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exponent.parse().unwrap_or(0);
    let trim = |text: &str| -> String {
        if text.contains('.') {
            text.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            text.to_string()
        }
    };
    if !(-4..6).contains(&exp) {
        return format!(
            "{}e{}{:02}",
            trim(mantissa),
            if exp < 0 { '-' } else { '+' },
            exp.abs()
        );
    }
    let decimals = (5 - exp).max(0) as usize;
    trim(&format!("{value:.decimals$}"))
}

/// `format_interval`: `10`, `1.5`, or `30s` under a minute; round-trips `parse_interval`.
pub fn format_interval(minutes: f64) -> String {
    if 0.0 < minutes && minutes < 1.0 {
        return format!("{}s", fmt_g(minutes * 60.0));
    }
    if minutes == minutes.trunc() {
        return format!("{}", minutes as i64);
    }
    fmt_g(minutes)
}

/// `parse_interval` for a string (the numeric and bool arms of the Python are the caller's:
/// a number is already minutes, see `parse_interval_number`).
pub fn parse_interval(value: &str) -> Result<f64, DunkError> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"^([0-9]*\.?[0-9]+)\s*(s|sec|secs|m|min|mins|h|hr|hrs|)$")
            .expect("static regex")
    });
    let text = value.trim().to_lowercase();
    let bad = || {
        DunkError::Refused(format!(
            "bad interval {} (try 10, 90s, 1.5h)",
            crate::context::store::py_repr(value)
        ))
    };
    let caps = re.captures(&text).ok_or_else(bad)?;
    let number: f64 = caps[1].parse().map_err(|_| bad())?;
    let unit = &caps[2];
    let minutes = if unit.starts_with('s') {
        number / 60.0
    } else if unit.starts_with('h') {
        number * 60.0
    } else {
        number
    };
    parse_interval_number(minutes)
}

/// The numeric arm of `parse_interval`: positive minutes.
pub fn parse_interval_number(minutes: f64) -> Result<f64, DunkError> {
    if minutes > 0.0 {
        Ok(minutes)
    } else {
        Err(DunkError::Refused("interval must be positive".into()))
    }
}

/// One dunk. Plain data plus a little presentation logic (`dunk_core.Dunker`).
#[derive(Debug, Clone, PartialEq)]
pub struct Dunker {
    pub id: String,
    pub name: String,
    pub target_pane_id: Option<String>,
    pub target_agent: Option<String>,
    pub target_tab: Option<String>,
    pub target_workspace: Option<String>,
    pub target_cwd: Option<String>,
    pub interval_minutes: f64,
    pub text: String,
    pub running: bool,
    pub only_when: Option<String>,
    pub max_sends: Option<i64>,
    pub send_count: i64,
    pub created_at: f64,
    pub started_at: Option<f64>,
    pub last_sent_at: Option<f64>,
    pub next_send_at: Option<f64>,
    pub last_error: Option<String>,
    pub skip_if_unconsumed: bool,
    /// The CURRENT run's skips; resets when the dunk sends.
    pub skip_count: i64,
    /// Lifetime skips: history, never the thing a threshold fires on.
    pub total_skips: i64,
    pub last_skipped_at: Option<f64>,
    pub awaiting_consumption: bool,
    pub last_sent_text: Option<String>,
    /// Consecutive inconclusive box reads (capped by `MAX_UNKNOWN_SKIPS`).
    pub unknown_skips: i64,
    /// Never type over a human (defaults on for old state files).
    pub hold_while_typing: bool,
    pub hold_count: i64,
    pub last_held_at: Option<f64>,
    /// Volatile, not persisted.
    pub status: String,
}

fn opt_str(data: &Map<String, Value>, key: &str) -> Option<String> {
    match data.get(key) {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Null) | None => None,
        Some(other) => Some(other.to_string()),
    }
}

fn opt_f64(data: &Map<String, Value>, key: &str) -> Option<f64> {
    data.get(key).and_then(Value::as_f64)
}

/// Python truthiness of a JSON value.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
    }
}

/// `int(kwargs.get(key) or 0)`.
fn int_or_zero(data: &Map<String, Value>, key: &str) -> i64 {
    match data.get(key) {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Some(Value::String(text)) => text.trim().parse().unwrap_or(0),
        Some(Value::Bool(true)) => 1,
        _ => 0,
    }
}

impl Dunker {
    /// `Dunker(id)` with every default.
    pub fn new(id: &str) -> Dunker {
        Dunker::with(id, &Map::new())
    }

    /// `Dunker.__init__(id, **kwargs)`: the Python's `or` defaults, key by key.
    pub fn with(id: &str, kwargs: &Map<String, Value>) -> Dunker {
        let interval = match kwargs.get("interval_minutes") {
            Some(v) if truthy(Some(v)) => v
                .as_f64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
                .unwrap_or(DEFAULT_INTERVAL_MINUTES),
            _ => DEFAULT_INTERVAL_MINUTES,
        };
        let skip_count = int_or_zero(kwargs, "skip_count");
        let total = int_or_zero(kwargs, "total_skips");
        Dunker {
            id: id.to_string(),
            name: opt_str(kwargs, "name").unwrap_or_default(),
            target_pane_id: opt_str(kwargs, "target_pane_id"),
            target_agent: opt_str(kwargs, "target_agent"),
            target_tab: opt_str(kwargs, "target_tab"),
            target_workspace: opt_str(kwargs, "target_workspace"),
            target_cwd: opt_str(kwargs, "target_cwd"),
            interval_minutes: interval,
            text: opt_str(kwargs, "text").unwrap_or_else(|| DEFAULT_TEXT.to_string()),
            running: truthy(kwargs.get("running")),
            only_when: opt_str(kwargs, "only_when").filter(|s| !s.is_empty()),
            max_sends: Some(int_or_zero(kwargs, "max_sends")).filter(|n| *n != 0),
            send_count: int_or_zero(kwargs, "send_count"),
            created_at: opt_f64(kwargs, "created_at")
                .filter(|f| *f != 0.0)
                .unwrap_or_else(clock::now_s),
            started_at: opt_f64(kwargs, "started_at"),
            last_sent_at: opt_f64(kwargs, "last_sent_at"),
            next_send_at: opt_f64(kwargs, "next_send_at"),
            last_error: opt_str(kwargs, "last_error"),
            skip_if_unconsumed: truthy(kwargs.get("skip_if_unconsumed")),
            skip_count,
            total_skips: if total != 0 { total } else { skip_count },
            last_skipped_at: opt_f64(kwargs, "last_skipped_at"),
            awaiting_consumption: truthy(kwargs.get("awaiting_consumption")),
            last_sent_text: opt_str(kwargs, "last_sent_text"),
            unknown_skips: int_or_zero(kwargs, "unknown_skips"),
            hold_while_typing: kwargs
                .get("hold_while_typing")
                .map_or(true, |v| truthy(Some(v))),
            hold_count: int_or_zero(kwargs, "hold_count"),
            last_held_at: opt_f64(kwargs, "last_held_at"),
            status: opt_str(kwargs, "status").unwrap_or_else(|| "Ready".to_string()),
        }
    }

    /// `target_label`: `(no target)`, the tab (or pane id), and `tab / agent` when known.
    pub fn target_label(&self) -> String {
        let Some(pane) = self.target_pane_id.as_deref().filter(|p| !p.is_empty()) else {
            return "(no target)".into();
        };
        let tab = self
            .target_tab
            .clone()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| pane.to_string());
        match self.target_agent.as_deref().filter(|a| !a.is_empty()) {
            Some(agent) => format!("{tab} / {agent}"),
            None => tab,
        }
    }

    /// `set_target`: point at a pane row (`pane_id`, `agent`, `cwd`, `tab_label`/`tab_id`,
    /// `workspace_label`/`workspace_id`).
    pub fn set_target(&mut self, pane: &Map<String, Value>) {
        let pick = |a: &str, b: &str| {
            opt_str(pane, a)
                .filter(|s| !s.is_empty())
                .or_else(|| opt_str(pane, b))
        };
        self.target_pane_id = opt_str(pane, "pane_id");
        self.target_agent = opt_str(pane, "agent");
        self.target_cwd = opt_str(pane, "cwd");
        self.target_tab = pick("tab_label", "tab_id");
        self.target_workspace = pick("workspace_label", "workspace_id");
    }

    fn field(&self, name: &str) -> Value {
        let text = |v: &Option<String>| v.as_ref().map_or(Value::Null, |s| json!(s));
        let num = |v: &Option<f64>| v.map_or(Value::Null, |n| json!(n));
        match name {
            "id" => json!(self.id),
            "name" => json!(self.name),
            "target_pane_id" => text(&self.target_pane_id),
            "target_agent" => text(&self.target_agent),
            "target_tab" => text(&self.target_tab),
            "target_workspace" => text(&self.target_workspace),
            "target_cwd" => text(&self.target_cwd),
            "interval_minutes" => json!(self.interval_minutes),
            "text" => json!(self.text),
            "running" => json!(self.running),
            "only_when" => text(&self.only_when),
            "max_sends" => self.max_sends.map_or(Value::Null, |n| json!(n)),
            "send_count" => json!(self.send_count),
            "created_at" => json!(self.created_at),
            "started_at" => num(&self.started_at),
            "last_sent_at" => num(&self.last_sent_at),
            "next_send_at" => num(&self.next_send_at),
            "last_error" => text(&self.last_error),
            "skip_if_unconsumed" => json!(self.skip_if_unconsumed),
            "skip_count" => json!(self.skip_count),
            "total_skips" => json!(self.total_skips),
            "last_skipped_at" => num(&self.last_skipped_at),
            "awaiting_consumption" => json!(self.awaiting_consumption),
            "last_sent_text" => text(&self.last_sent_text),
            "unknown_skips" => json!(self.unknown_skips),
            "hold_while_typing" => json!(self.hold_while_typing),
            "hold_count" => json!(self.hold_count),
            "last_held_at" => num(&self.last_held_at),
            _ => Value::Null,
        }
    }

    /// The persisted pairs: exactly `FIELDS`, in the Python's order. `dunks.json` is written
    /// from this, because `serde_json::Map` sorts its keys (no `preserve_order` feature).
    pub fn persisted_pairs(&self) -> Vec<(&'static str, Value)> {
        FIELDS.iter().map(|f| (*f, self.field(f))).collect()
    }

    /// The persisted fields as a map (key order is NOT preserved; see `persisted_pairs`).
    pub fn persisted(&self) -> Map<String, Value> {
        self.persisted_pairs()
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect()
    }

    /// `to_dict`: the persisted fields plus `status`, `target_label`, `next_send_in_s`.
    pub fn to_dict(&self) -> Map<String, Value> {
        let mut data = self.persisted();
        data.insert("status".into(), json!(self.status));
        data.insert("target_label".into(), json!(self.target_label()));
        let in_s = match (self.running, self.next_send_at) {
            (true, Some(next)) if next != 0.0 => {
                json!(((next - clock::now_s()).round_ties_even() as i64).max(0))
            }
            _ => Value::Null,
        };
        data.insert("next_send_in_s".into(), in_s);
        data
    }

    /// `from_dict`: drops the three derived keys, then constructs.
    pub fn from_dict(data: &Map<String, Value>) -> Result<Dunker, DunkError> {
        let mut data = data.clone();
        let id = match data.remove("id") {
            Some(Value::String(id)) => id,
            _ => return Err(DunkError::Refused("a dunk needs an id".into())),
        };
        for key in ["status", "target_label", "next_send_in_s"] {
            data.remove(key);
        }
        Ok(Dunker::with(&id, &data))
    }
}

/// `expand_template`: the known `{placeholders}`; other braces are left alone.
pub fn expand_template(text: &str, dunker: &Dunker) -> String {
    let now = chrono::Local::now();
    template_re()
        .replace_all(text, |caps: &regex::Captures| match &caps[1] {
            "id" => dunker.id.clone(),
            "name" if dunker.name.is_empty() => dunker.id.clone(),
            "name" => dunker.name.clone(),
            "count" => (dunker.send_count + 1).to_string(),
            "target" => dunker.target_label(),
            "interval" => format_interval(dunker.interval_minutes),
            "time" => now.format("%H:%M:%S").to_string(),
            _ => now.format("%Y-%m-%d").to_string(),
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kw(value: Value) -> Map<String, Value> {
        value.as_object().cloned().expect("object")
    }

    #[test]
    fn numbers_and_suffixes() {
        assert_eq!(10.0, parse_interval("10").unwrap());
        assert_eq!(1.5, parse_interval("90s").unwrap());
        assert_eq!(90.0, parse_interval("1.5h").unwrap());
        assert_eq!(2.0, parse_interval("2m").unwrap());
        assert_eq!(0.5, parse_interval_number(0.5).unwrap());
        assert_eq!(10.0, parse_interval_number(10.0).unwrap());
    }

    #[test]
    fn format_round_trips() {
        for minutes in [10.0, 1.5, 0.5, 1.0 / 12.0, 120.0] {
            let back = parse_interval(&format_interval(minutes)).unwrap();
            assert!((back - minutes).abs() < 1e-9, "{minutes} -> {back}");
        }
        assert_eq!("30s", format_interval(0.5));
        assert_eq!("5s", format_interval(1.0 / 12.0));
        assert_eq!("10", format_interval(10.0));
        assert_eq!("1.5", format_interval(1.5));
    }

    #[test]
    fn rejects_bad_values() {
        for bad in ["0", "abc", "", "10x"] {
            assert!(parse_interval(bad).is_err(), "{bad:?}");
        }
        assert!(parse_interval_number(-1.0).is_err());
    }

    #[test]
    fn known_placeholders_expand_and_others_survive() {
        let dunker = Dunker::with(
            "d7",
            &kw(
                json!({"name": "nightly", "interval_minutes": 60, "target_pane_id": "w1:p2",
                       "target_tab": "Claude Proj", "target_agent": "claude", "send_count": 2}),
            ),
        );
        let text =
            r#"dunk {id} "{name}" #{count} -> {target} every {interval}m {"json": 1} {unknown}"#;
        assert_eq!(
            r#"dunk d7 "nightly" #3 -> Claude Proj / claude every 60m {"json": 1} {unknown}"#,
            expand_template(text, &dunker)
        );
    }

    #[test]
    fn name_falls_back_to_id() {
        assert_eq!("d1", expand_template("{name}", &Dunker::new("d1")));
    }

    #[test]
    fn defaults_match_the_python_constructor() {
        let d = Dunker::new("d1");
        assert_eq!(
            ("", DEFAULT_TEXT, 10.0),
            (d.name.as_str(), d.text.as_str(), d.interval_minutes)
        );
        assert!(d.hold_while_typing && !d.running && !d.skip_if_unconsumed);
        assert_eq!("Ready", d.status);
        assert_eq!("(no target)", d.target_label());
        // total_skips falls back to skip_count for an old state file
        assert_eq!(
            4,
            Dunker::with("d2", &kw(json!({"skip_count": 4}))).total_skips
        );
        // `or None`: an empty gate and a zero cap are None
        let d = Dunker::with("d3", &kw(json!({"only_when": "", "max_sends": 0})));
        assert!(d.only_when.is_none() && d.max_sends.is_none());
    }

    #[test]
    fn to_dict_round_trips_through_from_dict_and_keeps_field_order() {
        let mut d = Dunker::with(
            "d9",
            &kw(json!({"target_pane_id": "w1:p1", "max_sends": 3})),
        );
        d.set_target(&kw(
            json!({"pane_id": "w2:p1", "agent": "claude", "cwd": "/x",
                                "tab_label": "", "tab_id": "t7", "workspace_id": "w2"}),
        ));
        assert_eq!(Some("t7"), d.target_tab.as_deref());
        assert_eq!(Some("w2"), d.target_workspace.as_deref());
        assert_eq!("t7 / claude", d.target_label());
        let keys: Vec<&str> = d.persisted_pairs().iter().map(|(k, _)| *k).collect();
        assert_eq!(FIELDS.to_vec(), keys);
        let again = Dunker::from_dict(&d.to_dict()).unwrap();
        assert_eq!(d.persisted(), again.persisted());
        assert!(d.to_dict()["next_send_in_s"].is_null(), "not running");
    }

    #[test]
    fn a_bad_interval_is_quoted_like_python_repr() {
        let error = parse_interval("abc").unwrap_err().to_string();
        assert!(
            error.contains("bad interval 'abc' (try 10, 90s, 1.5h)"),
            "{error}"
        );
    }

    #[test]
    fn fmt_g_matches_python_g() {
        assert_eq!("5e-05", fmt_g(5e-05));
        assert_eq!("0.0001", fmt_g(1e-4));
        assert_eq!("1e+06", fmt_g(999999.5));
        assert_eq!("999999", fmt_g(999999.0));
        assert_eq!("1.5", fmt_g(1.5));
        assert_eq!("0.833333", fmt_g(5.0 / 6.0));
        assert_eq!("1.23457e+08", fmt_g(123456789.0));
        // through the public path: 3e-3 s is 0.00005 minutes
        assert_eq!("0.003s", format_interval(0.00005));
    }

    #[test]
    fn next_send_in_s_rounds_half_to_even_like_python() {
        let mut d = Dunker::new("d1");
        d.running = true;
        let now = clock::now_s();
        // 0.5 s and 2.5 s ahead are ties: Python round() gives 0 and 2
        d.next_send_at = Some(now + 2.5);
        let two = d.to_dict()["next_send_in_s"].as_i64().unwrap();
        assert!(two == 2 || two == 3, "clock moved between reads: {two}");
        assert_eq!(0.0_f64.round_ties_even(), 0.5_f64.round_ties_even());
        assert_eq!(2.0, 2.5_f64.round_ties_even());
        assert_eq!(4.0, 3.5_f64.round_ties_even());
    }
}
