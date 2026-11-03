//! Report collection, file output, and listing over explicit raw sources.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::api::{Args, HandlerContext};
use crate::error::DunkError;

use super::{
    iso_timestamp, list_reports, prose_problems, render, shorten_path, write_report, ReportError,
    ReportFile, ReportListing, ReportSources,
};

const WINDOW_S: f64 = 24.0 * 60.0 * 60.0;

#[derive(Debug, thiserror::Error)]
pub enum ReportRunError {
    #[error(transparent)]
    Report(#[from] ReportError),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReportRun {
    pub path: PathBuf,
    pub text: String,
    pub report: Value,
    pub prose_rejected: Vec<String>,
    pub posted: Option<Value>,
}

/// The report facade. It consumes measurements as they are read and owns the shared run/list
/// behavior; the production daemon supplies `ReportSources` in its runtime integration task.
pub struct Report;

impl Report {
    /// Python `dunk_report.collect` over the raw stores and on-disk usage/watchdog caches.
    pub fn collect(sources: &dyn ReportSources, now: Option<f64>) -> Result<Value, ReportError> {
        let now = now
            .filter(|value| *value != 0.0)
            .unwrap_or_else(|| sources.now_s());
        let paths = sources.report_paths();
        Ok(json!({
            "generated_at": now,
            "iso": iso_timestamp(now)?,
            "sections": {
                "tasks": task_section(sources),
                "lanes": lanes_section(sources),
                "needs_owner": needs_owner_section(sources, now),
                "reviews": review_section(sources),
                "poolrad": poolrad_section(sources),
                "budget": budget_section(&sources.config_dir(), &paths.home_dir),
                "watchdog": watchdog_section(&sources.config_dir(), &paths.home_dir, now),
            }
        }))
    }

    /// Python `dunk_report.run`: build the deterministic block, optionally validate prose, write
    /// a unique file, then try posting. A post failure is returned as data and never loses the
    /// already-written file.
    pub fn run(
        sources: &dyn ReportSources,
        now: Option<f64>,
        out_dir: Option<&Path>,
        post: bool,
        prose: Option<&str>,
    ) -> Result<ReportRun, ReportRunError> {
        let report = Self::collect(sources, now)?;
        let mut text = render(&report)?;
        let prose_rejected = match prose.filter(|text| !text.is_empty()) {
            Some(prose) => prose_problems(prose, &report)?,
            None => Vec::new(),
        };
        if let Some(prose) = prose.filter(|text| !text.is_empty()) {
            if prose_rejected.is_empty() {
                text.push_str("\n## In plain language\n\n");
                text.push_str(prose.trim());
                text.push('\n');
            }
        }

        let mut paths = sources.report_paths();
        if let Some(out_dir) = out_dir {
            paths.output_dir = out_dir.to_path_buf();
        }
        let path = write_report(&paths, &report, &text)?;
        let posted = if post {
            Some(match sources.post_report(&report, &text) {
                Ok(value) => value,
                Err(error) => json!({"error": error}),
            })
        } else {
            None
        };
        Ok(ReportRun {
            path,
            text,
            report,
            prose_rejected,
            posted,
        })
    }

    /// Python `dunk_report.list_reports`, serialized to the command response shape.
    pub fn list(sources: &dyn ReportSources, out_dir: Option<&Path>) -> Result<Value, ReportError> {
        let mut paths = sources.report_paths();
        if let Some(out_dir) = out_dir {
            paths.output_dir = out_dir.to_path_buf();
        }
        let listing = list_reports(&paths)?;
        Ok(listing_value(listing))
    }
}

pub(crate) fn report_run_handler(
    context: &HandlerContext<'_>,
    args: Args,
) -> Result<Value, DunkError> {
    let sources = context.env.report_sources()?;
    let post = args
        .get("post")
        .map(|value| python_bool(value, true))
        .unwrap_or(true);
    let prose = match args.get("prose") {
        None | Some(Value::Null) => None,
        Some(Value::String(prose)) => Some(prose.as_str()),
        Some(_) => {
            return Err(DunkError::Refused(
                "report prose must be a string".to_owned(),
            ));
        }
    };
    let out_dir = optional_path(&args, "out_dir")?;
    let result = Report::run(sources, None, out_dir.as_deref(), post, prose)
        .map_err(|error| DunkError::Refused(error.to_string()))?;
    Ok(json!({
        "path": result.path,
        "prose_rejected": result.prose_rejected,
        "posted": result.posted,
        "text": result.text,
        "generated_at": result.report["iso"],
    }))
}

pub(crate) fn report_list_handler(
    context: &HandlerContext<'_>,
    args: Args,
) -> Result<Value, DunkError> {
    let sources = context.env.report_sources()?;
    let out_dir = optional_path(&args, "out_dir")?;
    Report::list(sources, out_dir.as_deref()).map_err(|error| DunkError::Refused(error.to_string()))
}

fn optional_path(args: &Args, key: &str) -> Result<Option<PathBuf>, DunkError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(path)) if path.is_empty() => Ok(None),
        Some(Value::String(path)) => Ok(Some(PathBuf::from(path))),
        Some(_) => Err(DunkError::Refused(format!("{key} must be a string"))),
    }
}

