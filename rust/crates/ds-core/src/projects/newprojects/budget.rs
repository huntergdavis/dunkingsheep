use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::error::DunkError;
use crate::projects::{usage_gate, Usage};

/// Explicit policy for one provider. Missing subscription and credit policy is never inferred.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderBudget {
    pub subscription: ExplicitOption<SubscriptionBudget>,
    pub credit_fallback: ExplicitOption<CreditFallback>,
}

/// An optional policy whose key must still be present: `null` explicitly disables that source.
#[derive(Debug, Clone)]
pub struct ExplicitOption<T>(pub Option<T>);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for ExplicitOption<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Option::<T>::deserialize(deserializer).map(Self)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubscriptionBudget {
    /// The provider's exact window name (for example `weekly` or `300min`).
    pub window: String,
    /// Explicit per-provider admission ceiling; no implicit percentage is supplied here.
    pub max_used_percent: f64,
    /// Maximum age accepted for a real provider reading.
    pub max_age_seconds: f64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreditFallback {
    /// Must be explicitly true to permit a credit reading when subscription is unusable.
    pub enabled: bool,
    /// Explicit project/provider budget in the provider's reported credit units.
    pub limit: f64,
    /// Maximum age accepted for project-attributed credit usage.
    pub max_age_seconds: f64,
}

/// Project budget config is strict: the legacy `credit_ceiling` key stays required in approved
/// config, while per-project/provider limits decide admission. Its value is never treated as
/// project usage, and account-wide deltas are not attribution.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectBudgetConfig {
    /// Required field for compatibility with the approved template. `null` means no account-wide
    /// value was configured; it supplies no amount or project attribution.
    pub credit_ceiling: Value,
    pub providers: BTreeMap<String, ProviderBudget>,
}

impl ProjectBudgetConfig {
    pub fn from_json(input: &str) -> Result<Self, DunkError> {
        let config: Self = serde_json::from_str(input)
            .map_err(|_| DunkError::Refused("project budget not configured".into()))?;
        let ceiling_ok = config.credit_ceiling.is_null()
            || config.credit_ceiling.as_f64().is_some_and(valid_positive);
        if !ceiling_ok
            || config.providers.is_empty()
            || config.providers.values().any(|provider| {
                provider
                    .subscription
                    .0
                    .as_ref()
                    .is_some_and(|subscription| {
                        subscription.window.trim().is_empty()
                            || !valid_percent(subscription.max_used_percent)
                            || !valid_positive(subscription.max_age_seconds)
                    })
                    || provider.credit_fallback.0.as_ref().is_some_and(|fallback| {
                        !valid_positive(fallback.limit) || !valid_positive(fallback.max_age_seconds)
                    })
            })
        {
            return Err(DunkError::Refused("project budget not configured".into()));
        }
        Ok(config)
    }

    /// Subscription first. Credit fallback requires an explicit enabled flag and a real,
    /// project-attributed provider reading. Account-wide balance/delta is never accepted.
    pub fn admit(
        &self,
        project: &str,
        provider: &str,
        five_hour: &Usage,
        reading: &Value,
        now: f64,
    ) -> Result<(), DunkError> {
        usage_gate(five_hour)?;
        let policy = self.providers.get(provider).ok_or_else(|| {
            DunkError::Refused(format!(
                "project {project}: no budget policy for provider {provider}"
            ))
        })?;

        if let Some(subscription) = &policy.subscription.0 {
            if subscription_reading_ok(reading, provider, subscription, now) {
                return Ok(());
            }
        }

        if let Some(fallback) = &policy.credit_fallback.0 {
            if fallback.enabled
                && attributed_credit_ok(
                    reading,
                    project,
                    provider,
                    fallback.limit,
                    fallback.max_age_seconds,
                    now,
                )
            {
                return Ok(());
            }
        }
        Err(DunkError::Refused(format!(
            "project {project}: {provider} has no usable configured subscription or explicitly enabled project credit budget"
        )))
    }

