use super::*;
use std::{
    cell::RefCell,
    io::Write,
    process::{Command, Stdio},
};

struct Env<'a> {
    flock: &'a Flock,
    config: Value,
    calls: RefCell<Vec<Value>>,
}
impl Env<'_> {
    fn call(&self, name: &str, args: Value) -> RecoveryResult<()> {
        assert!(
            self.flock.launches.try_lock().is_ok(),
            "launch lock held across {name}"
        );
        self.calls.borrow_mut().push(json!([name, args]));
        if self.config["fail"].as_str() == Some(name) {
            Err(python_error("RuntimeError", format!("{name} failed")))
        } else {
            Ok(())
        }
    }
}
impl RecoverEnv for Env<'_> {
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
            .find(|p| p.get("pane_id") == Some(id))
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
fn run_rust(config: &Value) -> Value {
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        config["record"].as_bool().unwrap_or(false),
    );
    for (id, row) in config["launches"].as_object().unwrap() {
        flock
            .launches
            .lock()
            .unwrap()
            .insert(id.clone(), row.as_object().unwrap().clone());
    }
    flock
        .dunks
        .load(json!({"dunkers": config["dunks"]}).as_object().unwrap());
    let before = flock.dunks.snapshot();
    let launches_before = flock.launches.lock().unwrap().clone();
    let env = Env {
        flock: &flock,
        config: config.clone(),
        calls: RefCell::new(Vec::new()),
    };
    let result = if config["doctor"] == true {
        flock.doctor(&env)
    } else {
        flock.recover(&env, config["apply"] == true, config["record"] == true)
    };
    assert_eq!(
        flock.dunks.snapshot(),
        before,
        "recovery never mutates dunk state"
    );
    if config["doctor"] == true {
        assert_eq!(
            *flock.launches.lock().unwrap(),
            launches_before,
            "doctor must not update launch memory"
        );
    }
    let mut launches = serde_json::Map::new();
    for (id, row) in flock.launches.lock().unwrap().iter() {
        let mut row = row.clone();
        row.remove("recorded_at");
        row.remove("applied_at");
        launches.insert(id.clone(), Value::Object(row));
    }
    let result = match result {
        Ok(value) => json!({"result": value}),
        Err(RecoveryCommandError::Python { kind, message }) => {
            json!({"error": message, "kind": kind})
        }
        Err(error) => panic!("unexpected backend error: {error}"),
    };
    json!({"outcome": result, "calls": env.calls.into_inner(), "launches": launches})
}

// Execute only the actual oracle methods, with deterministic in-memory collaborators.
// No live Flock construction/import, terminal, memory store, or host state is accessed.
const PYTHON_ORACLE: &str = r#"
import ast,json,os,re,shlex,sys,threading,time,types
payload=json.load(sys.stdin)
tree=ast.parse(payload['source'])
names={'_is_empty_shell','_resume_command','_launch_for','_with_cwd','_doctor_warnings','recover','doctor','_record_launch','_snapshot_launches','_guard_status'}
original=next(n for n in tree.body if isinstance(n,ast.ClassDef) and n.name=='Flock')
methods=[n for n in original.body if isinstance(n,ast.FunctionDef) and n.name in names]
cls=ast.ClassDef(name='Oracle',bases=[],keywords=[],body=methods,decorator_list=[])
exec(compile(ast.fix_missing_locations(ast.Module(body=[cls],type_ignores=[])),'actual_dunk_core.py','exec'))
def run(c):
    calls=[]
    def call(name,args=None):
        calls.append([name,args])
        if c.get('fail')==name: raise RuntimeError(name+' failed')
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
    flock.launches=c['launches']; flock.persist_enabled=c.get('record',False); flock._lock=threading.RLock()
    flock._save_launches=lambda: None
    def guard(): call('guard'); return c.get('guard',{'installed':True})
    sys.modules['dunk_guard']=types.SimpleNamespace(status=guard)
    flock.dunkers=[types.SimpleNamespace(**dict({'id':'d1','running':False,'target_pane_id':None,'target_agent':None,'target_cwd':None},**d)) for d in c['dunks']]
    try:
        result=flock.doctor() if c.get('doctor') else flock.recover(apply=c.get('apply',False),record=c.get('record',False))
        outcome={'result':result}
    except Exception as e: outcome={'kind':type(e).__name__,'error':str(e)}
    for row in flock.launches.values(): row.pop('recorded_at',None); row.pop('applied_at',None)
    return {'outcome':outcome,'calls':calls,'launches':flock.launches}