fn python_bool(value: &Value, default: bool) -> bool {
    match value {
        Value::Null => default,
        Value::Bool(value) => *value,
        Value::String(value) => matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "y" | "on"
        ),
        other => matches!(
            python_string(other).trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "y" | "on"
        ),
    }
}

fn task_section(sources: &dyn ReportSources) -> Value {
    let metrics = match sources.task_metrics(WINDOW_S) {
        Ok(metrics) => metrics,
        Err(error) => return json!({"error": format!("task metrics unavailable: {error}")}),
    };
    let window = metrics.get("window").filter(|value| truthy(value));
    let seconds = window
        .and_then(|value| value.get("seconds"))
        .filter(|value| truthy(value))
        .and_then(Value::as_f64)
        .unwrap_or(WINDOW_S);
    let current = metrics.get("current").filter(|value| truthy(value));
    let throughput = metrics.get("throughput").filter(|value| truthy(value));
    let flow = ["created", "claimed", "completed", "cancelled", "blocked"]
        .into_iter()
        .map(|key| {
            (
                key.to_owned(),
                throughput
                    .and_then(|value| value.get(key))
                    .cloned()
                    .unwrap_or(Value::Null),
            )
        })
        .collect::<Map<_, _>>();
    json!({
        "window_hours": round_ties_even(seconds / 3600.0 * 10.0) / 10.0,
        "current_status": current.and_then(|value| value.get("by_status")).filter(|value| truthy(value)).cloned().unwrap_or_else(|| json!({})),
        "current_priority": current.and_then(|value| value.get("by_priority")).filter(|value| truthy(value)).cloned().unwrap_or_else(|| json!({})),
        "flow": flow,
        "latency_ms": metrics.get("latency_ms").filter(|value| truthy(value)).cloned().unwrap_or_else(|| json!({})),
        "coverage": metrics.get("coverage").cloned().unwrap_or(Value::Null),
    })
}