    /// Run the existing provider-specific reader off the async executor. Unknown/error
    /// providers stay unknown; no generic account probe or estimate is substituted.
    pub async fn read_provider(
        &self,
        provider: &str,
        home: &std::path::Path,
        now: f64,
    ) -> Result<Value, DunkError> {
        let policy = self.providers.get(provider).ok_or_else(|| {
            DunkError::Refused(format!("no budget policy for provider {provider}"))
        })?;
        // Read broadly enough for either explicitly configured source. Admission applies each
        // source's own freshness limit, so a stale subscription cannot mask a still-fresh credit
        // fallback measurement.
        let max_age = policy
            .subscription
            .0
            .as_ref()
            .map(|policy| policy.max_age_seconds)
            .into_iter()
            .chain(
                policy
                    .credit_fallback
                    .0
                    .as_ref()
                    .map(|policy| policy.max_age_seconds),
            )
            .reduce(f64::max)
            .unwrap_or(0.0);
        let provider = provider.to_owned();
        let home = home.to_path_buf();
        tokio::task::spawn_blocking(move || match provider.as_str() {
            "codex" => strict_codex_reading(&home, max_age, now),
            "pi" => crate::usage_providers::pi(&home, max_age, now),
            "opencode-go" => crate::usage_providers::opencode(now),
            "claude" => crate::usage_providers::claude(now),
            _ => serde_json::json!({
                "provider": provider,
                "status": "unknown",
                "meaning": "unknown",
                "measured_at": now,
                "reason": "no provider-specific usage reader"
            }),
        })
        .await
        .map_err(|error| DunkError::Unavailable(format!("provider usage reader stopped: {error}")))
    }
}

/// Strict Codex adapter. The public usage summary intentionally tolerates a missing/unreadable
/// number as `0.0` after its first shape check; admission re-reads the provider event and rejects
/// values that cannot be represented as a finite percentage.
fn strict_codex_reading(home: &std::path::Path, max_age_seconds: f64, now: f64) -> Value {
    use std::time::UNIX_EPOCH;

    let paths = crate::usage_providers::newest_jsonl(&home.join(".codex/sessions"), 8);
    let mut best: Option<(f64, std::path::PathBuf, Value)> = None;
    for path in paths {
        let Ok(contents) = std::fs::read(&path) else {
            continue;
        };
        let text = String::from_utf8_lossy(&contents);
        let mut event = None;
        for line in text
            .lines()
            .rev()
            .filter(|line| line.contains("rate_limits"))
        {
            let Ok(parsed) = serde_json::from_str::<Value>(line) else {
                return serde_json::json!({
                    "provider": "codex", "status": "error", "meaning": "unknown",
                    "reason": "latest Codex rate_limits event is malformed"
                });
            };
            let Some(limits) = crate::usage_providers::find_rate_limits(&parsed) else {
                return serde_json::json!({
                    "provider": "codex", "status": "error", "meaning": "unknown",
                    "reason": "latest Codex rate_limits event has no rate_limits object"
                });
            };
            let measured = crate::usage_providers::epoch(&parsed["timestamp"])
                .filter(|time| *time != 0.0)
                .or_else(|| {
                    std::fs::metadata(&path)
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                        .map(|age| age.as_secs_f64())
                });
            let Some(measured) = measured.filter(|time| time.is_finite()) else {
                return serde_json::json!({
                    "provider": "codex", "status": "unknown", "meaning": "unknown",
                    "reason": "Codex measurement time is unavailable"
                });
            };
            event = Some((measured, limits));
            break;
        }
        if let Some((measured, limits)) = event {
            if best.as_ref().is_none_or(|current| measured > current.0) {
                best = Some((measured, path, limits));
            }
        }
    }
    let Some((measured, path, limits)) = best else {
        return serde_json::json!({
            "provider": "codex", "status": "error", "meaning": "unknown",
            "reason": "no Codex rate_limits snapshot in ~/.codex/sessions"
        });
    };
    let primary = &limits["primary"];
    let Some(used) = primary["used_percent"]
        .as_f64()
        .filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
    else {
        return serde_json::json!({
            "provider": "codex", "status": "unknown", "meaning": "unknown",
            "measured_at": measured,
            "reason": "Codex used_percent is missing, malformed, or outside 0..=100"
        });
    };
    let Some(minutes) = limits["primary"]["window_minutes"]
        .as_f64()
        .filter(|value| value.is_finite() && *value > 0.0)
    else {
        return serde_json::json!({
            "provider": "codex", "status": "unknown", "meaning": "unknown",
            "measured_at": measured, "reason": "Codex usage window is missing or malformed"
        });
    };
    let name = if minutes == 10080.0 {
        "weekly".to_owned()
    } else {
        let shown = match &limits["primary"]["window_minutes"] {
            Value::Number(number) => number.to_string(),
            _ => minutes.to_string(),
        };
        format!("{shown}min")
    };
    let age = now - measured;
    let status = if !now.is_finite() || age < 0.0 || age > max_age_seconds {
        "unknown"
    } else {
        "ok"
    };
    serde_json::json!({
        "provider": "codex",
        "status": status,
        "meaning": "percent_used",
        "measured_at": measured,
        "source": path.strip_prefix(home).unwrap_or(&path).display().to_string(),
        "snapshot_age_s": age,
        "windows": [{
            "name": name,
            "percent_used": used,
            "percent_remaining": 100.0 - used,
            "resets_at": primary["resets_at"],
            "window_minutes": limits["primary"]["window_minutes"]
        }]
    })
}

