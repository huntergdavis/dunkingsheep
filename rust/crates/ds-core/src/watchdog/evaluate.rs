//! `evaluate` and `self_error` from `dunk_watchdog.py`.
//!
//! This is the rule-table boundary: inputs are loaded read-only and lazily, individual failures
//! stay attached to the rule that encountered them, unavailable sources are reported as skipped,
//! and findings are ordered by the shared `sort_findings` implementation.

use std::cell::{OnceCell, RefCell};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::rules_delivery;
use super::rules_dunks;
use super::rules_host::{self, HostSample, Panes};
use super::rules_inbox::{self, InboxDb};
use super::rules_lanes::{self, Starvation};
use super::rules_resource;
use super::rules_security;
use super::{rules, rules_dunks::MetricsDb as DunkMetricsDb, sort_findings, TaskData};
use crate::error::DunkError;

/// Optional read-only pane inventory provider, equivalent to Python's `panes_provider` seam.
pub type PanesProvider<'a> = dyn Fn() -> Result<Option<Panes>, DunkError> + 'a;
/// Optional host sampler, equivalent to Python's `host_sampler` seam.
pub type HostSampler<'a> = dyn Fn(&Path) -> Result<HostSample, DunkError> + 'a;

const RULE_IDS: &[&str] = &[
    "delivery.ambiguous_queued",
    "dunk.overdue",
    "dunk.skip_loop",
    "fleet.assignment_starvation",
    "health.failed",
    "host.disk_full",
    "host.heavy_slot_stale",
    "host.load",
    "host.tmp",
    "inbox.ambiguous_sender",
    "inbox.undeliverable",
    "inbox.unread_dead",
    "inbox.unread_live",
    "lane.idle_holding_task",
    "lane.working_without_task",
    "money.spend_projection",
    "pane.retired",
    "resource.would_kill",
    "security.secret_text",
    "task.blocked_stale",
    "task.lease_stale",
];

struct Inputs<'a> {
    dir: PathBuf,
    now: f64,
    pane_source: &'a str,
    pane_id_pattern: &'a str,
    panes_provider: Option<&'a PanesProvider<'a>>,
    host_sampler: Option<&'a HostSampler<'a>>,
    tasks: OnceCell<TaskData>,
    dunks: OnceCell<Vec<Value>>,
    panes: OnceCell<Option<Panes>>,
    names: OnceCell<BTreeMap<String, String>>,
    starvation: OnceCell<Starvation>,
    host_samples: RefCell<BTreeMap<String, HostSample>>,
}

impl Inputs<'_> {
    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn cached<T>(
        cell: &OnceCell<T>,
        load: impl FnOnce() -> Result<T, DunkError>,
    ) -> Result<&T, DunkError> {
        if let Some(value) = cell.get() {
            return Ok(value);
        }
        let value = load()?;
        let _ = cell.set(value);
        Ok(cell
            .get()
            .expect("successful loader result was stored in the cache"))
    }

    fn tasks(&self) -> Result<&TaskData, DunkError> {
        Self::cached(&self.tasks, || {
            TaskData::load(&self.path("tasks.db"), self.now)
        })
    }

    fn dunks(&self) -> Result<&[Value], DunkError> {
        Self::cached(&self.dunks, || {
            rules_dunks::load_dunks(&self.path("dunks.json"))
        })
        .map(Vec::as_slice)
    }

    fn panes(&self) -> Result<Option<&Panes>, DunkError> {
        if self.pane_source == "none" {
            return Ok(None);
        }
        match Self::cached(&self.panes, || match self.panes_provider {
            Some(provider) => provider(),
            None => Err(DunkError::Unavailable(
                "pane inventory provider is not configured".into(),
            )),
        })? {
            Some(panes) => Ok(Some(panes)),
            None => Ok(None),
        }
    }

    fn names(&self) -> Result<&BTreeMap<String, String>, DunkError> {
        Self::cached(&self.names, || {
            InboxDb {
                path: &self.path("inbox.db"),
            }
            .name_to_pane(self.now)
        })
    }

    fn starvation(&self, rule: &Value) -> Result<&Starvation, DunkError> {
        let since = self.now - rule["quiet_minutes"].as_f64().unwrap_or(0.0) * 60.0;
        Self::cached(&self.starvation, || {
            Starvation::load(&self.path("tasks.db"), self.now, since)
        })
    }

    fn host(&self, path: &Path) -> Result<HostSample, DunkError> {
        let key = path.to_string_lossy().into_owned();
        if let Some(sample) = self.host_samples.borrow().get(&key) {
            return Ok(sample.clone());
        }
        let result = match self.host_sampler {
            Some(sampler) => sampler(path),
            None => rules_host::sample_host(path),
        }?;
        self.host_samples.borrow_mut().insert(key, result.clone());
        Ok(result)
    }
}