print(json.dumps([run(case) for case in payload['cases']],ensure_ascii=True))
"#;
fn shell(id: &str) -> Value {
    json!({"pane_id":id,"cwd":"/work","agent_status":"unknown","terminal_title_stripped":"u@host: /work"})
}
fn dunk(id: &str, pane: &str) -> Value {
    json!({"id":id,"running":true,"target_pane_id":pane,"target_agent":"codex","target_cwd":"/work"})
}
fn base() -> Value {
    json!({"panes":[shell("%1")],"dunks":[dunk("d1","%1")],"launches":{"%1":{"agent":"codex","session":"abc","cwd":"/work"}}})
}

#[test]
fn recover_doctor_match_actual_python_commands_and_error_order() {
    let mut cases = Vec::new();
    cases.push(base());
    let mut c = base();
    c["apply"] = json!(true);
    cases.push(c);
    for doctor in [false, true] {
        for panes in [
            json!([]),
            json!([shell("%1")]),
            json!([{"pane_id":"%1","agent":"claude","cwd":"/other","agent_session":{"value":"s"}}]),
        ] {
            let mut c = base();
            c["doctor"] = json!(doctor);
            c["panes"] = panes;
            cases.push(c);
        }
    }
    for recheck in [
        Value::Null,
        json!({}),
        json!({"agent":"codex"}),
        json!({"terminal_title_stripped":"human editor"}),
        shell("%1"),
    ] {
        let mut c = base();
        c["apply"] = json!(true);
        c["recheck"] = json!({"%1":recheck});
        cases.push(c);
    }
    for submit in [
        json!([false,{"reason":"busy"}]),
        json!([0, "no"]),
        json!(["yes", ["detail"]]),
    ] {
        let mut c = base();
        c["apply"] = json!(true);
        c["submit"] = json!({"%1":submit});
        cases.push(c);
    }
    for argv in [
        json!(["codex", "resume", "a'b"]),
        json!(["codex", "--model", "x"]),
        json!(["custom", "--resume", "s"]),
        json!(["cd ", "x"]),
        json!([]),
        json!("resume"),
        json!({"codex":1,"resume":2}),
        json!(["codex", false]),
        json!(["codex", 3]),
        json!(12),
    ] {
        let mut c = base();
        c["launches"]["%1"]["argv"] = argv;
        cases.push(c);
    }
    for session in [
        json!("a' b"),
        json!(""),
        Value::Null,
        json!(false),
        json!(42),
        json!(["s"]),
    ] {
        let mut c = base();
        c["launches"]["%1"]["session"] = session;
        cases.push(c);
    }
    for agent in ["pi", "claude", "opencode", "unknown"] {
        let mut c = base();
        c["launches"]["%1"]["agent"] = json!(agent);
        c["launches"]["%1"]["session"] = Value::Null;
        c["memory"] = json!([[agent,"/work",{"id":"mem ' session"}]]);
        cases.push(c);
    }
    for row in [
        json!({}),
        json!({"other":"x"}),
        json!({"id":false}),
        json!({"id":"s"}),
    ] {
        let mut c = base();
        c["launches"]["%1"]["session"] = Value::Null;
        c["memory"] = json!([["codex", "/work", row]]);
        cases.push(c);
    }
    for title in [
        json!("u@h:"),
        json!(" u@h: "),
        json!("u@h@x:"),
        json!("u\u{1c}@h:"),
        json!("u@h\u{1f}:"),
        json!("editor"),
        json!(23),
        json!([]),
    ] {
        let mut c = base();
        c["panes"][0]["terminal_title_stripped"] = title;
        cases.push(c);
    }
    for status in [
        Value::Null,
        json!(false),
        json!(0),
        json!("working"),
        json!([]),
    ] {
        let mut c = base();
        c["panes"][0]["agent_status"] = status;
        cases.push(c);
    }
    for fail in ["list", "memory", "get", "submit", "version"] {
        let mut c = base();
        c["apply"] = json!(true);
        c["fail"] = json!(fail);
        c["launches"]["%1"]["session"] = Value::Null;
        c["memory"] = json!([["codex","/work",{"id":"s"}]]);
        cases.push(c);
    }
    for fail in ["list", "read", "guard", "version", "get"] {
        let mut c = base();
        c["doctor"] = json!(true);
        c["fail"] = json!(fail);
        cases.push(c);
    }
    for read in [Value::Null, json!(false), json!(""), json!([])] {
        let mut c = base();
        c["doctor"] = json!(true);
        c["read"] = read;
        cases.push(c);
    }
    for guard in [
        json!({}),
        json!({"installed":0,"shadowing_herdr":"yes"}),
        json!(false),
    ] {
        let mut c = base();
        c["doctor"] = json!(true);
        c["guard"] = guard;
        cases.push(c);
    }
    for cwd in [
        "/work/.",
        "/work/x/..",
        "//work",
        "///work",
        "/other",
        "/a/../../work",
    ] {
        let mut c = base();
        c["doctor"] = json!(true);
        c["panes"][0]["cwd"] = json!(cwd);
        cases.push(c);
    }
    let mut c = base();
    c["record"] = json!(true);
    c["panes"][0]["agent"] = json!("pi");
    c["panes"][0]["agent_session"] = json!({"value":"recorded-live","kind":"id"});
    cases.push(c);
    // Orphan launch iteration must follow insertion order, not lexicographic pane id.
    let mut c = base();
    c["dunks"] = json!([]);
    c["panes"] = json!([shell("%9"), shell("%2")]);
    c["launches"] =
        json!({"%9":{"agent":"pi","session":"nine"},"%2":{"agent":"claude","session":"two"}});
    c["apply"] = json!(true);
    cases.push(c);
    // Multiple dunks to the same pane remain separate actions, unlike orphan launches.
    let mut c = base();
    c["dunks"] = json!([dunk("d1", "%1"), dunk("d2", "%1")]);
    c["apply"] = json!(true);
    cases.push(c);
    for entry in [
        json!({}),
        json!({"agent":"unknown","session":"x"}),
        json!({"agent":"pi","argv":["pi","--flag"],"session":null}),
    ] {
        let mut c = base();
        c["launches"]["%1"] = entry;
        cases.push(c);
    }
    for cwd in [
        json!("/path with ' quote"),
        json!(false),
        json!(0),
        json!(7),
        json!(["x"]),
    ] {
        let mut c = base();
        c["panes"][0]["cwd"] = cwd;
        cases.push(c);
    }
    for panes in [
        json!([{}]),
        json!([{"pane_id":null}]),
        json!([{"pane_id":[]}]),
        json!([false]),
    ] {
        let mut c = base();
        c["doctor"] = json!(true);
        c["panes"] = panes;
        cases.push(c);
    }
    let mut c = base();
    c["launches"]["%1"] = json!({"argv":["cd","/work","&&","codex"],"agent":"unknown"});
    cases.push(c);
    let mut c = base();
    c["dunks"] = json!([{"id":"d1","running":false,"target_pane_id":"%1"}]);
    cases.push(c);
    let input = json!({"source":include_str!("../../../../../dunk_core.py"),"cases":cases});
    let mut child = Command::new("python3")
        .args(["-c", PYTHON_ORACLE])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(serde_json::to_string(&input).unwrap().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    for (index, (case, expected)) in cases.iter().zip(expected).enumerate() {
        assert_eq!(
            run_rust(case),
            expected,
            "actual Python case {index}: {case}"
        );
    }
}

#[test]
fn recover_backend_error_category_is_not_suppressed() {
    struct Failing;
    impl RecoverEnv for Failing {
        fn list_panes(&self) -> RecoveryResult<Vec<Value>> {
            Err(DunkError::Retryable("backend timeout".into()).into())
        }
        fn get_pane(&self, _: &Value) -> RecoveryResult<Option<Value>> {
            panic!("not reached")
        }
        fn read_visible(&self, _: &Value, _: usize) -> RecoveryResult<Option<Value>> {
            panic!("not reached")
        }
        fn version(&self) -> RecoveryResult<Value> {
            panic!("not reached")
        }
        fn guard_status(&self) -> RecoveryResult<Value> {
            panic!("not reached")
        }
        fn has_memory(&self) -> bool {
            panic!("not reached")
        }
        fn latest_session(&self, _: &str, _: &Value) -> RecoveryResult<Option<Value>> {
            panic!("not reached")
        }
        fn submit_command(&self, _: &Value, _: &str) -> RecoveryResult<(Value, Value)> {
            panic!("not reached")
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        false,
    );
    for result in [flock.recover(&Failing, true, true), flock.doctor(&Failing)] {
        assert!(matches!(
            result,
            Err(RecoveryCommandError::Backend(DunkError::Retryable(_)))
        ));
    }
}