fn lanes_section(sources: &dyn ReportSources) -> Value {
    let panes = match sources.list_panes() {
        Ok(panes) => panes,
        Err(error) => return json!({"error": format!("pane list unavailable: {error}")}),
    };
    let live = panes
        .iter()
        .filter_map(|pane| {
            pane.get("pane_id")
                .filter(|value| truthy(value))
                .map(|id| (python_string(id), pane))
        })
        .collect::<BTreeMap<_, _>>();
    let mut working = BTreeMap::<String, Vec<String>>::new();
    let mut errors = Vec::new();
    let tasks_error = match sources.list_tasks(500) {
        Ok(tasks) => {
            for task in tasks {
                if let Some(leases) = task.get("leases").and_then(Value::as_array) {
                    for lease in leases {
                        if let Some(pane_id) = lease.get("pane_id").filter(|id| truthy(id)) {
                            working
                                .entry(python_string(pane_id))
                                .or_default()
                                .push(python_string(task.get("id").unwrap_or(&Value::Null)));
                        }
                    }
                }
            }
            None
        }
        Err(error) => Some(format!("task list unavailable: {error}")),
    };
    let mut idle = BTreeMap::<String, String>::new();
    for (pane_id, pane) in &live {
        if !working.contains_key(pane_id) {
            let label = pane
                .get("label")
                .filter(|value| truthy(value))
                .or_else(|| pane.get("agent").filter(|value| truthy(value)))
                .map(python_string)
                .unwrap_or_else(|| "-".to_owned());
            idle.insert(pane_id.clone(), label);
        }
    }
    if let Some(error) = tasks_error {
        errors.push(error);
    }
    let (mut dead, registry_error) = match sources.lane_registry() {
        Ok(registry) => {
            let lanes = registry
                .get("lanes")
                .filter(|value| truthy(value))
                .and_then(Value::as_object);
            let dead = lanes
                .into_iter()
                .flat_map(Map::iter)
                .filter(|(pane_id, _)| !live.contains_key(*pane_id))
                .map(|(pane_id, entry)| {
                    json!({
                        "pane": pane_id,
                        "label": entry.get("label").cloned().unwrap_or(Value::Null),
                        "role": entry.get("role").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect::<Vec<_>>();
            (dead, None)
        }
        Err(error) => (
            Vec::new(),
            Some(format!("lane registry unavailable: {error}")),
        ),
    };
    if let Some(error) = registry_error {
        errors.push(error);
    }
    if errors
        .iter()
        .any(|error| error.starts_with("task list unavailable:"))
    {
        idle.clear();
        working.clear();
    }
    if errors
        .iter()
        .any(|error| error.starts_with("lane registry unavailable:"))
    {
        dead.clear();
    }
    let mut section = json!({
        "working": working.into_iter().map(|(pane, tasks)| json!({"pane": pane, "tasks": tasks})).collect::<Vec<_>>(),
        "idle": idle.into_iter().map(|(pane, label)| json!({"pane": pane, "label": label})).collect::<Vec<_>>(),
        "dead": dead,
        "live": live.len(),
    });
    if !errors.is_empty() {
        section["error"] = json!(errors.join("; "));
    }
    section
}

fn needs_owner_section(sources: &dyn ReportSources, now: f64) -> Value {
    let result = match sources.needs_owner_tasks(50) {
        Ok(result) => result,
        Err(error) => return json!({"error": format!("needs-owner query unavailable: {error}")}),
    };
    let rows = result
        .get("tasks")
        .filter(|value| truthy(value))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|task| {
            let reason = task
                .get("block_reason")
                .filter(|value| truthy(value))
                .map(python_string)
                .unwrap_or_default();
            let question = reason.strip_prefix("needs-owner:")?.trim();
            let created = task
                .get("created_at")
                .and_then(Value::as_f64)
                .unwrap_or(now);
            Some(json!({
                "id": task.get("id").cloned().unwrap_or(Value::Null),
                "priority": task.get("priority").cloned().unwrap_or(Value::Null),
                "age_s": rounded_integer(now - created),
                "question": question,
            }))
        })
        .collect::<Vec<_>>();
    json!({"questions": rows})
}

fn review_section(sources: &dyn ReportSources) -> Value {
    let result = match sources.review_list() {
        Ok(result) => result,
        Err(error) => return json!({"error": format!("review queue unavailable: {error}")}),
    };
    let rows = result
        .get("reviews")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(20)
        .map(|row| {
            let head = row
                .get("head")
                .filter(|value| truthy(value))
                .map(python_string)
                .unwrap_or_default();
            json!({
                "task": row.get("task").cloned().unwrap_or(Value::Null),
                "repo": row.get("repo").cloned().unwrap_or(Value::Null),
                "pr": row.get("pr").cloned().unwrap_or(Value::Null),
                "head": head.chars().take(12).collect::<String>(),
                "age_s": row.get("age_s").cloned().unwrap_or(Value::Null),
                "reviewer": row.get("reviewer").cloned().unwrap_or(Value::Null),
            })
        })
        .collect::<Vec<_>>();
    json!({
        "reviews": rows,
        "count": result.get("count").cloned().unwrap_or(Value::Null),
        "oldest_age_s": result.get("oldest_age_s").cloned().unwrap_or(Value::Null),
    })
}

fn poolrad_section(sources: &dyn ReportSources) -> Value {
    sources
        .poolrad_progress()
        .unwrap_or_else(|error| json!({"error": format!("poolrad progress unavailable: {error}")}))
}

fn budget_section(config_dir: &Path, home_dir: &Path) -> Value {
    let path = config_dir.join("usage.json");
    let data = match read_cache(&path) {
        Ok(data) => data,
        Err(()) => {
            return json!({"error": format!("no usage cache at {}: budget not measured here", shorten_path(&path, home_dir))})
        }
    };
    let providers = data
        .get("providers")
        .filter(|value| truthy(value))
        .unwrap_or(&data);
    let mut providers = providers
        .as_object()
        .into_iter()
        .flat_map(Map::iter)
        .filter_map(|(name, entry)| {
            let entry = entry.as_object()?;
            let used = entry
                .get("used")
                .filter(|value| truthy(value))
                .or_else(|| entry.get("spent"));
            let limit = entry
                .get("limit")
                .filter(|value| truthy(value))
                .or_else(|| entry.get("budget"));
            Some(json!({
                "provider": name,
                "used": used.cloned().unwrap_or(Value::Null),
                "limit": limit.cloned().unwrap_or(Value::Null),
                "window": entry.get("window").cloned().unwrap_or(Value::Null),
            }))
        })
        .collect::<Vec<_>>();
    providers.sort_by(|left, right| left["provider"].as_str().cmp(&right["provider"].as_str()));
    json!({
        "providers": providers,
        "as_of": data.get("as_of").cloned().unwrap_or(Value::Null),
    })
}

fn watchdog_section(config_dir: &Path, home_dir: &Path, now: f64) -> Value {
    let path = config_dir.join("watchdog_state.json");
    let data = match read_cache(&path) {
        Ok(data) => data,
        Err(()) => {
            return json!({"error": format!("no watchdog state at {}", shorten_path(&path, home_dir))})
        }
    };
    let state = data
        .as_object()
        .into_iter()
        .flat_map(Map::iter)
        .filter(|(key, _)| key.as_str() != "history")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<Map<_, _>>();
    let heartbeat_age = data
        .get("heartbeat_at")
        .filter(|value| value.is_number())
        .and_then(Value::as_f64)
        .map(|at| rounded_integer(now - at))
        .unwrap_or(Value::Null);
    json!({"state": state, "heartbeat_age_s": heartbeat_age})
}

fn read_cache(path: &Path) -> Result<Value, ()> {
    let text = fs::read_to_string(path).map_err(|_| ())?;
    serde_json::from_str(&text).map_err(|_| ())
}

fn listing_value(listing: ReportListing) -> Value {
    json!({
        "dir": listing.dir,
        "reports": listing.reports.into_iter().map(report_file_value).collect::<Vec<_>>(),
        "count": listing.count,
    })
}

fn report_file_value(file: ReportFile) -> Value {
    json!({
        "name": file.name,
        "path": file.path,
        "size": file.size,
        "at": file.at,
    })
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_none_or(|number| number != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn python_string(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::String(value) => value.clone(),
        other => other.to_string(),
    }
}

fn round_ties_even(value: f64) -> f64 {
    value.round_ties_even()
}

fn rounded_integer(value: f64) -> Value {
    json!(round_ties_even(value) as i64)
}

#[cfg(test)]
mod tests {
    use super::super::ReportPaths;
    use super::*;
    use std::time::{Duration, SystemTime};

    use serde_json::json;

    struct Sources {
        paths: ReportPaths,
        config_dir: PathBuf,
        metrics: Result<Value, String>,
        panes: Result<Vec<Value>, String>,
        tasks: Result<Vec<Value>, String>,
        needs_owner: Result<Value, String>,
        lanes: Result<Value, String>,
        reviews: Result<Value, String>,
        poolrad: Result<Value, String>,
        post: Result<Value, String>,
        posted: std::sync::Mutex<Vec<Value>>,
    }

    impl Sources {
        fn new(root: &Path) -> Self {
            let home = root.join("home");
            let config_dir = home.join(".config/dunkingsheep");
            let output_dir = config_dir.join("reports");
            fs::create_dir_all(&config_dir).unwrap();
            Self {
                paths: ReportPaths::new(output_dir, home),
                config_dir,
                metrics: Ok(json!({
                    "window": {"seconds": WINDOW_S},
                    "current": {"by_status": {"open": 2}, "by_priority": {"high": 1}},
                    "throughput": {"created": 3, "completed": 1},
                    "latency_ms": {"claim": {"count": 2, "p50": 10, "p90": 30}},
                    "coverage": {"covered": true},
                })),
                panes: Ok(vec![
                    json!({"pane_id": "%working", "label": "worker"}),
                    json!({"pane_id": "%idle", "agent": "desk"}),
                ]),
                tasks: Ok(vec![
                    json!({"id": "t10", "leases": [{"pane_id": "%working"}]}),
                ]),
                needs_owner: Ok(json!({"tasks": [
                    {"id": "t11", "priority": "urgent", "created_at": 1_699_999_900.0,
                     "block_reason": "needs-owner: approve the plan?"},
                    {"id": "t12", "block_reason": "waiting for a dependency"},
                ]})),
                lanes: Ok(json!({"lanes": {"%dead": {"label": "gone", "role": "worker"}}})),
                reviews: Ok(json!({"count": 1, "oldest_age_s": 55, "reviews": [
                    {"task": "t13", "repo": "org/repo", "pr": 7, "head": "1234567890abcdef",
                     "age_s": 55, "reviewer": ["%reviewer"]}
                ]})),
                poolrad: Ok(json!({"credited": 2})),
                post: Ok(json!({"message_id": "m1", "transport": "report"})),
                posted: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl ReportSources for Sources {
        fn now_s(&self) -> f64 {
            1_700_000_000.0
        }
        fn report_paths(&self) -> ReportPaths {
            self.paths.clone()
        }
        fn config_dir(&self) -> PathBuf {
            self.config_dir.clone()
        }
        fn task_metrics(&self, window_s: f64) -> Result<Value, String> {
            assert_eq!(window_s, WINDOW_S);
            self.metrics.clone()
        }
        fn list_panes(&self) -> Result<Vec<Value>, String> {
            self.panes.clone()
        }
        fn list_tasks(&self, limit: usize) -> Result<Vec<Value>, String> {
            assert_eq!(limit, 500);
            self.tasks.clone()
        }
        fn needs_owner_tasks(&self, limit: usize) -> Result<Value, String> {
            assert_eq!(limit, 50);
            self.needs_owner.clone()
        }
        fn lane_registry(&self) -> Result<Value, String> {
            self.lanes.clone()
        }
        fn review_list(&self) -> Result<Value, String> {
            self.reviews.clone()
        }
        fn poolrad_progress(&self) -> Result<Value, String> {
            self.poolrad.clone()
        }
        fn post_report(&self, report: &Value, text: &str) -> Result<Value, String> {
            self.posted
                .lock()
                .unwrap()
                .push(json!({"report": report, "text": text}));
            self.post.clone()
        }
    }

    #[test]
    fn report_collect_measures_all_sections_and_marks_missing_caches_unavailable() {
        let temp = tempfile::tempdir().unwrap();
        let sources = Sources::new(temp.path());
        let report = Report::collect(&sources, Some(1_700_000_000.0)).unwrap();
        let sections = report["sections"].as_object().unwrap();
        assert_eq!(sections.len(), 7);
        assert_eq!(sections["tasks"]["current_status"]["open"], 2);
        assert_eq!(sections["tasks"]["flow"]["created"], 3);
        assert_eq!(
            sections["lanes"]["working"][0],
            json!({"pane": "%working", "tasks": ["t10"]})
        );
        assert_eq!(sections["lanes"]["idle"][0]["label"], "desk");
        assert_eq!(sections["lanes"]["dead"][0]["pane"], "%dead");
        assert_eq!(
            sections["needs_owner"]["questions"][0]["question"],
            "approve the plan?"
        );
        assert_eq!(sections["reviews"]["reviews"][0]["head"], "1234567890ab");
        assert_eq!(sections["poolrad"]["credited"], 2);
        assert_eq!(
            sections["budget"]["error"],
            "no usage cache at ~/.config/dunkingsheep/usage.json: budget not measured here"
        );
        assert_eq!(
            sections["watchdog"]["error"],
            "no watchdog state at ~/.config/dunkingsheep/watchdog_state.json"
        );
        let text = render(&report).unwrap();
        for heading in [
            "## Task metrics",
            "## Lanes",
            "## Needs the owner",
            "## Review queue",
            "## Poolrad",
            "## Budget",
            "## Watchdog",
        ] {
            assert!(text.contains(heading), "missing {heading}");
        }
        assert!(text.contains("not measured here"));
    }

    #[test]
    fn report_collect_keeps_measurement_failures_in_their_sections() {
        let temp = tempfile::tempdir().unwrap();
        let mut sources = Sources::new(temp.path());
        sources.metrics = Err("busy".into());
        sources.panes = Err("tmux unavailable".into());
        sources.needs_owner = Err("database locked".into());
        sources.reviews = Err("queue offline".into());
        sources.poolrad = Err("manifest missing".into());
        let report = Report::collect(&sources, Some(1_700_000_000.0)).unwrap();
        let sections = &report["sections"];
        assert_eq!(sections["tasks"]["error"], "task metrics unavailable: busy");
        assert_eq!(
            sections["lanes"]["error"],
            "pane list unavailable: tmux unavailable"
        );
        assert_eq!(
            sections["needs_owner"]["error"],
            "needs-owner query unavailable: database locked"
        );
        assert_eq!(
            sections["reviews"]["error"],
            "review queue unavailable: queue offline"
        );
        assert_eq!(
            sections["poolrad"]["error"],
            "poolrad progress unavailable: manifest missing"
        );
    }

    #[test]
    fn report_collect_reads_real_usage_and_watchdog_cache_files() {
        let temp = tempfile::tempdir().unwrap();
        let sources = Sources::new(temp.path());
        fs::write(
            sources.config_dir.join("usage.json"),
            r#"{"providers":{"zeta":{"spent":12.5,"budget":20,"window":"daily"},"alpha":{"used":3,"limit":9,"window":"monthly"}},"as_of":"now"}"#,
        )
        .unwrap();
        fs::write(
            sources.config_dir.join("watchdog_state.json"),
            r#"{"heartbeat_at":1699999990,"ok":true,"history":[1]}"#,
        )
        .unwrap();
        let report = Report::collect(&sources, Some(1_700_000_000.0)).unwrap();
        assert_eq!(
            report["sections"]["budget"]["providers"][0]["provider"],
            "alpha"
        );
        assert_eq!(report["sections"]["budget"]["providers"][0]["used"], 3);
        assert_eq!(
            report["sections"]["budget"]["providers"][1]["provider"],
            "zeta"
        );
        assert_eq!(report["sections"]["budget"]["providers"][1]["used"], 12.5);
        assert_eq!(report["sections"]["watchdog"]["heartbeat_age_s"], 10);
        assert_eq!(report["sections"]["watchdog"]["state"]["ok"], true);
        assert!(report["sections"]["watchdog"]["state"]
            .get("history")
            .is_none());
    }

    #[test]
    fn report_run_writes_before_posting_and_preserves_file_when_post_fails() {
        let temp = tempfile::tempdir().unwrap();
        let mut sources = Sources::new(temp.path());
        sources.post = Err("owner mailbox unavailable".into());
        let result = Report::run(
            &sources,
            Some(1_700_000_000.0),
            None,
            true,
            Some("t11 is waiting on the owner."),
        )
        .unwrap();
        assert!(result.path.is_file());
        assert_eq!(fs::read_to_string(&result.path).unwrap(), result.text);
        assert_eq!(
            result.posted,
            Some(json!({"error": "owner mailbox unavailable"}))
        );
        assert!(result.prose_rejected.is_empty());
        assert!(result.text.contains("## In plain language"));
        assert_eq!(sources.posted.lock().unwrap().len(), 1);
    }

    #[test]
    fn report_run_rejects_unsubstantiated_prose_without_appending_it() {
        let temp = tempfile::tempdir().unwrap();
        let sources = Sources::new(temp.path());
        let result = Report::run(
            &sources,
            Some(1_700_000_000.0),
            None,
            false,
            Some("t999 is done."),
        )
        .unwrap();
        assert_eq!(result.prose_rejected.len(), 2);
        assert!(!result.text.contains("## In plain language"));
        assert!(result.posted.is_none());
    }

    #[test]
    fn report_list_returns_markdown_newest_first() {
        let temp = tempfile::tempdir().unwrap();
        let sources = Sources::new(temp.path());
        let first = Report::run(&sources, Some(1_700_000_000.0), None, false, None).unwrap();
        let second = Report::run(&sources, Some(1_700_000_000.0), None, false, None).unwrap();
        let older = first.path;
        let newer = second.path;
        set_modified(&older, 100);
        set_modified(&newer, 200);
        fs::write(sources.paths.output_dir.join("ignored.txt"), "not a report").unwrap();
        let listing = Report::list(&sources, None).unwrap();
        assert_eq!(listing["count"], 2);
        assert_eq!(
            listing["reports"][0]["path"],
            newer.to_string_lossy().as_ref()
        );
        assert_eq!(
            listing["reports"][1]["path"],
            older.to_string_lossy().as_ref()
        );
    }

    fn set_modified(path: &Path, seconds: u64) {
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(seconds);
        fs::File::open(path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
    }
}
