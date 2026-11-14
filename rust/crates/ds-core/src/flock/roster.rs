//! `dunk_views.roster` (T38, slice 5): one row per lane - identity, state, current task,
//! inbox, cost, cpu. No screen reads: a UI polls this, and a roster that costs a pane read
//! per lane is a roster that times out. Every unknown field carries a reason (a cell).
//!
//! The Python reads `flock.resolve_lanes()`, `flock.tasks`, `flock.inbox`,
//! `dunk_activity.activity` and `dunk_load.sample`; each is a method of `RosterEnv` here so
//! the composition is tested without those stores, and the wiring is one impl later.

use std::collections::HashMap;

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
    /// The newest agent-state sample of every pane sampled at or after `since`, keyed by pane
    /// id: `{at, model, model_reason, thinking, argv}`, or the error text. Read-only.
    fn model_samples(&self, since: f64) -> Result<HashMap<String, Value>, String>;
    /// The daemon's own launch record of a pane: `{agent, argv, recorded_at, ...}`, None when
    /// the daemon did not launch it.
    fn launch(&self, pane_id: &str) -> Option<Value>;
    /// When the lane's pane root process started (epoch seconds), None when it cannot be read.
    fn pane_started_at(&self, lane: &Value) -> Option<f64>;
}

/// A model sample older than this is not shown as the lane's model (t4473).
pub const MODEL_FRESH_S: f64 = 15.0 * 60.0;
/// A launch record belongs to the pane's current process only when that process started
/// within this many seconds of the record (the record is written once the agent is up).
pub const LAUNCH_BIND_S: f64 = 120.0;
/// How far back the roster looks for a sample, so a stale one can be told from a missing one.
const MODEL_LOOKBACK_S: f64 = 24.0 * 3600.0;

