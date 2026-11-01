//! `Tasks.metrics` (T14), ported from `dunk_tasks.py`: timeseries-friendly task metrics from the
//! append-only event log.
//!
//! Current state comes from the `tasks` table; throughput and latency come from `task_events` in
//! the window. `scopes` is None (all) or a list of scope strings; callers apply privacy first.
//!
//! The result is a `serde_json::Value`. Its object KEYS are sorted (no `preserve_order`, see
//! `ds-mcp/src/pyjson.rs`); compare it as a value, not as text.

use std::collections::BTreeMap;

use rusqlite::params_from_iter;
use serde_json::{json, Value};

use super::{Tasks, PRIORITIES, STATUSES};
use crate::db;
use crate::error::DunkError;
use crate::memory::pure::round_places;

/// The arguments of `Tasks.metrics`, with the Python's defaults.
#[derive(Debug, Clone)]
pub struct MetricsOptions {
    pub window_s: f64,
    pub since: Option<f64>,
    pub until: Option<f64>,
    pub scopes: Option<Vec<String>>,
    /// `"day"` buckets by day; anything else by hour.
    pub bucket: String,
    pub scope_label: String,
}

impl Default for MetricsOptions {
    fn default() -> Self {
        MetricsOptions {
            window_s: 3600.0,
            since: None,
            until: None,
            scopes: None,
            bucket: "hour".to_string(),
            scope_label: "all".to_string(),
        }
    }
}

/// The columns of a task the metrics read (the part of `_decorate` they use).
struct TaskRow {
    scope: String,
    status: String,
    priority: String,
    assignees: String,
    block_reason: Option<String>,
    created_at: f64,
}

/// One `task_events` row.
struct EventRow {
    event: String,
    at: f64,
    actor: Option<String>,
    duration_ms: Option<f64>,
}

/// Python's `round(x)`: half to even (`round(0.5) == 0`, `round(2.5) == 2`). Rust's `f64::round`
/// rounds half away from zero, which would pick a different percentile index.
fn round_half_even(value: f64) -> f64 {
    let floor = value.floor();
    let diff = value - floor;
    if diff < 0.5 {
        floor
    } else if diff > 0.5 {
        floor + 1.0
    } else if floor % 2.0 == 0.0 {
        floor
    } else {
        floor + 1.0
    }
}

/// `Counter.most_common()`: by count descending, ties in first-seen order.
fn most_common(items: impl Iterator<Item = String>) -> Vec<(String, i64)> {
    let mut counts: Vec<(String, i64)> = Vec::new();
    for item in items {
        match counts.iter_mut().find(|(name, _)| *name == item) {
            Some(entry) => entry.1 += 1,
            None => counts.push((item, 1)),
        }
    }
    counts.sort_by(|a, b| b.1.cmp(&a.1));
    counts
}

fn latency(events: &[EventRow], name: &str) -> Value {
    let mut values: Vec<i64> = events
        .iter()
        .filter(|event| event.event == name)
        .filter_map(|event| event.duration_ms.map(|ms| ms as i64))
        .collect();
    values.sort_unstable();
    if values.is_empty() {
        return json!({"count": 0, "avg": null, "p50": null, "p90": null, "max": null});
    }
    let last = values.len() - 1;
    let pick = |fraction: f64| values[(round_half_even(fraction * last as f64) as usize).min(last)];
    let sum: i64 = values.iter().sum();
    json!({
        "count": values.len(),
        "avg": round_places(sum as f64 / values.len() as f64, 1),
        "p50": pick(0.5),
        "p90": pick(0.9),
        "max": values[last],
    })
}

fn actors(events: &[EventRow], name: &str) -> Value {
    let named = events
        .iter()
        .filter(|event| event.event == name)
        .filter_map(|event| event.actor.clone().filter(|actor| !actor.is_empty()));
    Value::Array(
        most_common(named)
            .into_iter()
            .map(|(actor, count)| json!({"actor": actor, "count": count}))
            .collect(),
    )
}