fn config_dir(config: &Value) -> PathBuf {
    let raw = config["stores"]["config_dir"].as_str().unwrap_or("");
    PathBuf::from(crate::native_session::expanduser(raw))
}

fn python_json_decode_error(raw: &str, error: &serde_json::Error) -> String {
    let serde_error = error.to_string();
    let reason = if serde_error.starts_with("key must be a string") {
        "Expecting property name enclosed in double quotes"
    } else if serde_error.starts_with("EOF while parsing a value")
        || serde_error.starts_with("expected value")
    {
        "Expecting value"
    } else if serde_error.starts_with("expected `:`") {
        "Expecting ':' delimiter"
    } else if serde_error.starts_with("expected `,`") {
        "Expecting ',' delimiter"
    } else {
        "Invalid JSON"
    };
    let line = error.line().max(1);
    let column = error.column().max(1);
    let char_offset = raw
        .split('\n')
        .take(line.saturating_sub(1))
        .map(|part| part.chars().count() + 1)
        .sum::<usize>()
        + column.saturating_sub(1);
    format!("{reason}: line {line} column {column} (char {char_offset})")
}

fn task_event_python_error(message: &str, inputs: &Inputs<'_>) -> Option<String> {
    if !(message.contains("malformed meta JSON") || message.contains("meta is not an object")) {
        return None;
    }
    let event_id = message.strip_prefix("task_events ")?.split(' ').next()?;
    let event_id = event_id.parse::<i64>().ok()?;
    let data = inputs.tasks().ok()?;
    let event = data
        .events
        .iter()
        .find(|event| event["id"].as_i64() == Some(event_id))?;
    let raw = event["meta"]
        .as_str()
        .filter(|text| !text.is_empty())
        .unwrap_or("{}");
    match serde_json::from_str::<Value>(raw) {
        Err(error) => Some(format!(
            "JSONDecodeError: {}",
            python_json_decode_error(raw, &error)
        )),
        Ok(value) if !value.is_object() => {
            let python_type = match value {
                Value::Null => "NoneType",
                Value::Bool(_) => "bool",
                Value::Number(ref number) if number.is_i64() || number.is_u64() => "int",
                Value::Number(_) => "float",
                Value::String(_) => "str",
                Value::Array(_) => "list",
                Value::Object(_) => "dict",
            };
            Some(format!(
                "AttributeError: '{python_type}' object has no attribute 'get'"
            ))
        }
        _ => None,
    }
}

fn error_text(rule_id: &str, error: &DunkError, inputs: &Inputs<'_>) -> String {
    if let Some(python_error) = task_event_python_error(&error.to_string(), inputs) {
        return python_error;
    }
    match error {
        DunkError::Refused(message)
            if message.starts_with("JSONDecodeError: ")
                || message.starts_with("FileNotFoundError: ")
                || message.starts_with("PermissionError: ")
                || message.starts_with("IsADirectoryError: ")
                || message.starts_with("NotADirectoryError: ") =>
        {
            message.clone()
        }
        DunkError::Refused(message) => {
            if message.starts_with("invalid JSON:") {
                if let Ok(raw) = fs::read_to_string(inputs.path("dunks.json")) {
                    if let Err(error) = serde_json::from_str::<Value>(&raw) {
                        return format!(
                            "JSONDecodeError: {}",
                            python_json_decode_error(&raw, &error)
                        );
                    }
                }
            }
            if let Some(task_id) = malformed_assignees_task(message) {
                if let Some(raw) = task_assignees(&inputs.path("tasks.db"), task_id) {
                    if let Err(error) = serde_json::from_str::<Value>(&raw) {
                        return format!(
                            "JSONDecodeError: {}",
                            python_json_decode_error(&raw, &error)
                        );
                    }
                }
            }
            if matches!(
                rule_id,
                "dunk.skip_loop"
                    | "dunk.overdue"
                    | "delivery.ambiguous_queued"
                    | "security.secret_text"
            ) {
                if let Some(errno) = os_error_number(message) {
                    return python_os_error(&inputs.path("dunks.json"), errno);
                }
            }
            if message.contains("unable to open database") {
                if let Some(path) = missing_sqlite_path(rule_id, inputs) {
                    // Python's _ro checks os.path.exists before opening SQLite, so a missing
                    // store is FileNotFoundError(path), not sqlite3.OperationalError.
                    return format!("FileNotFoundError: {}", path.display());
                }
            }
            // SQLite's Python reader surfaces sqlite3.OperationalError. The Rust SQLite adapter
            // preserves its diagnostic text in Refused; recover the source exception here.
            // These diagnostics come from rusqlite calls in watchdog rules; Python's
            // sqlite3 exposes them as sqlite3.OperationalError.
            if message.contains("no such table")
                || message.contains("no such column")
                || message.contains("unable to open database")
                || message.contains("database disk image is malformed")
                || message.contains("file is not a database")
            {
                format!("OperationalError: {message}")
            } else {
                format!("DunkError: {message}")
            }
        }
        DunkError::Busy => "OperationalError: database is locked".into(),
        other => error_text_for_transport(other),
    }
}

