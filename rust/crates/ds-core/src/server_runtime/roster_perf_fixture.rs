//! Private timing fixture: the same 20-lane roster core handler, with only CPU sampling varied.
//! Metadata is synthetic; the registered caller is this test process. No daemon socket or pane.
use super::*;
use crate::flock::{
    roster::{roster, RosterEnv},
    Flock,
};
use crate::server::RequestCtx;
use crate::terminal::{fakes::FakeTerminal, Terminal};
use std::{collections::HashMap, sync::Mutex, time::Instant};

struct Fixture<'a> {
    adapter: &'a RequestRuntimeAdapter<'a>,
    cached: bool,
    pid: i64,
    start: i64,
}
impl RosterEnv for Fixture<'_> {
    fn lanes(&self) -> Result<Vec<Value>, String> {
        Ok((0..20).map(|i| json!({"pane_id":format!("%fixture{i}"),"agent":"codex","agent_name":format!("fixture-{i}")})).collect())
    }
    fn leases_for_pane(&self, _: &str, _: f64) -> Vec<Value> {
        vec![
            json!({"task_id":"fixture-task","caller_pid":self.pid,"caller_start":self.start,"caller_name":"private-fixture"}),
        ]
    }
    fn task(&self, _: &str) -> Option<Value> {
        None
    }
    fn inbox_count(&self, _: &str) -> Option<(i64, Option<f64>)> {
        Some((0, None))
    }
    fn activity(&self, _: &Value, _: f64) -> Result<Value, String> {
        Ok(json!({"state":"idle","since":null,"source":"fixture","reason":null}))
    }
    fn cpu_sample(&self, caller: &Value, now: f64) -> Result<Option<Value>, String> {
        if self.cached {
            return self.adapter.cpu_sample(caller, now);
        }
        // Pre-change runtime behavior: rebuild the process map for each caller.
        let sampled = crate::load::sample(&[("private-fixture".into(), self.pid, self.start)], now);
        Ok(sampled["callers"].as_array().unwrap().first().cloned())
    }
    fn model_samples(&self, _: f64) -> Result<HashMap<String, Value>, String> {
        Ok(HashMap::new())
    }
    fn launch(&self, _: &str) -> Option<Value> {
        None
    }
    fn pane_started_at(&self, _: &Value) -> Option<f64> {
        None
    }
    fn launch_process_live(&self, _: &Value, _: i64, _: &str) -> Option<bool> {
        None
    }
}

#[test]
#[ignore = "explicit t5254 before/after measurement; no timing threshold"]
fn private_roster_cpu_before_after() {
    let dir = tempfile::tempdir().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
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
    let pid = i64::from(std::process::id());
    let start = crate::load::stat(pid).unwrap().starttime;
    let mut timings = Vec::new();
    for cached in [false, true] {
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);
        let env = Fixture {
            adapter: &adapter,
            cached,
            pid,
            start,
        };
        let began = Instant::now();
        let value = roster(&env, 1.0).unwrap();
        let elapsed = began.elapsed().as_secs_f64() * 1000.0;
        let rows = value["lanes"].as_array().unwrap();
        assert_eq!(rows.len(), 20);
        for row in rows {
            assert!(row["cpu"]["value"]["process_count"].as_i64().unwrap() >= 1);
        }
        assert_eq!(adapter.roster_ppids.get().is_some(), cached);
        timings.push(elapsed);
    }
    println!(
        "{}",
        json!({"fixture":"20 synthetic leased lanes, one live test caller; roster core handler","before_ms":timings[0],"after_ms":timings[1],"before_parent_walks":20,"after_parent_walks":1,"live_root_start_checks":20})
    );
}
