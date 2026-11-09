use super::*;
use crate::api::{CallerGate, CommandEnv, Dispatcher};
use crate::flock::{
    panes::PaneInfoEnv,
    recovery_commands::{RecoverEnv, RecoveryResult},
    Flock,
};
use crate::server::RequestCtx;
use crate::terminal::{fakes::FakeTerminal, Terminal};
use serde_json::{json, Map};
use std::{cell::RefCell, sync::Mutex};

struct Env {
    config: Value,
    calls: RefCell<Vec<Value>>,
    terminal: Mutex<Box<dyn Terminal>>,
}
impl Env {
    fn call(&self, name: &str, args: Value) -> RecoveryResult<()> {
        self.calls.borrow_mut().push(json!([name, args]));
        if self.config["fail"] == name {
            let message = format!("{name} failed");
            return Err(
                match self.config["error_kind"].as_str().unwrap_or("RuntimeError") {
                    "DunkError" => RecoveryCommandError::Backend(DunkError::Refused(message)),
                    "RetryableDunkError" => {
                        RecoveryCommandError::Backend(DunkError::Retryable(message))
                    }
                    "Busy" => RecoveryCommandError::Backend(DunkError::Busy),
                    "TypeError" => RecoveryCommandError::Python {
                        kind: "TypeError",
                        message,
                    },
                    "OSError" => RecoveryCommandError::Python {
                        kind: "OSError",
                        message,
                    },
                    _ => RecoveryCommandError::Python {
                        kind: "RuntimeError",
                        message,
                    },
                },
            );
        }
        Ok(())
    }
}
impl PaneInfoEnv for Env {
    fn list_panes_grouped(&self) -> Vec<Value> {
        panic!("wrong pane provider")
    }
    fn get_pane(&self, _: &str) -> Option<Value> {
        panic!("wrong pane provider")
    }
}
impl CommandEnv for Env {
    fn now_s(&self) -> f64 {
        1234.0
    }
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
        &self.terminal
    }
    fn recover_env(&self) -> Result<&dyn RecoverEnv, DunkError> {
        self.calls.borrow_mut().push(json!(["provider", null]));
        if self.config["unwired"] == true {
            Err(DunkError::Refused("recovery adapter is not wired".into()))
        } else {
            Ok(self)
        }
    }
}
impl RecoverEnv for Env {
    fn list_panes(&self) -> RecoveryResult<Vec<Value>> {
        self.call("list", Value::Null)?;
        Ok(self.config["panes"].as_array().unwrap().clone())
    }
    fn get_pane(&self, id: &Value) -> RecoveryResult<Option<Value>> {
        self.call("get", id.clone())?;
        if let Some(value) = id.as_str().and_then(|id| self.config["recheck"].get(id)) {
            return Ok((!value.is_null()).then(|| value.clone()));
        }
        Ok(self.config["panes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|pane| pane.get("pane_id") == Some(id))
            .cloned())
    }
    fn read_visible(&self, id: &Value, lines: usize) -> RecoveryResult<Option<Value>> {
        self.call("read", json!([id, lines, "visible"]))?;
        let value = self.config.get("read").cloned().unwrap_or(json!(""));
        Ok((!value.is_null()).then_some(value))
    }
    fn version(&self) -> RecoveryResult<Value> {
        self.call("version", Value::Null)?;
        Ok(json!("herdr fixture"))
    }
    fn guard_status(&self) -> RecoveryResult<Value> {
        self.call("guard", Value::Null)?;
        Ok(self
            .config
            .get("guard")
            .cloned()
            .unwrap_or(json!({"installed": true})))
    }
    fn has_memory(&self) -> bool {
        self.config["memory"].is_array()
    }
    fn latest_session(&self, harness: &str, cwd: &Value) -> RecoveryResult<Option<Value>> {
        self.call("memory", json!([harness, cwd]))?;
        Ok(self.config["memory"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row[0] == harness && &row[1] == cwd)
            .map(|row| row[2].clone())
            .filter(|row| !row.is_null()))
    }
    fn submit_command(&self, id: &Value, command: &str) -> RecoveryResult<(Value, Value)> {
        self.call("submit", json!([id, command]))?;
        let value = id
            .as_str()
            .and_then(|id| self.config["submit"].get(id))
            .cloned()
            .unwrap_or(json!([true, "sent"]));
        Ok((value[0].clone(), value[1].clone()))
    }
}
struct Gate<'a>(&'a Env);
impl CallerGate for Gate<'_> {
    fn caller_role(&self, _: Option<&str>) -> Result<String, DunkError> {
        self.0.calls.borrow_mut().push(json!(["role", null]));
        Ok(self.0.config["role"].as_str().unwrap_or("owner").into())
    }
    fn require_superuser(&self, _: Option<&str>, _: &str) -> Result<(), DunkError> {
        panic!("recovery commands have no owner-only gate")
    }
}
fn clean_launches(mut launches: Value) -> Value {
    for row in launches.as_object_mut().unwrap().values_mut() {
        row.as_object_mut().unwrap().remove("recorded_at");
        row.as_object_mut().unwrap().remove("applied_at");
    }
    launches
}
fn run_rust(config: &Value) -> Value {
    let dir = crate::testsupport::dir("api-recovery");
    let path = dir.path().join("launches.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&json!({"version": 1, "panes": config["launches"]})).unwrap(),
    )
    .unwrap();
    let before_disk = std::fs::read(&path).unwrap();
    let flock = Flock::new(path.clone(), dir.path().join("lanes.json"), true);
    flock
        .dunks
        .load(json!({"dunkers": config["dunks"]}).as_object().unwrap());
    let before_dunks = flock.dunks.snapshot();
    let env = Env {
        config: config.clone(),
        calls: RefCell::new(Vec::new()),
        terminal: Mutex::new(Box::new(FakeTerminal::default())),
    };
    let request = RequestCtx::default();
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
    dispatcher.register_doctor_recover();
    let command = config["command"].as_str().unwrap();
    let result = dispatcher.dispatch(
        &context,
        &Gate(&env),
        command,
        config["args"].as_object().cloned(),
    );
    let outcome = match result {
        Ok(value) => json!({"result": value}),
        Err(error) => json!({"error": error.to_string(), "retryable": error.retryable()}),
    };
    assert_eq!(
        flock.dunks.snapshot(),
        before_dunks,
        "handlers mutated dunks"
    );
    let mut ids: Vec<String> = config["launches"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect();
    ids.extend(
        config["panes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|pane| pane["pane_id"].as_str().map(str::to_owned)),
    );
    let launches: Map<String, Value> = ids
        .into_iter()
        .filter_map(|id| flock.launch(&id).map(|row| (id, Value::Object(row))))
        .collect();
    let saved = std::fs::read(&path).unwrap() != before_disk;
    if command == "doctor" {
        assert!(!saved, "doctor wrote canonical launches");
        assert_eq!(
            clean_launches(Value::Object(launches.clone())),
            config["launches"]
        );
    }
    json!({"outcome": outcome, "calls": env.calls.borrow().clone(), "launches": clean_launches(Value::Object(launches)), "saved": saved})
}

// Captured from pinned pre-retirement Python commit 1b14a9fb15b7ccf8763b2e15a2ac428824cdc3a8.
// Each fixture contains both the input and Python-observed result, calls, launches and saved flag.
fn base(command: &str) -> Value {
    json!({"command": command, "args": {}, "panes": [], "launches": {}, "dunks": []})
}

fn compare_fixture(fixture: &str, case_count: usize) {
    let oracle: Value = serde_json::from_str(fixture).unwrap();
    let inputs = oracle["inputs"].as_array().unwrap();
    let outputs = oracle["outputs"].as_array().unwrap();
    assert_eq!(inputs.len(), case_count);
    assert_eq!(inputs.len(), outputs.len());
    for (index, (case, expected)) in inputs.iter().zip(outputs).enumerate() {
        assert_eq!(
            &run_rust(case),
            expected,
            "pinned Python oracle case {index}: {case}"
        );
    }
}

#[test]
fn h_recover_matches_pinned_python_fixture() {
    compare_fixture(include_str!("fixtures/recover_python_1b14a9fb.json"), 84);
}

#[test]
fn h_doctor_matches_pinned_python_fixture() {
    compare_fixture(include_str!("fixtures/doctor_python_1b14a9fb.json"), 52);
}

#[test]
fn recovery_handlers_refuse_missing_provider_after_role_and_argument_checks() {
    for command in ["recover", "doctor"] {
        let mut case = base(command);
        case["unwired"] = json!(true);
        let result = run_rust(&case);
        assert_eq!(
            result["outcome"],
            json!({"error":"recovery adapter is not wired","retryable":false})
        );
        assert_eq!(result["calls"], json!([["role", null], ["provider", null]]));
        case["args"] = json!({"z":true});
        let result = run_rust(&case);
        assert_eq!(result["calls"], json!([["role", null]]));
        assert_eq!(
            result["outcome"]["error"],
            format!("{command}: unknown argument(s): z")
        );
    }
    // Exercise the trait's actual default separately, rather than the fixture override.
    struct Unwired(Mutex<Box<dyn Terminal>>);
    impl PaneInfoEnv for Unwired {
        fn list_panes_grouped(&self) -> Vec<Value> {
            panic!("unwired")
        }
        fn get_pane(&self, _: &str) -> Option<Value> {
            panic!("unwired")
        }
    }
    impl CommandEnv for Unwired {
        fn now_s(&self) -> f64 {
            1234.0
        }
        fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
            &self.0
        }
    }
    let env = Unwired(Mutex::new(Box::new(FakeTerminal::default())));
    assert_eq!(
        env.recover_env().err().unwrap().to_string(),
        "recovery adapter is not wired"
    );
}
