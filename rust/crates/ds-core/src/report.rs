//! Deterministic report formatting, prose checks and report-file primitives.
//!
//! Collection and posting are deliberately owned by a later report integration
//! task. These functions consume the measured JSON explicitly and never invent
//! an unavailable value.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use regex::Regex;
use serde_json::{Map, Value};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ReportError {
    #[error("missing or invalid report field {0:?}")]
    Field(String),
    #[error("report timestamp is out of range")]
    Timestamp,
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Explicit filesystem paths keep unit tests away from the live report directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportPaths {
    pub output_dir: PathBuf,
    pub home_dir: PathBuf,
}

impl ReportPaths {
    pub fn new(output_dir: impl Into<PathBuf>, home_dir: impl Into<PathBuf>) -> Self {
        Self {
            output_dir: output_dir.into(),
            home_dir: home_dir.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReportFile {
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
    pub at: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReportListing {
    pub dir: String,
    pub reports: Vec<ReportFile>,
    pub count: usize,
}

/// Format a Unix timestamp as the reporter's UTC ISO-8601 value.
pub fn iso_timestamp(seconds: f64) -> Result<String, ReportError> {
    let whole_seconds = seconds.floor();
    if !whole_seconds.is_finite()
        || whole_seconds < i64::MIN as f64
        || whole_seconds > i64::MAX as f64
    {
        return Err(ReportError::Timestamp);
    }
    DateTime::from_timestamp(whole_seconds as i64, 0)
        .map(|time| time.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .ok_or(ReportError::Timestamp)
}

/// Format an optional age in seconds with the reporter's bucket boundaries.
pub fn format_age(seconds: Option<f64>) -> Result<String, ReportError> {
    let Some(seconds) = seconds else {
        return Ok("-".to_owned());
    };
    if !seconds.is_finite() || seconds < i64::MIN as f64 || seconds >= -(i64::MIN as f64) {
        return Err(ReportError::Field("age_s".to_owned()));
    }
    let seconds = seconds as i64;
    Ok(format_age_integer(seconds))
}

/// Shorten the configured home directory in a filesystem path, as `_short` does.
pub fn shorten_path(path: &Path, home: &Path) -> String {
    path.to_string_lossy()
        .replace(&home.to_string_lossy().to_string(), "~")
}

/// Render the source's seven deterministic sections in their fixed order.
pub fn render(report: &Value) -> Result<String, ReportError> {
    let sections = required_object(report, "sections")?;
    let tasks = required_object_field(sections, "tasks")?;
    let lanes = required_object_field(sections, "lanes")?;
    let needs_owner = sections
        .get("needs_owner")
        .filter(|value| python_truthy(value))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let reviews = required_object_field(sections, "reviews")?;
    let poolrad = required_object_field(sections, "poolrad")?;
    let budget = required_object_field(sections, "budget")?;
    let watchdog = required_object_field(sections, "watchdog")?;
    let iso = required(report, "iso")?;

    let window_hours = tasks
        .get("window_hours")
        .map(python_str)
        .unwrap_or_else(|| "24".to_owned());
    let mut lines = vec![
        format!("# Fleet report {}", python_str(iso)),
        String::new(),
        "Deterministic block: every number below was read from the stores at generation time. A section that could not be measured says so.".to_owned(),
        String::new(),
        format!("## Task metrics (last {window_hours}h)"),
    ];

    if let Some(error) = tasks.get("error").filter(|value| python_truthy(value)) {
        lines.push(format!("- {}", python_str(error)));
    } else {
        lines.push(format!(
            "- board now, by status: {}",
            counts_line(tasks.get("current_status"))
        ));
        lines.push(format!(
            "- board now, by priority: {}",
            counts_line(tasks.get("current_priority"))
        ));
        let flow = object_or_empty(tasks.get("flow"));
        let flow_items = ["created", "claimed", "completed", "cancelled", "blocked"]
            .into_iter()
            .filter_map(|key| {
                flow.get(key)
                    .filter(|value| !value.is_null())
                    .map(|value| format!("{key} {}", python_str(value)))
            })
            .collect::<Vec<_>>();
        lines.push(format!("- in the window: {}", flow_items.join(", ")));
        let latency = object_or_empty(tasks.get("latency_ms"));
        let mut latencies: Vec<_> = latency.iter().collect();
        latencies.sort_by(|a, b| a.0.cmp(b.0));
        for (name, stats) in latencies {
            let Some(stats) = stats.as_object() else {
                continue;
            };
            if !stats.get("count").is_some_and(python_truthy) {
                continue;
            }
            lines.push(format!(
                "- {name} latency: p50 {}ms, p90 {}ms over {}",
                display_optional(stats.get("p50")),
                display_optional(stats.get("p90")),
                python_str(&stats["count"])
            ));
        }
    }

    lines.extend([String::new(), "## Lanes".to_owned()]);
    if let Some(error) = lanes.get("error").filter(|value| python_truthy(value)) {
        lines.push(format!("- {}", python_str(error)));
        if lanes.get("live").is_some_and(|value| !value.is_null()) {
            lines.push(format!("- live panes {}", python_str(&lanes["live"])));
        }
    } else {
        lines.push(format!(
            "- live panes {}",
            display_optional(lanes.get("live"))
        ));
    }
    if lanes
        .get("error")
        .filter(|value| python_truthy(value))
        .is_none()
        || lanes.get("live").is_some_and(|v| !v.is_null())
    {
        for row in array_or_empty(lanes.get("working")) {
            let pane = required(row, "pane")?;
            let tasks = required(row, "tasks")?
                .as_array()
                .ok_or_else(|| ReportError::Field("tasks".to_owned()))?
                .iter()
                .map(python_str)
                .collect::<Vec<_>>();
            lines.push(format!(
                "- working: {} on {}",
                python_str(pane),
                tasks.join(", ")
            ));
        }
        for row in array_or_empty(lanes.get("idle")) {
            let pane = required(row, "pane")?;
            let label = required(row, "label")?;
            lines.push(format!(
                "- idle: {} ({})",
                python_str(pane),
                python_str(label)
            ));
        }
        for row in array_or_empty(lanes.get("dead")) {
            let pane = required(row, "pane")?;
            lines.push(format!(
                "- dead: {} ({}, {}) — registered, no live pane",
                python_str(pane),
                display_optional(row.get("label")),
                display_optional(row.get("role"))
            ));
        }
    }

    lines.extend([String::new(), "## Needs the owner".to_owned()]);
    let mut questions = array_or_empty(needs_owner.get("questions"));
    if let Some(error) = needs_owner
        .get("error")
        .filter(|value| python_truthy(value))
    {
        lines.push(format!("- not measured here: {}", python_str(error)));
        questions = &[];
    } else if questions.is_empty() {
        lines.push("- none".to_owned());
    }
    for row in questions {
        let id = required(row, "id")?;
        let priority = required(row, "priority")?;
        let age = required(row, "age_s")?;
        let question = required(row, "question")?;
        lines.push(format!(
            "- {} [{}] waiting {}: {}",
            python_str(id),
            python_str(priority),
            age_string(age)?,
            truncate_chars(&python_str(question), 200)
        ));
    }

    lines.extend([String::new(), "## Review queue".to_owned()]);
    if let Some(error) = reviews.get("error").filter(|value| python_truthy(value)) {
        lines.push(format!("- {}", python_str(error)));
    } else {
        let rows = array_or_empty(reviews.get("reviews"));
        if rows.is_empty() {
            lines.push("- empty".to_owned());
        } else {
            lines.push(format!(
                "- {} open, oldest {}",
                display_optional(reviews.get("count")),
                age_string_or_none(reviews.get("oldest_age_s"))?
            ));
            for row in rows {
                let task = required(row, "task")?;
                let repo = required(row, "repo")?;
                let pr = required(row, "pr")?;
                let head = required(row, "head")?;
                let age = required(row, "age_s")?;
                let reviewers = array_or_empty(row.get("reviewer"))
                    .iter()
                    .map(python_str)
                    .collect::<Vec<_>>();
                let reviewer = if reviewers.is_empty() {
                    "unclaimed".to_owned()
                } else {
                    reviewers.join(", ")
                };
                lines.push(format!(
                    "- {} {}#{} @{} age {} reviewer {}",
                    python_str(task),
                    python_str(repo),
                    python_str(pr),
                    python_str(head),
                    age_string(age)?,
                    reviewer
                ));
            }
        }
    }

    lines.extend([String::new(), "## Poolrad".to_owned()]);
    if let Some(error) = poolrad.get("error").filter(|value| python_truthy(value)) {
        lines.push(format!("- {}", python_str(error)));
    } else {
        lines.push(format!(
            "- credited strict bodies {}, identities {}, main {}",
            display_optional(poolrad.get("credited")),
            display_optional(poolrad.get("identities")),
            display_optional(poolrad.get("main"))
        ));
        for key in ["ceiling_note", "replaced_note"] {
            if let Some(value) = poolrad.get(key).filter(|value| python_truthy(value)) {
                lines.push(format!("- {}", python_str(value)));
            }
        }
    }

    lines.extend([String::new(), "## Budget".to_owned()]);
    if let Some(error) = budget.get("error").filter(|value| python_truthy(value)) {
        lines.push(format!("- {}", python_str(error)));
    } else {
        let providers = array_or_empty(budget.get("providers"));
        if providers.is_empty() {
            lines.push("- no providers in the usage cache".to_owned());
        }
        for row in providers {
            let provider = required(row, "provider")?;
            let used = required(row, "used")?;
            let limit = required(row, "limit")?;
            let window = row
                .get("window")
                .filter(|value| python_truthy(value))
                .map(python_str)
                .unwrap_or_else(|| "window unknown".to_owned());
            lines.push(format!(
                "- {}: used {} of {} ({window})",
                python_str(provider),
                python_str(used),
                python_str(limit)
            ));
        }
    }

    lines.extend([String::new(), "## Watchdog".to_owned()]);
    if let Some(error) = watchdog.get("error").filter(|value| python_truthy(value)) {
        lines.push(format!("- {}", python_str(error)));
    } else {
        lines.push(format!(
            "- heartbeat age {}",
            age_string_or_none(watchdog.get("heartbeat_age_s"))?
        ));
        let state = object_or_empty(watchdog.get("state"));
        let mut state: Vec<_> = state.iter().collect();
        state.sort_by(|a, b| a.0.cmp(b.0));
        for (key, value) in state {
            lines.push(format!("- {key}: {}", python_str(value)));
        }
    }
    Ok(lines.join("\n") + "\n")
}

/// Return source-compatible task-id/status complaints in deterministic order.
pub fn prose_problems(prose: &str, report: &Value) -> Result<Vec<String>, ReportError> {
    let sections = required_object(report, "sections")?;
    let reviews = required_object_field(sections, "reviews")?;
    let lanes = required_object_field(sections, "lanes")?;
    let tasks = required_object_field(sections, "tasks")?;
    let mut known_ids = BTreeSet::new();
    let mut known_status = BTreeSet::new();
    for row in array_or_empty(reviews.get("reviews")) {
        if let Some(task) = row.get("task").filter(|task| python_truthy(task)) {
            known_ids.insert(python_str(task));
        }
    }
    if let Some(needs_owner) = sections
        .get("needs_owner")
        .filter(|value| python_truthy(value))
    {
        for row in array_or_empty(needs_owner.get("questions")) {
            if let Some(id) = row.get("id") {
                known_ids.insert(python_str(id));
            }
        }
    }
    for row in array_or_empty(lanes.get("working")) {
        for task in array_or_empty(row.get("tasks")) {
            known_ids.insert(python_str(task));
        }
    }
    for (status, count) in object_or_empty(tasks.get("current_status")) {
        if python_truthy(count) {
            known_status.insert(status.clone());
        }
    }

    static TASK_ID: OnceLock<Regex> = OnceLock::new();
    let task_id_regex = TASK_ID.get_or_init(|| Regex::new(r"\bt\d{1,5}\b").expect("valid regex"));
    let mut problems = Vec::new();
    for task_id in task_id_regex
        .find_iter(prose)
        .map(|m| m.as_str())
        .collect::<BTreeSet<_>>()
    {
        if !known_ids.contains(task_id) {
            problems.push(format!(
                "prose names {task_id}, which this report does not mention"
            ));
        }
    }
    for status in ["open", "in_progress", "blocked", "done", "cancelled"] {
        let regex = Regex::new(&format!(r"\b{status}\b")).expect("valid regex");
        if regex.is_match(prose) && !known_status.contains(status) {
            problems.push(format!(
                "prose says {status:?}, which this report does not report"
            ));
        }
    }
    Ok(problems)
}

/// Write without overwriting an earlier report, using the report's UTC timestamp.
pub fn write_report(
    paths: &ReportPaths,
    report: &Value,
    text: &str,
) -> Result<PathBuf, ReportError> {
    fs::create_dir_all(&paths.output_dir)?;
    let timestamp = timestamp(report)?;
    let stamp = timestamp.format("%Y%m%dT%H%M%SZ").to_string();
    let mut suffix = 1usize;
    loop {
        let name = if suffix == 1 {
            format!("{stamp}.md")
        } else {
            format!("{stamp}-{suffix}.md")
        };
        let path = paths.output_dir.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(text.as_bytes())?;
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => suffix += 1,
            Err(error) => return Err(error.into()),
        }
    }
}

/// List Markdown reports newest-first, preserving name order for equal mtimes.
/// A directory read failure is an empty listing, while individual metadata errors
/// propagate just as the Python `getsize`/`getmtime` calls do.
pub fn list_reports(paths: &ReportPaths) -> Result<ReportListing, ReportError> {
    let dir = short_path(&paths.output_dir, &paths.home_dir);
    let entries = match fs::read_dir(&paths.output_dir) {
        Ok(entries) => entries,
        Err(_) => {
            return Ok(ReportListing {
                dir,
                reports: Vec::new(),
                count: 0,
            });
        }
    };
    let mut names = entries
        .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
        .collect::<Result<Vec<_>, _>>()?;
    names.sort();
    let mut reports = Vec::new();
    for name in names.into_iter().filter(|name| name.ends_with(".md")) {
        let path = paths.output_dir.join(&name);
        let metadata = fs::metadata(&path)?;
        let at = metadata
            .modified()
            .map_err(ReportError::Io)
            .map(system_time_seconds)?;
        reports.push(ReportFile {
            name,
            path,
            size: metadata.len(),
            at,
        });
    }
    reports.sort_by(|a, b| b.at.total_cmp(&a.at));
    let count = reports.len();
    Ok(ReportListing {
        dir,
        reports,
        count,
    })
}

fn timestamp(report: &Value) -> Result<DateTime<Utc>, ReportError> {
    let value = required(report, "generated_at")?;
    let seconds = value
        .as_f64()
        .map(f64::floor)
        .or_else(|| value.as_i64().map(|v| v as f64))
        .ok_or_else(|| ReportError::Field("generated_at".to_owned()))?;
    if !seconds.is_finite() || seconds < i64::MIN as f64 || seconds > i64::MAX as f64 {
        return Err(ReportError::Timestamp);
    }
    DateTime::from_timestamp(seconds as i64, 0).ok_or(ReportError::Timestamp)
}

fn required<'a>(value: &'a Value, name: &str) -> Result<&'a Value, ReportError> {
    value
        .get(name)
        .ok_or_else(|| ReportError::Field(name.to_owned()))
}

fn required_object<'a>(
    value: &'a Value,
    name: &str,
) -> Result<&'a Map<String, Value>, ReportError> {
    value
        .get(name)
        .and_then(Value::as_object)
        .ok_or_else(|| ReportError::Field(name.to_owned()))
}

fn required_object_field<'a>(
    value: &'a Map<String, Value>,
    name: &str,
) -> Result<&'a Map<String, Value>, ReportError> {
    value
        .get(name)
        .and_then(Value::as_object)
        .ok_or_else(|| ReportError::Field(name.to_owned()))
}

