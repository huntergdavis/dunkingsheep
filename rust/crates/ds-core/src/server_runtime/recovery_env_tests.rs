//! Production `RecoverEnv` parity (t1605 / t3385): `doctor` and `recover` through the
//! production dispatcher against Python's `Flock.doctor` / `Flock.recover` over the same pane
//! dicts, the real `dunk_guard.status()` under the same HOME, DUNKINGSHEEP_DIR and PATH, and
//! the same launches. The Rust side is the real `RuntimeServices` over a fake terminal; the
//! Python side is the actual `dunk_core.py` source with a backend fake fed the Rust adapter's
//! own pane dicts.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};

use crate::api::CommandEnv;
use crate::config::Env;
use crate::flock::loop_env::LoopTerminalFactory;
use crate::flock::panes::Stores;
use crate::flock::recovery_commands::RecoverEnv;
use crate::roles::Roles;
use crate::server::RequestCtx;
use crate::server_runtime::services::{
    RuntimeMemoryConfig, RuntimeObservability, RuntimeServiceInputs, RuntimeServices,
};
use crate::server_runtime::RequestRuntimeAdapter;
use crate::tasks::Tasks;
use crate::terminal::{fakes::FakeTerminal, Pane};

const PANE: &str = "w2:p2";

struct Fixture {
    dir: tempfile::TempDir,
    services: Arc<RuntimeServices>,
    /// The fake terminal's raw events, shared by every terminal the factory creates.
    events: Arc<Mutex<Vec<Value>>>,
}

impl Fixture {
    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }
    fn config(&self) -> PathBuf {
        self.dir.path().join("config")
    }
    fn bin(&self) -> PathBuf {
        self.dir.path().join("bin")
    }
    fn launches_file(&self) -> PathBuf {
        self.config().join("launches.json")
    }
    fn path_env(&self) -> std::ffi::OsString {
        std::env::join_paths([self.config().join("bin"), self.bin()]).unwrap()
    }
}

fn pane(id: &str, command: &str, label: &str, cwd: &str) -> Pane {
    Pane {
        pane_id: id.into(),
        session_id: "$1".into(),
        window_id: "@1".into(),
        cwd: cwd.into(),
        command: command.into(),
        label: label.into(),
        title: label.into(),
        workspace: "main".into(),
        tab: "1".into(),
        window_index: 1,
        workspace_index: 1,
        pane_pid: Some(4242),
        width: 80,
        height: 24,
        ..Pane::default()
    }
}

/// An empty shell (`_is_empty_shell`: no agent, unknown status, a `user@host:` title).
fn shell(id: &str) -> Pane {
    pane(id, "bash", "u@host: /work", "/work")
}

fn executable(path: &Path, body: &[u8]) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// One indexed codex session at `/work`, so `_launch_for` finds it through the memory service.
/// The row goes in through `upsert_session` (the codex JSONL parser reads rollouts, not the
/// generic layout), the same statement the ingest worker uses.
fn seed_memory(db: &Path) {
    let writer = crate::db::open_writer(db).unwrap();
    crate::memory::ingest::apply_full_schema(&writer).unwrap();
    crate::memory::ingest::upsert_session(
        &writer,
        "s-codex-1",
        "codex",
        Some("work"),
        Some("/work"),
        None,
        Some(1_780_000_000.0),
        Some(1_780_000_000.0),
        None,
    )
    .unwrap();
}

