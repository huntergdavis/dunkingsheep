//! Shared provider readings, cadence and backoff. Independent of board aggregation and SQL.
use crate::ops_collector::{fromtimestamp_utc, parse_time, py_str};
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use ds_core::usage::{load_config, py_float_repr, run as run_usage};
use serde_json::{json, Map, Value};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
    thread::{self, JoinHandle},
};
type Obj = Map<String, Value>;
type PyError = &'static str;
pub type Clock = Arc<dyn Fn() -> f64 + Send + Sync>;
static NULL: Value = Value::Null;
const SOURCE: &str = "dunkingsheep --json usage";
pub const PROVIDERS: [&str; 4] = ["claude", "codex", "pi", "opencode-go"];
const DOLLARS_REASON: &str = "Dollars remaining and per-agent dollar balances are unknown. Subscriptions are fixed prices; allowance percentages are shared provider limits.";
/// One provider's reading: the CLI row for `provider`, or the text of the failure.
pub trait UsageReader: Send + Sync {
    fn read(&self, provider: &str) -> Result<Value, String>;
}

/// Production reader: the `ds_core` usage runner over the configured `usage.json`.
#[derive(Debug, Clone)]
pub struct CoreUsageReader {
    pub config_path: PathBuf,
    pub max_age_s: i64,
}

impl UsageReader for CoreUsageReader {
    fn read(&self, provider: &str) -> Result<Value, String> {
        let path = self.config_path.to_string_lossy();
        let config = load_config(&path).map_err(|error| error.to_string())?;
        let rows = run_usage(&config, Some(provider), Some(self.max_age_s as f64))
            .map_err(|error| error.to_string())?;
        let [row] = <[Value; 1]>::try_from(rows)
            .map_err(|_| "usage command returned an invalid provider list".to_owned())?;
        let Value::Object(map) = &row else {
            return Err("usage command returned an invalid provider list".to_owned());
        };
        if field(map, "provider").as_str() != Some(provider) {
            return Err("usage command returned the wrong provider".to_owned());
        }
        Ok(row)
    }
}

/// Where usage collection runs. `Threads` is production (one worker per due provider, joined
/// by `stop`). `Inline` runs each due collection before `snapshot` returns, which makes the
/// cadence and backoff tests deterministic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dispatch {
    Threads,
    Inline,
}

/// The subscription price for one provider (`subscription_costs`, latest row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    pub provider: String,
    pub monthly_usd_cents: i64,
    pub observed_at: String,
}

