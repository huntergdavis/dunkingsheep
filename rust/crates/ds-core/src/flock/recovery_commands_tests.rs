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
    json!({"outcome": result, "calls": env.calls.into_inner(), "launches": launches, "saved": flock.launches_file.exists()})
}

// Execute only the actual oracle methods, with deterministic in-memory collaborators.
// No live Flock construction/import, terminal, memory store, or host state is accessed.
const PYTHON_ORACLE: &str = r#"
import ast,copy,json,os,re,shlex,sys,threading,time,types
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
    saved=False
    def save():
        nonlocal saved
        if flock.persist_enabled: saved=True
    flock._save_launches=save
    def guard(): call('guard'); return c.get('guard',{'installed':True})
    sys.modules['dunk_guard']=types.SimpleNamespace(status=guard)
    flock.dunkers=[types.SimpleNamespace(**dict({'id':'d1','running':False,'target_pane_id':None,'target_agent':None,'target_cwd':None},**d)) for d in c['dunks']]
    try:
        result=flock.doctor() if c.get('doctor') else flock.recover(apply=c.get('apply',False),record=c.get('record',False))
        outcome={'result':result}
    except Exception as e: outcome={'kind':type(e).__name__,'error':str(e)}
    for row in flock.launches.values(): row.pop('recorded_at',None); row.pop('applied_at',None)
    return {'outcome':outcome,'calls':calls,'launches':flock.launches,'saved':saved}