/// Real services over a fake terminal, with HOME, DUNKINGSHEEP_DIR and PATH inside `dir` so the
/// guard status is deterministic and the Python oracle can run under the same three values.
fn build(tag: &str, panes: Vec<Pane>, launches: &Value, with_memory_session: bool) -> Fixture {
    let dir = crate::testsupport::dir(tag);
    let home = dir.path().join("home");
    let config = dir.path().join("config");
    let bin = dir.path().join("bin");
    for directory in [&home, &config, &bin] {
        std::fs::create_dir_all(directory).unwrap();
    }
    std::fs::write(
        config.join("launches.json"),
        serde_json::to_vec(&json!({"version": 1, "panes": launches})).unwrap(),
    )
    .unwrap();
    if with_memory_session {
        seed_memory(&config.join("memory.db"));
    }
    let path = std::env::join_paths([config.join("bin"), bin.clone()]).unwrap();
    let env = Env::from_pairs(&[
        ("HOME", home.to_str().unwrap()),
        ("DUNKINGSHEEP_DIR", config.to_str().unwrap()),
        ("PATH", path.to_str().unwrap()),
    ]);
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let tasks = Arc::new(
        runtime
            .block_on(Tasks::open(config.join("tasks.db")))
            .unwrap(),
    );
    runtime.block_on(tasks.allocation_init()).unwrap();
    let inbox = Arc::new(
        crate::inbox_store::Store::open(config.join("inbox.db"), config.join("blobs")).unwrap(),
    );
    let mut roles = Roles::new(
        &env,
        Some(config.join("roles.json")),
        Some(config.join("tokens.json")),
        None,
    )
    .unwrap();
    roles
        .register(PANE, "owner", None, None, None, None, None, None)
        .unwrap();
    let executable = config.join("daemon");
    std::fs::write(&executable, b"test daemon identity").unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let factory_events = Arc::clone(&events);
    let terminal_factory: LoopTerminalFactory = Arc::new(move || {
        let mut terminal = FakeTerminal::new();
        terminal.panes = panes.clone();
        terminal.raw_events = Arc::clone(&factory_events);
        Box::new(terminal)
    });
    let memory = RuntimeMemoryConfig::captured(config.join("memory.db"), &env).unwrap();
    let services = RuntimeServices::new_with_memory(
        RuntimeServiceInputs {
            launches_file: config.join("launches.json"),
            lane_registry_file: config.join("lanes.json"),
            persist_enabled: true,
            stores: Stores {
                tasks: Some(tasks),
                inbox: Some(inbox),
                ..Stores::default()
            },
            roles,
            legacy_superusers: None,
            release_version: "test-release".into(),
            state_file: config.join("whole-state.json"),
            executable,
            env,
            observability: Some(RuntimeObservability {
                state_dir: config.join("state"),
                memory_db: None,
            }),
            runtime,
            terminal_factory,
        },
        Some(memory),
    )
    .unwrap();
    Fixture {
        dir,
        services,
        events,
    }
}

fn request() -> RequestCtx {
    RequestCtx {
        caller: Some(crate::proc::Caller::new(
            Some(123),
            Some("start".into()),
            Some("codex".into()),
            Some(PANE.into()),
            "agent",
        )),
        self_pane_id: Some(PANE.into()),
        socket_peer: Some(true),
        ..RequestCtx::default()
    }
}

/// One request through the production dispatcher, as the socket server runs it.
fn dispatch(fixture: &Fixture, command: &str, args: Value) -> Value {
    let dispatcher = fixture.services.production_dispatcher();
    match dispatcher.dispatch(command, &args, &request()) {
        Ok(result) => json!({"result": result}),
        Err(error) => json!({"error": error.to_string(), "retryable": error.retryable()}),
    }
}

/// Borrow the production adapter the way the dispatcher builds it, for direct provider calls.
fn with_adapter<T>(fixture: &Fixture, call: impl FnOnce(&RequestRuntimeAdapter<'_>) -> T) -> T {
    let terminal = Mutex::new((fixture.services.terminal_factory)());
    let request = request();
    let adapter = RequestRuntimeAdapter::new(
        fixture.services.flock.as_ref(),
        &request,
        fixture.services.runtime.handle(),
        &terminal,
    )
    .with_dunk_runtime(&fixture.services)
    .unwrap();
    call(&adapter)
}

fn adapter_panes(fixture: &Fixture) -> Vec<Value> {
    with_adapter(fixture, |adapter| RecoverEnv::list_panes(adapter).unwrap())
}

fn clean_launches(mut launches: Value) -> Value {
    for row in launches.as_object_mut().unwrap().values_mut() {
        row.as_object_mut().unwrap().remove("recorded_at");
        row.as_object_mut().unwrap().remove("applied_at");
    }
    launches
}

/// The canonical launches the Rust Flock holds after a request, keyed like the fixture.
fn rust_launches(fixture: &Fixture, ids: &[&str]) -> Value {
    let launches: Map<String, Value> = ids
        .iter()
        .filter_map(|id| {
            fixture
                .services
                .flock
                .launch(id)
                .map(|row| ((*id).to_string(), Value::Object(row)))
        })
        .collect();
    clean_launches(Value::Object(launches))
}

fn python3() -> PathBuf {
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|directory| directory.join("python3"))
                .find(|candidate| candidate.is_file())
        })
        .expect("python3 on PATH")
}