/// Python `ops_budget.project`: the provider windows, states and prices for the board.
pub fn project(
    readings: &[Value],
    subscriptions: &[Subscription],
    now: DateTime<Utc>,
    error: Option<&str>,
    max_age_s: i64,
) -> Result<Value, PyError> {
    let generated = iso_micro(now);
    let mut prices: HashMap<&str, &Subscription> = HashMap::new();
    for row in subscriptions {
        prices.insert(row.provider.as_str(), row);
    }
    let mut providers: Vec<Value> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for reading in readings {
        let reading = obj(reading)?;
        let name = match field(reading, "provider") {
            Value::String(name) if !name.is_empty() => name.clone(),
            _ => continue,
        };
        seen.insert(name.clone());
        let last_good = field(reading, "last_good");
        let observation: &Obj = match last_good {
            Value::Object(good) => good,
            _ => reading,
        };
        let measured = moment_of(field(observation, "measured_at"));
        let age = measured.and_then(|moment| age_between(now, moment));
        let status = or_else(field(reading, "status"), json!("unknown"));
        let meaning = field(observation, "meaning").clone();
        let snapshot: Obj = match field(observation, "last_snapshot") {
            Value::Object(snapshot) => snapshot.clone(),
            _ => Obj::new(),
        };
        let mut raw_windows = field(observation, "windows").clone();
        if !truthy(&raw_windows) && !snapshot.is_empty() && meaning.as_str() == Some("percent_used")
        {
            raw_windows = json!([snapshot_window(&snapshot)]);
        }
        let local_estimate = meaning.as_str() == Some("local_session_estimate");
        let has_value = truthy(&raw_windows)
            || (local_estimate && number_of(field(observation, "session_cost_usd")).is_some());
        // Python `state in {"ok", "unknown"}` raises for an unhashable state.
        if matches!(status, Value::Array(_) | Value::Object(_)) {
            return Err("TypeError");
        }
        let mut state = status;
        if matches!(state.as_str(), Some("ok" | "unknown")) && has_value {
            state = json!(if age.is_none_or(|age| age > max_age_s) {
                "stale"
            } else {
                "ok"
            });
        }
        let reason = reason_text(reading, &name, &state);
        let failed = if state.as_str() == Some("error") {
            moment_of(field(reading, "measured_at")).map(iso_micro)
        } else {
            None
        };
        let mut windows: Vec<Value> = Vec::new();
        if meaning.as_str() == Some("percent_used") {
            if let Value::Array(items) = &raw_windows {
                for window in items {
                    let Value::Object(window) = window else {
                        continue;
                    };
                    let reset = moment_of(field(window, "resets_at"));
                    let mut window_state = if reset.is_some_and(|at| at <= now) {
                        json!("expired")
                    } else {
                        state.clone()
                    };
                    let used = number_of(field(window, "percent_used"));
                    let left = number_of(field(window, "percent_remaining"));
                    if used.is_none() && left.is_none() {
                        window_state = json!("unknown");
                    }
                    windows.push(json!({
                        "name": py_str(&or_else(field(window, "name"), json!("unnamed"))),
                        "state": window_state,
                        "percent_used": num_value(field(window, "percent_used")),
                        "percent_remaining": num_value(field(window, "percent_remaining")),
                        "resets_at": reset.map(iso_micro),
                    }));
                }
            }
        }
        let price_key = if name == "opencode-go" {
            "opencode"
        } else {
            name.as_str()
        };
        let price = prices
            .get(price_key)
            .map(|subscription| price_value(subscription));
        let extra = match field(observation, "extra_usage") {
            Value::Object(source) => Some(json!({
                "used_credits": num_value(field(source, "used_credits")),
                "monthly_limit": num_value(field(source, "monthly_limit")),
                "utilization": num_value(field(source, "utilization")),
                "spend_limit_reached": match field(source, "spend_limit_reached") {
                    Value::Bool(flag) => json!(flag),
                    _ => Value::Null,
                },
            })),
            _ => None,
        };
        let local = local_estimate.then(|| {
            json!({
                "session_cost_usd": num_value(field(observation, "session_cost_usd")),
                "last_context_tokens": num_value(field(observation, "last_context_tokens")),
                "session_messages_with_usage": num_value(field(observation, "session_messages_with_usage")),
            })
        });
        providers.push(json!({
            "provider": name,
            "source": SOURCE,
            "state": state,
            "meaning": meaning,
            "generated_at": generated,
            "measured_at": measured.map(iso_micro),
            "age_s": age,
            "reason": reason,
            "last_good": last_good.is_object(),
            "next_attempt_at": field(reading, "next_attempt_at"),
            "failed_at": failed,
            "windows": windows,
            "subscription": price,
            "extra_usage": extra,
            "local_estimate": local,
        }));
    }
    for subscription in subscriptions {
        let canonical = if subscription.provider == "opencode" {
            "opencode-go"
        } else {
            subscription.provider.as_str()
        };
        if seen.contains(canonical) {
            continue;
        }
        providers.push(json!({
            "provider": canonical,
            "source": SOURCE,
            "state": "unavailable",
            "meaning": "percent_used",
            "generated_at": generated,
            "measured_at": null,
            "age_s": null,
            "reason": error.filter(|text| !text.is_empty()).unwrap_or("no provider reading"),
            "windows": [],
            "extra_usage": null,
            "local_estimate": null,
            "subscription": price_value(subscription),
        }));
    }
    let state = if error.is_some_and(|text| !text.is_empty()) {
        "unavailable"
    } else {
        "ok"
    };
    Ok(json!({
        "source": SOURCE,
        "state": state,
        "generated_at": generated,
        "error": error,
        "providers": providers,
        "dollars_remaining": null,
        "dollars_reason": DOLLARS_REASON,
    }))
}

fn price_value(subscription: &Subscription) -> Value {
    json!({
        "monthly_usd": float_value(subscription.monthly_usd_cents as f64 / 100.0),
        "state": "ok",
        "source": "owner-reported subscription",
        "observed_at": subscription.observed_at,
    })
}

/// Python `_reason`: the 429 text, the reason verbatim (240 characters), or a fallback.
fn reason_text(reading: &Obj, name: &str, state: &Value) -> String {
    let reason = py_strip(&py_str(&or_else(
        field(reading, "reason"),
        or_else(field(reading, "error"), json!("")),
    )))
    .to_owned();
    if rate_limited(&reason) {
        return format!("{name} endpoint rate-limited (HTTP 429)");
    }
    if !reason.is_empty() {
        return reason.chars().take(240).collect();
    }
    if state.as_str() == Some("ok") {
        String::new()
    } else {
        format!("{name} usage {}; no further reason supplied", py_str(state))
    }
}