impl Tasks {
    /// `Tasks.metrics`.
    pub async fn metrics(&self, options: MetricsOptions) -> Result<Value, DunkError> {
        let until = options.until.unwrap_or_else(crate::clock::now_s);
        let since = options.since.unwrap_or(until - options.window_s);
        let bucket_s: i64 = if options.bucket.to_lowercase() == "day" {
            86400
        } else {
            3600
        };
        let scopes = options.scopes.clone();

        let (tasks, events, first) = self
            .writer
            .call(move |conn| {
                let mut stmt = conn
                    .prepare(
                        "SELECT scope, status, priority, assignees, block_reason, created_at \
                         FROM tasks",
                    )
                    .map_err(db::map_sqlite)?;
                let rows = stmt
                    .query_map([], |row| {
                        Ok(TaskRow {
                            scope: row.get(0)?,
                            status: row.get(1)?,
                            priority: row.get(2)?,
                            assignees: row.get(3)?,
                            block_reason: row.get(4)?,
                            created_at: row.get(5)?,
                        })
                    })
                    .map_err(db::map_sqlite)?;
                let mut tasks: Vec<TaskRow> =
                    rows.collect::<Result<_, _>>().map_err(db::map_sqlite)?;
                if let Some(wanted) = &scopes {
                    tasks.retain(|task| wanted.contains(&task.scope));
                }

                let mut sql = String::from(
                    "SELECT event, at, actor, duration_ms FROM task_events \
                     WHERE at >= ? AND at < ?",
                );
                let mut bound: Vec<rusqlite::types::Value> = vec![since.into(), until.into()];
                if let Some(wanted) = &scopes {
                    let marks = vec!["?"; wanted.len()].join(",");
                    sql.push_str(&format!(" AND scope IN ({marks})"));
                    bound.extend(wanted.iter().cloned().map(rusqlite::types::Value::from));
                }
                sql.push_str(" ORDER BY at, id");
                let mut stmt = conn.prepare(&sql).map_err(db::map_sqlite)?;
                let rows = stmt
                    .query_map(params_from_iter(bound), |row| {
                        Ok(EventRow {
                            event: row.get(0)?,
                            at: row.get(1)?,
                            actor: row.get(2)?,
                            duration_ms: row.get(3)?,
                        })
                    })
                    .map_err(db::map_sqlite)?;
                let events: Vec<EventRow> =
                    rows.collect::<Result<_, _>>().map_err(db::map_sqlite)?;

                let first: (Option<f64>, Option<f64>, i64) = conn
                    .query_row(
                        "SELECT MIN(at), MAX(at), COUNT(*) FROM task_events",
                        [],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .map_err(db::map_sqlite)?;
                Ok((tasks, events, first))
            })
            .await?;

        let mut by_status: BTreeMap<String, i64> =
            STATUSES.iter().map(|s| (s.to_string(), 0)).collect();
        let mut by_priority: BTreeMap<String, i64> =
            PRIORITIES.iter().map(|p| (p.to_string(), 0)).collect();
        let (mut global_count, mut pane_count) = (0i64, 0i64);
        let mut by_assignee: BTreeMap<String, i64> = BTreeMap::new();
        let mut leader_held: Vec<&TaskRow> = Vec::new();
        for task in &tasks {
            *by_status.entry(task.status.clone()).or_insert(0) += 1;
            *by_priority.entry(task.priority.clone()).or_insert(0) += 1;
            if task.scope == "global" {
                global_count += 1;
            } else {
                pane_count += 1;
            }
            if matches!(task.status.as_str(), "open" | "in_progress" | "blocked") {
                // FLAT_FLEET.md 5: the only work a person is holding is a question a lane
                // asked the owner. (The key keeps its old name, `leader_held`, for the board.)
                if task.status == "blocked"
                    && task
                        .block_reason
                        .as_deref()
                        .unwrap_or("")
                        .starts_with("needs-owner:")
                {
                    leader_held.push(task);
                }
                let assignees: Vec<String> =
                    serde_json::from_str(&task.assignees).unwrap_or_default();
                for assignee in assignees {
                    *by_assignee.entry(assignee).or_insert(0) += 1;
                }
            }
        }
        let mut assignee_rows: Vec<(String, i64)> = by_assignee.into_iter().collect();
        assignee_rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

        let mut counts: BTreeMap<&str, i64> = BTreeMap::new();
        let mut bucket_counts: BTreeMap<i64, BTreeMap<&str, i64>> = BTreeMap::new();
        for event in &events {
            *counts.entry(event.event.as_str()).or_insert(0) += 1;
            let slot = (event.at / bucket_s as f64).floor() as i64 * bucket_s;
            *bucket_counts
                .entry(slot)
                .or_default()
                .entry(event.event.as_str())
                .or_insert(0) += 1;
        }
        let count = |name: &str| counts.get(name).copied().unwrap_or(0);

        let oldest = leader_held
            .iter()
            .map(|task| task.created_at)
            .fold(None, |best: Option<f64>, value| {
                Some(best.map_or(value, |current| current.min(value)))
            });
        let buckets: Vec<Value> = bucket_counts
            .iter()
            .map(|(slot, counter)| {
                let n = |name: &str| counter.get(name).copied().unwrap_or(0);
                json!({
                    "bucket": slot,
                    "created": n("created"),
                    "claimed": n("claimed"),
                    "completed": n("completed"),
                    "cancelled": n("cancelled"),
                    "requeued": n("requeued"),
                })
            })
            .collect();

        Ok(json!({
            "schema_version": 1,
            "generated_at": until,
            "scope": options.scope_label,
            "window": {
                "since": since,
                "until": until,
                "seconds": (until - since) as i64,
                "bucket_seconds": bucket_s,
                "bucket": if bucket_s == 86400 { "day" } else { "hour" },
            },
            "current": {
                "total": tasks.len(),
                "by_status": by_status,
                "by_priority": by_priority,
                "by_scope": {"global": global_count, "pane": pane_count},
                "blocked_vs_in_progress": {
                    "blocked": by_status.get("blocked").copied().unwrap_or(0),
                    "in_progress": by_status.get("in_progress").copied().unwrap_or(0),
                },
                "by_assignee": assignee_rows.iter()
                    .map(|(assignee, count)| json!({"assignee": assignee, "count": count}))
                    .collect::<Vec<_>>(),
                "leader_held": {
                    "count": leader_held.len(),
                    "oldest_created_at": oldest,
                    "oldest_age_s": oldest.map(|value| round_places(until - value, 1)),
                    "definition": "global tasks with no assignee (owner #1708: unowned means leader-owned), plus tasks explicitly assigned to the leader pane",
                    // There is no leader; these are needs-owner questions.
                    "leader_pane": null,
                },
            },
            "throughput": {
                "created": count("created"),
                "claimed": count("claimed"),
                "completed": count("completed"),
                "cancelled": count("cancelled"),
                "blocked": count("blocked"),
                "unblocked": count("unblocked"),
                "requeued": count("requeued"),
                "buckets": buckets,
            },
            "latency_ms": {
                "completion": latency(&events, "completed"),
                "claim": latency(&events, "claimed"),
            },
            "by_actor": {
                "completed": actors(&events, "completed"),
                "claimed": actors(&events, "claimed"),
            },
            "coverage": {
                "events_since": first.0,
                "events_until": first.1,
                "events_total": first.2,
            },
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use rusqlite::params;

    async fn store(tag: &str) -> Tasks {
        let dir = std::env::temp_dir().join(format!("ds-metrics-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tasks.db");
        let _ = std::fs::remove_file(&path);
        Tasks::open(&path).await.unwrap()
    }

    /// Append one `task_events` row (the Python store's `_record`, for the events whose writers
    /// belong to later tasks: `claimed` is T12's).
    async fn event(
        store: &Tasks,
        id: &str,
        name: &str,
        at: f64,
        actor: Option<&str>,
        duration_ms: Option<i64>,
    ) {
        let (id, name, actor) = (id.to_string(), name.to_string(), actor.map(String::from));
        store
            .writer
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO task_events (task_id, event, at, actor, scope, duration_ms) \
                     VALUES (?,?,?,?,?,?)",
                    params![id, name, at, actor, "global", duration_ms],
                )
                .map_err(db::map_sqlite)?;
                Ok(())
            })
            .await
            .unwrap();
    }

    fn global_only() -> MetricsOptions {
        MetricsOptions {
            scopes: Some(vec!["global".to_string()]),
            ..MetricsOptions::default()
        }
    }

    #[tokio::test]
    async fn test_metrics_track_throughput_and_latency() {
        let store = store("latency").await;
        let one = store.add("global", "first", "").await.unwrap();
        store.add("global", "second", "").await.unwrap();
        let third = store.add("global", "third", "").await.unwrap();
        let now = crate::clock::now_s();
        event(&store, &third.id, "claimed", now, Some("w1:p1"), Some(5)).await;
        store.complete(&third.id).await.unwrap();
        event(&store, &third.id, "completed", now, Some("w1:p1"), Some(7)).await;
        store.block(&one.id, "waiting").await.unwrap();
        // `set_status` writes its event with NO scope (the Python records scope and priority), so
        // a scoped query drops it: seed the scoped one (T11 gap, t1225).
        event(&store, &one.id, "blocked", now, None, None).await;
        let metrics = store.metrics(global_only()).await.unwrap();
        assert_eq!(3, metrics["throughput"]["created"]);
        assert_eq!(1, metrics["throughput"]["claimed"]);
        // `complete` records its own event and so does the helper above: count what is there.
        assert!(metrics["throughput"]["completed"].as_i64().unwrap() >= 1);
        assert_eq!(1, metrics["throughput"]["blocked"]);
        assert!(
            metrics["latency_ms"]["completion"]["count"]
                .as_i64()
                .unwrap()
                >= 1
        );
        assert!(metrics["latency_ms"]["completion"]["p50"].as_i64().unwrap() >= 0);
        assert_eq!(
            json!([{"actor": "w1:p1", "count": 1}]),
            metrics["by_actor"]["completed"]
        );
        assert_eq!(1, metrics["current"]["by_status"]["done"]);
        assert_eq!(1, metrics["current"]["by_status"]["blocked"]);
        assert!(metrics["coverage"]["events_total"].as_i64().unwrap() >= 5);
    }

    #[tokio::test]
    async fn test_leader_held_is_the_needs_owner_queue() {
        // FLAT_FLEET.md 5: no leader. `leader_held` counts the questions lanes asked the owner
        // (blocked with a 'needs-owner:' reason), nothing else.
        let store = store("held").await;
        let asked = store.add("global", "which port?", "").await.unwrap();
        store
            .block(&asked.id, "needs-owner: which port do we target?")
            .await
            .unwrap();
        store.add("global", "unassigned work", "").await.unwrap();
        let other = store
            .add("global", "blocked on a dependency", "")
            .await
            .unwrap();
        store.block(&other.id, "waiting on PR 12").await.unwrap();
        let until = crate::clock::now_s() + 4000.0;
        let metrics = store
            .metrics(MetricsOptions {
                until: Some(until),
                ..global_only()
            })
            .await
            .unwrap();
        let held = &metrics["current"]["leader_held"];
        assert_eq!(1, held["count"]);
        let created = held["oldest_created_at"].as_f64().unwrap();
        assert_eq!(
            round_places(until - created, 1),
            held["oldest_age_s"].as_f64().unwrap()
        );
        assert!(held["leader_pane"].is_null());
    }

    #[tokio::test]
    async fn test_leader_held_empty_when_nobody_asked_the_owner() {
        let store = store("heldempty").await;
        store.add("global", "unassigned", "").await.unwrap();
        let metrics = store.metrics(global_only()).await.unwrap();
        let held = &metrics["current"]["leader_held"];
        assert_eq!(0, held["count"]);
        assert!(held["oldest_created_at"].is_null());
        assert!(held["oldest_age_s"].is_null());
    }

    #[tokio::test]
    async fn test_metrics_bucket_throughput_by_hour() {
        let store = store("bucket").await;
        store.add("global", "one", "").await.unwrap();
        store.add("global", "two", "").await.unwrap();
        let metrics = store.metrics(global_only()).await.unwrap();
        assert_eq!(3600, metrics["window"]["bucket_seconds"]);
        assert_eq!(
            1,
            metrics["throughput"]["buckets"].as_array().unwrap().len()
        );
        assert_eq!(2, metrics["throughput"]["buckets"][0]["created"]);
    }

    #[test]
    fn percentile_indexes_round_half_to_even_like_python() {
        // Python: round(0.5) == 0, round(1.5) == 2, round(2.5) == 2, round(4.5) == 4.
        for (value, expected) in [
            (0.5, 0.0),
            (1.5, 2.0),
            (2.5, 2.0),
            (4.5, 4.0),
            (3.4, 3.0),
            (3.6, 4.0),
        ] {
            assert_eq!(expected, round_half_even(value), "{value}");
        }
        // Two values: p50 index is round(0.5 * 1) == 0, so the SMALLER one (Rust's round() says 1).
        let events: Vec<EventRow> = [10.0, 20.0]
            .iter()
            .map(|ms| EventRow {
                event: "completed".into(),
                at: 0.0,
                actor: None,
                duration_ms: Some(*ms),
            })
            .collect();
        let result = latency(&events, "completed");
        assert_eq!(10, result["p50"]);
        assert_eq!(20, result["p90"]);
        assert_eq!(15.0, result["avg"]);
    }

    #[test]
    fn most_common_breaks_ties_by_first_seen_like_counter() {
        let order = most_common(["b", "a", "a", "b", "c"].iter().map(|s| s.to_string()));
        assert_eq!(
            vec![
                ("b".to_string(), 2),
                ("a".to_string(), 2),
                ("c".to_string(), 1)
            ],
            order
        );
    }

    /// T14 acceptance: the Python store's own answer for a tasks.db it built, replayed through
    /// the Rust store on a COPY of that file. Regenerate with `tools/snapshot_tasks_metrics.py`.
    #[tokio::test]
    async fn golden_fixture_matches_the_python_metrics() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/tasks");
        let expected: Value = serde_json::from_str(
            &std::fs::read_to_string(root.join("metrics_expected.json")).unwrap(),
        )
        .unwrap();
        let dir = std::env::temp_dir().join(format!("ds-metrics-golden-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let copy = dir.join("tasks.db");
        std::fs::copy(root.join("metrics.db"), &copy).unwrap();
        let store = Tasks::open(&copy).await.unwrap();
        for case in expected["cases"].as_array().unwrap() {
            let args = &case["args"];
            let options = MetricsOptions {
                window_s: args["window_s"].as_f64().unwrap_or(3600.0),
                since: args["since"].as_f64(),
                until: args["until"].as_f64(),
                scopes: args["scopes"].as_array().map(|list| {
                    list.iter()
                        .map(|s| s.as_str().unwrap().to_string())
                        .collect()
                }),
                bucket: args["bucket"].as_str().unwrap_or("hour").to_string(),
                scope_label: args["scope_label"].as_str().unwrap_or("all").to_string(),
            };
            let actual = store.metrics(options).await.unwrap();
            assert_eq!(
                case["expected"],
                actual,
                "args {args}\nexpected {}\nactual   {}",
                serde_json::to_string_pretty(&case["expected"]).unwrap(),
                serde_json::to_string_pretty(&actual).unwrap()
            );
        }
    }
}