/// The Python outcome for one case, computed by the actual `dunk_core.py` / `dunk_api.py`
/// source and the real `dunk_guard` module under the fixture's HOME, DUNKINGSHEEP_DIR and PATH.
fn python_oracle(fixture: &Fixture, case: &Value) -> Value {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let mut child = Command::new(python3())
        .arg("-c")
        .arg(PYTHON_ORACLE)
        .current_dir(root)
        .env("HOME", fixture.home())
        .env("DUNKINGSHEEP_DIR", fixture.config())
        .env("PATH", fixture.path_env())
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
                "api": include_str!("../../../../../dunk_api.py"),
                "case": case,
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
    serde_json::from_slice(&output.stdout).unwrap()
}

/// Run `command` through both sides over the same inputs and return the (identical) outcome.
fn compare(
    fixture: &Fixture,
    command: &str,
    args: Value,
    launches: &Value,
    memory: Value,
) -> Value {
    let panes = adapter_panes(fixture);
    let before = std::fs::read(fixture.launches_file()).unwrap();
    let rust = dispatch(fixture, command, args.clone());
    let saved = std::fs::read(fixture.launches_file()).unwrap() != before;
    let mut ids: Vec<&str> = launches
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    let pane_ids: Vec<String> = panes
        .iter()
        .filter_map(|pane| pane["pane_id"].as_str().map(str::to_owned))
        .collect();
    ids.extend(pane_ids.iter().map(String::as_str));
    let rust_launches = rust_launches(fixture, &ids);
    let case = json!({
        "command": command, "args": args, "panes": panes, "launches": launches,
        "version": "tmux 3.4 (fake)", "memory": memory,
    });
    let python = python_oracle(fixture, &case);
    assert_eq!(rust, python["outcome"], "outcome: {command} {case}");
    assert_eq!(
        rust_launches, python["launches"],
        "launches after {command}"
    );
    assert_eq!(
        json!(saved),
        python["saved"],
        "launches.json written after {command}"
    );
    rust
}