/// Python `re.search(r"HTTP\s*429\b|rate_limit_error", text, re.I)`.
fn rate_limited(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("rate_limit_error") {
        return true;
    }
    let mut from = 0;
    while let Some(found) = lower[from..].find("http") {
        let after = from + found + 4;
        let rest = lower[after..].trim_start_matches(char::is_whitespace);
        if let Some(tail) = rest.strip_prefix("429") {
            if !tail
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
            {
                return true;
            }
        }
        from = after;
    }
    false
}

/// Python `ops_budget._collect`'s `snapshot` window: the newest snapshot as a window row.
fn snapshot_window(snapshot: &Obj) -> Value {
    let minutes = field(snapshot, "window_minutes");
    let given = field(snapshot, "name");
    let name = if truthy(given) {
        given.clone()
    } else {
        json!(default_window_name(minutes))
    };
    let remaining = match number_of(field(snapshot, "percent_used")) {
        Some(used) if (0.0..=100.0).contains(&used) => {
            subtract_from_hundred(field(snapshot, "percent_used"))
        }
        _ => Value::Null,
    };
    let mut window = snapshot.clone();
    set_key_map(&mut window, "name", name);
    set_key_map(&mut window, "percent_remaining", remaining);
    Value::Object(window)
}

/// Python `100 - used`: an int stays an int, and a float stays a float.
fn subtract_from_hundred(raw: &Value) -> Value {
    match raw.as_i64() {
        Some(int) => json!(100 - int),
        None => float_value(100.0 - number_of(raw).unwrap_or(0.0)),
    }
}

fn default_window_name(minutes: &Value) -> String {
    if number_of(minutes) == Some(10080.0) {
        "weekly".to_owned()
    } else if truthy(minutes) {
        format!("{}min", py_str(minutes))
    } else {
        "last window".to_owned()
    }
}

/// Python `UsageCache`: a per-provider cadence, a failure backoff, and the last good reading.
pub struct UsageCache {
    inner: Arc<UsageInner>,
    dispatch: Dispatch,
}

struct UsageInner {
    refresh_s: i64,
    backoff_s: Vec<i64>,
    reader: Arc<dyn UsageReader>,
    clock: Clock,
    state: Mutex<UsageState>,
}

#[derive(Default)]
struct UsageState {
    rows: HashMap<String, Value>,
    good: HashMap<String, Value>,
    due: HashMap<String, f64>,
    failures: HashMap<String, usize>,
    futures: HashMap<String, JoinHandle<()>>,
}

impl UsageCache {
    pub fn new(
        refresh_s: i64,
        backoff_s: Vec<i64>,
        reader: Arc<dyn UsageReader>,
        clock: Clock,
        dispatch: Dispatch,
    ) -> Self {
        Self {
            inner: Arc::new(UsageInner {
                refresh_s,
                backoff_s,
                reader,
                clock,
                state: Mutex::new(UsageState::default()),
            }),
            dispatch,
        }
    }

    /// Python `snapshot`: submit each due provider, then return every row in oracle order.
    /// A provider with no reading yet is `not_read_yet`, never a zero.
    pub fn snapshot(&self) -> Result<Vec<Value>, String> {
        let due = {
            let mut state = lock(&self.inner.state);
            let now = (self.inner.clock)();
            let mut due = Vec::new();
            for provider in PROVIDERS {
                let idle = state
                    .futures
                    .get(provider)
                    .is_none_or(JoinHandle::is_finished);
                if idle && now >= state.due.get(provider).copied().unwrap_or(0.0) {
                    due.push(provider);
                }
            }
            if self.dispatch == Dispatch::Threads {
                for provider in &due {
                    let inner = Arc::clone(&self.inner);
                    let name = (*provider).to_owned();
                    let handle = thread::Builder::new()
                        .name(format!("vp-usage-{name}"))
                        .spawn(move || inner.collect(&name))
                        .map_err(|error| error.to_string())?;
                    state.futures.insert((*provider).to_owned(), handle);
                }
                Vec::new()
            } else {
                due
            }
        };
        for provider in due {
            self.inner.collect(provider);
        }
        let state = lock(&self.inner.state);
        Ok(PROVIDERS
            .iter()
            .map(|provider| match state.rows.get(*provider) {
                Some(Value::Object(row)) if !row.is_empty() => Value::Object(row.clone()),
                _ => json!({
                    "provider": provider,
                    "status": "not_read_yet",
                    "meaning": "unknown",
                    "measured_at": null,
                    "reason": "Waiting for the first provider reading",
                }),
            })
            .collect())
    }

    /// Python `stop`: wait for the in-flight provider reads to finish.
    pub fn stop(&self) {
        let handles: Vec<JoinHandle<()>> = lock(&self.inner.state)
            .futures
            .drain()
            .map(|(_, handle)| handle)
            .collect();
        for handle in handles {
            let _ = handle.join();
        }
    }
}

