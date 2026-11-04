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
use std::{
    cell::RefCell,
    io::Write,
    process::{Command, Stdio},
    sync::Mutex,
};

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

fn base(command: &str) -> Value {
    json!({"command": command, "args": {}, "panes": [], "launches": {}, "dunks": []})
}
fn shell() -> Value {
    json!({"pane_id":"%1","cwd":"/work","agent_status":"unknown","terminal_title_stripped":"u@host: /work"})
}
fn launch_case() -> Value {
    let mut case = base("recover");
    case["panes"] = json!([shell()]);
    case["launches"] =
        json!({"%1":{"agent":"pi","cwd":"/work","argv":["pi","--resume","session"]}});
    case
}
fn compare_python(cases: Vec<Value>) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let mut child = Command::new("python3")
        .arg("-c")
        .arg(PYTHON_ORACLE)
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
        .write_all(
            serde_json::to_string(&json!({
                "source": include_str!("../../../../../dunk_core.py"),
                "api": include_str!("../../../../../dunk_api.py"), "cases": cases,
            }))
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let oracle: Value = serde_json::from_slice(&output.stdout).unwrap();
    let inputs = oracle["inputs"].as_array().unwrap();
    let outputs = oracle["outputs"].as_array().unwrap();
    assert_eq!(inputs.len(), cases.len());
    assert_eq!(outputs.len(), cases.len());
    for (index, (case, expected)) in inputs.iter().zip(outputs).enumerate() {
        assert_eq!(
            &run_rust(case),
            expected,
            "actual Python handler/producer case {index}: {case}"
        );
    }
}
#[test]
fn h_recover_matches_python() {
    let mut cases = vec![base("recover"), launch_case()];
    for raw in [
        "null",
        "false",
        "true",
        "0",
        "-0.0",
        "1",
        "-1",
        "1e999",
        "-1e999",
        "9999999999999999999999999999999999999999999",
        "\"\"",
        "\"false\"",
        "\"0\"",
        "[]",
        "[false]",
        "{}",
        "{\"x\":null}",
    ] {
        let value: Value = serde_json::from_str(raw).unwrap();
        let mut apply = launch_case();
        apply["args"] = json!({"apply": value});
        cases.push(apply);
        let mut record = base("recover");
        record["args"] = json!({"record": value});
        record["panes"] = json!([{"pane_id":"%1","agent":"pi","cwd":"/work","agent_session":{"value":"s","kind":"id"}}]);
        cases.push(record);
    }
    for role in [
        "owner",
        "system",
        "web",
        "desk",
        "planner",
        "worker",
        "reviewer",
        "anonymous",
    ] {
        for args in [
            json!({"apply":true}),
            json!({"z":null,"a":true,"record":true}),
        ] {
            let mut case = launch_case();
            case["role"] = json!(role);
            case["args"] = args;
            cases.push(case);
        }
    }
    for kind in [
        "DunkError",
        "RetryableDunkError",
        "Busy",
        "TypeError",
        "RuntimeError",
        "OSError",
    ] {
        for fail in ["list", "get", "submit", "version"] {
            let mut case = launch_case();
            case["args"] = json!({"apply":true});
            case["fail"] = json!(fail);
            case["error_kind"] = json!(kind);
            cases.push(case);
        }
    }
    for recheck in [
        Value::Null,
        json!({}),
        json!(false),
        json!({"pane_id":"%1","agent":"codex"}),
    ] {
        let mut case = launch_case();
        case["args"] = json!({"apply":true});
        case["recheck"] = json!({"%1":recheck});
        cases.push(case);
    }
    let mut partial = base("recover");
    partial["args"] = json!({"record":true});
    partial["panes"] = json!([{"pane_id":"%1","agent":"pi","agent_session":{"value":"s"}}, {"pane_id":"%2","agent":"pi","agent_session":true}]);
    cases.push(partial);
    let mut memory = base("recover");
    memory["panes"] = json!([shell()]);
    memory["dunks"] = json!([{"id":"d1","running":true,"target_pane_id":"%1","target_agent":"pi","target_cwd":"/work"}]);
    memory["memory"] = json!([["pi","/work",{"id":"memory-session"}]]);
    cases.push(memory.clone());
    memory["fail"] = json!("memory");
    cases.push(memory.clone());
    memory.as_object_mut().unwrap().remove("fail");
    memory["memory"] = json!([]);
    cases.push(memory);
    compare_python(cases);
}
#[test]
fn h_doctor_matches_python() {
    let mut cases = vec![base("doctor")];
    for role in [
        "owner",
        "system",
        "web",
        "desk",
        "planner",
        "worker",
        "reviewer",
        "anonymous",
    ] {
        let mut case = base("doctor");
        case["role"] = json!(role);
        case["panes"] = json!([shell()]);
        cases.push(case.clone());
        case["args"] = json!({"z":null,"a":true});
        cases.push(case);
    }
    for kind in [
        "DunkError",
        "RetryableDunkError",
        "Busy",
        "TypeError",
        "RuntimeError",
        "OSError",
    ] {
        for fail in ["list", "read", "guard", "version", "get"] {
            let mut case = base("doctor");
            case["panes"] = json!([shell()]);
            case["fail"] = json!(fail);
            case["error_kind"] = json!(kind);
            cases.push(case);
        }
    }
    for read in [Value::Null, json!(false), json!(""), json!({"raw":true})] {
        let mut case = base("doctor");
        case["panes"] = json!([shell()]);
        case["read"] = read;
        cases.push(case);
    }
    let mut warning = base("doctor");
    warning["panes"] = json!([{"pane_id":"%1","agent":"pi","cwd":"/now"}]);
    warning["dunks"] = json!([{"id":"d1","running":true,"target_pane_id":"%1","target_agent":"codex","target_cwd":"/old"}]);
    cases.push(warning);
    compare_python(cases);
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

// Actual Python dispatch, handlers and reached Flock methods with isolated adapters.
const PYTHON_ORACLE: &str = r#"
import ast,copy,json,math,os,re,shlex,sys,threading,time,types
import dunk_roles
payload=json.load(sys.stdin)
tree=ast.parse(payload['source'])
names={'_is_empty_shell','_resume_command','_launch_for','_with_cwd','_doctor_warnings','recover','doctor','_record_launch','_snapshot_launches','_guard_status'}
original=next(n for n in tree.body if isinstance(n,ast.ClassDef) and n.name=='Flock')
methods=[n for n in original.body if isinstance(n,ast.FunctionDef) and n.name in names]
cls=ast.ClassDef(name='Oracle',bases=[],keywords=[],body=methods,decorator_list=[])
exec(compile(ast.fix_missing_locations(ast.Module(body=[cls],type_ignores=[])),'actual_dunk_core.py','exec'))

class DunkError(Exception): pass
class RetryableDunkError(DunkError): retryable=True
class Busy(DunkError): retryable=True
api_tree=ast.parse(payload['api'])
api_names={'_opt','h_recover','h_doctor','dispatch'}
api_nodes=[n for n in api_tree.body if (isinstance(n,ast.FunctionDef) and n.name in api_names) or (isinstance(n,ast.ClassDef) and n.name=='Command') or (isinstance(n,ast.Assign) and any(isinstance(t,ast.Name) and t.id=='SUPERUSER_ONLY' for t in n.targets))]
exec(compile(ast.fix_missing_locations(ast.Module(body=api_nodes,type_ignores=[])),'actual_dunk_api.py','exec'))
COMMANDS_BY_NAME={}
for node in ast.walk(api_tree):
    if isinstance(node,ast.Call) and isinstance(node.func,ast.Name) and node.func.id=='Command' and node.args and isinstance(node.args[0],ast.Constant) and node.args[0].value in ('doctor','recover'):
        command=eval(compile(ast.Expression(node),'actual_command','eval'))
        COMMANDS_BY_NAME[command.name]=command

def run(c):
    calls=[]
    def call(name,args=None):
        calls.append([name,args])
        if c.get('fail')==name:
            kind=c.get('error_kind','RuntimeError')
            cls={'DunkError':DunkError,'RetryableDunkError':RetryableDunkError,'Busy':Busy,'TypeError':TypeError,'OSError':OSError,'RuntimeError':RuntimeError}[kind]
            raise cls('database busy; retry the request' if kind=='Busy' else name+' failed')
    class Backend:
        def list_panes(self): call('list'); return c['panes']
        def get_pane(self,id):
            call('get',id)
            if id in c.get('recheck',{}): return c['recheck'][id]
            return next((p for p in c['panes'] if p.get('pane_id')==id),None)
        def read_pane(self,id,lines,source): call('read',[id,lines,source]); return c.get('read','')
        def version(self): call('version'); return 'herdr fixture'
        def send_text_and_enter(self,id,command): call('submit',[id,command]); return c.get('submit',{}).get(id,[True,'sent'])
    class Memory:
        def latest_session(self,harness,cwd):
            call('memory',[harness,cwd])
            return next((row[2] for row in c['memory'] if row[:2]==[harness,cwd]),None)
    flock=Oracle(); flock.herdr=Backend(); flock.memory=Memory() if isinstance(c.get('memory'),list) else None
    flock.launches=c['launches']; flock.persist_enabled=True; flock._lock=threading.RLock()
    saved=False
    def save():
        nonlocal saved
        if flock.persist_enabled: saved=True
    flock._save_launches=save
    def guard(): call('guard'); return c.get('guard',{'installed':True})
    sys.modules['dunk_guard']=types.SimpleNamespace(status=guard)
    flock.dunkers=[types.SimpleNamespace(**dict({'id':'d1','running':False,'target_pane_id':None,'target_agent':None,'target_cwd':None},**d)) for d in c['dunks']]
    def role(_): call('role'); return c.get('role','owner')
    flock.caller_role=role
    def owner(*_): raise AssertionError('unexpected owner gate')
    flock.require_superuser=owner
    recover=flock.recover; doctor=flock.doctor
    def recover_provider(**kwargs): call('provider'); return recover(**kwargs)
    def doctor_provider(): call('provider'); return doctor()
    flock.recover=recover_provider; flock.doctor=doctor_provider
    try:
        result=dispatch(flock,c['command'],c.get('args'),None)
        outcome={'result':result}
    except DunkError as e: outcome={'error':str(e),'retryable':getattr(e,'retryable',False)}
    except Exception as e: outcome={'error':'internal error: '+str(e),'retryable':False}
    for row in flock.launches.values(): row.pop('recorded_at',None); row.pop('applied_at',None)
    return {'outcome':outcome,'calls':calls,'launches':flock.launches,'saved':saved}
# Both implementations must start from the same decoded metadata. Python floats
# are binary64, while arbitrary-precision serde JSON can retain decimal tokens.
# Preserve an independent copy before run mutates canonical launch state.
inputs=copy.deepcopy(payload['cases'])
outputs=[run(case) for case in payload['cases']]
def encode(value):
    if isinstance(value,float) and math.isinf(value): return '-1e999' if value<0 else '1e999'
    if isinstance(value,list): return '['+','.join(encode(v) for v in value)+']'
    if isinstance(value,dict): return '{'+','.join(json.dumps(k)+':'+encode(v) for k,v in value.items())+'}'
    return json.dumps(value,ensure_ascii=True)
print(encode({'inputs':inputs,'outputs':outputs}))
"#;