#[test]
fn doctor_through_the_production_dispatcher_matches_python() {
    let fixture = build(
        "recovery-env-doctor",
        vec![shell("%1"), pane("%2", "codex", "codex", "/work")],
        &json!({}),
        false,
    );
    // Nothing installed: the guard reports the home fallback and doctor is not ok.
    let doctor = compare(&fixture, "doctor", json!({}), &json!({}), json!([]));
    let result = &doctor["result"];
    assert_eq!(result["ok"], false);
    assert_eq!(result["herdr_version"], "tmux 3.4 (fake)");
    assert_eq!(
        result["panes"],
        json!({"total": 2, "with_agent": 1, "with_session": 0})
    );
    assert_eq!(result["surfaces"]["pane_list"], true);
    assert_eq!(result["surfaces"]["pane_get"], true);
    assert_eq!(result["surfaces"]["read_visible"], true);
    assert_eq!(result["surfaces"]["guard"]["installed"], false);
    assert_eq!(
        result["surfaces"]["guard"]["real_herdr"],
        fixture.home().join(".local/bin/herdr").to_str().unwrap()
    );
    assert_eq!(result["warnings"], json!([]));
    assert_eq!(result["launches"], 0);
    // Installed, first on PATH, in ~/.bashrc; a real herdr further along PATH is not shadowed.
    let guard_dir = fixture.config().join("bin");
    std::fs::create_dir_all(&guard_dir).unwrap();
    executable(
        &guard_dir.join("herdr"),
        b"#!/bin/sh\n# dunkingsheep-herdr-guard\n",
    );
    std::fs::write(
        fixture.home().join(".bashrc"),
        format!("alias ll='ls -l'\n{}\n", crate::guard::MARKER),
    )
    .unwrap();
    executable(
        &fixture.bin().join("herdr"),
        b"#!/bin/sh\necho real herdr\n",
    );
    let doctor = compare(&fixture, "doctor", json!({}), &json!({}), json!([]));
    let guard = &doctor["result"]["surfaces"]["guard"];
    assert_eq!(doctor["result"]["ok"], true);
    assert_eq!(guard["installed"], true);
    assert_eq!(guard["active_in_this_shell"], true);
    assert_eq!(guard["shell_configured"], true);
    assert_eq!(guard["shadowing_herdr"], false);
    assert_eq!(
        guard["real_herdr"],
        fixture.bin().join("herdr").to_str().unwrap()
    );
    // Shadowed: the wrapper where herdr lived, the real binary beside it.
    executable(
        &fixture.bin().join("herdr"),
        b"#!/bin/sh\n# dunkingsheep-herdr-guard\n",
    );
    executable(
        &fixture.bin().join("herdr-real"),
        b"#!/bin/sh\necho real herdr\n",
    );
    let doctor = compare(&fixture, "doctor", json!({}), &json!({}), json!([]));
    let guard = &doctor["result"]["surfaces"]["guard"];
    assert_eq!(guard["shadowing_herdr"], true);
    assert_eq!(guard["covers_agents_started_by_herdr"], true);
    assert_eq!(
        guard["real_herdr"],
        fixture.bin().join("herdr-real").to_str().unwrap()
    );
    // A ~/.bashrc Python cannot decode: the guard surface is the exception text, as
    // `Flock._guard_status` reports it, and doctor is not ok.
    std::fs::write(fixture.home().join(".bashrc"), b"export LANG=caf\xe9\n").unwrap();
    let doctor = compare(&fixture, "doctor", json!({}), &json!({}), json!([]));
    assert_eq!(doctor["result"]["ok"], false);
    assert_eq!(
        doctor["result"]["surfaces"]["guard"],
        json!({"error": "'utf-8' codec can't decode byte 0xe9 in position 15: invalid continuation byte"})
    );
}

#[test]
fn doctor_with_no_panes_matches_python() {
    let fixture = build("recovery-env-doctor-empty", Vec::new(), &json!({}), false);
    let doctor = compare(&fixture, "doctor", json!({}), &json!({}), json!([]));
    assert_eq!(doctor["result"]["ok"], false);
    assert_eq!(
        doctor["result"]["surfaces"],
        json!({"pane_list": false, "pane_get": false, "read_visible": null,
               "guard": doctor["result"]["surfaces"]["guard"]})
    );
}

#[test]
fn recover_plan_through_the_production_dispatcher_matches_python() {
    let launches = json!({
        "%1": {"agent": "pi", "cwd": "/work", "argv": ["pi", "--resume", "session"]},
        "%2": {"agent": "codex", "cwd": "/work", "argv": ["codex", "resume", "old"]},
    });
    let fixture = build(
        "recovery-env-recover-plan",
        vec![
            shell("%1"),
            pane("%2", "codex", "codex", "/work"),
            shell("%3"),
        ],
        &launches,
        false,
    );
    let recover = compare(&fixture, "recover", json!({}), &launches, json!([]));
    let result = &recover["result"];
    assert_eq!(result["ok"], true);
    assert_eq!(result["apply"], false);
    assert_eq!(result["herdr"], "tmux 3.4 (fake)");
    // %1 is an empty shell with a recorded argv; %2 runs an agent (not in the plan); %3 has
    // nothing recorded.
    assert_eq!(
        result["plan"],
        json!([{"pane_id": "%1", "dunk": null, "agent": "pi", "cwd": "/work",
                "action": "launch", "command": "cd /work && pi --resume session",
                "source": "recorded-argv", "reason": "recorded pane (no running dunk)"}])
    );
    assert_eq!(result["applied"], json!([]));
    assert_eq!(result["launches"], 2);
    assert!(fixture.events.lock().unwrap().is_empty(), "plan-only typed");
}