impl UsageInner {
    fn backoff_for(&self, failures: usize) -> i64 {
        if self.backoff_s.is_empty() {
            return self.refresh_s;
        }
        let index = failures.saturating_sub(1).min(self.backoff_s.len() - 1);
        self.backoff_s[index]
    }

    /// Python `_collect`: one provider read, then the cadence and backoff update.
    fn collect(&self, provider: &str) {
        let row = match self.reader.read(provider) {
            Ok(row) => row,
            Err(message) => json!({
                "provider": provider,
                "status": "error",
                "meaning": "unknown",
                "measured_at": (self.clock)(),
                "error": message.chars().take(240).collect::<String>(),
            }),
        };
        let Value::Object(mut row) = row else {
            return;
        };
        let mut state = lock(&self.state);
        let now = (self.clock)();
        let snapshot: Obj = match field(&row, "last_snapshot") {
            Value::Object(snapshot) => snapshot.clone(),
            _ => Obj::new(),
        };
        let has_snapshot = number_of(field(&snapshot, "percent_used")).is_some();
        let has_local = field(&row, "meaning").as_str() == Some("local_session_estimate")
            && number_of(field(&row, "session_cost_usd")).is_some();
        let status = field(&row, "status").as_str();
        let success =
            status == Some("ok") || (status == Some("unknown") && (has_snapshot || has_local));
        let due = if success {
            state
                .good
                .insert(provider.to_owned(), Value::Object(row.clone()));
            state.failures.insert(provider.to_owned(), 0);
            now + self.refresh_s as f64
        } else {
            let failures = state.failures.get(provider).copied().unwrap_or(0) + 1;
            state.failures.insert(provider.to_owned(), failures);
            let wait = self.backoff_for(failures).min(self.refresh_s);
            if let Some(good) = state.good.get(provider).cloned() {
                row.insert("last_good".to_owned(), good);
            }
            now + wait as f64
        };
        state.due.insert(provider.to_owned(), due);
        row.insert("next_attempt_at".to_owned(), float_value(due));
        state.rows.insert(provider.to_owned(), Value::Object(row));
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn set_key_map(map: &mut Obj, key: &str, new: Value) {
    map.insert(key.to_owned(), new);
}

fn field<'a>(map: &'a Obj, key: &str) -> &'a Value {
    map.get(key).unwrap_or(&NULL)
}

fn obj(value: &Value) -> Result<&Obj, PyError> {
    match value {
        Value::Object(map) => Ok(map),
        _ => Err("AttributeError"),
    }
}

/// Python truthiness for the JSON values returned by usage providers.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_none_or(|float| float != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// Python `x or default`.
fn or_else(value: &Value, default: Value) -> Value {
    if truthy(value) {
        value.clone()
    } else {
        default
    }
}

/// Accept finite JSON numbers only.
fn number_of(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64().filter(|float| float.is_finite()),
        _ => None,
    }
}

/// The original number for output (Python keeps an int as an int and a float as a float).
fn num_value(value: &Value) -> Value {
    if number_of(value).is_some() {
        value.clone()
    } else {
        Value::Null
    }
}

/// Match Python whitespace trimming, including its extra control separators.
fn py_strip(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c))
}

/// A Python float, spelled as Python spells it, as a JSON number.
fn float_value(value: f64) -> Value {
    serde_json::from_str(&py_float_repr(value)).unwrap_or(Value::Null)
}

/// UTC ISO timestamp with microseconds only when present.
fn iso_micro(value: DateTime<Utc>) -> String {
    if value.timestamp_subsec_micros() == 0 {
        value.format("%Y-%m-%dT%H:%M:%SZ").to_string()
    } else {
        value.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
    }
}

/// Python `_time` and `_age_seconds`' parse: a number is a Unix time, text is ISO-8601.
fn moment_of(value: &Value) -> Option<DateTime<Utc>> {
    match value {
        Value::Number(_) => fromtimestamp_utc(number_of(value)?).ok(),
        Value::String(text) => parse_moment(text),
        _ => None,
    }
}

fn parse_moment(text: &str) -> Option<DateTime<Utc>> {
    let normalized = text.replace('Z', "+00:00");
    if let Ok(moment) = parse_time(&normalized) {
        return Some(moment);
    }
    for format in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(&normalized, format) {
            return Some(naive.and_utc());
        }
    }
    NaiveDate::parse_from_str(&normalized, "%Y-%m-%d")
        .ok()?
        .and_hms_opt(0, 0, 0)
        .map(|naive| naive.and_utc())
}

/// Python `max(0, int((now - moment).total_seconds()))`.
fn age_between(now: DateTime<Utc>, moment: DateTime<Utc>) -> Option<i64> {
    let micros = (now - moment).num_microseconds()?;
    Some((micros / 1_000_000).max(0))
}