fn missing_sqlite_path(rule_id: &str, inputs: &Inputs<'_>) -> Option<PathBuf> {
    let filenames: &[&str] = match rule_id {
        "task.blocked_stale" | "task.lease_stale" | "resource.would_kill" => &["tasks.db"],
        "dunk.skip_loop" => &["metrics.db"],
        "inbox.undeliverable"
        | "inbox.unread_live"
        | "inbox.unread_dead"
        | "inbox.ambiguous_sender" => &["inbox.db"],
        "lane.working_without_task" | "lane.idle_holding_task" => &["tasks.db", "inbox.db"],
        "fleet.assignment_starvation" => &["tasks.db", "metrics.db"],
        "security.secret_text" => &["inbox.db", "tasks.db"],
        _ => &[],
    };
    filenames
        .iter()
        .map(|filename| inputs.path(filename))
        .find(|path| !path.exists())
}

fn malformed_assignees_task(message: &str) -> Option<&str> {
    let rest = message.strip_prefix("task ")?;
    rest.split_once(" has malformed assignees JSON")
        .map(|(id, _)| id)
}

fn task_assignees(path: &Path, task_id: &str) -> Option<String> {
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    connection
        .query_row(
            "SELECT assignees FROM tasks WHERE id=?1",
            [task_id],
            |row| row.get::<_, String>(0),
        )
        .ok()
}

fn os_error_number(message: &str) -> Option<i32> {
    let (_, suffix) = message.rsplit_once("(os error ")?;
    suffix.strip_suffix(')')?.parse().ok()
}

fn python_os_error(path: &Path, errno: i32) -> String {
    let python_type = match errno {
        2 => "FileNotFoundError",
        13 => "PermissionError",
        20 => "NotADirectoryError",
        21 => "IsADirectoryError",
        _ => "OSError",
    };
    let detail = std::io::Error::from_raw_os_error(errno)
        .to_string()
        .split(" (os error ")
        .next()
        .unwrap_or("I/O error")
        .to_string();
    format!(
        "{python_type}: [Errno {errno}] {detail}: '{}'",
        path.display()
    )
}

fn error_text_for_transport(error: &DunkError) -> String {
    let kind = match error {
        DunkError::Refused(_) => "DunkError",
        DunkError::Retryable(_) => "RetryableDunkError",
        DunkError::Ambiguous(_) => "AmbiguousTarget",
        // Identity refusals are surfaced by Python as the base DunkError class.
        DunkError::Identity(_) => "DunkError",
        // The pane client raises ConnectionError when its daemon is unavailable.
        DunkError::Unavailable(_) => "ConnectionError",
        DunkError::Busy => "OperationalError",
    };
    format!("{kind}: {error}")
}

fn read_json(path: &Path) -> Result<Value, DunkError> {
    let text = fs::read_to_string(path).map_err(|error| {
        let errno = error.raw_os_error().unwrap_or(5);
        DunkError::Refused(python_os_error(path, errno))
    })?;
    serde_json::from_str(&text).map_err(|error| {
        DunkError::Refused(format!(
            "JSONDecodeError: {}",
            python_json_decode_error(&text, &error)
        ))
    })
}