#[test]
fn recover_finds_the_session_through_the_attached_memory_service() {
    let launches = json!({"%1": {"agent": "codex", "cwd": "/work"}});
    let fixture = build(
        "recovery-env-recover-memory",
        vec![shell("%1")],
        &launches,
        true,
    );
    let row = with_adapter(&fixture, |adapter| {
        assert!(adapter.has_memory());
        adapter
            .latest_session("codex", &json!("/work"))
            .unwrap()
            .expect("the seeded session")
    });
    assert_eq!(row["id"], "s-codex-1");
    assert_eq!(row["harness"], "codex");
    assert_eq!(row["cwd"], "/work");
    with_adapter(&fixture, |adapter| {
        assert_eq!(adapter.latest_session("pi", &json!("/work")).unwrap(), None);
        assert_eq!(adapter.latest_session("codex", &json!("")).unwrap(), None);
        assert_eq!(adapter.latest_session("codex", &Value::Null).unwrap(), None);
        assert_eq!(adapter.latest_session("codex", &json!(0)).unwrap(), None);
        assert_eq!(
            adapter
                .latest_session("codex", &json!({"x": 1}))
                .unwrap_err()
                .to_string(),
            "Error binding parameter 2: type 'dict' is not supported"
        );
    });
    let memory = json!([["codex", "/work", row]]);
    let recover = compare(&fixture, "recover", json!({}), &launches, memory);
    assert_eq!(
        recover["result"]["plan"],
        json!([{"pane_id": "%1", "dunk": null, "agent": "codex", "cwd": "/work",
                "action": "launch", "command": "cd /work && codex resume s-codex-1",
                "source": "memory-latest", "reason": "recorded pane (no running dunk)"}])
    );
}

#[test]
fn recover_apply_types_into_the_shell_and_records_the_launch_like_python() {
    let launches = json!({
        "%1": {"agent": "pi", "cwd": "/work", "argv": ["pi", "--resume", "session"]},
    });
    let fixture = build(
        "recovery-env-recover-apply",
        vec![shell("%1")],
        &launches,
        false,
    );
    let recover = compare(
        &fixture,
        "recover",
        json!({"apply": true}),
        &launches,
        json!([]),
    );
    let result = &recover["result"];
    assert_eq!(result["apply"], true);
    assert_eq!(result["applied"], json!(["%1"]));
    assert_eq!(result["plan"][0]["applied"], true);
    assert_eq!(result["plan"][0]["apply_detail"], "sent");
    assert_eq!(
        *fixture.events.lock().unwrap(),
        vec![json!(["typed", "%1", "cd /work && pi --resume session"])],
        "the launch command was typed through the backend"
    );
    let recorded = fixture
        .services
        .flock
        .launch("%1")
        .expect("recorded launch");
    assert_eq!(recorded["source"], "recover");
    assert_eq!(recorded["command"], "cd /work && pi --resume session");
    assert!(recorded["applied_at"].is_number());
}

