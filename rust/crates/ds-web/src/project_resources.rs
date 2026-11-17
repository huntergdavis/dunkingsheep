//! Small shared resource projection. No task scans, operations SQL, GitHub or board timer.
use crate::{
    routes::{
        auth::{AuthBackend, Identity, RequestScheme},
        messages_write::authenticate,
    },
    usage_runtime::{project, UsageCache},
};
use axum::{
    extract::{Extension, Request},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use chrono::Utc;
use serde_json::{json, Value};
use std::{
    io::Read,
    path::PathBuf,
    sync::{Arc, Mutex},
};

pub struct ProjectResources {
    pub usage: UsageCache,
    pub max_age_s: i64,
    pub proc_root: PathBuf,
    pub host_watch: PathBuf,
    pub cpu_sample: Mutex<Option<(u64, u64)>>,
}
impl ProjectResources {
    pub fn snapshot(&self) -> Value {
        let now = Utc::now();
        let budget = match self.usage.snapshot() {
            Ok(rows) => project(&rows, &[], now, None, self.max_age_s)
                .unwrap_or_else(|_| json!({"state":"unavailable"})),
            Err(_) => json!({"state":"unavailable"}),
        };
        let read = |name: &str| -> Option<String> {
            let mut text = String::new();
            std::fs::File::open(self.proc_root.join(name))
                .ok()?
                .take(65536)
                .read_to_string(&mut text)
                .ok()?;
            Some(text)
        };
        let memory = read("meminfo").and_then(|text| {
            let value = |key: &str| text.lines().find_map(|line| {
                let mut parts = line.split_whitespace();
                (parts.next()? == key).then(|| parts.next()?.parse::<u64>().ok()).flatten()
            });
            let total = value("MemTotal:")?;
            let available = value("MemAvailable:")?;
            let total_bytes = total.checked_mul(1024)?;
            let available_bytes = available.checked_mul(1024)?;
            (total > 0 && available <= total).then(|| json!({"total_bytes":total_bytes,"available_bytes":available_bytes,"used_percent":100.0*(total-available) as f64/total as f64}))
        });
        let cpu = read("stat").and_then(|text| {
            let mut fields = text.lines().next()?.split_whitespace();
            if fields.next()? != "cpu" {
                return None;
            }
            // Guest times are already included in user/nice; use only the first eight counters.
            let values: Vec<u64> = fields
                .take(8)
                .map(str::parse)
                .collect::<Result<_, _>>()
                .ok()?;
            if values.len() < 5 {
                return None;
            }
            let total: u64 = values
                .iter()
                .try_fold(0_u64, |sum, value| sum.checked_add(*value))?;
            let idle = values[3].checked_add(values[4])?;
            let mut previous = self.cpu_sample.lock().ok()?;
            let percent = previous.and_then(|(old_total, old_idle)| {
                let delta = total.checked_sub(old_total)?;
                let idle_delta = idle.checked_sub(old_idle)?;
                (delta > 0 && idle_delta <= delta)
                    .then(|| 100.0 * (delta - idle_delta) as f64 / delta as f64)
            });
            *previous = Some((total, idle));
            percent
        });
        let pressure = |name: &str| {
            read(name)
                .and_then(|text| {
                    text.lines()
                        .find(|line| line.starts_with("some "))
                        .and_then(|line| {
                            line.split_whitespace()
                                .find_map(|part| part.strip_prefix("avg60=")?.parse::<f64>().ok())
                        })
                })
                .filter(|v| v.is_finite() && (0.0..=100.0).contains(v))
        };
        json!({"generated_at":now.to_rfc3339(),"budget":budget,"host":{
            "scope":"shared_host", "observed_at":now.to_rfc3339(), "cpu_used_percent":cpu,
            "memory":memory,"cpu_pressure_avg60":pressure("pressure/cpu"),"memory_pressure_avg60":pressure("pressure/memory"),
            "saved_health":crate::ops_summary::host_health(now,&self.host_watch)
        }})
    }
}
pub fn router(
    resources: Arc<ProjectResources>,
    auth: Arc<dyn AuthBackend>,
    scheme: RequestScheme,
) -> Router {
    Router::new()
        .route("/api/project-resources", get(snapshot))
        .layer(Extension(resources))
        .layer(Extension(auth))
        .layer(Extension(scheme))
}
async fn snapshot(
    Extension(resources): Extension<Arc<ProjectResources>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    match authenticate(auth.as_ref(), request.headers(), scheme).await {
        Ok(Identity::Owner) => {}
        Ok(_) => return StatusCode::FORBIDDEN.into_response(),
        Err(error) => return error.into_response(),
    }
    match tokio::task::spawn_blocking(move || resources.snapshot()).await {
        Ok(value) => ([(header::CACHE_CONTROL, "no-store")], Json(value)).into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage_runtime::{Dispatch, UsageReader};
    struct Unavailable;
    impl UsageReader for Unavailable {
        fn read(&self, _: &str) -> Result<Value, String> {
            Err("unavailable".into())
        }
    }
    #[test]
    fn missing_measurements_are_unknown_and_cpu_uses_counter_deltas() {
        let root = tempfile::tempdir().unwrap();
        let resources = ProjectResources {
            usage: UsageCache::new(
                60,
                vec![60],
                Arc::new(Unavailable),
                Arc::new(|| 100.0),
                Dispatch::Inline,
            ),
            max_age_s: 180,
            proc_root: root.path().into(),
            host_watch: root.path().join("missing"),
            cpu_sample: Mutex::new(None),
        };
        let absent = resources.snapshot();
        assert!(absent["host"]["memory"].is_null());
        assert!(absent["host"]["cpu_used_percent"].is_null());
        assert!(absent["host"]["cpu_pressure_avg60"].is_null());
        std::fs::write(
            root.path().join("meminfo"),
            "MemTotal: 1000 kB\nMemAvailable: 250 kB\n",
        )
        .unwrap();
        std::fs::write(root.path().join("stat"), "cpu 10 0 0 90 0 0 0 0 10 0\n").unwrap();
        let first = resources.snapshot();
        assert_eq!(first["host"]["memory"]["used_percent"], 75.0);
        assert!(first["host"]["cpu_used_percent"].is_null());
        std::fs::write(root.path().join("stat"), "cpu 30 0 0 170 0 0 0 0 30 0\n").unwrap();
        assert_eq!(resources.snapshot()["host"]["cpu_used_percent"], 20.0);
    }
}