# Both implementations must start from the same decoded metadata. Python floats
# are binary64, while arbitrary-precision serde JSON can retain decimal tokens.
# Preserve an independent copy before run mutates canonical launch state.
inputs=copy.deepcopy(payload['cases'])
outputs=[run(case) for case in payload['cases']]
print(json.dumps({'inputs':inputs,'outputs':outputs},ensure_ascii=True))
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
        json!(false),
        json!(0),
        json!(""),
        json!([]),
        json!(1),
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
    for session in [
        Value::Null,
        json!(false),
        json!([]),
        json!(""),
        json!(3),
        json!(["bad"]),
        json!({"agent":"pi","value":0}),
        json!({"agent":false,"value":"s"}),
    ] {
        let mut c = base();
        c["record"] = json!(true);
        c["panes"][0]["agent_session"] = session;
        cases.push(c);
    }
    let mut c = base();
    c["record"] = json!(true);
    c["panes"] = json!([{"pane_id":"%1","agent":"pi","agent_session":{"value":"s"}}, {"pane_id":"%2","agent_session":7}]);
    cases.push(c);
    let mut c = base();
    c["record"] = json!(true);
    c["launches"]["%1"]["source"] = Value::Null;
    c["panes"][0]["agent"] = json!("pi");
    c["panes"][0]["agent_session"] = json!({"value":"s"});
    cases.push(c);
    // Review t3093: Python recursive numeric equality must not rewrite stored
    // values, add source, or save. Session-kind also exercises false/0 because
    // a falsey session value itself is skipped earlier by the Python oracle.
    let raw_pairs = [
        ("1", "true"),
        ("true", "1"),
        ("0", "false"),
        ("false", "0"),
        ("1", "1.0"),
        ("1.0", "true"),
        ("0", "-0.0"),
        ("-0.0", "false"),
        ("1", "1.5"),
        ("9007199254740992", "9007199254740992.0"),
        ("9007199254740993", "9007199254740992.0"),
        ("9007199254740993", "9007199254740993.0"),
        ("-9007199254740993", "-9007199254740992.0"),
        ("18446744073709551616", "18446744073709551616.0"),
        ("18446744073709551617", "18446744073709551616.0"),
        ("1000000000000000000000000000000", "1e30"),
        ("1000000000000000019884624838656", "1e30"),
        ("1000000000000000019884624838657", "1e30"),
        ("1e30", "1.000000000000000019884624838656e30"),
        ("0.1", "0.10000000000000001"),
        ("0.1", "0.10000000000000002"),
        ("1", "\"1\""),
        ("null", "0"),
        ("null", "false"),
        ("[1]", "[true]"),
        ("[false,1.0]", "[0,true]"),
        ("[0,1]", "[1,0]"),
        ("{\"x\":1}", "{\"x\":true}"),
        (
            "{\"x\":[1,{\"y\":false}],\"z\":2}",
            "{\"z\":2.0,\"x\":[true,{\"y\":0}]}",
        ),
        ("{\"x\":1}", "{\"y\":1}"),
        ("{\"x\":9007199254740993}", "{\"x\":9007199254740992.0}"),
    ];
    for (left, right) in raw_pairs {
        let left: Value = serde_json::from_str(left).unwrap();
        let right: Value = serde_json::from_str(right).unwrap();
        for key in ["session", "session_kind"] {
            let mut c = json!({"panes":[{"pane_id":"%1","agent":"pi","agent_session":{"value":"s"}}],"dunks":[],"launches":{"%1":{"agent":"pi","session":"s"}},"record":true});
            c["launches"]["%1"][key] = left.clone();
            c["panes"][0]["agent_session"][if key == "session" { "value" } else { "kind" }] =
                right.clone();
            cases.push(c.clone());
            // An unequal field updates in memory before the later raw metadata
            // error, while the early exception still prevents persistence.
            c["panes"]
                .as_array_mut()
                .unwrap()
                .push(json!({"pane_id":"%2","agent_session":7}));
            cases.push(c);
        }
    }
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
    let oracle: Value = serde_json::from_slice(&output.stdout).unwrap();
    let inputs = oracle["inputs"].as_array().unwrap();
    let expected = oracle["outputs"].as_array().unwrap();
    assert_eq!(inputs.len(), cases.len());
    let mut mismatches = Vec::new();
    for (index, (case, expected)) in inputs.iter().zip(expected).enumerate() {
        let actual = run_rust(case);
        if &actual != expected {
            mismatches.push(format!(
                "actual Python case {index}: {case}\nRust: {actual}\nPython: {expected}"
            ));
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
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

#[test]
fn recovery_launch_order_is_in_memory_insertion_but_persisted_json_stays_sorted() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("launches.json");
    let flock = Flock::new(&path, dir.path().join("lanes.json"), true);
    flock.record_launch("%9", &[("agent", json!("pi")), ("session", json!("nine"))]);
    flock.record_launch(
        "%2",
        &[("agent", json!("claude")), ("session", json!("two"))],
    );
    flock.record_launch("%9", &[("session", json!("updated"))]);
    assert_eq!(
        flock
            .launches
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ["%9", "%2"]
    );
    let disk: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(disk["version"], 1);
    assert_eq!(
        disk["panes"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ["%2", "%9"]
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let loaded = Flock::new(&path, dir.path().join("lanes2.json"), true);
    assert_eq!(
        loaded
            .launches
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ["%2", "%9"]
    );
    assert_eq!(loaded.launch("%9").unwrap()["session"], "updated");
    // Loading an external unsorted file follows its actual object order, like json.load.
    std::fs::write(
        &path,
        r#"{"version":1,"panes":{"%9":{"agent":"pi"},"%2":{"agent":"claude"}}}"#,
    )
    .unwrap();
    let loaded = Flock::new(&path, dir.path().join("lanes3.json"), true);
    assert_eq!(
        loaded
            .launches
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ["%9", "%2"]
    );
    loaded.record_launch("%1", &[("agent", json!("codex"))]);
    assert_eq!(
        loaded
            .launches
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ["%9", "%2", "%1"]
    );
}

#[test]
fn recovery_record_refuses_noncanonical_identity_without_synthesizing_keys() {
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        true,
    );
    let config = json!({"panes":[{"pane_id":null,"agent":"pi","agent_session":{"value":"s"}}],"dunks":[],"launches":{}});
    let env = Env {
        flock: &flock,
        config,
        calls: RefCell::new(Vec::new()),
    };
    assert!(matches!(
        flock.recover(&env, true, true),
        Err(RecoveryCommandError::Backend(DunkError::Identity(_)))
    ));
    assert!(flock.launches.lock().unwrap().is_empty());
    assert_eq!(env.calls.into_inner(), [json!(["list", null])]);
}

#[test]
fn recovery_metadata_equality_matches_python_numeric_boundaries_without_rewriting() {
    let mut tokens: Vec<String> = [
        "true",
        "false",
        "null",
        "0",
        "-0",
        "0.0",
        "-0.0",
        "1",
        "1.0",
        "-1",
        "-1.0",
        "0.1",
        "0.10000000000000001",
        "0.10000000000000002",
        "1.5",
        "5e-324",
        "0e-999",
        "9007199254740992",
        "9007199254740993",
        "9007199254740992.0",
        "9007199254740993.0",
        "-9007199254740993",
        "-9007199254740992.0",
        "18446744073709551616",
        "18446744073709551617",
        "18446744073709551616.0",
        "1e30",
        "1000000000000000019884624838656",
        "1000000000000000019884624838657",
        "1e100",
        "1.7976931348623157e308",
        "1e999",
        "2e999",
        "-1e999",
        "\"1\"",
        "[true]",
        "[1.0]",
        "{\"x\":1,\"y\":false}",
        "{\"y\":0,\"x\":true}",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    // Exact integers and adjacent integers well beyond u128, plus an integer
    // larger than any finite f64. Never route these integer tokens through f64.
    tokens.push("10000000000000000159028911097599180468360808563945281389781327557747838772170381060813469985856815104".into());
    tokens.push("10000000000000000159028911097599180468360808563945281389781327557747838772170381060813469985856815105".into());
    tokens.push(format!("1{}", "0".repeat(400)));
    let mut child = Command::new("python3").args(["-c", "import json,sys; v=[json.loads(s) for s in json.load(sys.stdin)]; print(json.dumps([a==b for a in v for b in v]))"])
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(serde_json::to_string(&tokens).unwrap().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected: Vec<bool> = serde_json::from_slice(&output.stdout).unwrap();
    let values: Vec<Value> = tokens
        .iter()
        .map(|raw| serde_json::from_str(raw).unwrap())
        .collect();
    for (i, left) in values.iter().enumerate() {
        for (j, right) in values.iter().enumerate() {
            let equal = expected[i * values.len() + j];
            assert_eq!(
                python_metadata_equal(left, right),
                equal,
                "{} == {}",
                tokens[i],
                tokens[j]
            );
            if equal {
                let case = json!({"panes":[{"pane_id":"%1","agent":"pi","agent_session":{"value":"s","kind":right}}],"dunks":[],"launches":{"%1":{"agent":"pi","session":"s","session_kind":left}},"record":true});
                let actual = run_rust(&case);
                assert_eq!(actual["saved"], false, "equal metadata must not save");
                assert_eq!(
                    actual["launches"]["%1"], case["launches"]["%1"],
                    "raw stored representation must remain unchanged"
                );
                assert!(!actual["launches"]["%1"]
                    .as_object()
                    .unwrap()
                    .contains_key("source"));
                if let Value::Object(original) = left {
                    assert_eq!(
                        actual["launches"]["%1"]["session_kind"]
                            .as_object()
                            .unwrap()
                            .keys()
                            .collect::<Vec<_>>(),
                        original.keys().collect::<Vec<_>>()
                    );
                }
            }
        }
    }
    // The wire codec now supplies a numeric NaN carrier; null is not a replacement.
    // This producer check does not extend the equality cross-product above to NaN.
    let nan = crate::github::python_float_json_value(f64::NAN).expect("numeric NaN carrier");
    assert!(nan.is_number());
    assert_eq!(crate::wire_json::to_string(&nan).unwrap(), "NaN");
    assert!(crate::github::parse_python_json_float(&nan.to_string())
        .unwrap()
        .is_nan());
    assert!(!python_metadata_equal(&Value::Null, &json!(0)));
}

// t3355 (t820 proof D) gap rows 7-10: test_recover.py, oracle commit 79bc92fa.
// Fixture mirrors RecoverTests.setUp: four panes, one dunk per pane, and one
// recorded launch for w1:p2. Every expected value is written out, not compared
// with the Python oracle, so these pin the condition by name.
fn gap_pane(
    id: &str,
    cwd: &str,
    agent: Option<&str>,
    session: Option<&str>,
    status: &str,
) -> Value {
    let mut pane = json!({
        "pane_id": id,
        "cwd": cwd,
        "agent_status": status,
        "terminal_title_stripped": format!("hunter@cell: {cwd}"),
    });
    if let Some(agent) = agent {
        pane["agent"] = json!(agent);
        if let Some(session) = session {
            pane["agent_session"] = json!({"agent": agent, "kind": "id", "value": session});
        }
    }
    pane
}
fn gap_four_panes() -> Value {
    json!({
        "panes": [
            gap_pane("w1:p1", "/w/pi", Some("pi"), Some("live-session"), "working"),
            gap_pane("w1:p2", "/w/codex", None, None, "unknown"),
            gap_pane("w1:p3", "/w/x", None, None, "unknown"),
            gap_pane("w1:p4", "/w/pi2", None, None, "unknown"),
        ],
        "dunks": [
            {"id": "d1", "running": true, "target_pane_id": "w1:p1", "target_agent": "pi", "target_cwd": "/w/pi"},
            {"id": "d2", "running": true, "target_pane_id": "w1:p2", "target_agent": "codex", "target_cwd": "/w/codex"},
            {"id": "d3", "running": true, "target_pane_id": "w1:p3", "target_agent": "opencode", "target_cwd": "/w/x"},
            {"id": "d4", "running": true, "target_pane_id": "w1:p4", "target_agent": "pi", "target_cwd": "/w/pi2"},
        ],
        "launches": {"w1:p2": {"agent": "codex", "session": "abc", "cwd": "/w/codex"}},
        "memory": [["pi", "/w/pi2", {"id": "p2"}]],
    })
}
fn gap_plan_by_pane(actual: &Value) -> std::collections::HashMap<String, Value> {
    actual["outcome"]["result"]["plan"]
        .as_array()
        .expect("plan array")
        .iter()
        .map(|item| {
            let id = item["pane_id"].as_str().expect("pane id").to_owned();
            (id, item.clone())
        })
        .collect()
}

#[test]
fn recover_plan_distinguishes_live_bare_and_unknown_panes() {
    let actual = run_rust(&gap_four_panes());
    let plan = gap_plan_by_pane(&actual);
    assert_eq!(plan["w1:p1"]["action"], "already-running");
    assert_eq!(plan["w1:p2"]["action"], "launch");
    assert_eq!(plan["w1:p2"]["command"], "cd /w/codex && codex resume abc");
    assert_eq!(plan["w1:p2"]["source"], "recorded-session");
    assert_eq!(plan["w1:p3"]["action"], "no-session");
    assert_eq!(plan["w1:p4"]["action"], "launch");
    assert_eq!(plan["w1:p4"]["command"], "cd /w/pi2 && pi --session p2");
    assert_eq!(plan["w1:p4"]["source"], "memory-latest");
    assert_eq!(actual["outcome"]["result"]["ok"], false);
}

#[test]
fn recover_apply_types_only_into_empty_shells() {
    let mut config = gap_four_panes();
    config["apply"] = json!(true);
    let actual = run_rust(&config);
    let mut applied: Vec<&str> = actual["outcome"]["result"]["applied"]
        .as_array()
        .expect("applied array")
        .iter()
        .map(|id| id.as_str().expect("pane id"))
        .collect();
    applied.sort_unstable();
    assert_eq!(applied, ["w1:p2", "w1:p4"]);
    let mut submits: Vec<(&str, &str)> = actual["calls"]
        .as_array()
        .expect("calls array")
        .iter()
        .filter(|call| call[0] == "submit")
        .map(|call| {
            (
                call[1][0].as_str().expect("submit pane id"),
                call[1][1].as_str().expect("submit command"),
            )
        })
        .collect();
    submits.sort_unstable();
    assert_eq!(
        submits,
        [
            ("w1:p2", "cd /w/codex && codex resume abc"),
            ("w1:p4", "cd /w/pi2 && pi --session p2"),
        ],
        "the live pane w1:p1 and the no-session pane w1:p3 must never be typed into"
    );
    assert_eq!(actual["launches"]["w1:p2"]["source"], "recover");
}

#[test]
fn doctor_flags_stale_target_metadata_as_agent_and_cwd_mismatch() {
    let mut config = gap_four_panes();
    config["doctor"] = json!(true);
    config["guard"] = json!({"installed": true, "shadowing_herdr": true});
    config["panes"][0]["cwd"] = json!("/w/real");
    config["dunks"]
        .as_array_mut()
        .expect("dunks array")
        .push(json!({"id": "d52", "running": true, "target_pane_id": "w1:p1", "target_agent": "opencode", "target_cwd": "/w/old"}));
    let actual = run_rust(&config);
    let result = &actual["outcome"]["result"];
    let kinds: Vec<&str> = result["warnings"]
        .as_array()
        .expect("warnings array")
        .iter()
        .map(|warning| warning["kind"].as_str().expect("warning kind"))
        .collect();
    assert!(kinds.contains(&"agent-mismatch"), "{kinds:?}");
    assert!(kinds.contains(&"cwd-mismatch"), "{kinds:?}");
    assert_eq!(result["ok"], false);
    assert_eq!(result["surfaces"]["read_visible"], true);
    assert_eq!(result["herdr_version"], "herdr fixture");
}

#[test]
fn doctor_is_ok_when_the_only_dunk_matches_its_pane() {
    let mut config = gap_four_panes();
    config["doctor"] = json!(true);
    config["guard"] = json!({"installed": true, "shadowing_herdr": true});
    config["dunks"] = json!([
        {"id": "d1", "running": true, "target_pane_id": "w1:p1", "target_agent": "pi", "target_cwd": "/w/pi"},
    ]);
    let actual = run_rust(&config);
    let result = &actual["outcome"]["result"];
    assert_eq!(result["warnings"], json!([]));
    assert_eq!(result["ok"], true);
}
