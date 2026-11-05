//! The read-only `metrics` API handler, composed from Flock's attached stores.

use super::{Args, HandlerContext};
use crate::error::DunkError;
use serde_json::{json, Map, Value};

fn definitions(rows: &[(&str, &str)]) -> Value {
    Value::Object(
        rows.iter()
            .map(|(name, description)| {
                (
                    (*name).to_string(),
                    Value::String((*description).to_string()),
                )
            })
            .collect(),
    )
}

fn selected_dunks(snapshot: &Value) -> Value {
    Value::Array(
        snapshot
            .get("dunkers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|dunk| {
                let field = |name: &str| dunk.get(name).cloned().unwrap_or(Value::Null);
                json!({
                    "id": field("id"),
                    "target_pane_id": field("target_pane_id"),
                    "running": field("running"),
                    "interval_minutes": field("interval_minutes"),
                    "send_count": field("send_count"),
                    "skip_count": field("skip_count"),
                    "total_skips": field("total_skips"),
                    "hold_count": field("hold_count"),
                    "created_at": field("created_at"),
                })
            })
            .collect(),
    )
}

fn queued_created_at(snapshot: &Value) -> Vec<f64> {
    snapshot
        .get("outbox")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|message| message.get("status").and_then(Value::as_str) == Some("queued"))
        .filter_map(|message| message.get("created_at").and_then(Value::as_f64))
        .collect()
}

fn round_seconds(value: f64) -> f64 {
    // Formatting to a fixed decimal precision rounds the exact binary value, like Python's
    // round(value, 3), rather than using f64::round's half-away-from-zero rule.
    format!("{value:.3}").parse().unwrap_or(value)
}

