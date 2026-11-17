//! Provider projection/cache regressions retained when the Operations board was retired.
use chrono::{DateTime, TimeZone, Utc};
use ds_web::usage_runtime::{project, Dispatch, Subscription, UsageCache, UsageReader, PROVIDERS};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};
/// Provider readings the test sets per provider. An unset provider fails like a missing command.
#[derive(Default)]
struct Usage {
    readings: Mutex<HashMap<String, Result<Value, String>>>,
    calls: Mutex<Vec<String>>,
}

impl Usage {
    fn set(&self, provider: &str, reading: Result<Value, String>) {
        self.readings
            .lock()
            .unwrap()
            .insert(provider.to_owned(), reading);
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

impl UsageReader for Usage {
    fn read(&self, provider: &str) -> Result<Value, String> {
        self.calls.lock().unwrap().push(provider.to_owned());
        self.readings
            .lock()
            .unwrap()
            .get(provider)
            .cloned()
            .unwrap_or_else(|| Err("no reading configured".to_owned()))
    }
}

#[test]
fn provider_cadence_backoff_and_last_good_follow_the_oracle() {
    let reader = Arc::new(Usage::default());
    reader.set(
        "claude",
        Ok(
            json!({"provider": "claude", "status": "ok", "meaning": "percent_used",
            "measured_at": 1000.0, "windows": [{"name": "weekly", "percent_used": 10,
            "percent_remaining": 90}]}),
        ),
    );
    reader.set("codex", Err("codex down".to_owned()));
    reader.set("pi", Err("pi down".to_owned()));
    reader.set("opencode-go", Err("oc down".to_owned()));
    let clock = Arc::new(Mutex::new(1000.0_f64));
    let source = Arc::clone(&clock);
    let cache = UsageCache::new(
        1800,
        vec![300, 600, 1800],
        reader.clone(),
        Arc::new(move || *source.lock().unwrap()),
        Dispatch::Inline,
    );
    let row = |rows: &[Value], name: &str| -> Value {
        rows.iter().find(|r| r["provider"] == name).unwrap().clone()
    };

    // First read: every provider is due, and each row carries its next attempt.
    let rows = cache.snapshot().unwrap();
    assert_eq!(reader.call_count(), 4);
    assert_eq!(row(&rows, "claude")["next_attempt_at"], 2800.0);
    assert_eq!(
        row(&rows, "codex")["next_attempt_at"],
        1300.0,
        "first failure backs off 300 s"
    );

    // Before any provider is due again, nothing is read.
    *clock.lock().unwrap() = 1100.0;
    reader.set("claude", Err("HTTP 500".to_owned()));
    cache.snapshot().unwrap();
    assert_eq!(reader.call_count(), 4, "no provider is due at 1100 s");

    // At 1300 s the failing providers are due; claude is not, so its good row stands.
    *clock.lock().unwrap() = 1300.0;
    let rows = cache.snapshot().unwrap();
    assert_eq!(reader.call_count(), 7);
    assert_eq!(
        row(&rows, "codex")["next_attempt_at"],
        1900.0,
        "second failure backs off 600 s"
    );
    assert_eq!(row(&rows, "claude")["status"], "ok");

    // At 2800 s claude is due and fails: its last good reading is kept beside the error.
    *clock.lock().unwrap() = 2800.0;
    let rows = cache.snapshot().unwrap();
    let claude = row(&rows, "claude");
    assert_eq!(claude["status"], "error");
    assert_eq!(claude["last_good"]["status"], "ok");
    assert_eq!(claude["next_attempt_at"], 3100.0);
}

#[test]
fn budget_projection_spells_prices_and_windows_like_python() {
    let now = DateTime::<Utc>::from_timestamp_micros(
        Utc.with_ymd_and_hms(2026, 11, 5, 12, 0, 0)
            .unwrap()
            .timestamp_micros()
            + 123_456,
    )
    .unwrap();
    let subscriptions = vec![
        Subscription {
            provider: "claude".into(),
            monthly_usd_cents: 2050,
            observed_at: "2026-11-01T00:00:00Z".into(),
        },
        Subscription {
            provider: "pi".into(),
            monthly_usd_cents: 2000,
            observed_at: "2026-11-01T00:00:00Z".into(),
        },
    ];
    let readings = vec![
        json!({"provider": "codex", "status": "ok", "meaning": "percent_used",
            "measured_at": "2026-11-05T11:59:00Z",
            "last_snapshot": {"percent_used": 37, "window_minutes": 300, "name": "5h"}}),
        json!({"provider": "pi", "status": "ok", "meaning": "percent_used",
            "measured_at": "2026-11-05T11:59:00Z",
            "windows": [{"name": "old", "percent_used": 5, "percent_remaining": 95,
                         "resets_at": "2026-11-05T11:00:00Z"}]}),
    ];
    let budget = project(&readings, &subscriptions, now, None, 3600).unwrap();

    assert_eq!(
        budget["generated_at"], "2026-11-05T12:00:00.123456Z",
        "microseconds show"
    );
    assert_eq!(budget["state"], "ok");
    assert_eq!(budget["dollars_remaining"], Value::Null);

    let providers = budget["providers"].as_array().unwrap();
    // Readings keep their order; a subscription with no reading comes last.
    assert_eq!(providers[0]["provider"], "codex");
    assert_eq!(providers[0]["state"], "ok");
    assert_eq!(providers[0]["windows"][0]["name"], "5h");
    assert_eq!(
        providers[0]["windows"][0]["percent_remaining"].to_string(),
        "63"
    );
    assert_eq!(providers[1]["provider"], "pi");
    assert_eq!(
        providers[1]["windows"][0]["state"], "expired",
        "a past reset is expired"
    );
    assert_eq!(
        providers[1]["subscription"]["monthly_usd"].to_string(),
        "20.0"
    );
    assert_eq!(providers[2]["provider"], "claude");
    assert_eq!(providers[2]["state"], "unavailable");
    assert_eq!(
        providers[2]["subscription"]["monthly_usd"].to_string(),
        "20.5"
    );
    assert_eq!(providers[2]["reason"], "no provider reading");

    // A cache-level failure is reported on the budget and degrades every provider row.
    let failed = project(&[], &subscriptions, now, Some("usage unavailable"), 3600).unwrap();
    assert_eq!(failed["state"], "unavailable");
    assert_eq!(failed["providers"][0]["reason"], "usage unavailable");
}

struct SlowUsage {
    finished: Arc<AtomicBool>,
}

impl UsageReader for SlowUsage {
    fn read(&self, _provider: &str) -> Result<Value, String> {
        thread::sleep(Duration::from_millis(150));
        self.finished.store(true, Ordering::SeqCst);
        Err("slow read finished".to_owned())
    }
}

#[test]
fn shutdown_waits_for_the_usage_workers_it_started() {
    let finished = Arc::new(AtomicBool::new(false));
    let cache = UsageCache::new(
        1800,
        vec![300],
        Arc::new(SlowUsage {
            finished: Arc::clone(&finished),
        }),
        Arc::new(|| 1000.0),
        Dispatch::Threads,
    );
    let rows = cache.snapshot().unwrap();
    assert_eq!(rows.len(), PROVIDERS.len());
    cache.stop();
    assert!(
        finished.load(Ordering::SeqCst),
        "stop returns only after the provider workers have joined"
    );
}
