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
    fn lanes(&self) -> Result<Vec<Value>, String>;
    /// `flock.tasks.leases_for_pane(pane_id, now=now)`; an error is an empty list.
    fn leases_for_pane(&self, pane_id: &str, now: f64) -> Vec<Value>;
    /// `flock.tasks.get(task_id)`: a row with `id` and `title`, or None when it is gone.
    fn task(&self, task_id: &str) -> Option<Value>;
    /// `flock.inbox.count(name)`: `(unread, oldest_unread_s)`, None when the count failed.
    /// A failed count is reported as unknown (`unread: null` with a reason), never as 0.
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

/// Include detected agents and live panes with explicit project or global membership.
/// The runtime supplies `project`/`global` only after validating the pane PID/start binding.
fn roster_lane(lane: &Value) -> bool {
    if truthy(lane.get("agent")) {
        return true;
    }
    match lane.pointer("/membership/scope").and_then(Value::as_str) {
        Some("project") => lane
            .pointer("/membership/project")
            .and_then(Value::as_str)
            .is_some_and(|name| !name.is_empty()),
        Some("global") => true,
        _ => false,
    }
}

/// Why a lane's `inbox.unread` is null.
pub const INBOX_COUNT_FAILED: &str = "the inbox count could not be read";

/// `roster`: detected agents plus live panes with explicit, valid membership.
pub fn roster(env: &dyn RosterEnv, now: f64) -> Result<Value, String> {
    let mut rows = Vec::new();
    for lane in env.lanes()?.iter().filter(|l| roster_lane(l)) {
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
        // t4742: a count that could not be read is UNKNOWN, not zero. A lane with waiting
        // mail must not look caught up because the inbox store failed.
        let inbox = match env.inbox_count(name.unwrap_or(pane_id)) {
            Some((unread, oldest)) => json!({
                "unread": unread,
                "unread_reason": null,
                "oldest_unread_s": cell(
                    oldest.map(|o| json!(o)).unwrap_or(Value::Null), None,
                    if oldest.is_some() { None } else { Some("no unread mail") }),
            }),
            None => json!({
                "unread": null,
                "unread_reason": INBOX_COUNT_FAILED,
                "oldest_unread_s": cell(Value::Null, None, Some(INBOX_COUNT_FAILED)),
            }),
        };
        let harness = lane["agent"].as_str().filter(|name| !name.is_empty());
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
            "membership": lane.get("membership").cloned().unwrap_or_else(|| json!({"scope":"unknown","project":null})),
            "name": name.unwrap_or(pane_id),
            "harness": harness,
            "model": cell(Value::Null, None, Some(model_reason(harness.unwrap_or("")))),
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
            "inbox": inbox,
            "cost": cell(Value::Null, None, Some("cost is metered per provider, not per lane")),
            "cpu": cpu_for(env, &leases, now),
        }));
    }
    Ok(json!({"as_of": iso(Some(now)), "lanes": rows}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct Env {
        lanes: Vec<Value>,
        lane_error: Option<String>,
        leases: BTreeMap<String, Vec<Value>>,
        tasks: BTreeMap<String, Value>,
        inbox: BTreeMap<String, (i64, Option<f64>)>,
        inbox_fails: bool,
        doing: Option<Result<Value, String>>,
        cpu: Option<Result<Option<Value>, String>>,
    }

    impl RosterEnv for Env {
        fn lanes(&self) -> Result<Vec<Value>, String> {
            match &self.lane_error {
                Some(error) => Err(error.clone()),
                None => Ok(self.lanes.clone()),
            }
        }
        fn leases_for_pane(&self, pane_id: &str, _: f64) -> Vec<Value> {
            self.leases.get(pane_id).cloned().unwrap_or_default()
        }
        fn task(&self, id: &str) -> Option<Value> {
            self.tasks.get(id).cloned()
        }
        fn inbox_count(&self, mailbox: &str) -> Option<(i64, Option<f64>)> {
            if self.inbox_fails {
                return None;
            }
            Some(self.inbox.get(mailbox).copied().unwrap_or((0, None)))
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
    fn explicitly_bound_panes_survive_unknown_harness_without_including_unbound_shells() {
        let env = Env {
            lanes: vec![
                json!({"pane_id":"%1","agent":"","membership":{"scope":"project","project":"elite-force-switch"}}),
                json!({"pane_id":"%2","agent":null,"membership":{"scope":"global","project":null}}),
                json!({"pane_id":"%3","agent":"","membership":{"scope":"stale","project":null}}),
                json!({"pane_id":"%4","agent":"","membership":{"scope":"unknown","project":null}}),
                json!({"pane_id":"%5","agent":"","membership":{"scope":"unavailable","project":null}}),
                json!({"pane_id":"%6","agent":"","membership":{"scope":"project","project":""}}),
            ],
            doing: Some(Ok(
                json!({"state":"unknown","since":null,"source":"none","reason":"unknown harness"}),
            )),
            ..Env::default()
        };
        let out = roster(&env, NOW).unwrap();
        let rows = out["lanes"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["pane_id"], "%1");
        assert_eq!(rows[0]["membership"]["project"], "elite-force-switch");
        assert!(rows[0]["harness"].is_null());
        assert_eq!(rows[0]["state"], "unknown");
        assert!(rows[0]["model"]["value"].is_null());
        assert_eq!(rows[1]["membership"]["scope"], "global");
    }

    #[test]
    fn a_bare_shell_is_not_a_lane_and_the_shape_is_the_pythons() {
        let env = Env {
            lanes: vec![lane("%1", "", None), lane("%2", "pi", Some("worker-2"))],
            ..Env::default()
        };
        let out = roster(&env, NOW).unwrap();
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
            json!({"unread": 0, "unread_reason": null, "oldest_unread_s": {"value": null, "as_of": null, "reason": "no unread mail"}})
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
    fn lane_resolution_errors_propagate_instead_of_becoming_an_empty_roster() {
        let env = Env {
            lane_error: Some("cannot list panes: socket unavailable".into()),
            ..Env::default()
        };
        assert_eq!(
            roster(&env, NOW).unwrap_err(),
            "cannot list panes: socket unavailable"
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
        let gone = roster(&env, NOW).unwrap();
        assert_eq!(
            gone["lanes"][0]["task"],
            json!({"id": "t5", "title": null, "since": null})
        );
        env.tasks
            .insert("t5".into(), json!({"id": "t5", "title": "do it"}));
        let found = roster(&env, NOW).unwrap();
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
        let row = &roster(&env, NOW).unwrap()["lanes"][0];
        assert_eq!(row["inbox"]["unread"], 3);
        assert_eq!(
            row["inbox"]["oldest_unread_s"],
            json!({"value": 42.0, "as_of": null, "reason": null})
        );
    }

    #[test]
    fn a_failed_inbox_count_is_unknown_with_a_reason_not_zero_unread() {
        let mut env = Env {
            lanes: vec![lane("%2", "pi", Some("w2"))],
            inbox_fails: true,
            ..Env::default()
        };
        // Mail IS waiting; the count just cannot be read.
        env.inbox.insert("w2".into(), (3, Some(42.0)));
        let row = &roster(&env, NOW).unwrap()["lanes"][0];
        assert!(row["inbox"]["unread"].is_null(), "not 0: {row}");
        assert_eq!(row["inbox"]["unread_reason"], INBOX_COUNT_FAILED);
        assert_eq!(
            row["inbox"]["oldest_unread_s"],
            json!({"value": null, "as_of": null, "reason": INBOX_COUNT_FAILED})
        );
        assert_eq!(row["name"], "w2", "the rest of the row is still served");
    }

    #[test]
    fn a_failed_activity_probe_is_unknown_with_a_reason_not_a_failed_row() {
        let env = Env {
            lanes: vec![lane("%2", "pi", None)],
            doing: Some(Err("boom".into())),
            ..Env::default()
        };
        let row = &roster(&env, NOW).unwrap()["lanes"][0];
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
        let row = &roster(&env, NOW).unwrap()["lanes"][0];
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
            roster(&env, NOW).unwrap()["lanes"][0]["cpu"]["reason"],
            "the caller pid is not running"
        );
        env.cpu = Some(Ok(Some(
            json!({"cpu_pct": 12.5, "rss_mb": 300, "process_count": 4, "extra": 1}),
        )));
        let cpu = &roster(&env, NOW).unwrap()["lanes"][0]["cpu"];
        assert_eq!(
            cpu["value"],
            json!({"cpu_pct": 12.5, "rss_mb": 300, "process_count": 4})
        );
        assert_eq!(cpu["as_of"], "1970-01-02T00:00:00Z");
        env.cpu = Some(Err("no /proc".into()));
        assert_eq!(
            roster(&env, NOW).unwrap()["lanes"][0]["cpu"]["reason"],
            "cpu sample failed: no /proc"
        );
    }
}
