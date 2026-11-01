//! `dunk_views.roster` (T38, slice 5): one row per lane - identity, state, current task,
//! inbox, cost, cpu. No screen reads: a UI polls this, and a roster that costs a pane read
//! per lane is a roster that times out. Every unknown field carries a reason (a cell).
//!
//! The Python reads `flock.resolve_lanes()`, `flock.tasks`, `flock.inbox`,
//! `dunk_activity.activity` and `dunk_load.sample`; each is a method of `RosterEnv` here so
//! the composition is tested without those stores, and the wiring is one impl later.

use serde_json::{json, Value};

use super::views::{cell, iso};

/// What `roster` reads from the rest of the daemon.
pub trait RosterEnv {
    /// `flock.resolve_lanes()`.
    fn lanes(&self) -> Vec<Value>;
    /// `flock.tasks.leases_for_pane(pane_id, now=now)`; an error is an empty list.
    fn leases_for_pane(&self, pane_id: &str, now: f64) -> Vec<Value>;
    /// `flock.tasks.get(task_id)`: a row with `id` and `title`, or None when it is gone.
    fn task(&self, task_id: &str) -> Option<Value>;
    /// `flock.inbox.count(name)`: `(unread, oldest_unread_s)`, None when the count failed.
    fn inbox_count(&self, mailbox: &str) -> Option<(i64, Option<f64>)>;
    /// `dunk_activity.activity(lane, ...)`: `{state, since, source, reason}`, or the error text.
    fn activity(&self, lane: &Value, now: f64) -> Result<Value, String>;
    /// `dunk_load.sample([caller], now=now)`: the first caller's row, None when the pid is not
    /// running, or the error text.
    fn cpu_sample(&self, caller: &Value, now: f64) -> Result<Option<Value>, String>;
}

/// `MODEL_SOURCES`: what each harness reports about its model. Unknown is null WITH a reason.
fn model_reason(harness: &str) -> &'static str {
    match harness.to_lowercase().as_str() {
        "claude" => "the session list carries no model field",
        "codex" => "codex does not report its model on the pane record",
        "pi" => "pi does not report its model on the pane record",
        _ => "unknown harness",
    }
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn iso_cell(at: Option<f64>) -> Value {
    iso(at).map(Value::String).unwrap_or(Value::Null)
}

/// `_cpu_for`: CPU for a lane's caller pid, or null with a reason.
fn cpu_for(env: &dyn RosterEnv, leases: &[Value], now: f64) -> Value {
    let Some(lease) = leases.iter().find(|l| truthy(l.get("caller_pid"))) else {
        return cell(
            Value::Null,
            None,
            Some("no caller pid on a lease for this lane"),
        );
    };
    let caller = json!({
        "caller_pid": lease["caller_pid"],
        "caller_start": lease.get("caller_start").cloned().unwrap_or(Value::Null),
        "caller_name": lease.get("caller_name").cloned().unwrap_or(Value::Null),
    });
    match env.cpu_sample(&caller, now) {
        Ok(None) => cell(Value::Null, None, Some("the caller pid is not running")),
        Ok(Some(entry)) => {
            let value = json!({
                "cpu_pct": entry.get("cpu_pct").cloned().unwrap_or(Value::Null),
                "rss_mb": entry.get("rss_mb").cloned().unwrap_or(Value::Null),
                "process_count": entry.get("process_count").cloned().unwrap_or(Value::Null),
            });
            cell(value, iso(Some(now)).as_deref(), None)
        }
        Err(error) => cell(
            Value::Null,
            None,
            Some(&format!("cpu sample failed: {error}")),
        ),
    }
}