fn object_or_empty(value: Option<&Value>) -> &Map<String, Value> {
    value
        .and_then(Value::as_object)
        .unwrap_or_else(|| empty_object())
}

fn empty_object() -> &'static Map<String, Value> {
    static EMPTY: OnceLock<Map<String, Value>> = OnceLock::new();
    EMPTY.get_or_init(Map::new)
}

fn array_or_empty(value: Option<&Value>) -> &[Value] {
    value
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn display_optional(value: Option<&Value>) -> String {
    value.map(python_str).unwrap_or_else(|| "None".to_owned())
}

pub fn counts_line(value: Option<&Value>) -> String {
    let mapping = object_or_empty(value);
    if mapping.is_empty() {
        return "none".to_owned();
    }
    let mut keys: Vec<_> = mapping.keys().collect();
    keys.sort();
    keys.into_iter()
        .filter_map(|key| {
            let value = &mapping[key];
            python_truthy(value).then(|| format!("{key} {}", python_str(value)))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn age_string_or_none(value: Option<&Value>) -> Result<String, ReportError> {
    value.map(age_string).unwrap_or_else(|| Ok("-".to_owned()))
}

fn age_string(value: &Value) -> Result<String, ReportError> {
    if value.is_null() {
        return Ok("-".to_owned());
    }
    let seconds = value
        .as_i64()
        .or_else(|| {
            value
                .as_u64()
                .and_then(|seconds| i64::try_from(seconds).ok())
        })
        .or_else(|| value.as_f64().map(|seconds| seconds as i64))
        .or_else(|| value.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
        .ok_or_else(|| ReportError::Field("age_s".to_owned()))?;
    Ok(format_age_integer(seconds))
}

fn format_age_integer(seconds: i64) -> String {
    if seconds < 90 {
        format!("{seconds}s")
    } else if seconds < 5400 {
        format!("{}m", seconds / 60)
    } else if seconds < 172800 {
        format!("{}h", seconds / 3600)
    } else {
        format!("{}d", seconds / 86400)
    }
}

fn python_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn python_str(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::String(value) => value.clone(),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(python_repr)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| format!("{}: {}", python_quote(key), python_repr(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Number(value) => value.to_string(),
    }
}

fn python_repr(value: &Value) -> String {
    match value {
        Value::String(value) => python_quote(value),
        _ => python_str(value),
    }
}

fn python_quote(value: &str) -> String {
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut output = String::with_capacity(value.len() + 2);
    output.push(quote);
    for ch in value.chars() {
        match ch {
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            ch if ch == quote => {
                output.push('\\');
                output.push(ch);
            }
            ch if ch.is_control() => {
                use std::fmt::Write as _;
                let code = ch as u32;
                if code <= 0xff {
                    write!(&mut output, "\\x{code:02x}").expect("writing to String");
                } else if code <= 0xffff {
                    write!(&mut output, "\\u{code:04x}").expect("writing to String");
                } else {
                    write!(&mut output, "\\U{code:08x}").expect("writing to String");
                }
            }
            ch => output.push(ch),
        }
    }
    output.push(quote);
    output
}

fn truncate_chars(value: &str, count: usize) -> String {
    value.chars().take(count).collect()
}

fn short_path(path: &Path, home: &Path) -> String {
    shorten_path(path, home)
}

fn system_time_seconds(time: std::time::SystemTime) -> f64 {
    match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(duration) => duration.as_secs_f64(),
        Err(error) => -error.duration().as_secs_f64(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::thread;
    use std::time::Duration;

    fn full_report() -> Value {
        serde_json::from_str(include_str!("../tests/fixtures/report-full.json")).unwrap()
    }

    #[test]
    fn render_matches_python_generated_full_report_fixture() {
        let expected = include_str!("../tests/fixtures/report-full.md");
        assert_eq!(render(&full_report()).unwrap(), expected);
    }

    #[test]
    fn render_matches_python_generated_unavailable_sections_fixture() {
        let report: Value =
            serde_json::from_str(include_str!("../tests/fixtures/report-unavailable.json"))
                .unwrap();
        let expected = include_str!("../tests/fixtures/report-unavailable.md");
        assert_eq!(render(&report).unwrap(), expected);
    }

    #[test]
    fn pure_formatting_helpers_match_python_boundaries() {
        assert_eq!(
            iso_timestamp(1_700_000_000.9).unwrap(),
            "2023-11-14T22:13:20Z"
        );
        assert_eq!(format_age(None).unwrap(), "-");
        assert_eq!(format_age(Some(89.9)).unwrap(), "89s");
        assert_eq!(format_age(Some(90.0)).unwrap(), "1m");
        assert_eq!(format_age(Some(5_400.0)).unwrap(), "1h");
        assert_eq!(format_age(Some(172_800.0)).unwrap(), "2d");
        assert_eq!(
            counts_line(Some(&json!({"open": 2, "done": 0, "blocked": 1}))),
            "blocked 1, open 2"
        );
        assert_eq!(
            shorten_path(Path::new("/home/person/reports"), Path::new("/home/person")),
            "~/reports"
        );
    }

    #[test]
    fn prose_checks_only_python_oracle_task_ids_and_statuses() {
        let report = full_report();
        assert_eq!(
            prose_problems("t999 is done; t123 is open; value is 123", &report).unwrap(),
            vec![
                "prose names t999, which this report does not mention",
                "prose says 'done', which this report does not report"
            ]
        );
        assert!(prose_problems("t123 is open and 123 units", &report)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn writes_timestamped_files_without_overwriting_and_lists_markdown_newest_first() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let output = home.join("reports");
        let paths = ReportPaths::new(&output, &home);
        let report = json!({"generated_at": 1_700_000_000});
        let first = write_report(&paths, &report, "first\n").unwrap();
        let second = write_report(&paths, &report, "second\n").unwrap();
        assert_eq!(first.file_name().unwrap(), "20231114T221320Z.md");
        assert_eq!(second.file_name().unwrap(), "20231114T221320Z-2.md");
        assert_eq!(fs::read_to_string(first).unwrap(), "first\n");
        assert_eq!(fs::read_to_string(second).unwrap(), "second\n");

        fs::write(output.join("ignored.txt"), "not a report").unwrap();
        thread::sleep(Duration::from_millis(20));
        fs::write(output.join("newest.md"), "newest").unwrap();
        fs::write(output.join("tie-b.md"), "tie").unwrap();
        fs::hard_link(output.join("tie-b.md"), output.join("tie-a.md")).unwrap();
        let listing = list_reports(&paths).unwrap();
        assert_eq!(listing.dir, "~/reports");
        assert_eq!(listing.count, 5);
        assert_eq!(listing.reports[0].name, "newest.md");
        assert!(listing
            .reports
            .iter()
            .all(|file| file.name.ends_with(".md")));
        assert_eq!(listing.reports[0].size, 6);
        let tie_names = listing
            .reports
            .iter()
            .filter(|file| file.name.starts_with("tie-"))
            .map(|file| file.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(tie_names, ["tie-a.md", "tie-b.md"]);
    }

    #[test]
    fn missing_directory_is_an_empty_listing_with_shortened_path() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let paths = ReportPaths::new(home.join("missing"), &home);
        let listing = list_reports(&paths).unwrap();
        assert_eq!(listing.dir, "~/missing");
        assert!(listing.reports.is_empty());
        assert_eq!(listing.count, 0);
    }

    #[cfg(unix)]
    #[test]
    fn individual_metadata_failure_propagates() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("reports");
        fs::create_dir(&output).unwrap();
        symlink(output.join("missing-target"), output.join("broken.md")).unwrap();
        let paths = ReportPaths::new(&output, temp.path());
        assert!(matches!(list_reports(&paths), Err(ReportError::Io(_))));
    }
}
