//! Concrete task and lane reads for `flock::roster::RosterEnv`.

use serde_json::{json, Value};

use super::RequestRuntimeAdapter;

impl RequestRuntimeAdapter<'_> {
    #[allow(dead_code)] // The complete RosterEnv implementation is owned by its assembly leaf.
    fn helper_rosterenv_lanes(&self) -> Result<Vec<Value>, String> {
        match self
            .terminal
            .flock
            .resolve_lanes(&self.terminal, crate::clock::now_s())
        {
            Ok(lanes) => Ok(lanes),
            Err(error) => Err(error.to_string()),
        }
    }

    #[allow(dead_code)] // The complete RosterEnv implementation is owned by its assembly leaf.
    fn helper_rosterenv_leases_for_pane(&self, pane_id: &str, now: f64) -> Vec<Value> {
        let Some(tasks) = self.terminal.flock.stores().tasks else {
            return Vec::new();
        };
        match self
            .runtime
            .block_on(tasks.leases_for_pane(pane_id, Some(now)))
        {
            Ok(leases) => leases
                .into_iter()
                .map(|lease| {
                    json!({
                        "task_id": lease.task_id,
                        "pane_id": lease.pane_id,
                        "claimed_at": lease.claimed_at,
                        "heartbeat_at": lease.heartbeat_at,
                        "expires_at": lease.expires_at,
                        "caller_pid": lease.caller_pid,
                        "caller_start": lease.caller_start,
                    })
                })
                .collect(),
            Err(error) => {
                eprintln!("roster lease lookup failed for {pane_id}: {error}");
                Vec::new()
            }
        }
    }

    #[allow(dead_code)] // The complete RosterEnv implementation is owned by its assembly leaf.
    fn helper_rosterenv_task(&self, task_id: &str) -> Option<Value> {
        let tasks = self.terminal.flock.stores().tasks?;
        self.runtime
            .block_on(tasks.get(task_id))
            .ok()
            .map(|task| json!({"id": task.id, "title": task.title}))
    }

    #[allow(dead_code)] // The complete RosterEnv implementation is owned by its assembly leaf.
    fn helper_rosterenv_inbox_count(&self, mailbox: &str) -> Option<(i64, Option<f64>)> {
        let inbox = self.terminal.flock.stores().inbox?;
        match crate::inbox::count(inbox.read_pool(), mailbox, &[], crate::clock::now_s()) {
            Ok(counts) => Some((
                counts.unread,
                counts.oldest_unread_s.map(|seconds| seconds as f64),
            )),
            Err(error) => {
                eprintln!("roster inbox count failed for {mailbox}: {error}");
                None
            }
        }
    }

    #[allow(dead_code)] // The complete RosterEnv implementation is owned by its assembly leaf.
    fn helper_rosterenv_activity(&self, lane: &Value, now: f64) -> Result<Value, String> {
        Ok(self.terminal.flock.roster_activity(lane, now))
    }

    #[allow(dead_code)] // The complete RosterEnv implementation is owned by its assembly leaf.
    fn helper_rosterenv_cpu_sample(
        &self,
        caller: &Value,
        now: f64,
    ) -> Result<Option<Value>, String> {
        let Some(pid) = caller
            .get("caller_pid")
            .and_then(Value::as_i64)
            .filter(|pid| *pid != 0)
        else {
            return Ok(None);
        };
        let start = caller.get("caller_start").and_then(|start| {
            start
                .as_i64()
                .or_else(|| start.as_str()?.parse::<i64>().ok())
        });
        let Some(start) = start else {
            return Ok(None);
        };
        let name = caller
            .get("caller_name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("pid {pid}"));
        let sample = crate::load::sample(&[(name, pid, start)], now);
        Ok(sample
            .get("callers")
            .and_then(Value::as_array)
            .and_then(|callers| callers.first())
            .cloned())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::flock::panes::Stores;
    use crate::flock::Flock;
    use crate::server::RequestCtx;
    use crate::tasks::{ClaimOptions, Tasks};
    use crate::terminal::{fakes::FakeTerminal, Terminal};

    use super::*;

    #[test]
    fn production_env_roster_lanes_uses_concrete_services() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let tasks = Arc::new(
            runtime
                .block_on(Tasks::open(dir.path().join("tasks.db")))
                .unwrap(),
        );
        runtime.block_on(tasks.allocation_init()).unwrap();
        let task = runtime
            .block_on(tasks.add("global", "roster task", "body"))
            .unwrap();
        runtime
            .block_on(tasks.claim_next("global", "w1:p1", ClaimOptions::default()))
            .unwrap();

        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        );
        flock.attach_stores(Stores {
            tasks: Some(tasks),
            ..Stores::default()
        });
        let request = RequestCtx::default();
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);

        let lanes = adapter.helper_rosterenv_lanes().unwrap();
        assert_eq!(lanes.len(), 3);
        assert!(lanes.iter().any(|lane| lane["pane_id"] == "w1:p1"));

        let leases = adapter.helper_rosterenv_leases_for_pane("w1:p1", crate::clock::now_s());
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0]["task_id"], task.id);
        assert_eq!(leases[0]["pane_id"], "w1:p1");
        assert!(leases[0]["claimed_at"].as_f64().is_some());

        assert_eq!(
            adapter.helper_rosterenv_task(&task.id),
            Some(json!({"id": task.id, "title": "roster task"}))
        );
    }

    #[test]
    fn production_env_roster_signals_uses_concrete_services() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let inbox = Arc::new(
            crate::inbox_store::Store::open(dir.path().join("inbox.db"), dir.path().join("blobs"))
                .unwrap(),
        );
        inbox
            .ensure_next_job("nj-roster", "worker-mailbox", "w1:p1", "body", "subject")
            .unwrap();
        let tasks = Arc::new(
            runtime
                .block_on(Tasks::open(dir.path().join("tasks.db")))
                .unwrap(),
        );
        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        );
        flock.attach_stores(Stores {
            tasks: Some(tasks),
            inbox: Some(inbox),
            ..Stores::default()
        });
        let request = RequestCtx::default();
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);

        let count = adapter
            .helper_rosterenv_inbox_count("worker-mailbox")
            .unwrap();
        assert_eq!(count.0, 1);
        assert!(count.1.is_some());

        let lane = json!({"pane_id": "w1:p1", "agent": "pi", "agent_status": null});
        let activity = adapter
            .helper_rosterenv_activity(&lane, crate::clock::now_s())
            .unwrap();
        assert_eq!(activity["state"], "unknown");
        assert!(activity["reason"]
            .as_str()
            .unwrap()
            .contains("no activity source"));

        let terminal_working =
            json!({"pane_id": "w1:p1", "agent": "pi", "agent_status": "working"});
        let pi_activity = adapter
            .helper_rosterenv_activity(&terminal_working, crate::clock::now_s())
            .unwrap();
        assert_eq!(pi_activity["state"], "unknown");
        assert_eq!(pi_activity["source"], "pi turn events");

        adapter.terminal.flock.set_turn(
            "w1:p1",
            crate::activity::Turn {
                started_at: Some(10.0),
                expires_at: Some(crate::clock::now_s() + 60.0),
                ended_at: None,
            },
        );
        let turn_activity = adapter
            .helper_rosterenv_activity(&terminal_working, crate::clock::now_s())
            .unwrap();
        assert_eq!(turn_activity["state"], "working");
        assert_eq!(turn_activity["source"], "pi turn events");

        let claude_terminal_working =
            json!({"pane_id": "w1:p1", "agent": "claude", "agent_status": "working"});
        let claude_activity = adapter
            .helper_rosterenv_activity(&claude_terminal_working, crate::clock::now_s())
            .unwrap();
        assert_eq!(claude_activity["state"], "unknown");
        assert_eq!(
            claude_activity["reason"],
            "no claude status client available"
        );

        let absent = adapter
            .helper_rosterenv_cpu_sample(
                &json!({
                    "caller_pid": i64::MAX,
                    "caller_start": "1",
                    "caller_name": "gone"
                }),
                crate::clock::now_s(),
            )
            .unwrap();
        assert_eq!(absent, None);
    }

    #[test]
    fn roster_inbox_count_errors_return_none() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("inbox.db");
        let inbox =
            Arc::new(crate::inbox_store::Store::open(&database, dir.path().join("blobs")).unwrap());
        rusqlite::Connection::open(database)
            .unwrap()
            .execute_batch("DROP TABLE messages")
            .unwrap();
        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        );
        flock.attach_stores(Stores {
            inbox: Some(inbox),
            ..Stores::default()
        });
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let request = RequestCtx::default();
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);

        assert_eq!(adapter.helper_rosterenv_inbox_count("worker-mailbox"), None);
    }

    #[test]
    fn roster_activity_without_source_stays_unknown_with_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        );
        let request = RequestCtx::default();
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);

        let activity = adapter
            .helper_rosterenv_activity(
                &json!({"pane_id": "w1:p1", "agent": "codex", "agent_status": "unknown"}),
                crate::clock::now_s(),
            )
            .unwrap();
        assert_eq!(activity["state"], Value::Null);
        assert!(activity["reason"]
            .as_str()
            .unwrap()
            .contains("no activity source"));
    }

    #[test]
    fn roster_lease_lookup_errors_fall_back_to_empty() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("tasks.db");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let tasks = Arc::new(runtime.block_on(Tasks::open(&database)).unwrap());
        runtime.block_on(tasks.allocation_init()).unwrap();
        rusqlite::Connection::open(database)
            .unwrap()
            .execute_batch("DROP TABLE task_leases")
            .unwrap();

        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        );
        flock.attach_stores(Stores {
            tasks: Some(tasks),
            ..Stores::default()
        });
        let request = RequestCtx::default();
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);

        assert!(adapter
            .helper_rosterenv_leases_for_pane("w1:p1", crate::clock::now_s())
            .is_empty());
    }

    #[test]
    fn roster_missing_task_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let tasks = Arc::new(
            runtime
                .block_on(Tasks::open(dir.path().join("tasks.db")))
                .unwrap(),
        );
        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        );
        flock.attach_stores(Stores {
            tasks: Some(tasks),
            ..Stores::default()
        });
        let request = RequestCtx::default();
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);

        assert_eq!(adapter.helper_rosterenv_task("missing-task"), None);
    }
}