/// `roster`: one row per lane that has an agent (a bare shell has no harness and no mail).
pub fn roster(env: &dyn RosterEnv, now: f64) -> Value {
    let mut rows = Vec::new();
    for lane in env.lanes().iter().filter(|l| truthy(l.get("agent"))) {
        let pane_id = lane["pane_id"].as_str().unwrap_or("");
        // NULL, not the string "unknown" (reviewer on t227): the owner asked for a dash where
        // there is no status.
        let status = lane.get("agent_status").cloned().unwrap_or(Value::Null);
        let leases = env.leases_for_pane(pane_id, now);
        let task = leases.first().map(|first| {
            let task_id = first["task_id"].as_str().unwrap_or("");
            match env.task(task_id) {
                Some(row) => json!({"id": row["id"], "title": row["title"], "since": iso_cell(first["claimed_at"].as_f64())}),
                None => json!({"id": first["task_id"], "title": null, "since": null}),
            }
        });
        let name = lane
            .get("agent_name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty());
        let (unread, oldest) = env
            .inbox_count(name.unwrap_or(pane_id))
            .unwrap_or((0, None));
        let harness = lane["agent"].as_str().unwrap_or("");
        // What the lane is DOING comes from its own tool; an absent signal is UNKNOWN.
        let doing = env.activity(lane, now).unwrap_or_else(|error| {
            json!({"state": "unknown", "since": null, "source": "activity", "reason": format!("activity probe failed: {error}")})
        });
        let since = doing
            .get("since")
            .and_then(Value::as_f64)
            .filter(|s| *s != 0.0);
        rows.push(json!({
            "pane_id": pane_id, // internal; the UI drops it
            "name": name.unwrap_or(pane_id),
            "harness": harness,
            "model": cell(Value::Null, None, Some(model_reason(harness))),
            "state": doing.get("state").cloned().unwrap_or(Value::Null),
            "state_source": doing.get("source").cloned().unwrap_or(Value::Null),
            "state_reason": doing.get("reason").cloned().unwrap_or(Value::Null),
            "since": cell(
                since.map(|s| json!(s)).unwrap_or(Value::Null),
                iso(since).as_deref(),
                if since.is_some() { None } else { Some("the source reports a state, not when it changed") },
            ),
            "pane_status": status,
            "task": task,
            "inbox": {"unread": unread, "oldest_unread_s": cell(
                oldest.map(|o| json!(o)).unwrap_or(Value::Null), None,
                if oldest.is_some() { None } else { Some("no unread mail") })},
            "cost": cell(Value::Null, None, Some("cost is metered per provider, not per lane")),
            "cpu": cpu_for(env, &leases, now),
        }));
    }
    json!({"as_of": iso(Some(now)), "lanes": rows})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct Env {
        lanes: Vec<Value>,
        leases: BTreeMap<String, Vec<Value>>,
        tasks: BTreeMap<String, Value>,
        inbox: BTreeMap<String, (i64, Option<f64>)>,
        doing: Option<Result<Value, String>>,
        cpu: Option<Result<Option<Value>, String>>,
    }

    impl RosterEnv for Env {
        fn lanes(&self) -> Vec<Value> {
            self.lanes.clone()
        }
        fn leases_for_pane(&self, pane_id: &str, _: f64) -> Vec<Value> {
            self.leases.get(pane_id).cloned().unwrap_or_default()
        }
        fn task(&self, id: &str) -> Option<Value> {
            self.tasks.get(id).cloned()
        }
        fn inbox_count(&self, mailbox: &str) -> Option<(i64, Option<f64>)> {
            self.inbox.get(mailbox).copied()
        }
        fn activity(&self, _: &Value, _: f64) -> Result<Value, String> {
            self.doing.clone().unwrap_or_else(|| Ok(json!({"state": "idle", "since": null, "source": "terminal backend", "reason": null})))
        }
        fn cpu_sample(&self, _: &Value, _: f64) -> Result<Option<Value>, String> {
            self.cpu.clone().unwrap_or(Ok(None))
        }
    }

    const NOW: f64 = 86_400.0;

    fn lane(id: &str, agent: &str, name: Option<&str>) -> Value {
        json!({"pane_id": id, "agent": agent, "agent_name": name, "agent_status": null})
    }

    #[test]
    fn a_bare_shell_is_not_a_lane_and_the_shape_is_the_pythons() {
        let env = Env {
            lanes: vec![lane("%1", "", None), lane("%2", "pi", Some("worker-2"))],
            ..Env::default()
        };
        let out = roster(&env, NOW);
        assert_eq!(out["as_of"], "1970-01-02T00:00:00Z");
        let rows = out["lanes"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "a bare shell has no harness and no mail");
        let row = &rows[0];
        assert_eq!(
            (
                row["pane_id"].as_str(),
                row["name"].as_str(),
                row["harness"].as_str()
            ),
            (Some("%2"), Some("worker-2"), Some("pi"))
        );
        assert_eq!(
            row["model"],
            json!({"value": null, "as_of": null, "reason": "pi does not report its model on the pane record"})
        );
        assert_eq!(row["state"], "idle");
        assert_eq!(row["state_source"], "terminal backend");
        assert_eq!(
            row["since"]["reason"],
            "the source reports a state, not when it changed"
        );
        assert!(
            row["pane_status"].is_null(),
            "NULL, not the string 'unknown'"
        );
        assert!(row["task"].is_null());
        assert_eq!(
            row["inbox"],
            json!({"unread": 0, "oldest_unread_s": {"value": null, "as_of": null, "reason": "no unread mail"}})
        );
        assert_eq!(
            row["cost"]["reason"],
            "cost is metered per provider, not per lane"
        );
        assert_eq!(
            row["cpu"]["reason"],
            "no caller pid on a lease for this lane"
        );
    }

    #[test]
    fn the_lease_names_the_current_task_and_a_missing_task_row_still_names_the_id() {
        let mut env = Env {
            lanes: vec![lane("%2", "claude", None)],
            ..Env::default()
        };
        env.leases.insert(
            "%2".into(),
            vec![json!({"task_id": "t5", "claimed_at": 3600.0})],
        );
        let gone = roster(&env, NOW);
        assert_eq!(
            gone["lanes"][0]["task"],
            json!({"id": "t5", "title": null, "since": null})
        );
        env.tasks
            .insert("t5".into(), json!({"id": "t5", "title": "do it"}));
        let found = roster(&env, NOW);
        assert_eq!(
            found["lanes"][0]["task"],
            json!({"id": "t5", "title": "do it", "since": "1970-01-01T01:00:00Z"})
        );
        assert_eq!(
            found["lanes"][0]["name"], "%2",
            "no agent name falls back to the pane id"
        );
    }

    #[test]
    fn inbox_counts_are_read_by_agent_name_and_oldest_is_a_cell() {
        let mut env = Env {
            lanes: vec![lane("%2", "pi", Some("w2"))],
            ..Env::default()
        };
        env.inbox.insert("w2".into(), (3, Some(42.0)));
        let row = &roster(&env, NOW)["lanes"][0];
        assert_eq!(row["inbox"]["unread"], 3);
        assert_eq!(
            row["inbox"]["oldest_unread_s"],
            json!({"value": 42.0, "as_of": null, "reason": null})
        );
    }

    #[test]
    fn a_failed_activity_probe_is_unknown_with_a_reason_not_a_failed_row() {
        let env = Env {
            lanes: vec![lane("%2", "pi", None)],
            doing: Some(Err("boom".into())),
            ..Env::default()
        };
        let row = &roster(&env, NOW)["lanes"][0];
        assert_eq!(
            (row["state"].as_str(), row["state_source"].as_str()),
            (Some("unknown"), Some("activity"))
        );
        assert_eq!(row["state_reason"], "activity probe failed: boom");
    }

    #[test]
    fn a_state_that_carries_a_since_reports_it_with_its_iso_time() {
        let env = Env {
            lanes: vec![lane("%2", "codex", None)],
            doing: Some(Ok(
                json!({"state": "working", "since": 7200.0, "source": "codex turns", "reason": null}),
            )),
            ..Env::default()
        };
        let row = &roster(&env, NOW)["lanes"][0];
        assert_eq!(
            row["since"],
            json!({"value": 7200.0, "as_of": "1970-01-01T02:00:00Z", "reason": null})
        );
        assert_eq!(
            row["model"]["reason"],
            "codex does not report its model on the pane record"
        );
    }

    #[test]
    fn cpu_comes_from_the_first_lease_with_a_caller_pid_or_says_why_not() {
        let mut env = Env {
            lanes: vec![lane("%2", "pi", None)],
            ..Env::default()
        };
        env.leases.insert("%2".into(), vec![json!({"task_id": "t1", "caller_pid": 0}), json!({"task_id": "t2", "caller_pid": 99, "caller_start": "s", "caller_name": "pi"})]);
        env.cpu = Some(Ok(None));
        assert_eq!(
            roster(&env, NOW)["lanes"][0]["cpu"]["reason"],
            "the caller pid is not running"
        );
        env.cpu = Some(Ok(Some(
            json!({"cpu_pct": 12.5, "rss_mb": 300, "process_count": 4, "extra": 1}),
        )));
        let cpu = &roster(&env, NOW)["lanes"][0]["cpu"];
        assert_eq!(
            cpu["value"],
            json!({"cpu_pct": 12.5, "rss_mb": 300, "process_count": 4})
        );
        assert_eq!(cpu["as_of"], "1970-01-02T00:00:00Z");
        env.cpu = Some(Err("no /proc".into()));
        assert_eq!(
            roster(&env, NOW)["lanes"][0]["cpu"]["reason"],
            "cpu sample failed: no /proc"
        );
    }
}