#[test]
fn a_detached_adapter_refuses_the_recovery_providers() {
    let fixture = build(
        "recovery-env-detached",
        vec![shell("%1")],
        &json!({}),
        false,
    );
    let terminal = Mutex::new((fixture.services.terminal_factory)());
    let request = request();
    let adapter = RequestRuntimeAdapter::new(
        fixture.services.flock.as_ref(),
        &request,
        fixture.services.runtime.handle(),
        &terminal,
    );
    assert_eq!(
        adapter.recover_env().err().map(|error| error.to_string()),
        Some("request runtime services are not attached".into())
    );
    let attached = adapter.with_dunk_runtime(&fixture.services).unwrap();
    assert!(attached.recover_env().is_ok());
    // Backend calls take canonical string ids; anything else is Python's subprocess refusal.
    assert_eq!(
        RecoverEnv::get_pane(&attached, &json!(7))
            .unwrap_err()
            .to_string(),
        "expected str, bytes or os.PathLike object, not int"
    );
    assert_eq!(
        RecoverEnv::get_pane(&attached, &json!("%1"))
            .unwrap()
            .map(|pane| pane["pane_id"].clone()),
        Some(json!("%1"))
    );
    assert_eq!(RecoverEnv::get_pane(&attached, &json!("%9")).unwrap(), None);
    assert_eq!(
        RecoverEnv::read_visible(&attached, &json!("%9"), 3).unwrap(),
        None
    );
    assert_eq!(
        RecoverEnv::read_visible(&attached, &json!("%1"), 3).unwrap(),
        Some(json!(""))
    );
    assert_eq!(RecoverEnv::version(&attached).unwrap(), "tmux 3.4 (fake)");
}

/// The actual Python producers and `dispatch`, as in `api/recovery_tests.rs`, but with the
/// real `dunk_guard` module (this process's HOME, DUNKINGSHEEP_DIR and PATH) and a backend fake
/// that returns the pane dicts the Rust adapter produced.
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
api_tree=ast.parse(payload['api'])
api_names={'_opt','h_recover','h_doctor','dispatch'}
api_nodes=[n for n in api_tree.body if (isinstance(n,ast.FunctionDef) and n.name in api_names) or (isinstance(n,ast.ClassDef) and n.name=='Command') or (isinstance(n,ast.Assign) and any(isinstance(t,ast.Name) and t.id=='SUPERUSER_ONLY' for t in n.targets))]
exec(compile(ast.fix_missing_locations(ast.Module(body=api_nodes,type_ignores=[])),'actual_dunk_api.py','exec'))
COMMANDS_BY_NAME={}
for node in ast.walk(api_tree):
    if isinstance(node,ast.Call) and isinstance(node.func,ast.Name) and node.func.id=='Command' and node.args and isinstance(node.args[0],ast.Constant) and node.args[0].value in ('doctor','recover'):
        command=eval(compile(ast.Expression(node),'actual_command','eval'))
        COMMANDS_BY_NAME[command.name]=command

c=payload['case']
class Backend:
    def list_panes(self): return copy.deepcopy(c['panes'])
    def get_pane(self,id): return next((copy.deepcopy(p) for p in c['panes'] if p.get('pane_id')==id),None)
    def read_pane(self,id,lines,source):
        return '' if any(p.get('pane_id')==id for p in c['panes']) else None
    def version(self): return c['version']
    def send_text_and_enter(self,id,command): return True,'sent'
class Memory:
    def latest_session(self,harness,cwd):
        return next((copy.deepcopy(row[2]) for row in c['memory'] if row[:2]==[harness,cwd]),None)
flock=Oracle(); flock.herdr=Backend(); flock.memory=Memory()
flock.launches=copy.deepcopy(c['launches']); flock.persist_enabled=True; flock._lock=threading.RLock()
saved=[False]
def save(): saved[0]=True
flock._save_launches=save
flock.dunkers=[]
flock.caller_role=lambda _pane: 'owner'
def owner(*_): raise AssertionError('unexpected owner gate')
flock.require_superuser=owner
try:
    result=dispatch(flock,c['command'],c.get('args'),None)
    outcome={'result':result}
except DunkError as e: outcome={'error':str(e),'retryable':getattr(e,'retryable',False)}
except Exception as e: outcome={'error':'internal error: '+str(e),'retryable':False}
for row in flock.launches.values(): row.pop('recorded_at',None); row.pop('applied_at',None)
print(json.dumps({'outcome':outcome,'launches':flock.launches,'saved':saved[0]}))
"#;
