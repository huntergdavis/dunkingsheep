//! Watchdog config schema and validation: ported from `dunk_watchdog.py` (t1400 slice A).
//!
//! Every key is required and unknown keys are refused, so nothing runs on a default nobody chose.
//! Thresholds, windows, holds, caps and cooldowns must be strictly positive: a zero threshold fires
//! on every run and a zero rate limit is no limit, so both are config bugs the validator refuses.
//!
//! The config is a `serde_json::Value` (ints stay `i64`, floats `f64`), the same shape the rules read.
//! `_validate` order is part of the contract: unknown keys first (sorted), then schema order, so the
//! first error named is stable.

use std::path::Path;

use regex::Regex;
use serde_json::{json, Map, Value};

pub const SCHEMA_ID: &str = "dunkingsheep-watchdog/2";
pub const SEVERITIES: [&str; 3] = ["info", "warning", "critical"];
pub const CLASSES: [&str; 5] = ["security", "outage", "money", "ops", "project"];
pub const OWNER_CLASSES: [&str; 3] = ["security", "outage", "money"];
/// Spec section 1: the only rules that may carry an owner class. Adding one is a VP-approved edit to
/// the spec, then to this list.
pub const OWNER_ELIGIBLE_RULES: [&str; 3] = [
    "host.disk_full",
    "security.secret_text",
    "money.spend_projection",
];

/// A named config problem: the dotted key and the reason. Refuse to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub key: String,
    pub reason: String,
}

impl ConfigError {
    fn new(key: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.key, self.reason)
    }
}

impl std::error::Error for ConfigError {}

// -- Python-shaped text -------------------------------------------------------

/// A JSON number the way Python prints it: ints bare, floats with a `.0` when whole.
fn py_num(value: &Value) -> String {
    match value {
        Value::Number(n) if n.is_i64() || n.is_u64() => n.to_string(),
        Value::Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 1e16 => format!("{f:.1}"),
            Some(f) => format!("{f}"),
            None => n.to_string(),
        },
        other => other.to_string(),
    }
}