/// `dunk_api.h_metrics` / `Flock.metrics_summary`.
pub fn h_metrics(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    // The Python API handler converts this argument before entering
    // Flock.metrics_summary, even when `since` is truthy and the window is
    // not used to calculate the start.
    let window = match args.get("window_s") {
        None | Some(Value::Null) => 3600.0,
        Some(value) => super::python_float(value)?,
    };
    let now = context.env.now_s();
    let end = match args.get("until").filter(|value| super::truthy(value)) {
        Some(value) => super::python_float(value)?,
        None => now,
    };
    let start = match args.get("since").filter(|value| super::truthy(value)) {
        Some(value) => super::python_float(value)?,
        None => end - window.max(1.0),
    };

    let stores = context.flock.stores();
    let metrics = stores
        .metrics
        .ok_or_else(|| DunkError::Refused("metrics store is not wired".into()))?;
    let events = metrics.events_between(start, end)?;
    let summary = crate::metrics::summary(&events);
    let earliest = metrics.earliest()?;

    let missing = [
        ("queue_wait", summary.durations.queue_wait.is_none()),
        ("delivery", summary.durations.delivery.is_none()),
        ("handling", summary.durations.handling.is_none()),
        ("blocked", summary.durations.blocked.is_none()),
    ]
    .into_iter()
    .filter_map(|(name, absent)| absent.then_some(Value::String(name.into())))
    .collect::<Vec<_>>();
    let notes = match earliest {
        None => "no events recorded yet; durable telemetry began with this daemon and records as traffic happens",
        Some(at) if at > start => "the window starts before durable recording did, so it is only covered from coverage.events_since onwards",
        Some(_) => "the window is fully covered by durable recording",
    };

    let flock_snapshot = context.flock.snapshot();
    let queued = queued_created_at(&flock_snapshot);
    let oldest_wait = queued
        .iter()
        .copied()
        .reduce(f64::min)
        .map(|created_at| (now - created_at) as i64);
    let inbox = stores
        .inbox
        .as_ref()
        .ok_or_else(|| DunkError::Refused("inbox store is not wired".into()))?;
    let inbox_stats = crate::inbox::stats(inbox.read_pool(), now)?;
    let unread = inbox_stats.iter().map(|row| row.unread).sum::<i64>();

    let summary = serde_json::to_value(summary)?;
    let mut output = Map::new();
    output.insert(
        "schema_version".into(),
        json!(crate::metrics::SCHEMA_VERSION),
    );
    output.insert("generated_at".into(), json!(now));
    let mut interval = Map::new();
    interval.insert(
        "start".into(),
        crate::github::python_float_json_value(start).unwrap_or(Value::Null),
    );
    interval.insert(
        "end".into(),
        crate::github::python_float_json_value(end).unwrap_or(Value::Null),
    );
    interval.insert(
        "seconds".into(),
        crate::github::python_float_json_value(round_seconds(end - start)).unwrap_or(Value::Null),
    );
    output.insert("interval".into(), Value::Object(interval));
    output.insert(
        "units".into(),
        json!({
            "duration": "milliseconds",
            "size": "bytes",
            "timestamp": "unix_epoch_seconds",
            "rate": "per_hour",
        }),
    );
    output.insert(
        "event_definitions".into(),
        definitions(crate::metrics::KINDS),
    );
    output.insert(
        "outcome_definitions".into(),
        definitions(crate::metrics::OUTCOMES),
    );
    output.insert(
        "reason_definitions".into(),
        definitions(crate::metrics::REASONS),
    );
    output.insert(
        "counts".into(),
        summary.get("counts").cloned().unwrap_or(Value::Null),
    );
    output.insert(
        "durations".into(),
        summary.get("durations").cloned().unwrap_or(Value::Null),
    );
    output.insert(
        "retries".into(),
        summary.get("retries").cloned().unwrap_or(Value::Null),
    );
    output.insert(
        "queue".into(),
        json!({
            "depth_now": queued.len(),
            "oldest_wait_s": oldest_wait,
        }),
    );
    output.insert(
        "inbox".into(),
        json!({
            "unread_now": unread,
            "mailboxes": inbox_stats.len(),
        }),
    );
    output.insert(
        "cumulative".into(),
        json!({
            "note": "counters kept per schedule since it was created; these are not windowed and must not be read as a rate",
            "dunks": selected_dunks(&flock_snapshot),
        }),
    );
    let mut coverage = Map::new();
    coverage.insert(
        "events_since".into(),
        earliest
            .map(|at| {
                crate::github::python_float_json_value(at)
                    .expect("SQLite REAL event timestamp has a numeric carrier")
            })
            .unwrap_or(Value::Null),
    );
    coverage.insert(
        "window_has_events".into(),
        Value::Bool(summary["counts"]["events"] != 0),
    );
    coverage.insert("missing".into(), Value::Array(missing));
    coverage.insert("retention_s".into(), json!(metrics.retention_s));
    coverage.insert("notes".into(), Value::String(notes.to_string()));
    output.insert("coverage".into(), Value::Object(coverage));
    Ok(Value::Object(output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{CallerGate, CommandEnv, Dispatcher};
    use crate::error::DunkError;
    use crate::flock::dunks::Dunker;
    use crate::flock::panes::PaneInfoEnv;
    use crate::flock::Flock;
    use crate::inbox_store::{Outgoing, Store as InboxStore};
    use crate::server::RequestCtx;
    use crate::terminal::{fakes::FakeTerminal, Terminal};
    use serde_json::json;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

    struct MetricsEnv {
        terminal: Mutex<Box<dyn Terminal>>,
        now: f64,
    }

    impl MetricsEnv {
        fn new(now: f64) -> Self {
            Self {
                terminal: Mutex::new(Box::new(FakeTerminal::default())),
                now,
            }
        }
    }

    impl PaneInfoEnv for MetricsEnv {
        fn list_panes_grouped(&self) -> Vec<Value> {
            Vec::new()
        }

        fn get_pane(&self, _pane_id: &str) -> Option<Value> {
            None
        }
    }

    impl CommandEnv for MetricsEnv {
        fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
            &self.terminal
        }

        fn now_s(&self) -> f64 {
            self.now
        }
    }

    struct FixedRoleForMetrics(&'static str);

    impl CallerGate for FixedRoleForMetrics {
        fn caller_role(&self, _pane: Option<&str>) -> Result<String, DunkError> {
            Ok(self.0.to_string())
        }

        fn require_superuser(&self, _pane: Option<&str>, name: &str) -> Result<(), DunkError> {
            Err(DunkError::Refused(format!("{name} needs a superuser")))
        }
    }

    fn dispatch(
        flock: &Flock,
        args: Option<Args>,
        role: &'static str,
        now: f64,
    ) -> Result<Value, DunkError> {
        let env = MetricsEnv::new(now);
        let request = RequestCtx::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        let context = HandlerContext {
            flock,
            request: &request,
            env: &env,
            runtime: runtime.handle(),
        };
        let mut dispatcher = Dispatcher::new();
        dispatcher.register_metrics();
        dispatcher.dispatch(&context, &FixedRoleForMetrics(role), "metrics", args)
    }

    fn args(value: Value) -> Args {
        value.as_object().cloned().expect("object args")
    }

    fn flock(tag: &str) -> (tempfile::TempDir, Flock) {
        let dir = crate::testsupport::dir(tag);
        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        );
        flock.wire_observability(dir.path(), None).unwrap();
        (dir, flock)
    }

    fn attach_inbox(flock: &Flock, dir: &Path) -> Arc<InboxStore> {
        let inbox = Arc::new(InboxStore::open(dir.join("inbox.db"), dir.join("blobs")).unwrap());
        let mut stores = flock.stores();
        stores.inbox = Some(Arc::clone(&inbox));
        flock.attach_stores(stores);
        inbox
    }

    fn seed_dunk(flock: &Flock) {
        let mut dunk = Dunker::new("d7").to_dict();
        dunk.insert("text".into(), json!("PRIVATE DUNK TEXT"));
        dunk.insert("target_pane_id".into(), json!("w1:p2"));
        dunk.insert("send_count".into(), json!(7));
        dunk.insert("skip_count".into(), json!(2));
        dunk.insert("total_skips".into(), json!(4));
        dunk.insert("hold_count".into(), json!(1));
        dunk.insert("created_at".into(), json!(10.0));
        let payload = json!({"next_id": 8, "dunkers": [dunk]});
        flock.dunks.load(payload.as_object().unwrap());
    }

    fn seed_queue(flock: &Flock) {
        flock.restore_outbox(&[json!({
            "id": "m1",
            "pane_id": "w1:p2",
            "target_label": "pane",
            "text": "PRIVATE QUEUED TEXT",
            "source": "send_text",
            "created_at": 1975.0,
        })]);
    }

    #[test]
    fn h_metrics_matches_python() {
        let (dir, flock) = flock("api-metrics-contract");
        let inbox = attach_inbox(&flock, dir.path());
        inbox
            .send(&Outgoing {
                mailbox: "private@example.test",
                body: Some("PRIVATE INBOX BODY"),
                subject: Some("PRIVATE INBOX SUBJECT"),
                sender: Some("private sender"),
                ..Default::default()
            })
            .unwrap();
        seed_dunk(&flock);
        seed_queue(&flock);

        let metrics = flock.stores().metrics.unwrap();
        metrics.record(&crate::metrics::Event {
            at: 1500.0,
            kind: "inbox.read".into(),
            pane_id: Some("w1:p1".into()),
            mailbox: Some("team".into()),
            sender: Some("w1:p2".into()),
            duration_ms: Some(10),
            ..Default::default()
        });
        metrics.record(&crate::metrics::Event {
            at: 1600.0,
            kind: "message.sent".into(),
            source: Some("send_text".into()),
            duration_ms: Some(20),
            ..Default::default()
        });

        let report = dispatch(&flock, None, "worker", 2000.0).unwrap();
        assert_eq!(1, report["schema_version"]);
        assert_eq!(2000.0, report["generated_at"]);
        assert_eq!(
            json!({"start": -1600.0, "end": 2000.0, "seconds": 3600.0}),
            report["interval"]
        );
        assert_eq!("milliseconds", report["units"]["duration"]);
        assert!(report["event_definitions"].is_object());
        assert_eq!(2, report["counts"]["events"]);
        assert_eq!(1, report["counts"]["by_kind"]["inbox.read"]);
        assert_eq!(10, report["durations"]["queue_wait"]["max"]);
        assert_eq!(20, report["durations"]["delivery"]["max"]);
        assert_eq!(Value::Null, report["durations"]["handling"]);
        assert_eq!(1, report["queue"]["depth_now"]);
        assert_eq!(25, report["queue"]["oldest_wait_s"]);
        assert_eq!(1, report["inbox"]["unread_now"]);
        assert_eq!(1, report["inbox"]["mailboxes"]);
        assert_eq!(1, report["cumulative"]["dunks"].as_array().unwrap().len());
        assert_eq!(7, report["cumulative"]["dunks"][0]["send_count"]);
        assert_eq!(4, report["cumulative"]["dunks"][0]["total_skips"]);
        assert_eq!(1500.0, report["coverage"]["events_since"]);
        assert!(report["coverage"]["window_has_events"].as_bool().unwrap());
        assert_eq!(
            json!(["handling", "blocked"]),
            report["coverage"]["missing"]
        );

        let serialized = report.to_string();
        for secret in [
            "PRIVATE DUNK TEXT",
            "PRIVATE QUEUED TEXT",
            "PRIVATE INBOX BODY",
            "PRIVATE INBOX SUBJECT",
            "private@example.test",
            "private sender",
        ] {
            assert!(!serialized.contains(secret), "metrics leaked {secret:?}");
        }
        assert_eq!(
            "metrics",
            crate::registry::command("metrics").unwrap().name,
            "the production API registry must expose this command"
        );
        assert!(dispatch(&flock, None, "anonymous", 2000.0).is_ok());
    }

    #[test]
    fn h_metrics_empty_window_reports_missing_coverage() {
        let (dir, flock) = flock("api-metrics-empty");
        attach_inbox(&flock, dir.path());

        let report = dispatch(&flock, None, "worker", 2000.0).unwrap();
        assert_eq!(0, report["counts"]["events"]);
        assert_eq!(Value::Null, report["coverage"]["events_since"]);
        assert!(!report["coverage"]["window_has_events"].as_bool().unwrap());
        assert_eq!(
            json!(["queue_wait", "delivery", "handling", "blocked"]),
            report["coverage"]["missing"]
        );
        for duration in ["queue_wait", "delivery", "handling", "blocked"] {
            assert_eq!(Value::Null, report["durations"][duration]);
        }
        assert_eq!(
            "no events recorded yet; durable telemetry began with this daemon and records as traffic happens",
            report["coverage"]["notes"]
        );
    }

    #[test]
    fn h_metrics_preserves_truthiness_and_conversion_order() {
        let (dir, flock) = flock("api-metrics-args");
        attach_inbox(&flock, dir.path());

        let report = dispatch(
            &flock,
            Some(args(json!({"window_s": 99, "since": 0, "until": 0}))),
            "worker",
            2000.0,
        )
        .unwrap();
        assert_eq!(
            json!({"start": 1901.0, "end": 2000.0, "seconds": 99.0}),
            report["interval"]
        );

        // The public handler converts window_s before entering Flock, even when
        // a truthy `since` means the resulting duration is not used.
        let explicit_invalid_window = dispatch(
            &flock,
            Some(args(json!({"since": 1000, "until": 1200, "window_s": []}))),
            "worker",
            2000.0,
        )
        .unwrap_err()
        .to_string();
        assert!(explicit_invalid_window
            .contains("float() argument must be a string or a real number, not 'list'"));

        let minimum = dispatch(
            &flock,
            Some(args(json!({"window_s": -5}))),
            "worker",
            2000.0,
        )
        .unwrap();
        assert_eq!(
            json!({"start": 1999.0, "end": 2000.0, "seconds": 1.0}),
            minimum["interval"]
        );

        let missing_metrics = Flock::new(
            dir.path().join("bare-launches"),
            dir.path().join("bare-lanes"),
            false,
        );
        let invalid_window_precedes_endpoint_args = dispatch(
            &missing_metrics,
            Some(args(json!({"window_s": [], "until": {}, "since": []}))),
            "worker",
            2000.0,
        )
        .unwrap_err()
        .to_string();
        assert!(invalid_window_precedes_endpoint_args
            .contains("float() argument must be a string or a real number, not 'list'"));

        let invalid_until_precedes_since = dispatch(
            &missing_metrics,
            Some(args(json!({
                "window_s": 3600,
                "until": {"invalid": true},
                "since": []
            }))),
            "worker",
            2000.0,
        )
        .unwrap_err()
        .to_string();
        assert!(invalid_until_precedes_since
            .contains("float() argument must be a string or a real number, not 'dict'"));
    }

    #[test]
    fn h_metrics_preserves_signed_infinity_interval_carriers() {
        let (dir, flock) = flock("api-metrics-infinity");
        attach_inbox(&flock, dir.path());

        let positive =
            args(serde_json::from_str(r#"{"since":1e999,"until":2000,"window_s":3600}"#).unwrap());
        let positive = dispatch(&flock, Some(positive), "worker", 2000.0).unwrap();
        assert_eq!("1e+999", positive["interval"]["start"].to_string());
        assert_eq!("-1e+999", positive["interval"]["seconds"].to_string());

        let negative =
            args(serde_json::from_str(r#"{"since":-1e999,"until":2000,"window_s":3600}"#).unwrap());
        let negative = dispatch(&flock, Some(negative), "worker", 2000.0).unwrap();
        assert_eq!("-1e+999", negative["interval"]["start"].to_string());
        assert_eq!("1e+999", negative["interval"]["seconds"].to_string());
    }

    #[test]
    fn h_metrics_derived_nan_survives_common_wire_codec_without_mutation() {
        let (dir, flock) = flock("api-metrics-derived-nan");
        attach_inbox(&flock, dir.path());
        let before = flock.snapshot();
        let input = crate::wire_json::from_str(r#"{"until":Infinity}"#).unwrap();
        let report = dispatch(&flock, Some(args(input)), "anonymous", 2000.0).unwrap();
        assert_eq!(flock.snapshot(), before);
        assert!(report["interval"]["seconds"].is_number());
        assert_eq!(
            crate::api::python_value_string(&report["interval"]["seconds"]),
            "nan"
        );
        let wire = crate::protocol::encode_line(&crate::protocol::Response::ok(json!(1), report));
        assert!(
            wire.contains(r#""interval":{"start":Infinity,"end":Infinity,"seconds":NaN}"#),
            "{wire}"
        );
        let decoded = crate::wire_json::from_str(&wire).unwrap();
        assert!(decoded["result"]["interval"]["seconds"].is_number());
    }

    #[test]
    fn h_metrics_coverage_infinite_timestamp_survives_common_wire_codec() {
        let dir = crate::testsupport::dir("api-metrics-coverage-infinity");
        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            true,
        );
        flock.wire_observability(dir.path(), None).unwrap();
        attach_inbox(&flock, dir.path());
        let metrics = flock.stores().metrics.unwrap();
        metrics.record(&crate::metrics::Event {
            at: f64::INFINITY,
            kind: "test.event".into(),
            ..Default::default()
        });
        assert_eq!(Some(f64::INFINITY), metrics.earliest().unwrap());

        let count = || {
            rusqlite::Connection::open(dir.path().join("metrics.db"))
                .unwrap()
                .query_row("SELECT COUNT(*) FROM events", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap()
        };
        let before = count();
        let report = dispatch(&flock, None, "anonymous", 2000.0).unwrap();
        assert!(report["coverage"]["events_since"].is_number());
        assert_eq!(
            crate::api::python_value_string(&report["coverage"]["events_since"]),
            "inf"
        );
        let wire = crate::protocol::encode_line(&crate::protocol::Response::ok(json!(1), report));
        assert!(wire.contains(r#""events_since":Infinity"#), "{wire}");
        let decoded = crate::wire_json::from_str(&wire).unwrap();
        assert!(decoded["result"]["coverage"]["events_since"].is_number());
        assert_eq!(count(), before, "metrics summary must be read-only");
    }

    #[test]
    fn h_metrics_refuses_missing_stores_and_preserves_read_only_role_access() {
        let (dir, flock) = flock("api-metrics-missing");
        let missing_inbox = dispatch(&flock, None, "worker", 2000.0)
            .unwrap_err()
            .to_string();
        assert_eq!("inbox store is not wired", missing_inbox);

        let bare = Flock::new(
            dir.path().join("bare-launches"),
            dir.path().join("bare-lanes"),
            false,
        );
        let missing_metrics = dispatch(&bare, None, "worker", 2000.0)
            .unwrap_err()
            .to_string();
        assert_eq!("metrics store is not wired", missing_metrics);

        // Unknown roles receive READ_ONLY commands in the Python/Rust dispatcher; metrics is
        // read-only, so role authorization succeeds and the missing-store refusal is next.
        let unknown_role = dispatch(&bare, None, "not-a-role", 2000.0)
            .unwrap_err()
            .to_string();
        assert_eq!("metrics store is not wired", unknown_role);
    }
}