/// The roster's `model` cell (t4473). It says only what is known and how it is known:
///
/// * `launch flag`: the model named on the command the daemon launched the lane with, with
///   the launch time. `observed` is false: this is what the lane was started as, not a reading.
///   The record must be for the kind of agent the lane runs, and the pane's root process must
///   have started within `LAUNCH_BIND_S` of it; a record of an earlier process is unknown.
/// * `process environment`: the model named in the lane process's environment, from the
///   newest agent-state sample, when that sample is at most `MODEL_FRESH_S` old. Also what the
///   lane was started as (`observed` false); `as_of` is when the environment was read.
/// * otherwise null with the reason.
///
/// Nothing today reads the model from the running agent itself, so no cell has `observed`
/// true; a later source that does may say so. A stale sample's value is never carried.
fn model_cell(
    launch: Option<&Value>,
    started: Option<f64>,
    sample: Option<&Value>,
    read_error: Option<&str>,
    harness: Option<&str>,
    now: f64,
) -> Value {
    let age = |at: Option<f64>| at.map(|at| (now - at).max(0.0).floor() as i64);
    let unknown = |reason: String, at: Option<f64>| {
        json!({
            "value": null,
            "effort": null,
            "source": null,
            "observed": false,
            "launched_at": null,
            "as_of": iso_cell(at),
            "age_s": age(at),
            "reason": reason,
        })
    };
    let argv_of = |record: &Value| -> Vec<String> {
        record
            .get("argv")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    if let Some(record) = launch {
        let argv = argv_of(record);
        let flagged =
            crate::state::flag(&argv, &crate::state::MODEL_FLAGS).filter(|model| !model.is_empty());
        if let Some(model) = flagged {
            // A launch record is kept after its process is gone, and the pane may run something
            // else by now. It counts only for the kind of agent the lane runs and only when the
            // pane's root process started when the record was written.
            let launched = record
                .get("recorded_at")
                .and_then(Value::as_f64)
                .filter(|at| *at != 0.0);
            let kind = argv
                .first()
                .map(|exe| exe.rsplit('/').next().unwrap_or(exe).to_lowercase())
                .unwrap_or_default();
            if !crate::state::AGENT_EXES.contains(&kind.as_str()) {
                return unknown(
                    "the launch record's command is not a known agent".into(),
                    None,
                );
            }
            if let Some(running) = harness.filter(|running| !running.eq_ignore_ascii_case(&kind)) {
                return unknown(
                    format!("the launch record is for {kind}, and the lane runs {running}"),
                    None,
                );
            }
            let Some(launched) = launched else {
                return unknown(
                    "the launch record has no time, so it cannot be tied to the lane's process"
                        .into(),
                    None,
                );
            };
            let Some(started) = started else {
                return unknown(
                    "the start of the lane's process could not be read, so the launch record cannot be tied to it".into(),
                    None,
                );
            };
            if (launched - started).abs() > LAUNCH_BIND_S {
                return unknown(
                    "the launch record is for an earlier process in this pane".into(),
                    None,
                );
            }
            return json!({
                "value": model,
                "effort": crate::state::flag(&argv, &crate::state::THINKING_FLAGS)
                    .filter(|effort| !effort.is_empty()),
                "source": "launch flag",
                "observed": false,
                "launched_at": iso_cell(Some(launched)),
                "as_of": iso_cell(Some(launched)),
                "age_s": age(Some(launched)),
                "reason": null,
            });
        }
    }
    if let Some(error) = read_error {
        return unknown(format!("model samples could not be read: {error}"), None);
    }
    let Some(sample) = sample else {
        return unknown(
            format!(
                "no model flag on the launch command and no agent-state sample in the last 24 hours ({})",
                model_reason(harness.unwrap_or(""))
            ),
            None,
        );
    };
    let at = sample.get("at").and_then(Value::as_f64);
    if !at.is_some_and(|at| (now - at).max(0.0) <= MODEL_FRESH_S) {
        return unknown(
            format!(
                "no model flag on the launch command, and the last sample is older than {} minutes",
                (MODEL_FRESH_S / 60.0) as i64
            ),
            at,
        );
    }
    let text = |key: &str| {
        sample
            .get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
    };
    let Some(model) = text("model") else {
        return unknown(
            text("model_reason")
                .unwrap_or("the sample names no model")
                .to_string(),
            at,
        );
    };
    if crate::state::flag(&argv_of(sample), &crate::state::MODEL_FLAGS).is_some() {
        // The sample copied a launch command the daemon no longer holds a record for: it
        // cannot say when that launch was, so the value is not reported.
        return unknown(
            "the sample repeats a launch flag, and there is no launch record to date it".into(),
            at,
        );
    }
    json!({
        "value": model,
        "effort": text("thinking"),
        "source": "process environment",
        "observed": false,
        "launched_at": null,
        "as_of": iso_cell(at),
        "age_s": age(at),
        "reason": null,
    })
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
    let lanes = env.lanes()?;
    // One read for every lane; a failed read makes every model unknown with that reason.
    let samples = env.model_samples(now - MODEL_LOOKBACK_S);
    for lane in lanes.iter().filter(|l| roster_lane(l)) {
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
            "model": model_cell(
                env.launch(pane_id).as_ref(),
                env.pane_started_at(lane),
                samples.as_ref().ok().and_then(|samples| samples.get(pane_id)),
                samples.as_ref().err().map(String::as_str),
                harness,
                now,
            ),
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
        samples: HashMap<String, Value>,
        samples_error: Option<String>,
        launches: HashMap<String, Value>,
        started: Option<f64>,
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
        fn launch(&self, pane_id: &str) -> Option<Value> {
            self.launches.get(pane_id).cloned()
        }
        fn pane_started_at(&self, _: &Value) -> Option<f64> {
            self.started
        }
        fn model_samples(&self, since: f64) -> Result<HashMap<String, Value>, String> {
            match &self.samples_error {
                Some(error) => Err(error.clone()),
                None => Ok(self
                    .samples
                    .iter()
                    .filter(|(_, sample)| sample["at"].as_f64().is_some_and(|at| at >= since))
                    .map(|(pane, sample)| (pane.clone(), sample.clone()))
                    .collect()),
            }
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
            json!({
                "value": null, "effort": null, "source": null, "observed": false,
                "launched_at": null, "as_of": null, "age_s": null,
                "reason": "no model flag on the launch command and no agent-state sample in the last 24 hours (pi does not report its model on the pane record)",
            })
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
            "no model flag on the launch command and no agent-state sample in the last 24 hours (codex does not report its model on the pane record)"
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

    fn model_of(launch: Option<Value>, samples: Value, error: Option<&str>) -> Value {
        model_on("claude", launch, Some(NOW - 10_831.0), samples, error)
    }

    /// The model cell of one lane running `harness` whose pane root process started at
    /// `started`.
    fn model_on(
        harness: &str,
        launch: Option<Value>,
        started: Option<f64>,
        samples: Value,
        error: Option<&str>,
    ) -> Value {
        // A lane with no detected agent is still a lane when it has a membership.
        let mut row = lane("%1", harness, Some("writer"));
        row["membership"] = json!({"scope": "global", "project": null});
        let env = Env {
            lanes: vec![row],
            samples: samples
                .as_object()
                .unwrap()
                .iter()
                .map(|(pane, sample)| (pane.clone(), sample.clone()))
                .collect(),
            samples_error: error.map(str::to_string),
            launches: launch
                .into_iter()
                .map(|record| ("%1".to_string(), record))
                .collect(),
            started,
            ..Env::default()
        };
        roster(&env, NOW).unwrap()["lanes"][0]["model"].clone()
    }

    /// A launch record as `start_agent` writes it: `agent` is the lane NAME, not the harness.
    fn launched(exe: &str, at: Option<f64>) -> Value {
        let mut record =
            json!({"agent": "writer", "argv": [exe, "--model", "opus-5.5", "--effort", "medium"]});
        if let Some(at) = at {
            record["recorded_at"] = json!(at);
        }
        record
    }

    #[test]
    fn a_launch_flag_is_reported_as_what_the_lane_was_launched_as_with_the_launch_time() {
        // Launched three hours ago; the pane's shell started 31 s before the record was written.
        // A launch fact does not go stale, and it is never "seen".
        let cell = model_of(
            Some(launched("/usr/local/bin/claude", Some(NOW - 10_800.0))),
            // A fresh sample that repeats the flag changes nothing.
            json!({"%1": {"at": NOW - 60.0, "model": "opus-5.5", "argv": ["claude", "--model", "opus-5.5"]}}),
            None,
        );
        assert_eq!(
            cell,
            json!({
                "value": "opus-5.5", "effort": "medium", "source": "launch flag",
                "observed": false, "launched_at": "1970-01-01T21:00:00Z",
                "as_of": "1970-01-01T21:00:00Z", "age_s": 10_800, "reason": null,
            })
        );
        // The same record counts when the harness of the lane is not known: the process tie
        // is what binds it.
        let unnamed = model_on(
            "",
            Some(launched("codex", Some(NOW - 10_800.0))),
            Some(NOW - 10_800.0 + LAUNCH_BIND_S),
            json!({}),
            None,
        );
        assert_eq!(unnamed["value"], "opus-5.5");
    }

    #[test]
    fn a_launch_record_is_used_only_for_the_process_and_agent_kind_it_was_written_for() {
        let reason = |cell: Value| {
            assert_eq!(cell["value"], Value::Null, "{cell}");
            assert!(!cell.to_string().contains("opus"), "{cell}");
            cell["reason"].as_str().unwrap().to_string()
        };
        let at = NOW - 10_800.0;
        // The pane's root process started long after the record: a later process in the pane.
        assert_eq!(
            reason(model_on(
                "claude",
                Some(launched("claude", Some(at))),
                Some(at + LAUNCH_BIND_S + 1.0),
                json!({}),
                None
            )),
            "the launch record is for an earlier process in this pane"
        );
        assert_eq!(
            reason(model_on(
                "claude",
                Some(launched("claude", Some(at))),
                Some(at - LAUNCH_BIND_S - 1.0),
                json!({}),
                None
            )),
            "the launch record is for an earlier process in this pane"
        );
        assert_eq!(
            reason(model_on("claude", Some(launched("claude", Some(at))), None, json!({}), None)),
            "the start of the lane's process could not be read, so the launch record cannot be tied to it"
        );
        assert_eq!(
            reason(model_on(
                "claude",
                Some(launched("claude", None)),
                Some(at),
                json!({}),
                None
            )),
            "the launch record has no time, so it cannot be tied to the lane's process"
        );
        // The record's command decides the kind, not its `agent` field (the lane name).
        assert_eq!(
            reason(model_on(
                "codex",
                Some(launched("claude", Some(at))),
                Some(at),
                json!({}),
                None
            )),
            "the launch record is for claude, and the lane runs codex"
        );
        assert_eq!(
            reason(model_on(
                "claude",
                Some(launched("bash", Some(at))),
                Some(at),
                json!({}),
                None
            )),
            "the launch record's command is not a known agent"
        );
        // A record with no model flag says nothing; the sample path answers.
        let no_flag = model_of(
            Some(json!({"agent": "writer", "recorded_at": at, "argv": ["claude"]})),
            json!({}),
            None,
        );
        assert!(no_flag["reason"]
            .as_str()
            .unwrap()
            .starts_with("no model flag on the launch command and no agent-state sample"));
    }

    #[test]
    fn a_fresh_process_environment_sample_is_a_launch_value_with_its_read_time_not_an_observation()
    {
        let cell = model_of(
            None,
            json!({"%1": {"at": NOW - 180.0, "model": "sol", "thinking": "high", "argv": ["pi"]}}),
            None,
        );
        assert_eq!(
            cell,
            json!({
                "value": "sol", "effort": "high", "source": "process environment",
                "observed": false, "launched_at": null,
                "as_of": "1970-01-01T23:57:00Z", "age_s": 180, "reason": null,
            })
        );
        let at_limit = model_of(
            None,
            json!({"%1": {"at": NOW - MODEL_FRESH_S, "model": "sol", "argv": ["pi"]}}),
            None,
        );
        assert_eq!(at_limit["value"], "sol");
    }

    #[test]
    fn a_stale_sample_is_unknown_and_its_value_is_not_carried() {
        let stale = model_of(
            None,
            json!({"%1": {"at": NOW - MODEL_FRESH_S - 1.0, "model": "sol", "thinking": "high", "argv": ["pi"]}}),
            None,
        );
        assert_eq!(
            stale,
            json!({
                "value": null, "effort": null, "source": null, "observed": false,
                "launched_at": null, "as_of": "1970-01-01T23:44:59Z", "age_s": 901,
                "reason": "no model flag on the launch command, and the last sample is older than 15 minutes",
            })
        );
        assert!(!stale.to_string().contains("sol"));
    }

    #[test]
    fn a_sampled_launch_flag_without_a_launch_record_is_unknown() {
        // The finding on PR 970: a re-sampled flag is not a reading of the running lane, and
        // without the launch record nothing dates it.
        let cell = model_of(
            None,
            json!({"%1": {"at": NOW - 60.0, "model": "opus-5.5", "argv": ["claude", "--model", "opus-5.5"]}}),
            None,
        );
        assert_eq!(cell["value"], Value::Null);
        assert_eq!(
            cell["reason"],
            "the sample repeats a launch flag, and there is no launch record to date it"
        );
        assert!(!cell.to_string().contains("opus"));
    }

    #[test]
    fn a_missing_or_modelless_or_unreadable_sample_is_unknown_with_the_reason() {
        let missing = model_of(
            None,
            json!({"%9": {"at": NOW, "model": "other-lane"}}),
            None,
        );
        assert_eq!(missing["value"], Value::Null);
        assert_eq!(missing["age_s"], Value::Null);
        assert_eq!(missing["observed"], false);

        let modelless = model_of(
            None,
            json!({"%1": {"at": NOW - 60.0, "model": null, "model_reason": "no --model flag in the argv", "argv": ["claude"]}}),
            None,
        );
        assert_eq!(modelless["value"], Value::Null);
        assert_eq!(modelless["reason"], "no --model flag in the argv");
        assert_eq!(modelless["age_s"], 60);

        let unreadable = model_of(None, json!({}), Some("database is locked"));
        assert_eq!(unreadable["value"], Value::Null);
        assert_eq!(
            unreadable["reason"],
            "model samples could not be read: database is locked"
        );
    }
}