fn py_str_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `repr()` of a JSON value as Python would print the loaded object.
fn py_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(_) => py_num(value),
        Value::String(text) => py_str_repr(text),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(py_repr).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(map) => format!(
            "{{{}}}",
            map.iter()
                .map(|(k, v)| format!("{}: {}", py_str_repr(k), py_repr(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn choices_repr(choices: &[&str]) -> String {
    format!(
        "[{}]",
        choices
            .iter()
            .map(|c| py_str_repr(c))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

// -- validators ---------------------------------------------------------------

/// A validator returns the problem text, or `None` when the value is fine.
type Check = Box<dyn Fn(&Value) -> Option<String> + Send + Sync>;

fn is_int(value: &Value) -> bool {
    matches!(value, Value::Number(n) if n.is_i64() || n.is_u64())
}

fn num(minimum: Option<i64>, maximum: Option<i64>, integer: bool, positive: bool) -> Check {
    Box::new(move |value| {
        // A JSON bool is not a number here (Python: `isinstance(value, bool)` is refused first).
        let Value::Number(n) = value else {
            return Some("must be a number".into());
        };
        if integer && !is_int(value) {
            return Some("must be an integer".into());
        }
        let v = n.as_f64().unwrap_or(f64::NAN);
        if positive && v <= 0.0 {
            return Some(format!("must be > 0, got {}", py_num(value)));
        }
        if let Some(min) = minimum {
            if v < min as f64 {
                return Some(format!("must be >= {min}, got {}", py_num(value)));
            }
        }
        if let Some(max) = maximum {
            if v > max as f64 {
                return Some(format!("must be <= {max}, got {}", py_num(value)));
            }
        }
        None
    })
}

fn positive() -> Check {
    num(None, None, false, true)
}
fn positive_int() -> Check {
    num(None, None, true, true)
}
fn fraction() -> Check {
    num(None, Some(1), false, true)
}

fn boolean() -> Check {
    Box::new(|value| {
        if value.is_boolean() {
            None
        } else {
            Some("must be true or false".into())
        }
    })
}

fn non_empty(value: &Value) -> bool {
    matches!(value, Value::String(text) if !text.trim().is_empty())
}

fn string() -> Check {
    Box::new(|value| {
        if non_empty(value) {
            None
        } else {
            Some("must be a non-empty string".into())
        }
    })
}

fn opt_string() -> Check {
    Box::new(|value| {
        if value.is_null() || non_empty(value) {
            None
        } else {
            Some("must be null or a non-empty string".into())
        }
    })
}

fn enumeration(choices: &'static [&'static str]) -> Check {
    Box::new(move |value| {
        if value.as_str().is_some_and(|text| choices.contains(&text)) {
            None
        } else {
            Some(format!(
                "must be one of {}, got {}",
                choices_repr(choices),
                py_repr(value)
            ))
        }
    })
}

fn enum_list(choices: &'static [&'static str], allow_empty: bool) -> Check {
    Box::new(move |value| {
        let Some(items) = value.as_array() else {
            return Some(format!(
                "must be a list drawn from {}",
                choices_repr(choices)
            ));
        };
        if !items
            .iter()
            .all(|v| v.as_str().is_some_and(|t| choices.contains(&t)))
        {
            return Some(format!(
                "must be a list drawn from {}",
                choices_repr(choices)
            ));
        }
        let mut seen: Vec<&Value> = Vec::new();
        for item in items {
            if seen.contains(&item) {
                return Some("must not repeat entries".into());
            }
            seen.push(item);
        }
        if !allow_empty && items.is_empty() {
            return Some("must not be empty".into());
        }
        None
    })
}

fn str_list() -> Check {
    Box::new(|value| {
        let ok = value.as_array().is_some_and(|items| {
            !items.is_empty()
                && items
                    .iter()
                    .all(|v| matches!(v, Value::String(t) if !t.is_empty()))
        });
        if ok {
            None
        } else {
            Some("must be a non-empty list of non-empty strings".into())
        }
    })
}

fn hex_list() -> Check {
    Box::new(|value| {
        // `[0-9a-f]{64}\Z`: exactly 64 lowercase hex digits and nothing after.
        let ok = value.as_array().is_some_and(|items| {
            items.iter().all(|v| {
                matches!(v, Value::String(t) if t.len() == 64
                    && t.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
            })
        });
        if ok {
            None
        } else {
            Some("must be a list of lowercase sha256 hex digests (may be empty)".into())
        }
    })
}

/// The problem with one regular expression, or `None`.
///
/// NOT identical to Python's `re`: the Rust `regex` crate has no lookaround or backreferences, so a
/// pattern Python accepts may be refused here, and the parser's error text differs. The key named in
/// the error is the same.
fn regex_problem(value: &Value) -> Option<String> {
    let Some(text) = value.as_str().filter(|t| !t.is_empty()) else {
        return Some("must be a non-empty regular expression".into());
    };
    match Regex::new(text) {
        Ok(_) => None,
        Err(error) => Some(format!("is not a valid regular expression ({error})")),
    }
}

fn regex_check() -> Check {
    Box::new(regex_problem)
}

fn regex_list() -> Check {
    Box::new(|value| {
        let Some(items) = value.as_array().filter(|items| !items.is_empty()) else {
            return Some("must be a non-empty list of regular expressions".into());
        };
        for item in items {
            if let Some(problem) = regex_problem(item) {
                return Some(format!("entry {} {problem}", py_repr(item)));
            }
        }
        None
    })
}

fn opt_positive() -> Check {
    let inner = positive();
    Box::new(
        move |value| {
            if value.is_null() {
                None
            } else {
                inner(value)
            }
        },
    )
}

// -- the schema ---------------------------------------------------------------

/// One schema node: a check on a value, or a nested object (order = schema order).
enum Spec {
    Check(Check),
    Group(Vec<(String, Spec)>),
}

fn leaf(name: &str, check: Check) -> (String, Spec) {
    (name.to_string(), Spec::Check(check))
}

fn group(name: &str, children: Vec<(String, Spec)>) -> (String, Spec) {
    (name.to_string(), Spec::Group(children))
}

const IDLE_STATUSES: &[&str] = &["idle", "done", "blocked", "unknown"];
const HOLDING_STATUSES: &[&str] = &["open", "in_progress", "blocked"];
const SEVERITY_CHOICES: &[&str] = &SEVERITIES;
const CLASS_CHOICES: &[&str] = &CLASSES;
const OWNER_CLASS_CHOICES: &[&str] = &OWNER_CLASSES;

/// `RULE_COMMON` keys, then the rule's own, in the order the Python dict builds them.
fn rule_common() -> Vec<(String, Spec)> {
    vec![
        leaf("enabled", boolean()),
        leaf("severity", enumeration(SEVERITY_CHOICES)),
        leaf("class", enumeration(CLASS_CHOICES)),
        leaf("cooldown_minutes", positive()),
        leaf("must_hold_runs", positive_int()),
    ]
}

/// `RULE_PARAMS`, in the Python's order (it is the order `rules` is validated in).
fn rule_params() -> Vec<(&'static str, Vec<(String, Spec)>)> {
    vec![
        (
            "fleet.assignment_starvation",
            vec![
                leaf("quiet_minutes", positive()),
                leaf("min_empty_workers", positive_int()),
                leaf("worker_panes", str_list()),
                leaf("superuser_panes", str_list()),
            ],
        ),
        (
            "task.blocked_stale",
            vec![leaf("threshold_minutes", positive())],
        ),
        (
            "task.lease_stale",
            vec![
                leaf("never_renewed_minutes", positive()),
                leaf("lookback_minutes", positive()),
                leaf("fresh_minutes", positive()),
            ],
        ),
        (
            "dunk.skip_loop",
            vec![
                leaf("consecutive_skips", positive_int()),
                leaf("lookback_hours", positive()),
                // A streak that stopped N minutes ago is history, not a condition.
                leaf("fresh_minutes", positive_int()),
            ],
        ),
        ("dunk.overdue", vec![leaf("missed_intervals", positive())]),
        // t409: how old and how often refused before this is worth saying. Both are read from the
        // t269 budget that already exists (HOLD_ATTEMPT_LIMIT attempts over the backoff), not new
        // policy.
        (
            "delivery.ambiguous_queued",
            vec![
                leaf("min_holds", positive_int()),
                leaf("min_age_minutes", positive()),
            ],
        ),
        // Above the 24h auto-archive: a page must mean "the archive did not catch this", not "the
        // archive is about to catch this".
        (
            "inbox.undeliverable",
            vec![
                leaf("min_messages", positive_int()),
                leaf("min_age_hours", positive()),
            ],
        ),
        // A WORKING lane's unread mail is a queue; an IDLE lane's is a miss.
        (
            "inbox.unread_live",
            vec![
                leaf("age_minutes", positive()),
                leaf("min_messages", positive_int()),
                leaf("idle_statuses", enum_list(IDLE_STATUSES, false)),
            ],
        ),
        // min_age_minutes: mail must be OLD before "this lane is gone" is worth saying (measured
        // 2026-10-24: the rule fired on a message 0.0h old while the pane was being superseded).
        (
            "inbox.unread_dead",
            vec![
                leaf("min_messages", positive_int()),
                leaf("min_age_minutes", positive()),
            ],
        ),
        (
            "inbox.ambiguous_sender",
            vec![
                leaf("lookback_hours", positive()),
                leaf("sample_ids", num(Some(1), Some(50), true, false)),
                leaf("require_live_pane", boolean()),
            ],
        ),
        (
            "lane.working_without_task",
            vec![leaf("quiet_minutes", positive())],
        ),
        (
            "lane.idle_holding_task",
            vec![
                leaf("quiet_minutes", positive()),
                leaf("idle_statuses", enum_list(IDLE_STATUSES, false)),
                leaf("holding_statuses", enum_list(HOLDING_STATUSES, false)),
            ],
        ),
        // load1_* names are kept for config compatibility; the rule judges load5/cpu.
        (
            "host.load",
            vec![
                leaf("load1_per_cpu", positive()),
                leaf("critical_load1_per_cpu", positive()),
                leaf("critical_hold_runs", positive_int()),
            ],
        ),
        (
            "host.tmp",
            vec![
                leaf("max_used_fraction", fraction()),
                leaf("critical_used_fraction", fraction()),
                leaf("min_free_bytes", positive_int()),
                leaf("max_inode_fraction", fraction()),
                leaf("path", string()),
            ],
        ),
        (
            "host.heavy_slot_stale",
            vec![
                leaf("directory", string()),
                leaf("prefix", string()),
                leaf("max_age_minutes", positive()),
            ],
        ),
        (
            "security.secret_text",
            vec![
                leaf("patterns", regex_list()),
                leaf("allowlist_sha256", hex_list()),
                leaf("lookback_hours", positive()),
            ],
        ),
        (
            "money.spend_projection",
            vec![leaf("source", opt_string()), leaf("cap", opt_positive())],
        ),
        (
            "host.disk_full",
            vec![
                leaf("max_used_fraction", fraction()),
                leaf("paths", str_list()),
            ],
        ),
        ("health.failed", vec![leaf("stale_minutes", positive())]),
        // A would-kill is a THRESHOLD-wrong signal, not an incident.
        (
            "resource.would_kill",
            vec![
                leaf("policies_path", string()),
                leaf("state_path", string()),
                leaf("require_task", boolean()),
            ],
        ),
        // A retired pane is a STATE we report, never a condition we page about.
        ("pane.retired", vec![]),
    ]
}

fn top_spec() -> Vec<(String, Spec)> {
    let rules = rule_params()
        .into_iter()
        .map(|(name, params)| {
            let mut children = rule_common();
            children.extend(params);
            group(name, children)
        })
        .collect();
    vec![
        leaf("schema", enumeration(&[SCHEMA_ID])),
        leaf("enabled", boolean()),
        leaf("dry_run", boolean()),
        leaf("cadence_minutes", positive()),
        leaf("min_run_spacing_seconds", positive()),
        leaf("max_alerts_per_run", positive_int()),
        leaf("state_path", string()),
        leaf("heartbeat_path", string()),
        leaf("state_retention_days", positive()),
        group("stores", vec![leaf("config_dir", string())]),
        group(
            "panes",
            vec![
                leaf(
                    "source",
                    // `_enum("herdr", "none")`
                    enumeration(&["herdr", "none"]),
                ),
                leaf("pane_id_pattern", regex_check()),
            ],
        ),
        group(
            "escalation",
            vec![
                group(
                    "vp",
                    vec![
                        leaf("mailbox", string()),
                        leaf("min_severity", enumeration(SEVERITY_CHOICES)),
                    ],
                ),
                group(
                    "owner",
                    vec![
                        leaf("enabled", boolean()),
                        leaf("channel", enumeration(&["desk-brief"])),
                        leaf("classes", enum_list(OWNER_CLASS_CHOICES, true)),
                        leaf("min_severity", enumeration(SEVERITY_CHOICES)),
                        leaf("approved_by", opt_string()),
                        leaf("max_pages_per_hour", positive_int()),
                        leaf("confirm_within_minutes", positive()),
                        leaf("confirm_path", string()),
                    ],
                ),
            ],
        ),
        // t377 (owner2281/2282): the ONE action this watchdog may take on a pane, narrow by
        // construction - the daemon performs a single guarded Enter and verifies consumption; the
        // watchdog only observes, accounts and reports. The numbers below are the whole policy.
        group(
            "stuck_recovery",
            vec![
                leaf("enabled", boolean()),
                leaf("threshold_s", positive()),
                leaf("max_attempts", positive_int()),
                leaf("cooldown_s", positive()),
                // must be <= threshold; checked in validate_config
                leaf("observation_interval_s", positive()),
            ],
        ),
        group("rules", rules),
    ]
}

/// The t269 budget, mirrored so the watchdog can default the new rule without importing the daemon's
/// (the two are separate deployments and a test asserts they agree).
pub const HOLD_ATTEMPT_LIMIT_DEFAULT: i64 = 10;

/// Rules added AFTER a config was written. Only these are filled in when absent; every older rule
/// stays REQUIRED, so a config that accidentally loses one still fails loudly.
pub const RULE_ADDITIONS: [&str; 2] = ["delivery.ambiguous_queued", "fleet.assignment_starvation"];

/// Shipped defaults for the rules that can be added to an existing config. Only the ones whose
/// presence is a judgement need an entry; everything else defaults to 1.
fn rule_example_default(rule_id: &str, key: &str) -> Value {
    match (rule_id, key) {
        ("fleet.assignment_starvation", "quiet_minutes") => json!(10),
        ("fleet.assignment_starvation", "min_empty_workers") => json!(2),
        ("fleet.assignment_starvation", "worker_panes") => json!(["configure-worker-pane"]),
        ("fleet.assignment_starvation", "superuser_panes") => json!(["configure-superuser-pane"]),
        ("delivery.ambiguous_queued", "min_holds") => json!(HOLD_ATTEMPT_LIMIT_DEFAULT),
        ("delivery.ambiguous_queued", "min_age_minutes") => json!(20),
        _ => json!(1),
    }
}

/// Fill in rules a config predates, DISABLED, so an old config keeps behaving exactly as it did
/// (preserve deployed policy) and enabling the new rule is an explicit edit. Returns the added ids.
pub fn migrate_config(config: &mut Value) -> Vec<String> {
    let Some(rules) = config.get_mut("rules").and_then(Value::as_object_mut) else {
        return Vec::new();
    };
    let mut added = Vec::new();
    for rule_id in RULE_ADDITIONS {
        if rules.contains_key(rule_id) {
            continue;
        }
        let params = rule_params()
            .into_iter()
            .find(|(name, _)| *name == rule_id)
            .map(|(_, params)| params)
            .unwrap_or_default();
        let mut block = Map::new();
        // RULE_DEFAULTS
        block.insert("severity".into(), json!("warning"));
        block.insert("class".into(), json!("ops"));
        block.insert("cooldown_minutes".into(), json!(240));
        block.insert("must_hold_runs".into(), json!(1));
        block.insert("enabled".into(), json!(false));
        for (key, _) in params {
            block.insert(key.clone(), rule_example_default(rule_id, &key));
        }
        rules.insert(rule_id.to_string(), Value::Object(block));
        added.push(rule_id.to_string());
    }
    added
}

fn validate_node(spec: &[(String, Spec)], value: &Value, prefix: &str) -> Result<(), ConfigError> {
    let Some(map) = value.as_object() else {
        let key = prefix.trim_end_matches('.');
        return Err(ConfigError::new(
            if key.is_empty() { "<root>" } else { key },
            "must be an object",
        ));
    };
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();
    for key in keys {
        if !spec.iter().any(|(name, _)| name == key) {
            return Err(ConfigError::new(format!("{prefix}{key}"), "unknown key"));
        }
    }
    // schema order, so the first error named is stable
    for (key, sub) in spec {
        let Some(item) = map.get(key) else {
            return Err(ConfigError::new(
                format!("{prefix}{key}"),
                "missing (no defaults: every key must be set)",
            ));
        };
        match sub {
            Spec::Group(children) => validate_node(children, item, &format!("{prefix}{key}."))?,
            Spec::Check(check) => {
                if let Some(problem) = check(item) {
                    return Err(ConfigError::new(format!("{prefix}{key}"), problem));
                }
            }
        }
    }
    Ok(())
}

fn num_at(map: &Map<String, Value>, key: &str) -> f64 {
    map.get(key).and_then(Value::as_f64).unwrap_or(f64::NAN)
}

fn flag(map: &Map<String, Value>, key: &str) -> bool {
    map.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Migrate (rules added after the config was written are filled in disabled), then return `Ok` or
/// the `ConfigError` naming the FIRST bad key. Like the Python, the migration edits `config` in place.
pub fn validate_config(config: &mut Value) -> Result<(), ConfigError> {
    migrate_config(config);
    validate_node(&top_spec(), config, "")?;
    // Past the schema every key above exists with the right type, so these lookups cannot miss.
    let root = config.as_object().expect("validated as an object");
    let section = |name: &str| root[name].as_object().expect("validated as an object");

    let stuck = section("stuck_recovery");
    if flag(stuck, "enabled")
        && num_at(stuck, "observation_interval_s") > num_at(stuck, "threshold_s")
    {
        return Err(ConfigError::new(
            "stuck_recovery.observation_interval_s",
            "must be <= threshold_s, or the 120 s bound cannot be honoured",
        ));
    }
    if flag(stuck, "enabled") && num_at(stuck, "max_attempts") < 1.0 {
        return Err(ConfigError::new(
            "stuck_recovery.max_attempts",
            "must be at least 1",
        ));
    }
    let owner = section("escalation")["owner"]
        .as_object()
        .expect("validated as an object");
    if flag(owner, "enabled") {
        if owner["approved_by"].is_null() {
            return Err(ConfigError::new(
                "escalation.owner.approved_by",
                "required when owner paging is enabled",
            ));
        }
        if owner["classes"].as_array().is_some_and(Vec::is_empty) {
            return Err(ConfigError::new(
                "escalation.owner.classes",
                "must name at least one class when owner paging is enabled",
            ));
        }
    }
    let rules = section("rules");
    let tmp = rules["host.tmp"]
        .as_object()
        .expect("validated as an object");
    if num_at(tmp, "critical_used_fraction") < num_at(tmp, "max_used_fraction") {
        return Err(ConfigError::new(
            "rules.host.tmp.critical_used_fraction",
            "must be >= max_used_fraction",
        ));
    }
    let money = rules["money.spend_projection"]
        .as_object()
        .expect("validated as an object");
    if flag(money, "enabled") && (money["source"].is_null() || money["cap"].is_null()) {
        return Err(ConfigError::new(
            "rules.money.spend_projection.enabled",
            "cannot be enabled without both source and cap (no metering source exists; VP i19821)",
        ));
    }
    for (rule_id, rule) in rules {
        let class = rule["class"].as_str().unwrap_or("");
        if OWNER_CLASSES.contains(&class) && !OWNER_ELIGIBLE_RULES.contains(&rule_id.as_str()) {
            return Err(ConfigError::new(
                format!("rules.{rule_id}.class"),
                format!(
                    "{} is owner-eligible, but {rule_id} is not an approved owner rule (spec \
                     section 1); use ops or project",
                    py_str_repr(class)
                ),
            ));
        }
    }
    Ok(())
}

/// Read, parse and validate a config file. A bad file refuses to start and the error is the
/// watchdog's own (the deadman reads it).
///
/// NOT byte-identical to Python for a JSON syntax error: the parser's message text differs.
pub fn load_config(path: &str) -> Result<Value, ConfigError> {
    if path.is_empty() || !Path::new(path).exists() {
        return Err(ConfigError::new(
            "<file>",
            format!("config file not found: {path}"),
        ));
    }
    let text = std::fs::read_to_string(path)
        .map_err(|error| ConfigError::new("<file>", format!("unreadable or not JSON: {error}")))?;
    let mut config: Value = serde_json::from_str(&text)
        .map_err(|error| ConfigError::new("<file>", format!("unreadable or not JSON: {error}")))?;
    validate_config(&mut config)?;
    Ok(config)
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;
