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

use super::next_job::Handoff;
use crate::config::Env;
use crate::error::DunkError;
use crate::flock::loop_env::{configured_loop_terminal_factory, LoopTerminalFactory};
use crate::flock::Flock;
use crate::server_runtime::LoopRuntimeAdapter;

/// A paused dispatch must remain distinguishable from a materialisation failure.
#[allow(dead_code)] // The next-job transition consumes these variants in a dependent leaf.
#[derive(Debug)]
pub(crate) enum MaterialiseError {
    DispatchPaused(String),
    Other(DunkError),
}

impl From<DunkError> for MaterialiseError {
    fn from(error: DunkError) -> Self {
        Self::Other(error)
    }
}

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

    #[allow(dead_code)] // Used by the dependent next-job transition leaves.
    pub(crate) async fn materialise(
        &self,
        handoff: &Handoff,
    ) -> Result<crate::inbox::Status, MaterialiseError> {
        let env = process_gate_env()
            .map_err(|_| MaterialiseError::DispatchPaused("readiness_unknown".into()))?;
        self.materialise_with(handoff, env, None).await
    }

    async fn materialise_with(
        &self,
        handoff: &Handoff,
        env: Env,
        factory: Option<LoopTerminalFactory>,
    ) -> Result<crate::inbox::Status, MaterialiseError> {
        let field = |name: &str| {
            handoff
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| DunkError::Refused(format!("missing next-job {name}")))
        };
        let pane = field("pane_id")?;
        if let Some(reason) = self.gate_with(&pane, None, env, factory.clone()).await {
            return Err(MaterialiseError::DispatchPaused(reason));
        }
        let factory = match factory {
            Some(factory) => factory,
            None => configured_loop_terminal_factory()?,
        };
        let adapter = LoopRuntimeAdapter::new(Arc::clone(&self.flock), factory);
        let requested = pane.clone();
        let (mailbox, _, actual_pane) = adapter
            .with_loop_terminal(move |flock, terminal| {
                flock.resolve_mailbox(terminal, &requested, None, true, "")
            })
            .await
            .map_err(|error| DunkError::Refused(format!("terminal worker failed: {error}")))??;
        if actual_pane.as_deref() != Some(pane.as_str()) {
            return Err(DunkError::Refused("next-job pane changed".into()).into());
        }
        let task_id = field("task_id")?;
        let message_id = field("message_id")?;
        let stores = self.flock.stores();
        let tasks = stores
            .tasks
            .ok_or_else(|| DunkError::Refused("task store unavailable".into()))?;
        let task = tasks.get(&task_id).await?;
        let source = handoff
            .get("from_task")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| handoff.get("trigger").and_then(Value::as_str))
            .unwrap_or("None");
        let body = format!("TASK {} [{}] {}\n\n{}\n\nAlready atomically claimed for {}; start this job now.\nheartbeat: dunkingsheep task heartbeat {}\ncomplete: dunkingsheep task complete {}\nhandoff: {} from {}", task.id, task.priority, task.title, task.body, pane, task.id, task.id, message_id, source);
        let subject = format!("TASK {}: {}", task.id, task.title);
        let inbox = stores
            .inbox
            .ok_or_else(|| DunkError::Refused("inbox store unavailable".into()))?;
        tokio::task::spawn_blocking(move || {
            inbox.ensure_next_job(&message_id, &mailbox, &pane, &body, &subject)
        })
        .await
        .map_err(|error| DunkError::Refused(format!("inbox worker failed: {error}")))?
        .map_err(Into::into)
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

    /// Bind a sent handoff to the producer generation observed for its pane.
    ///
    /// The sender owns this lock while it writes the delivery receipt, so an
    /// observation arriving during that window is bound by the sender's final
    /// `bind_turn_locked` call instead of waiting here.
    #[allow(dead_code)] // Wired by the turn observation/dispatch runtime leaves.
    pub(crate) async fn bind_turn(&self, pane: String) -> Result<(), DunkError> {
        let lock = self.pane_lock(&pane);
        let Ok(_guard) = lock.try_lock_owned() else {
            return Ok(());
        };
        self.bind_turn_locked(&pane).await
    }

    /// Bind while the caller already owns this pane's async lock.
    #[allow(dead_code)] // Called by delivery while it owns the pane lock.
    async fn bind_turn_locked(&self, pane: &str) -> Result<(), DunkError> {
        let stores = self.flock.stores();
        let tasks = stores
            .tasks
            .ok_or_else(|| DunkError::Refused("task store unavailable".into()))?;
        let inbox = stores
            .inbox
            .ok_or_else(|| DunkError::Refused("inbox store unavailable".into()))?;

        for handoff in tasks.next_job_handoffs(Some(pane)).await? {
            if handoff.get("state").and_then(Value::as_str) != Some("sent")
                || handoff
                    .get("delivered_at")
                    .and_then(Value::as_f64)
                    .is_none_or(|at| at == 0.0)
            {
                continue;
            }
            let Some(message_id) = handoff.get("message_id").and_then(Value::as_str) else {
                continue;
            };
            let Some(turn) = handoff.get("observed_turn").and_then(Value::as_object) else {
                continue;
            };
            let Some(turn_id) = turn
                .get("turn_id")
                .and_then(Value::as_str)
                .filter(|turn_id| !turn_id.is_empty())
            else {
                continue;
            };
            if turn.get("handoff_id").and_then(Value::as_str) != Some(message_id) {
                continue;
            }
            let Some(started_at) = turn.get("started_at").and_then(Value::as_f64) else {
                continue;
            };
            let Some(attempt_at) = handoff.get("attempt_at").and_then(Value::as_f64) else {
                continue;
            };
            if started_at < attempt_at {
                continue;
            }

            let receipt =
                crate::inbox::status(inbox.read_pool(), message_id, crate::clock::now_s())?;
            if receipt.notify_state.as_deref() != Some("sent") {
                continue;
            }

            let mut changes = serde_json::Map::new();
            changes.insert("state".into(), Value::String("started".into()));
            changes.insert("turn_id".into(), Value::String(turn_id.to_owned()));
            changes.insert("turn_started_at".into(), Value::from(started_at));
            changes.insert(
                "turn_source".into(),
                turn.get("source").cloned().unwrap_or(Value::Null),
            );
            changes.insert(
                "turn_caller".into(),
                turn.get("caller").cloned().unwrap_or(Value::Null),
            );
            tasks.update_next_job(&handoff, None, changes).await?;
        }
        Ok(())
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
                Value::Object(object) if object.is_empty() => {}
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

    async fn materialise_fixture() -> (
        tempfile::TempDir,
        NextJobs,
        Arc<crate::tasks::Tasks>,
        Arc<crate::inbox_store::Store>,
    ) {
        let (dir, jobs, tasks) = configured_jobs().await;
        let db = dir.path().join("inbox.db");
        let blobs = dir.path().join("blobs");
        let inbox = Arc::new(
            tokio::task::spawn_blocking(move || crate::inbox_store::Store::open(db, blobs))
                .await
                .unwrap()
                .unwrap(),
        );
        jobs.flock.attach_stores(crate::flock::panes::Stores {
            tasks: Some(Arc::clone(&tasks)),
            inbox: Some(Arc::clone(&inbox)),
            ..Default::default()
        });
        (dir, jobs, tasks, inbox)
    }

    fn fake_factory() -> LoopTerminalFactory {
        Arc::new(|| Box::new(crate::terminal::fakes::FakeTerminal::new()))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_materialise_is_idempotent_and_preserves_body() {
        let (dir, jobs, tasks, inbox) = materialise_fixture().await;
        let task = tasks
            .add("global", "Fix the gate", "First line\nSecond line")
            .await
            .unwrap();
        let h = json!({"pane_id":"w1:p1", "task_id":task.id, "message_id":"nj-test", "from_task":"t-old", "trigger":"completion"}).as_object().unwrap().clone();
        let env = || test_env(dir.path(), "1", "w1:p1");
        let first = jobs
            .materialise_with(&h, env(), Some(fake_factory()))
            .await
            .unwrap();
        assert_eq!(first.message_id, "nj-test");
        assert_eq!(first.pane_id.as_deref(), Some("w1:p1"));
        let receipt_store = Arc::clone(&inbox);
        tokio::task::spawn_blocking(move || {
            receipt_store.record_notify(
                "nj-test",
                "sent",
                Some("accepted"),
                Some("receipt-1"),
                Some(1),
            )
        })
        .await
        .unwrap()
        .unwrap();
        let second = jobs
            .materialise_with(&h, env(), Some(fake_factory()))
            .await
            .unwrap();
        assert_eq!(first.created_at, second.created_at);
        assert_eq!(first.mailbox, second.mailbox);
        assert_eq!(second.notify_state.as_deref(), Some("sent"));
        assert_eq!(second.notify_transport_id.as_deref(), Some("receipt-1"));
        let pool = crate::db::ReadPool::open(dir.path().join("inbox.db"), 1).unwrap();
        let (summary, body) = crate::inbox::get(&pool, "nj-test", crate::clock::now_s()).unwrap();
        assert_eq!(
            summary.subject.as_deref(),
            Some(format!("TASK {}: Fix the gate", task.id).as_str())
        );
        assert_eq!(body, format!("TASK {} [low] Fix the gate\n\nFirst line\nSecond line\n\nAlready atomically claimed for w1:p1; start this job now.\nheartbeat: dunkingsheep task heartbeat {}\ncomplete: dunkingsheep task complete {}\nhandoff: nj-test from t-old", task.id, task.id, task.id));
        let count: i64 = rusqlite::Connection::open(dir.path().join("inbox.db"))
            .unwrap()
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        tokio::task::spawn_blocking(move || drop((jobs, inbox)))
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_materialise_refuses_changed_or_unavailable_pane() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (dir, jobs, tasks, inbox) = materialise_fixture().await;
        let task = tasks.add("global", "Example", "body").await.unwrap();
        let h = json!({"pane_id":"w1:p1", "task_id":task.id, "message_id":"nj-unavailable", "trigger":"restart"}).as_object().unwrap().clone();
        let disabled = jobs
            .materialise_with(
                &h,
                test_env(dir.path(), "off", "w1:p1"),
                Some(fake_factory()),
            )
            .await;
        assert!(
            matches!(disabled, Err(MaterialiseError::DispatchPaused(ref reason)) if reason == "disabled")
        );
        let missing_task = json!({"pane_id":"w1:p1", "task_id":"t999999999", "message_id":"nj-missing", "trigger":"restart"}).as_object().unwrap().clone();
        let result = jobs
            .materialise_with(
                &missing_task,
                test_env(dir.path(), "1", "w1:p1"),
                Some(fake_factory()),
            )
            .await;
        assert!(
            matches!(result, Err(MaterialiseError::Other(DunkError::Refused(ref reason))) if reason == "no task with id \"t999999999\"")
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let unavailable: LoopTerminalFactory = Arc::new(move || {
            let mut fake = crate::terminal::fakes::FakeTerminal::new();
            if counter.fetch_add(1, Ordering::SeqCst) > 0 {
                fake.panes.clear();
            }
            Box::new(fake)
        });
        let result = jobs
            .materialise_with(&h, test_env(dir.path(), "1", "w1:p1"), Some(unavailable))
            .await;
        assert!(matches!(result, Err(MaterialiseError::Other(_))));
        let h = json!({"pane_id":"worker", "task_id":task.id, "message_id":"nj-changed", "trigger":"restart"}).as_object().unwrap().clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let changed: LoopTerminalFactory = Arc::new(move || {
            let mut fake = crate::terminal::fakes::FakeTerminal::new();
            fake.panes.truncate(1);
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                fake.panes[0].pane_id = "worker".into();
            } else {
                fake.panes[0].label = "worker".into();
            }
            Box::new(fake)
        });
        let result = jobs
            .materialise_with(&h, test_env(dir.path(), "1", "worker"), Some(changed))
            .await;
        assert!(
            matches!(result, Err(MaterialiseError::Other(DunkError::Refused(ref reason))) if reason == "next-job pane changed")
        );
        let count: i64 = rusqlite::Connection::open(dir.path().join("inbox.db"))
            .unwrap()
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
        tokio::task::spawn_blocking(move || drop((jobs, inbox)))
            .await
            .unwrap();
    }

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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_bind_turn_requires_sent_receipt_and_matching_generation() {
        let (_dir, jobs, tasks, inbox) = materialise_fixture().await;

        async fn seed_handoff(
            tasks: &crate::tasks::Tasks,
            inbox: &Arc<crate::inbox_store::Store>,
            pane: &str,
            message_id: &str,
            notify_state: &str,
            turn_id: &str,
            handoff_id: &str,
            started_at: f64,
        ) {
            let task = tasks.add("global", "Bind turn", "body").await.unwrap();
            let inbox = Arc::clone(inbox);
            let message_id = message_id.to_owned();
            let pane = pane.to_owned();
            let notify_state = notify_state.to_owned();
            tokio::task::spawn_blocking(move || {
                inbox.ensure_next_job(&message_id, &pane, &pane, "body", "subject")?;
                inbox.record_notify(&message_id, &notify_state, None, Some("transport"), Some(1))
            })
            .await
            .unwrap()
            .unwrap();
            let details = json!({
                "state": "sent",
                "attempt_at": 100.0,
                "delivered_at": 200.0,
                "observed_turn": {
                    "turn_id": turn_id,
                    "started_at": started_at,
                    "source": "producer",
                    "caller": "worker",
                    "handoff_id": handoff_id
                }
            })
            .as_object()
            .unwrap()
            .clone();
            tasks
                .record_next_job_handoff(
                    &task.id,
                    &pane,
                    None,
                    Some(&message_id),
                    1,
                    None,
                    Some(201.0),
                    Some(details),
                )
                .await
                .unwrap();
        }

        seed_handoff(
            &tasks,
            &inbox,
            "%valid",
            "nj-bind-valid",
            "sent",
            "turn-1",
            "nj-bind-valid",
            101.0,
        )
        .await;
        let lock = jobs.pane_lock("%valid");
        let held = lock.clone().try_lock_owned().unwrap();
        jobs.bind_turn("%valid".into()).await.unwrap();
        let busy_result = tasks.next_job_handoffs(Some("%valid")).await.unwrap();
        assert_eq!(
            busy_result[0].get("state").and_then(Value::as_str),
            Some("sent")
        );
        assert!(busy_result[0].get("turn_id").is_none());
        drop(held);

        jobs.bind_turn("%valid".into()).await.unwrap();
        let valid = tasks.next_job_handoffs(Some("%valid")).await.unwrap();
        assert_eq!(
            valid[0].get("state").and_then(Value::as_str),
            Some("started")
        );
        assert_eq!(
            valid[0].get("turn_id").and_then(Value::as_str),
            Some("turn-1")
        );
        assert_eq!(
            valid[0].get("turn_started_at").and_then(Value::as_f64),
            Some(101.0)
        );
        assert_eq!(
            valid[0].get("turn_source").and_then(Value::as_str),
            Some("producer")
        );
        assert_eq!(
            valid[0].get("turn_caller").and_then(Value::as_str),
            Some("worker")
        );

        for (pane, message_id, notify, turn_id, observed_id, started_at) in [
            (
                "%pending",
                "nj-bind-pending",
                "queued",
                "turn-2",
                "nj-bind-pending",
                101.0,
            ),
            (
                "%wrong",
                "nj-bind-wrong",
                "sent",
                "turn-3",
                "nj-other",
                101.0,
            ),
            (
                "%early",
                "nj-bind-early",
                "sent",
                "turn-4",
                "nj-bind-early",
                99.0,
            ),
            (
                "%empty",
                "nj-bind-empty",
                "sent",
                "",
                "nj-bind-empty",
                101.0,
            ),
        ] {
            seed_handoff(
                &tasks,
                &inbox,
                pane,
                message_id,
                notify,
                turn_id,
                observed_id,
                started_at,
            )
            .await;
            jobs.bind_turn(pane.to_owned()).await.unwrap();
            let rows = tasks.next_job_handoffs(Some(pane)).await.unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].get("state").and_then(Value::as_str), Some("sent"));
            assert!(rows[0].get("turn_id").is_none());
        }

        tokio::task::spawn_blocking(move || drop((jobs, inbox)))
            .await
            .unwrap();
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
        for content in ["false", "0", r#"{"lanes":false}"#, r#"{"lanes":{}}"#] {
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
