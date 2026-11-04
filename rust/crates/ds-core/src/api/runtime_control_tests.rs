use super::*;
use crate::api::{CallerGate, CommandEnv, Dispatcher};
use crate::flock::{panes::PaneInfoEnv, Flock};
use crate::server::RequestCtx;
use crate::terminal::{fakes::FakeTerminal, Terminal};
use std::{
    cell::RefCell,
    io::Write,
    process::{Command, Stdio},
    sync::Mutex,
};

struct Env {
    config: Value,
    calls: RefCell<Vec<Value>>,
    state: RefCell<Value>,
    terminal: Mutex<Box<dyn Terminal>>,
}
impl Env {
    fn call(&self, name: &str, args: Value, default: Value) -> ControlResult<Value> {
        self.calls.borrow_mut().push(json!([name, args]));
        if self.config["fail"] == name {
            let message = format!("{name} failed");
            return Err(
                match self.config["kind"].as_str().unwrap_or("RuntimeError") {
                    "DunkError" => ControlError::Backend(DunkError::Refused(message)),
                    "Busy" => ControlError::Backend(DunkError::Busy),
                    "RetryableDunkError" => ControlError::Backend(DunkError::Retryable(message)),
                    "TaskError" => python("TaskError", message),
                    "InboxError" => python("InboxError", message),
                    "TypeError" => python("TypeError", message),
                    "OSError" => python("OSError", message),
                    _ => python("RuntimeError", message),
                },
            );
        }
        Ok(self.config["responses"]
            .get(name)
            .cloned()
            .unwrap_or(default))
    }
    fn unit(&self, name: &str, args: Value) -> ControlResult<()> {
        self.call(name, args, Value::Null).map(|_| ())
    }
}
impl PaneInfoEnv for Env {
    fn list_panes_grouped(&self) -> Vec<Value> {
        panic!("wrong provider")
    }
    fn get_pane(&self, _: &str) -> Option<Value> {
        panic!("wrong provider")
    }
}
impl CommandEnv for Env {
    fn now_s(&self) -> f64 {
        25000.0
    }
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
        &self.terminal
    }
    fn runtime_control_env(&self) -> Result<&dyn RuntimeControlEnv, DunkError> {
        if self.config["unwired"] == true {
            Err(DunkError::Refused(
                "runtime control adapter is not wired".into(),
            ))
        } else {
            Ok(self)
        }
    }
}
impl DispatchQueue for Env {
    fn add(&self, b: &Value, t: &Value, l: &Value) -> ControlResult<Value> {
        self.call("add", json!([b, t, l]), json!({"id":7}))
    }
    fn items(&self) -> ControlResult<Vec<Value>> {
        Ok(self
            .call("items", json!([]), json!([]))?
            .as_array()
            .unwrap()
            .clone())
    }
    fn remove(&self, id: &Value) -> ControlResult<Value> {
        self.call("remove", json!([id]), json!({"removed":true}))
    }
}
impl AllocationStore for Env {
    fn status(&self) -> ControlResult<Value> {
        self.call("status", json!([]), json!({"workers":[],"offers":[]}))
    }
    fn register(
        &self,
        p: &Value,
        po: &Value,
        r: &Value,
        s: &Value,
        c: Option<&str>,
        en: &Value,
    ) -> ControlResult<Value> {
        self.call(
            "register",
            json!([p, po, r, s, c, en]),
            json!({"registered":true}),
        )
    }
    fn offer(&self, t: &Value, p: &Value, r: &Value, c: Option<&str>) -> ControlResult<Value> {
        self.call("offer", json!([t, p, r, c]), json!({"offered":true}))
    }
    fn review(
        &self,
        t: &Value,
        r: &Value,
        b: &Value,
        p: &Value,
        req: &Value,
        c: Option<&str>,
    ) -> ControlResult<Value> {
        self.call(
            "review",
            json!([t, r, b, p, req, c]),
            json!({"reviewed":true}),
        )
    }
}
impl RuntimeControlEnv for Env {
    fn caller_role(&self, c: Option<&str>) -> ControlResult<String> {
        Ok(self
            .call("role", json!([c]), self.config["role"].clone())?
            .as_str()
            .unwrap()
            .into())
    }
    fn dispatch_queue(&self) -> ControlResult<&dyn DispatchQueue> {
        self.unit("queue", json!([]))?;
        Ok(self)
    }
    fn dispatch_state(&self) -> ControlResult<Value> {
        self.call("state", json!([]), json!({}))
    }
    fn resolve_lanes(&self) -> ControlResult<Vec<Value>> {
        Ok(self
            .call(
                "lanes",
                json!([]),
                json!([{"pane_id":"%7","agent_status":"idle"}]),
            )?
            .as_array()
            .unwrap()
            .clone())
    }
    fn dispatch_allowlist(&self, a: &Args, l: &[Value]) -> ControlResult<BTreeSet<String>> {
        let v = self.call("allow", json!([a, l]), json!(["%7"]))?;
        Ok(v.as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().into())
            .collect())
    }
    fn task_list(&self, d: bool, l: usize) -> ControlResult<Value> {
        self.call("tasks", json!([d, l]), json!({"tasks":[]}))
    }
    fn task_leases(&self, id: &Value) -> ControlResult<Vec<Value>> {
        Ok(self
            .call("leases", json!([id]), json!([]))?
            .as_array()
            .unwrap()
            .clone())
    }
    fn is_retired(&self, id: &Value) -> ControlResult<bool> {
        Ok(self
            .call("retired", json!([id]), json!(false))?
            .as_bool()
            .unwrap())
    }
    fn all_retired_count(&self) -> ControlResult<usize> {
        Ok(self
            .call("all_retired", json!([]), json!(2))?
            .as_u64()
            .unwrap() as usize)
    }
    fn now_s(&self) -> f64 {
        25000.0
    }
    fn dispatch_tick(&self, i: DispatchTick<'_>) -> ControlResult<DispatchTickResult> {
        // Assert the tick receives precisely the queue opened for this request.
        let original: &dyn DispatchQueue = self;
        assert!(std::ptr::addr_eq(i.queue, original));
        let state = self.call(
            "tick",
            json!([i.panes, i.derived, i.allow, i.dry_run, i.caller]),
            json!({"dispatched":[],"marker":"fixture tick"}),
        )?;
        Ok(DispatchTickResult {
            state,
            expired: vec!["%9".into(), "%7".into(), "%9".into()],
        })
    }
    fn allocation_store(&self) -> ControlResult<&dyn AllocationStore> {
        self.unit("store", json!([]))?;
        Ok(self)
    }
    fn allocation_last_result(&self) -> ControlResult<Value> {
        self.call(
            "last_result",
            json!([]),
            self.state.borrow()["last_result"].clone(),
        )
    }
    fn require_turn_caller(&self, c: Option<&str>) -> ControlResult<()> {
        self.unit("turn", json!([c, c]))
    }
    fn require_superuser(&self, c: Option<&str>, a: &str) -> ControlResult<()> {
        self.unit("superuser", json!([c, a]))
    }
    fn task_get(&self, t: &Value) -> ControlResult<Value> {
        self.call("task_get", json!([t]), json!({"id":"t9"}))
    }
    fn authorize_task_complete(&self, t: &Value, c: Option<&str>, o: bool) -> ControlResult<()> {
        self.unit("authorize", json!([t, c, o]))
    }
    fn allocation_reset_last_tick(&self) -> ControlResult<()> {
        self.unit("reset", json!([]))?;
        self.state.borrow_mut()["last_tick"] = json!(0);
        Ok(())
    }
    fn next_job_wake(&self) -> ControlResult<()> {
        self.unit("wake", json!([]))?;
        self.state.borrow_mut()["wakes"] = json!(1);
        Ok(())
    }
    fn stop_all(&self) -> ControlResult<()> {
        self.unit("stop_all", json!([]))?;
        self.state.borrow_mut()["stops"] = json!(1);
        Ok(())
    }
}
struct Gate<'a>(&'a Env);
impl CallerGate for Gate<'_> {
    fn caller_role(&self, c: Option<&str>) -> Result<String, DunkError> {
        RuntimeControlEnv::caller_role(self.0, c).map_err(|e| wire(e, false))
    }
    fn require_superuser(&self, c: Option<&str>, a: &str) -> Result<(), DunkError> {
        RuntimeControlEnv::require_superuser(self.0, c, a).map_err(|e| wire(e, false))
    }
}
fn initial() -> Value {
    json!({"last_tick":123,"last_result":{"cached":17},"wakes":0,"stops":0})
}
fn run_rust(config: &Value) -> Value {
    let dir = crate::testsupport::dir("runtime-control-api");
    let flock = Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        true,
    );
    let env = Env {
        config: config.clone(),
        calls: RefCell::new(vec![]),
        state: RefCell::new(initial()),
        terminal: Mutex::new(Box::new(FakeTerminal::default())),
    };
    let request = RequestCtx {
        self_pane_id: Some("%7".into()),
        ..Default::default()
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let context = HandlerContext {
        flock: &flock,
        request: &request,
        env: &env,
        runtime: runtime.handle(),
    };
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_runtime_control();
    assert_eq!(
        dispatcher.registration_attempts(),
        ["dispatch", "allocation", "shutdown"]
    );
    let result = dispatcher.dispatch(
        &context,
        &Gate(&env),
        config["command"].as_str().unwrap(),
        config["args"].as_object().cloned(),
    );
    let outcome = match result {
        Ok(v) => json!({"result":v}),
        Err(e) => json!({"error":e.to_string(),"retryable":e.retryable()}),
    };
    json!({"outcome":outcome,"calls":env.calls.into_inner(),"state":env.state.into_inner()})
}
fn run_python(configs: &[Value]) -> Vec<Value> {
    let script = include_str!("runtime_control_oracle.py");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let mut child = Command::new("python3")
        .arg("-c")
        .arg(script)
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(serde_json::to_string(configs).unwrap().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}
fn case(command: &str, args: Value) -> Value {
    json!({"command":command,"args":args,"role":"owner","responses":{}})
}
fn check(cases: Vec<Value>) {
    let expected = run_python(&cases);
    for (c, p) in cases.iter().zip(expected) {
        assert_eq!(run_rust(c), p, "config {c}");
    }
}
#[test]
fn h_dispatch_matches_python() {
    let mut cases = vec![];
    for action in [
        Value::Null,
        json!(false),
        json!(0),
        json!(""),
        json!([]),
        json!({}),
        json!(true),
        json!(1),
        json!([1]),
        json!({"x":2}),
        json!("bad"),
        json!("status"),
        json!("list"),
        json!("tick"),
        json!("add"),
        json!("remove"),
    ] {
        cases.push(case("dispatch", json!({"action":action})));
    }
    cases.push(case("dispatch", json!({})));
    for value in [
        Value::Null,
        json!(false),
        json!(0),
        json!(" hi "),
        json!([1]),
        json!({"raw":true}),
        json!(1e100),
    ] {
        cases.push(case(
            "dispatch",
            json!({"action":"add","brief":value,"task_id":value,"lane":value}),
        ));
        cases.push(case("dispatch", json!({"action":"remove","id":value})));
        cases.push(case("dispatch", json!({"action":"tick","dry_run":value})));
    }
    for action in ["add", "remove", "tick", "list", "status"] {
        let mut c = case("dispatch", json!({"action":action}));
        c["role"] = json!("worker");
        cases.push(c);
    }
    check(cases);
}
#[test]
fn h_allocation_matches_python() {
    let mut cases = vec![case("allocation", json!({}))];
    for action in [
        Value::Null,
        json!(false),
        json!(0),
        json!(""),
        json!([]),
        json!({}),
        json!("status"),
        json!("register"),
        json!("offer"),
        json!("review"),
        json!("bad"),
    ] {
        cases.push(case("allocation", json!({"action":action})));
    }
    for value in [
        Value::Null,
        json!(false),
        json!(0),
        json!(""),
        json!(["x", 2]),
        json!({"raw":true}),
        json!(1e100),
    ] {
        cases.push(case("allocation",json!({"action":"register","pane_id":"%7","pools":value,"roles":value,"skills":value,"enabled":value})));
        cases.push(case(
            "allocation",
            json!({"action":"offer","task_id":value,"pool":value,"requires":value}),
        ));
        cases.push(case("allocation",json!({"action":"review","task_id":value,"revision":value,"body":value,"pool":value,"requires":value})));
    }
    cases.push(case(
        "allocation",
        json!({"action":"register","pane_id":"%7"}),
    ));
    check(cases);
}
#[test]
fn h_shutdown_matches_python() {
    let mut cases = vec![case("shutdown", json!({}))];
    for value in [
        Value::Null,
        json!(true),
        json!(false),
        json!(0),
        json!(1),
        json!(1.0),
        json!("TRUE"),
        json!(" yes "),
        json!("false"),
        json!([]),
        json!([1]),
        json!({"on":true}),
    ] {
        cases.push(case("shutdown", json!({"stop_all":value})));
    }
    for role in ["worker", "system", "web", "desk"] {
        let mut c = case("shutdown", json!({"stop_all":true}));
        c["role"] = json!(role);
        cases.push(c);
    }
    check(cases);
}
#[test]
fn runtime_control_errors_preserve_order_and_partial_mutation() {
    let mut cases = vec![];
    for (command, args, names) in [
        (
            "dispatch",
            json!({"action":"tick"}),
            vec![
                "queue",
                "lanes",
                "allow",
                "tasks",
                "retired",
                "state",
                "items",
                "tick",
                "all_retired",
            ],
        ),
        (
            "allocation",
            json!({"action":"register","pane_id":"%7"}),
            vec![
                "store",
                "turn",
                "superuser",
                "lanes",
                "register",
                "reset",
                "wake",
            ],
        ),
        (
            "allocation",
            json!({"action":"review","task_id":"t9"}),
            vec!["store", "task_get", "authorize", "review", "reset", "wake"],
        ),
        (
            "allocation",
            json!({"action":"status"}),
            vec!["store", "status", "last_result"],
        ),
        ("shutdown", json!({"stop_all":true}), vec!["stop_all"]),
    ] {
        for name in names {
            for kind in [
                "DunkError",
                "TaskError",
                "InboxError",
                "TypeError",
                "OSError",
                "RetryableDunkError",
            ] {
                let mut c = case(command, args.clone());
                c["fail"] = json!(name);
                c["kind"] = json!(kind);
                cases.push(c);
            }
        }
    }
    check(cases);
}
#[test]
fn runtime_control_tick_filters_and_lease_failure_match_python() {
    let mut cases = vec![];
    for leases in [json!([]), json!([{"pane_id":"%7"}])] {
        let mut c = case("dispatch", json!({"action":"tick","dry_run":"false"}));
        c["responses"] = json!({"tasks":{"tasks":[{"id":"t1","title":"do it","status":"open","created_at":1,"priority":"high"}]},"leases":leases});
        cases.push(c);
    }
    let mut c = case("dispatch", json!({"action":"tick"}));
    c["responses"]["tasks"] =
        json!({"tasks":[{"id":"t1","title":"do it","status":"open","created_at":1}]});
    c["fail"] = json!("leases");
    cases.push(c);
    let mut c = case("dispatch", json!({"action":"tick"}));
    c["responses"]["lanes"] = json!([{"pane_id":"%7","agent_status":"working"},{"pane_id":"%8","agent_status":"unknown"},{"pane_id":"%9"}]);
    cases.push(c);
    check(cases);
}
#[test]
fn runtime_control_missing_provider_refuses_without_mutation() {
    for (command, args) in [
        ("dispatch", json!({})),
        ("allocation", json!({})),
        ("shutdown", json!({"stop_all":true})),
    ] {
        let mut c = case(command, args);
        c["unwired"] = json!(true);
        let result = run_rust(&c);
        assert_eq!(
            result["outcome"]["error"],
            "runtime control adapter is not wired"
        );
        assert_eq!(result["state"], initial());
    }
    let mut c = case("shutdown", json!({"stop_all":false}));
    c["unwired"] = json!(true);
    assert_eq!(
        run_rust(&c)["outcome"]["result"],
        json!({"shutting_down":true,"stopped_all":false})
    );
}

#[test]
fn runtime_control_raw_state_and_numeric_edges_match_python() {
    let mut cases = vec![];
    for state in [
        json!({"dispatched_1h":3,"idle_unfilled":["%7"],"blocked":[{"name":"w","reason":"busy"}],"dead":[{"name":"gone"}],"queue_empty":true}),
        json!({"x":1}),
        json!([1]),
        json!({"blocked":[{}]}),
        json!({"dead":[{"name":7}]}),
        json!({"idle_unfilled":3}),
        json!({"blocked":"x"}),
        json!({"idle_unfilled":"abc"}),
    ] {
        let mut c = case("dispatch", json!({"action":"status"}));
        c["responses"]["state"] = state;
        cases.push(c);
    }
    for token in [
        "1e999",
        "-1e999",
        "9999999999999999999999999999999999999999999999999",
    ] {
        let v: Value = serde_json::from_str(token).unwrap();
        cases.push(case("dispatch", json!({"action":v})));
        cases.push(case("allocation", json!({"action":v})));
        cases.push(case("shutdown", json!({"stop_all":v})));
    }
    cases.push(case("shutdown", json!({"stop_all":"\u{1c}YES\u{1f}"})));
    for task in [json!(7), json!([]), json!("oops")] {
        let mut c = case("dispatch", json!({"action":"tick"}));
        c["responses"]["tasks"] = json!({"tasks":[task]});
        cases.push(c);
    }
    check(cases);
}