fn subscription_reading_ok(
    reading: &Value,
    provider: &str,
    policy: &SubscriptionBudget,
    now: f64,
) -> bool {
    if reading["provider"].as_str() != Some(provider)
        || reading["status"].as_str() != Some("ok")
        || reading["meaning"].as_str() != Some("percent_used")
        || !reading["source"]
            .as_str()
            .is_some_and(|source| !source.is_empty())
        || !now.is_finite()
    {
        return false;
    }
    let Some(measured_at) = reading["measured_at"].as_f64() else {
        return false;
    };
    if !measured_at.is_finite() || measured_at > now || now - measured_at > policy.max_age_seconds {
        return false;
    }
    reading["windows"].as_array().is_some_and(|windows| {
        windows.iter().any(|window| {
            window["name"].as_str() == Some(policy.window.as_str())
                && window["percent_used"].as_f64().is_some_and(|used| {
                    used.is_finite()
                        && (0.0..=100.0).contains(&used)
                        && used < policy.max_used_percent
                })
        })
    })
}

fn attributed_credit_ok(
    reading: &Value,
    project: &str,
    provider: &str,
    limit: f64,
    max_age_seconds: f64,
    now: f64,
) -> bool {
    let credit = &reading["project_credit_usage"];
    reading["provider"].as_str() == Some(provider)
        && reading["status"].as_str() == Some("ok")
        && reading["meaning"].as_str() == Some("project_credit_usage")
        && reading["source"]
            .as_str()
            .is_some_and(|source| !source.is_empty())
        && reading["measured_at"].as_f64().is_some_and(|measured| {
            measured.is_finite() && measured <= now && now - measured <= max_age_seconds
        })
        && credit["project"].as_str() == Some(project)
        && credit["provider"].as_str() == Some(provider)
        && credit["scope"].as_str() == Some("project")
        && credit["amount"]
            .as_f64()
            .is_some_and(|amount| amount.is_finite() && amount >= 0.0 && amount < limit)
}

fn valid_positive(value: f64) -> bool {
    value.is_finite() && value > 0.0
}

fn valid_percent(value: f64) -> bool {
    value.is_finite() && (0.0..=100.0).contains(&value)
}