fn read_outbox(path: &Path) -> Result<Vec<Value>, DunkError> {
    let state = read_json(path)?;
    let object = state
        .as_object()
        .ok_or_else(|| DunkError::Refused("dunks.json root is not an object".into()))?;
    let Some(rows) = object.get("outbox").filter(|value| truthy(value)) else {
        return Ok(Vec::new());
    };
    rows.as_array()
        .cloned()
        .ok_or_else(|| DunkError::Refused("dunks.json 'outbox' is not an array".into()))
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

fn retired_path(inputs: &Inputs<'_>) -> PathBuf {
    inputs.path("retired-panes.json")
}

fn run_rule(rule_id: &str, rule: &Value, inputs: &Inputs<'_>) -> Result<Vec<Value>, DunkError> {
    let inbox_path = inputs.path("inbox.db");
    let inbox = InboxDb { path: &inbox_path };
    let metrics_path = inputs.path("metrics.db");
    let retired = retired_path(inputs);
    match rule_id {
        "task.blocked_stale" => Ok(rules::rule_task_blocked_stale(
            inputs.tasks()?,
            rule,
            inputs.now,
        )),
        "task.lease_stale" => rules::rule_task_lease_stale(inputs.tasks()?, rule, inputs.now),
        "dunk.skip_loop" => {
            let metrics = DunkMetricsDb {
                path: &metrics_path,
            };
            let retired = rules_dunks::retired_panes(&retired);
            rules_dunks::rule_dunk_skip_loop(inputs.dunks()?, &metrics, &retired, rule, inputs.now)
        }
        "dunk.overdue" => Ok(rules_dunks::rule_dunk_overdue(
            inputs.dunks()?,
            rule,
            inputs.now,
        )),
        "delivery.ambiguous_queued" => {
            let outbox = read_outbox(&inputs.path("dunks.json"))?;
            let observations = rules_delivery::MetricsDb {
                path: &metrics_path,
            };
            Ok(rules_delivery::rule_delivery_ambiguous_queued(
                &outbox,
                &observations,
                rule,
                inputs.now,
            ))
        }
        "inbox.undeliverable" => rules_inbox::rule_inbox_undeliverable(&inbox, rule, inputs.now),
        "inbox.unread_live" => {
            rules_inbox::rule_inbox_unread_live(&inbox, inputs.panes()?, &retired, rule, inputs.now)
        }
        "inbox.unread_dead" => {
            rules_inbox::rule_inbox_unread_dead(&inbox, inputs.panes()?, &retired, rule, inputs.now)
        }
        "inbox.ambiguous_sender" => {
            let panes = if rule["require_live_pane"].as_bool().unwrap_or(false) {
                inputs.panes()?
            } else {
                None
            };
            rules_inbox::rule_inbox_ambiguous_sender(
                &inbox,
                inputs.pane_id_pattern,
                panes,
                rule,
                inputs.now,
            )
        }
        "lane.working_without_task" => rules_lanes::rule_lane_working_without_task(
            inputs.panes()?,
            inputs.tasks()?,
            inputs.names()?,
            inputs.pane_id_pattern,
            rule,
            inputs.now,
        ),
        "lane.idle_holding_task" => rules_lanes::rule_lane_idle_holding_task(
            inputs.panes()?,
            inputs.tasks()?,
            &inbox,
            inputs.names()?,
            inputs.pane_id_pattern,
            rule,
            inputs.now,
        ),
        "fleet.assignment_starvation" => {
            let snapshot = inputs.starvation(rule)?;
            rules_lanes::rule_fleet_assignment_starvation(
                snapshot,
                &|| {
                    rules_lanes::turn_panes(
                        &metrics_path,
                        inputs.now - rule["quiet_minutes"].as_f64().unwrap_or(0.0) * 60.0,
                        inputs.now,
                    )
                },
                &|| inputs.panes().map(|panes| panes.cloned()),
                rule,
                inputs.now,
            )
        }
        "host.load" => {
            let host = inputs.host(Path::new("/"))?;
            let heavy = rules_host::heavy_jobs(Path::new("/tmp"));
            Ok(rules_host::rule_host_load(
                &host,
                inputs.panes()?,
                heavy,
                rule,
            ))
        }
        "host.tmp" => {
            let path = PathBuf::from(rule["path"].as_str().unwrap_or(""));
            let host = inputs.host(&path)?;
            Ok(rules_host::rule_host_tmp(&host, rule))
        }
        "host.disk_full" => {
            rules_host::rule_host_disk_full(&|path| inputs.host(Path::new(path)), rule)
        }
        "host.heavy_slot_stale" => rules_host::rule_host_heavy_slot_stale(rule, inputs.now),
        "resource.would_kill" => {
            rules_resource::rule_resource_would_kill(inputs.tasks()?, rule, inputs.now)
        }
        "pane.retired" => {
            let records: Vec<_> = crate::panes::all_retired(&retired);
            Ok(rules_host::rule_pane_retired(&records, rule))
        }
        "security.secret_text" => {
            let inbox_path = inputs.path("inbox.db");
            let tasks_path = inputs.path("tasks.db");
            let dunks_path = inputs.path("dunks.json");
            rules_security::rule_security_secret_text(
                &|since| {
                    rules_security::text_records(
                        &inbox_path,
                        &tasks_path,
                        &dunks_path,
                        since,
                        inputs.now,
                    )
                },
                rule,
                inputs.now,
            )
        }
        "money.spend_projection" => rules_host::rule_money_spend_projection(rule)
            .map_err(|unavailable| DunkError::Unavailable(unavailable.to_string())),
        "health.failed" => {
            let health = read_json(&inputs.path("health.json"))?;
            Ok(rules_host::rule_health_failed(&health, rule, inputs.now))
        }
        _ => Ok(Vec::new()),
    }
}

/// Run every configured rule, or only the exact names in `only`.
///
/// Stores and SQLite databases are opened read-only. Pane and host readers are injectable for
/// deterministic callers; with `panes.source != "none"`, an absent pane provider is reported as
/// a rule error instead of silently appearing empty. A failed rule contributes an entry to
/// `errors`, while source-declared unavailability contributes an entry to `skipped`.
pub fn evaluate(
    config: &Value,
    now: f64,
    panes_provider: Option<&PanesProvider<'_>>,
    host_sampler: Option<&HostSampler<'_>>,
    only: Option<&[&str]>,
) -> Value {
    let pane_source = config["panes"]["source"].as_str().unwrap_or("none");
    let pane_id_pattern = config["panes"]["pane_id_pattern"].as_str().unwrap_or("");
    let inputs = Inputs {
        dir: config_dir(config),
        now,
        pane_source,
        pane_id_pattern,
        panes_provider,
        host_sampler,
        tasks: OnceCell::new(),
        dunks: OnceCell::new(),
        panes: OnceCell::new(),
        names: OnceCell::new(),
        starvation: OnceCell::new(),
        host_samples: RefCell::new(BTreeMap::new()),
    };
    let mut findings = Vec::new();
    let mut errors = Vec::new();
    let mut skipped = Vec::new();
    for rule_id in RULE_IDS {
        if only.is_some_and(|selected| !selected.contains(rule_id)) {
            continue;
        }
        let rule = &config["rules"][*rule_id];
        if *rule_id == "money.spend_projection"
            && (rule["source"].is_null() || rule["cap"].is_null())
        {
            skipped.push(json!({"rule": rule_id, "error": "no metering source"}));
            continue;
        }
        if !rule["enabled"].as_bool().unwrap_or(false) {
            continue;
        }
        if matches!(
            *rule_id,
            "fleet.assignment_starvation"
                | "inbox.unread_dead"
                | "lane.working_without_task"
                | "lane.idle_holding_task"
        ) && pane_source == "none"
        {
            skipped.push(json!({
                "rule": rule_id,
                "error": "pane source disabled (panes.source=none)"
            }));
            continue;
        }
        match run_rule(rule_id, rule, &inputs) {
            Ok(mut found) => findings.append(&mut found),
            Err(error) if *rule_id == "money.spend_projection" => {
                skipped.push(json!({"rule": rule_id, "error": error.to_string()}));
            }
            Err(error) => errors.push(json!({
                "rule": rule_id,
                "error": error_text(rule_id, &error, &inputs)
            })),
        }
    }
    sort_findings(&mut findings);
    json!({"findings": findings, "errors": errors, "skipped": skipped})
}

/// One `watchdog.self_error` finding carrying the raw per-run problems.
pub fn self_error(problems: &[Value]) -> Value {
    json!({
        "rule": "watchdog.self_error",
        "key": "run",
        "severity": "self",
        "class": "self",
        "source": "watchdog-self",
        "summary": format!(
            "watchdog could not do {} thing(s); coverage or owner delivery is affected",
            problems.len()
        ),
        "threshold": "-",
        "measured": format!("{} problem(s)", problems.len()),
        "window": "this run",
        "evidence": {"problems": problems},
    })
}

#[cfg(test)]
#[path = "evaluate_tests.rs"]
mod evaluate_tests;
