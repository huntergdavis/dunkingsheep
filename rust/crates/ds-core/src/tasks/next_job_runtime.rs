//! Runtime state and per-pane serialization for next-job operations.
//!
//! The lock map mirrors `dunk_next_job.NextJobs._pane_lock`: a pane always maps to
//! the same async mutex, while different panes can progress independently. The
//! standard mutex protects only the map and is never held across an await.

use std::collections::HashMap;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::OwnedMutexGuard;

use crate::config::Env;
use crate::flock::loop_env::{configured_loop_terminal_factory, LoopTerminalFactory};
use crate::flock::Flock;
use crate::server_runtime::LoopRuntimeAdapter;

pub struct NextJobs {
    // Later runtime leaves use the shared Flock for dispatch, tasks, and inbox work.
    #[allow(dead_code)]
    flock: Arc<Flock>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl NextJobs {
    pub fn new(flock: Arc<Flock>) -> Self {
        Self {
            flock,
            locks: Mutex::new(HashMap::new()),
        }
    }

    fn pane_lock(&self, pane: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(
            locks
                .entry(pane.to_owned())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }

    #[allow(dead_code)] // Used by the next-job methods added in following runtime leaves.
    async fn lock_pane(&self, pane: &str) -> OwnedMutexGuard<()> {
        self.pane_lock(pane).lock_owned().await
    }

    /// Dispatch permission is distinct from task allocation: registration never bypasses
    /// the global switch and an unreadable dependency cannot become permission.
    #[allow(dead_code)] // Consumed by the next-job transition leaves.
    pub(crate) async fn gate(&self, pane: &str, lanes: Option<Vec<Value>>) -> Option<String> {
        let env = match process_gate_env() {
            Ok(env) => env,
            Err(()) => return Some("readiness_unknown".into()),
        };
        self.gate_with(pane, lanes, env, None).await
    }

    async fn gate_with(
        &self,
        pane: &str,
        lanes: Option<Vec<Value>>,
        env: Env,
        factory: Option<LoopTerminalFactory>,
    ) -> Option<String> {
        if matches!(
            env.get("DUNKINGSHEEP_TURN_END_DISPATCH")
                .unwrap_or("1")
                .trim()
                .to_lowercase()
                .as_str(),
            "0" | "false" | "no" | "off"
        ) {
            return Some("disabled".into());
        }
        let lanes = match lanes {
            Some(lanes) => lanes,
            None => {
                let factory = match factory.or_else(|| configured_loop_terminal_factory().ok()) {
                    Some(factory) => factory,
                    None => return Some("readiness_unknown".into()),
                };
                let adapter = LoopRuntimeAdapter::new(Arc::clone(&self.flock), factory);
                match adapter
                    .with_loop_terminal(|flock, terminal| {
                        flock.resolve_lanes(terminal, crate::clock::now_s())
                    })
                    .await
                {
                    Ok(Ok(lanes)) => lanes,
                    _ => return Some("readiness_unknown".into()),
                }
            }
        };
        let Some(tasks) = self.flock.stores().tasks else {
            return Some("readiness_unknown".into());
        };
        let status = match tasks.allocation_status().await {
            Ok(status) => status,
            Err(_) => return Some("readiness_unknown".into()),
        };
        let Some(workers) = status.get("workers").and_then(Value::as_array) else {
            return Some("readiness_unknown".into());
        };
        let registered = workers.iter().any(|row| {
            row.get("pane_id").and_then(Value::as_str) == Some(pane)
                && row.get("enabled").is_some_and(|value| {
                    value
                        .as_bool()
                        .unwrap_or_else(|| value.as_i64().is_some_and(|n| n != 0))
                })
        });
        let env_for_config = env.clone();
        let wanted =
            match tokio::task::spawn_blocking(move || dispatch_entries(&env_for_config)).await {
                Ok(Ok(wanted)) => wanted,
                _ => return Some("readiness_unknown".into()),
            };
        let allowed = dispatch_allowlist(&wanted, &lanes);
        if !allowed.contains(pane) && !registered {
            return Some("not_allowed".into());
        }
        let mut present = false;
        for lane in &lanes {
            let Some(row) = lane.as_object() else {
                return Some("readiness_unknown".into());
            };
            let Some(id) = row.get("pane_id") else {
                return Some("readiness_unknown".into());
            };
            if id.as_str() == Some(pane) {
                present = true;
                break;
            }
        }
        if !present {
            return Some("unavailable_lane".into());
        }
        let path = match crate::panes::default_path(&env) {
            Ok(path) => path,
            Err(_) => return Some("readiness_unknown".into()),
        };
        let owned_pane = pane.to_owned();
        match tokio::task::spawn_blocking(move || retirement_status(&owned_pane, &path)).await {
            Ok(Ok(true)) => Some("unavailable_lane".into()),
            Ok(Ok(false)) => None,
            _ => Some("readiness_unknown".into()),
        }
    }
}

/// Read one snapshot so an unreadable or corrupt retirement registry cannot become
/// permission. A missing file has the shared store's accepted empty semantics.
fn retirement_status(pane: &str, path: &std::path::Path) -> Result<bool, ()> {
    let records: BTreeMap<String, Value> =
        crate::atomic_json::read_or_default(path).map_err(|_| ())?;
    let Some(record) = records.get(pane.trim()) else {
        return Ok(false);
    };
    serde_json::from_value::<crate::panes::Retired>(record.clone()).map_err(|_| ())?;
    Ok(true)
}

fn process_gate_env() -> Result<Env, ()> {
    let mut values = Vec::new();
    for key in [
        "DUNKINGSHEEP_TURN_END_DISPATCH",
        "DUNKINGSHEEP_DIR",
        "HOME",
        "USERPROFILE",
        "DUNKINGSHEEP_DISPATCH_LANES",
    ] {
        if let Some(value) = std::env::var_os(key) {
            let value = value.into_string().map_err(|_| ())?;
            if key == "DUNKINGSHEEP_TURN_END_DISPATCH"
                && matches!(
                    value.trim().to_lowercase().as_str(),
                    "0" | "false" | "no" | "off"
                )
            {
                return Ok(Env::from_pairs(&[(key, value.as_str())]));
            }
            values.push((key, value));
        }
    }
    let refs: Vec<_> = values
        .iter()
        .map(|(key, value)| (*key, value.as_str()))
        .collect();
    Ok(Env::from_pairs(&refs))
}

/// Config file precedes the environment; an empty/missing list falls back to the environment.
fn dispatch_entries(env: &Env) -> Result<Vec<String>, ()> {
    let path = crate::config::config_dir(env)
        .map_err(|_| ())?
        .join("dispatch_lanes.json");
    let mut entries = Vec::new();
    match std::fs::read_to_string(path) {
        Ok(data) => {
            let loaded: Value = serde_json::from_str(&data).map_err(|_| ())?;
            let listed = if let Some(object) = loaded.as_object() {
                object.get("lanes").unwrap_or(&Value::Null)
            } else {
                &loaded
            };
            match listed {
                Value::Null => {}
                Value::Bool(false) => {}
                Value::Number(number) if number.as_f64() == Some(0.0) => {}
                Value::String(text) => entries.extend(
                    text.split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned),
                ),
                Value::Array(values) => {
                    for value in values {
                        let text = match value {
                            Value::String(text) => text.clone(),
                            Value::Number(_) => value.to_string(),
                            Value::Bool(value) => if *value { "True" } else { "False" }.into(),
                            Value::Null => "None".into(),
                            _ => return Err(()),
                        };
                        entries.extend(
                            text.split(',')
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(str::to_owned),
                        );
                    }
                }
                _ => return Err(()),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(()),
    }
    if entries.is_empty() {
        entries.extend(
            env.get("DUNKINGSHEEP_DISPATCH_LANES")
                .unwrap_or("")
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned),
        );
    }
    Ok(entries)
}

fn dispatch_allowlist(entries: &[String], lanes: &[Value]) -> BTreeSet<String> {
    let mut resolved = BTreeSet::new();
    for entry in entries {
        let wanted = entry.to_lowercase();
        let matches: Vec<_> = lanes
            .iter()
            .filter_map(|lane| {
                let id = lane
                    .get("pane_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())?;
                let name = lane.get("agent_name").and_then(Value::as_str).unwrap_or("");
                (id.to_lowercase() == wanted || name.to_lowercase() == wanted)
                    .then(|| id.to_string())
            })
            .collect();
        if matches.is_empty() {
            resolved.insert(entry.clone());
        } else {
            resolved.extend(matches);
        }
    }
    resolved
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn next_jobs() -> NextJobs {
        NextJobs::new(Arc::new(Flock::new(PathBuf::new(), PathBuf::new(), false)))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_reuses_one_lock_per_pane() {
        let jobs = Arc::new(next_jobs());
        let first = jobs.pane_lock("%12");
        let repeated = jobs.pane_lock("%12");
        assert!(Arc::ptr_eq(&first, &repeated));

        let held = first
            .clone()
            .try_lock_owned()
            .expect("first lock available");
        assert!(repeated.try_lock_owned().is_err());
        drop(held);

        let owned_guard = jobs.lock_pane("%12").await;
        drop(owned_guard);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_locks_are_independent_across_panes() {
        let jobs = next_jobs();
        let first = jobs.pane_lock("%12");
        let second = jobs.pane_lock("%13");
        assert!(!Arc::ptr_eq(&first, &second));

        let first_guard = first.try_lock_owned().expect("first pane lock available");
        let second_guard = second.try_lock_owned().expect("second pane lock available");
        drop((first_guard, second_guard));
    }

    async fn configured_jobs() -> (tempfile::TempDir, NextJobs, Arc<crate::tasks::Tasks>) {
        let dir = tempfile::tempdir().unwrap();
        let tasks = Arc::new(
            crate::tasks::Tasks::open(dir.path().join("tasks.db"))
                .await
                .unwrap(),
        );
        tasks.allocation_init().await.unwrap();
        let flock = Arc::new(Flock::new(
            dir.path().join("launches"),
            dir.path().join("lanes"),
            false,
        ));
        flock.attach_stores(crate::flock::panes::Stores {
            tasks: Some(Arc::clone(&tasks)),
            ..Default::default()
        });
        (dir, NextJobs::new(flock), tasks)
    }

    fn test_env(dir: &std::path::Path, switch: &str, lanes: &str) -> Env {
        Env::from_pairs(&[
            ("DUNKINGSHEEP_DIR", dir.to_str().unwrap()),
            ("DUNKINGSHEEP_TURN_END_DISPATCH", switch),
            ("DUNKINGSHEEP_DISPATCH_LANES", lanes),
        ])
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_gate_preserves_dispatch_refusals() {
        let (dir, jobs, tasks) = configured_jobs().await;
        let lanes = vec![
            json!({"pane_id":"%1", "agent_name":"Writer"}),
            json!({"pane_id":"%2", "agent_name":"writer"}),
        ];
        tasks
            .allocation_register(
                "%1",
                &json!(["code"]),
                &json!(["worker"]),
                &json!([]),
                "%owner",
                &json!(true),
            )
            .await
            .unwrap();
        assert_eq!(
            jobs.gate_with(
                "%1",
                Some(lanes.clone()),
                test_env(dir.path(), "off", "writer"),
                None
            )
            .await
            .as_deref(),
            Some("disabled")
        );
        assert_eq!(
            jobs.gate_with(
                "%1",
                Some(lanes.clone()),
                test_env(dir.path(), "1", ""),
                None
            )
            .await,
            None
        );
        assert_eq!(
            jobs.gate_with(
                "%2",
                Some(lanes.clone()),
                test_env(dir.path(), "1", "WRITER"),
                None
            )
            .await,
            None
        );
        assert_eq!(
            jobs.gate_with(
                "%2",
                Some(lanes.clone()),
                test_env(dir.path(), "1", "%other"),
                None
            )
            .await
            .as_deref(),
            Some("not_allowed")
        );
        assert_eq!(
            jobs.gate_with("%1", Some(vec![]), test_env(dir.path(), "1", ""), None)
                .await
                .as_deref(),
            Some("unavailable_lane")
        );
        crate::panes::retire(
            "%1",
            "fixture",
            Some("%owner"),
            &[],
            &[],
            &dir.path().join("retired-panes.json"),
            None,
        )
        .unwrap();
        assert_eq!(
            jobs.gate_with("%1", Some(lanes), test_env(dir.path(), "1", ""), None)
                .await
                .as_deref(),
            Some("unavailable_lane")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_gate_fails_closed_on_lane_or_config_error() {
        let (dir, jobs, tasks) = configured_jobs().await;
        tasks
            .allocation_register(
                "%1",
                &json!(["code"]),
                &json!(["worker"]),
                &json!([]),
                "%owner",
                &json!(true),
            )
            .await
            .unwrap();
        let failing: LoopTerminalFactory = Arc::new(|| {
            let mut terminal = crate::terminal::fakes::FakeTerminal::new();
            terminal.fail_grouped_panes =
                Some(crate::terminal::TerminalError::refused("listing failed"));
            Box::new(terminal)
        });
        assert_eq!(
            jobs.gate_with("%1", None, test_env(dir.path(), "1", "%1"), Some(failing))
                .await
                .as_deref(),
            Some("readiness_unknown")
        );
        std::fs::write(dir.path().join("dispatch_lanes.json"), "{").unwrap();
        assert_eq!(
            jobs.gate_with(
                "%1",
                Some(vec![json!({"pane_id":"%1"})]),
                test_env(dir.path(), "1", "%1"),
                None
            )
            .await
            .as_deref(),
            Some("readiness_unknown")
        );
        std::fs::remove_file(dir.path().join("dispatch_lanes.json")).unwrap();
        assert_eq!(
            jobs.gate_with(
                "%1",
                Some(vec![json!({"other":1})]),
                test_env(dir.path(), "1", "%1"),
                None
            )
            .await
            .as_deref(),
            Some("readiness_unknown")
        );
        let no_tasks = next_jobs();
        assert_eq!(
            no_tasks
                .gate_with(
                    "%1",
                    Some(vec![json!({"pane_id":"%1"})]),
                    test_env(dir.path(), "1", "%1"),
                    None
                )
                .await
                .as_deref(),
            Some("readiness_unknown")
        );
        let conn = rusqlite::Connection::open(dir.path().join("tasks.db")).unwrap();
        conn.execute("DROP TABLE allocation_workers", []).unwrap();
        drop(conn);
        assert_eq!(
            jobs.gate_with(
                "%1",
                Some(vec![json!({"pane_id":"%1"})]),
                test_env(dir.path(), "1", "%1"),
                None
            )
            .await
            .as_deref(),
            Some("readiness_unknown")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_gate_fails_closed_on_retirement_read_error() {
        let (dir, jobs, _) = configured_jobs().await;
        let path = dir.path().join("retired-panes.json");
        let lanes = vec![json!({"pane_id":"%1"})];
        let env = || test_env(dir.path(), "1", "%1");
        // The existing store treats a missing file as empty.
        assert_eq!(
            jobs.gate_with("%1", Some(lanes.clone()), env(), None).await,
            None
        );
        for bad in ["{", r#"{"%1":{}}"#] {
            std::fs::write(&path, bad).unwrap();
            assert_eq!(
                jobs.gate_with("%1", Some(lanes.clone()), env(), None)
                    .await
                    .as_deref(),
                Some("readiness_unknown")
            );
        }
        std::fs::write(&path, r#"{"%2":{}}"#).unwrap();
        assert_eq!(
            jobs.gate_with("%1", Some(lanes.clone()), env(), None).await,
            None
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert_eq!(
            jobs.gate_with("%1", Some(lanes), env(), None)
                .await
                .as_deref(),
            Some("readiness_unknown")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_registration_is_dispatch_opt_in_but_not_kill_switch_override() {
        let (dir, jobs, tasks) = configured_jobs().await;
        tasks
            .allocation_register(
                "%1",
                &json!(["code"]),
                &json!(["worker"]),
                &json!([]),
                "%owner",
                &json!(true),
            )
            .await
            .unwrap();
        let lane = vec![json!({"pane_id":"%1"})];
        assert_eq!(
            jobs.gate_with(
                "%1",
                Some(lane.clone()),
                test_env(dir.path(), "0", ""),
                None
            )
            .await
            .as_deref(),
            Some("disabled")
        );
        assert_eq!(
            jobs.gate_with("%1", Some(lane), test_env(dir.path(), "1", ""), None)
                .await,
            None
        );
        tasks
            .allocation_register(
                "%1",
                &json!(["code"]),
                &json!(["worker"]),
                &json!([]),
                "%owner",
                &json!(false),
            )
            .await
            .unwrap();
        assert_eq!(
            jobs.gate_with(
                "%1",
                Some(vec![json!({"pane_id":"%1"})]),
                test_env(dir.path(), "1", ""),
                None
            )
            .await
            .as_deref(),
            Some("not_allowed")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_gate_uses_given_snapshot_and_real_allowlist_file() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (dir, jobs, _) = configured_jobs().await;
        std::fs::write(
            dir.path().join("dispatch_lanes.json"),
            r#"{"lanes":["Writer, other"]}"#,
        )
        .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&calls);
        let factory: LoopTerminalFactory = Arc::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
            Box::new(crate::terminal::fakes::FakeTerminal::new())
        });
        let lanes = vec![json!({"pane_id":"%1","agent_name":"writer"})];
        assert_eq!(
            jobs.gate_with(
                "%1",
                Some(lanes),
                test_env(dir.path(), "1", "%other"),
                Some(factory)
            )
            .await,
            None
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        for content in ["false", "0", r#"{"lanes":false}"#] {
            std::fs::write(dir.path().join("dispatch_lanes.json"), content).unwrap();
            assert_eq!(
                jobs.gate_with(
                    "%1",
                    Some(vec![json!({"pane_id":"%1"})]),
                    test_env(dir.path(), "1", "%1"),
                    None
                )
                .await,
                None
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_gate_resolves_real_terminal_on_blocking_worker() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (dir, jobs, _) = configured_jobs().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&calls);
        let factory: LoopTerminalFactory = Arc::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
            Box::new(crate::terminal::fakes::FakeTerminal::new())
        });
        assert_eq!(
            jobs.gate_with(
                "w1:p1",
                None,
                test_env(dir.path(), "1", "w1:p1"),
                Some(factory)
            )
            .await,
            None
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_gate_production_entry_refuses_missing_store() {
        let jobs = next_jobs();
        let reason = jobs.gate("%fixture", Some(vec![])).await;
        assert!(matches!(
            reason.as_deref(),
            Some("disabled" | "readiness_unknown")
        ));
    }
}
