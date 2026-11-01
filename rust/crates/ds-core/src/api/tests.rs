//! Tests for `api.rs`, ported from tests/test_command_smoke.py (the gate-order cases) plus the
//! `dispatch` gate sequence in dunk_api.py. The allowlist tests of
//! tests/test_dunk_dispatch_allowlist.py exercise `_dispatch_allowlist`, not `dispatch`, and
//! belong to the dispatch-command slice.

use super::*;
use serde_json::json;
use std::cell::Cell;
use std::sync::Mutex;

use crate::config::Env;
use crate::flock::commands_turns::{TurnHooks, TurnState};
use crate::flock::dunks::DunkList;
use crate::flock::panes::PaneInfoEnv;
use crate::flock::status::{StatusEnv, ViewCache};
use crate::proc::Caller;
use crate::roles::Roles;
use crate::server::RequestCtx;
use crate::terminal::{fakes::FakeTerminal, Terminal};

struct FixedRole(&'static str, bool);

impl CallerGate for FixedRole {
    fn caller_role(&self, _pane: Option<&str>) -> Result<String, DunkError> {
        Ok(self.0.to_string())
    }
    fn require_superuser(&self, _pane: Option<&str>, name: &str) -> Result<(), DunkError> {
        if self.1 {
            Ok(())
        } else {
            Err(DunkError::Refused(format!("{name} needs a superuser")))
        }
    }
}

struct TestEnv(Mutex<Box<dyn Terminal>>);

impl Default for TestEnv {
    fn default() -> Self {
        Self(Mutex::new(Box::new(FakeTerminal::default())))
    }
}

impl PaneInfoEnv for TestEnv {
    fn list_panes_grouped(&self) -> Vec<Value> {
        Vec::new()
    }
    fn get_pane(&self, _pane_id: &str) -> Option<Value> {
        None
    }
}

impl CommandEnv for TestEnv {
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
        &self.0
    }
    fn now_s(&self) -> f64 {
        1_800_000_000.0
    }
}

struct CompletionEnv(TestEnv);

impl PaneInfoEnv for CompletionEnv {
    fn list_panes_grouped(&self) -> Vec<Value> {
        Vec::new()
    }
    fn get_pane(&self, pane_id: &str) -> Option<Value> {
        Some(json!({
            "pane_id": pane_id,
            "pane_pid": std::process::id(),
        }))
    }
}

impl CommandEnv for CompletionEnv {
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
        self.0.terminal()
    }
    fn now_s(&self) -> f64 {
        self.0.now_s()
    }
}

struct ParityEnv {
    terminal: Mutex<Box<dyn Terminal>>,
    dunks: DunkList,
    view_cache: ViewCache,
    now_s: f64,
    pane_pid: Option<i64>,
    probes: Cell<u32>,
    turn_calls: Mutex<Vec<String>>,
}

impl ParityEnv {
    fn new(pane_pid: Option<i64>) -> Self {
        Self {
            terminal: Mutex::new(Box::new(FakeTerminal::default())),
            dunks: DunkList::new(),
            view_cache: ViewCache::new(),
            now_s: 1_800_000_100.0,
            pane_pid,
            probes: Cell::new(0),
            turn_calls: Mutex::new(Vec::new()),
        }
    }
}

impl PaneInfoEnv for ParityEnv {
    fn list_panes_grouped(&self) -> Vec<Value> {
        Vec::new()
    }
    fn get_pane(&self, pane_id: &str) -> Option<Value> {
        self.pane_pid
            .map(|pid| json!({"pane_id": pane_id, "pane_pid": pid}))
    }
}

impl StatusEnv for ParityEnv {
    fn version(&self) -> String {
        "2.99.0".into()
    }
    fn federated_input(&self) -> Value {
        json!(false)
    }
    fn code_status(&self) -> serde_json::Map<String, Value> {
        serde_json::Map::from_iter([
            ("code_hash".into(), json!("abc")),
            ("code_stale".into(), json!(false)),
        ])
    }
    fn dunks(&self) -> &DunkList {
        &self.dunks
    }
    fn started_at(&self) -> f64 {
        1_800_000_000.0
    }
    fn pid(&self) -> u32 {
        42
    }
    fn now(&self) -> f64 {
        self.now_s
    }
    fn queued_messages(&self) -> usize {
        3
    }
    fn inbox_unread(&self) -> Result<i64, DunkError> {
        Ok(7)
    }
    fn task_totals(&self) -> Result<Value, DunkError> {
        Ok(json!({"open": 2}))
    }
    fn task_followers(&self) -> usize {
        1
    }
    fn quarantined_mailboxes(&self) -> Option<Value> {
        None
    }
    fn last_message(&self) -> String {
        "idle".into()
    }
    fn herdr_available(&self) -> bool {
        self.probes.set(self.probes.get() + 1);
        true
    }
    fn herdr_hint(&self) -> String {
        "start herdr".into()
    }
    fn state_file(&self) -> Option<String> {
        Some("/tmp/dunks.json".into())
    }
    fn inbox_count(&self, pane: &str) -> Result<Value, String> {
        Ok(json!({"mailbox": pane, "unread": 1}))
    }
}

impl TurnHooks for ParityEnv {
    fn record(
        &self,
        kind: &str,
        pane_id: &str,
        _: Option<&str>,
        _: Option<&str>,
        outcome: &str,
        _: f64,
        _: Option<i64>,
    ) {
        self.turn_calls
            .lock()
            .unwrap()
            .push(format!("record {kind} {pane_id} {outcome}"));
    }
    fn observe_turn(&self, pane_id: &str, state: &TurnState) {
        self.turn_calls
            .lock()
            .unwrap()
            .push(format!("observe {pane_id} {}", state.turn_id.len()));
    }
    fn dispatch_on_turn_end(&self, pane_id: &str) -> Value {
        json!({"trigger": "turn.end", "action": "skip", "reason": pane_id, "delivered": false})
    }
}

impl CommandEnv for ParityEnv {
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
        &self.terminal
    }
    fn now_s(&self) -> f64 {
        self.now_s
    }
    fn status_env(&self) -> Result<&dyn StatusEnv, DunkError> {
        Ok(self)
    }
    fn view_cache(&self) -> Result<&ViewCache, DunkError> {
        Ok(&self.view_cache)
    }
    fn turn_hooks(&self) -> Result<&dyn TurnHooks, DunkError> {
        Ok(self)
    }
}

fn dispatch<G: CallerGate + ?Sized>(
    dispatcher: &Dispatcher,
    flock: &Flock,
    gate: &G,
    name: &str,
    args: Option<Args>,
    self_pane_id: Option<&str>,
) -> Result<Value, DunkError> {
    let env = TestEnv::default();
    let request = RequestCtx {
        self_pane_id: self_pane_id.map(str::to_owned),
        ..RequestCtx::default()
    };
    dispatch_with_env(dispatcher, flock, gate, name, args, &request, &env)
}

fn dispatch_with_env<G: CallerGate + ?Sized>(
    dispatcher: &Dispatcher,
    flock: &Flock,
    gate: &G,
    name: &str,
    args: Option<Args>,
    request: &RequestCtx,
    env: &dyn CommandEnv,
) -> Result<Value, DunkError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let context = HandlerContext {
        flock,
        request,
        env,
        runtime: runtime.handle(),
    };
    dispatcher.dispatch(&context, gate, name, args)
}

fn flock() -> (tempfile::TempDir, Flock) {
    let dir = tempfile::tempdir().expect("tempdir");
    let flock = Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        false,
    );
    (dir, flock)
}

fn superuser_fixture() -> (tempfile::TempDir, Flock, Dispatcher) {
    const OWNER: &str = "w1:p2";
    let (dir, flock) = flock();
    let mut roles = Roles::new(
        &Env::default(),
        Some(dir.path().join("roles.json")),
        Some(dir.path().join("tokens.json")),
        None,
    )
    .expect("role registry");
    roles
        .register(OWNER, "owner", None, None, None, None, None, None)
        .expect("owner registration");
    flock.attach_roles(roles, None);
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_superuser_status_add_remove();
    (dir, flock, dispatcher)
}

fn dispatch_superuser(
    dispatcher: &Dispatcher,
    flock: &Flock,
    name: &str,
    args: Option<Args>,
    pane: Option<&str>,
) -> Result<Value, DunkError> {
    let env = TestEnv::default();
    let request = RequestCtx {
        self_pane_id: pane.map(str::to_owned),
        ..RequestCtx::default()
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let context = HandlerContext {
        flock,
        request: &request,
        env: &env,
        runtime: runtime.handle(),
    };
    let gate = FlockGate(&context);
    dispatcher.dispatch(&context, &gate, name, args)
}

#[test]
fn h_superuser_status_matches_python() {
    let (_dir, flock, dispatcher) = superuser_fixture();
    let result = dispatch_superuser(&dispatcher, &flock, "superuser_status", None, Some("w1:p2"))
        .expect("status");
    assert_eq!(result["superuser"], true);
    assert_eq!(result["pane_id"], "w1:p2");
    assert_eq!(result["role"], "owner");
    assert_eq!(result["default"], "anonymous");
    assert!(result["lanes"]["w1:p2"].is_object());
}

#[test]
fn h_superuser_add_matches_python() {
    let (_dir, flock, dispatcher) = superuser_fixture();
    let result = dispatch_superuser(
        &dispatcher,
        &flock,
        "superuser_add",
        args(json!({"entry": "w2:p9"})),
        Some("w1:p2"),
    )
    .expect("add");
    assert_eq!(result["added"], "w2:p9");
    assert_eq!(result["superuser"], true);
    assert_eq!(result["entry"]["role"], "owner");

    assert!(matches!(
        dispatch_superuser(
            &dispatcher,
            &flock,
            "superuser_add",
            args(json!({"entry": "owner name"})),
            Some("w1:p2"),
        ),
        Err(DunkError::Refused(message)) if message.starts_with("superuser add takes a pane id;")
    ));
}

#[test]
fn h_superuser_remove_matches_python() {
    let (_dir, flock, dispatcher) = superuser_fixture();
    let mut roles = Roles::new(
        &Env::default(),
        Some(
            flock
                .lane_registry_path
                .parent()
                .unwrap()
                .join("roles.json"),
        ),
        Some(
            flock
                .lane_registry_path
                .parent()
                .unwrap()
                .join("tokens.json"),
        ),
        None,
    )
    .expect("role registry");
    roles
        .register("w2:p9", "owner", None, None, None, None, None, None)
        .expect("second owner registration");
    flock.attach_roles(roles, None);
    let result = dispatch_superuser(
        &dispatcher,
        &flock,
        "superuser_remove",
        args(json!({"entry": "w2:p9"})),
        Some("w1:p2"),
    )
    .expect("remove");
    assert_eq!(result["removed"], true);
    assert_eq!(result["entry"], "w2:p9");
    assert_eq!(result["superuser"], true);

    assert!(matches!(
        dispatch_superuser(
            &dispatcher,
            &flock,
            "superuser_remove",
            args(json!({"entry": "w1:p2"})),
            Some("w1:p2"),
        ),
        Err(DunkError::Refused(message)) if message == "refusing to remove the last owner"
    ));
}

fn refusal(result: Result<Value, DunkError>) -> String {
    match result {
        Err(DunkError::Refused(text)) => text,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn args(value: Value) -> Option<Args> {
    value.as_object().cloned()
}

fn echo(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    Ok(json!({"pane": context.self_pane_id(), "args": args}))
}

fn current_caller(context: &HandlerContext<'_>, _: Args) -> Result<Value, DunkError> {
    std::thread::sleep(std::time::Duration::from_millis(20));
    Ok(context.flock.current_caller().unwrap_or(Value::Null))
}

fn fail(_: &HandlerContext<'_>, _: Args) -> Result<Value, DunkError> {
    Err(DunkError::Refused("handler failed".into()))
}

fn completion_binding_probe(context: &HandlerContext<'_>, _: Args) -> Result<Value, DunkError> {
    let pane = context.self_pane_id().expect("request pane");
    context
        .flock
        .require_completion_binding(pane, context.env, context.request.caller.as_ref())?;
    Ok(json!({"socket_peer": context.flock.socket_peer_ok()}))
}

fn request_context<'a>(
    flock: &'a Flock,
    request: &'a RequestCtx,
    env: &'a dyn CommandEnv,
    runtime: &'a tokio::runtime::Runtime,
) -> HandlerContext<'a> {
    HandlerContext {
        flock,
        request,
        env,
        runtime: runtime.handle(),
    }
}

fn dispatch_request(
    dispatcher: &Dispatcher,
    flock: &Flock,
    request: &RequestCtx,
) -> Result<Value, DunkError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let env = CompletionEnv(TestEnv::default());
    let context = request_context(flock, request, &env, &runtime);
    dispatcher.dispatch(
        &context,
        &FixedRole("owner", true),
        "get_dunk",
        args(json!({"id": "d1"})),
    )
}

#[test]
fn test_every_command_runs_or_refuses_but_never_crashes() {
    let (_dir, flock) = flock();
    let dispatcher = Dispatcher::new();
    for command in registry::COMMANDS {
        let text = refusal(dispatch(
            &dispatcher,
            &flock,
            &FixedRole("owner", true),
            command.name,
            None,
            Some("w1:p1"),
        ));
        assert!(
            !text.trim().is_empty(),
            "{}: refused with no message",
            command.name
        );
        if command.required.is_empty() {
            assert_eq!(text, format!("command {} is not ported yet", command.name));
        }
    }
}

#[test]
fn test_unknown_command_refusal_text() {
    let (_dir, flock) = flock();
    let d = Dispatcher::new();
    let text = refusal(dispatch(
        &d,
        &flock,
        &FixedRole("owner", true),
        "nope",
        None,
        None,
    ));
    assert_eq!(text, "unknown command 'nope'");
    let text = refusal(dispatch(
        &d,
        &flock,
        &FixedRole("owner", true),
        "it's",
        None,
        None,
    ));
    assert_eq!(text, "unknown command \"it's\"");
}

#[test]
fn test_role_gate_names_the_pane_or_the_missing_credential() {
    let (_dir, flock) = flock();
    let d = Dispatcher::new();
    let with = refusal(dispatch(
        &d,
        &flock,
        &FixedRole("anonymous", true),
        "task_add",
        None,
        Some("%4"),
    ));
    assert_eq!(
        with,
        "task_add is not permitted for role anonymous (pane %4); see docs/FLAT_FLEET.md section 4.3"
    );
    let without = refusal(dispatch(
        &d,
        &flock,
        &FixedRole("anonymous", true),
        "task_add",
        None,
        None,
    ));
    assert_eq!(
        without,
        "task_add is not permitted for role anonymous (no pane, no token); see docs/FLAT_FLEET.md section 4.3"
    );
}

#[test]
fn test_owner_only_commands_go_through_the_superuser_gate() {
    let (_dir, flock) = flock();
    let d = Dispatcher::new();
    let denied = refusal(dispatch(
        &d,
        &flock,
        &FixedRole("worker", false),
        "close",
        None,
        Some("%4"),
    ));
    assert_eq!(denied, "close needs a superuser");
    // system and web skip the owner gate and fall to the per-role table (the Python elif):
    // system is denied `shutdown` there; web is allowed whatever WEB_DENIED does not name.
    let text = refusal(dispatch(
        &d,
        &flock,
        &FixedRole("system", false),
        "shutdown",
        None,
        None,
    ));
    assert_eq!(
        text,
        "shutdown is not permitted for role system (no pane, no token); see docs/FLAT_FLEET.md section 4.3"
    );
    let open = registry::COMMANDS
        .iter()
        .find(|c| c.owner_only && c.required.is_empty() && roles::allowed("web", c.name))
        .expect("an owner-only command web may call");
    let text = refusal(dispatch(
        &d,
        &flock,
        &FixedRole("web", false),
        open.name,
        None,
        None,
    ));
    assert_eq!(text, format!("command {} is not ported yet", open.name));
}

#[test]
fn test_owner_only_matches_the_python_superuser_only_set() {
    let expected = [
        "add_dunk",
        "update_dunk",
        "remove_dunk",
        "start_dunk",
        "stop_dunk",
        "toggle_dunk",
        "stop_all",
        "fire_dunk",
        "start_agent",
        "respawn_pane",
        "create_workspace",
        "create_tab",
        "split_pane",
        "rename",
        "focus",
        "close",
        "herdr",
        "shutdown",
        "memory_ingest",
        "lane_set",
        "lane_unregister",
        "token_issue",
        "token_revoke",
        "review_sync",
        "context_promote",
        "context_retire",
        "poolrad_sync",
        "memory_backfill",
    ];
    let mut got: Vec<&str> = registry::COMMANDS
        .iter()
        .filter(|c| c.owner_only)
        .map(|c| c.name)
        .collect();
    got.sort_unstable();
    let mut want = expected.to_vec();
    want.sort_unstable();
    assert_eq!(got, want);
}

#[test]
fn test_required_then_unknown_argument_checks() {
    let (_dir, flock) = flock();
    let d = Dispatcher::new();
    let owner = FixedRole("owner", true);
    let text = refusal(dispatch(
        &d,
        &flock,
        &owner,
        "get_dunk",
        args(json!({"id": ""})),
        None,
    ));
    assert_eq!(text, "get_dunk: missing required argument(s): id");
    let text = refusal(dispatch(
        &d,
        &flock,
        &owner,
        "get_dunk",
        args(json!({"id": null})),
        None,
    ));
    assert_eq!(text, "get_dunk: missing required argument(s): id");
    // missing wins over unknown (the Python order), and unknown names are sorted.
    let text = refusal(dispatch(
        &d,
        &flock,
        &owner,
        "get_dunk",
        args(json!({"zed": 1, "bar": 2})),
        None,
    ));
    assert_eq!(text, "get_dunk: missing required argument(s): id");
    let text = refusal(dispatch(
        &d,
        &flock,
        &owner,
        "get_dunk",
        args(json!({"id": "d1", "zed": 1, "bar": 2})),
        None,
    ));
    assert_eq!(text, "get_dunk: unknown argument(s): bar, zed");
    // a falsy-but-present value (0, false) is NOT missing.
    let text = refusal(dispatch(
        &d,
        &flock,
        &owner,
        "get_dunk",
        args(json!({"id": 0})),
        None,
    ));
    assert_eq!(text, "command get_dunk is not ported yet");
}

#[test]
fn test_a_registered_handler_gets_the_pane_and_args() {
    let (_dir, flock) = flock();
    let mut d = Dispatcher::new();
    d.register("get_dunk", echo);
    let got = dispatch(
        &d,
        &flock,
        &FixedRole("owner", true),
        "get_dunk",
        args(json!({"id": "d1"})),
        Some("%9"),
    )
    .expect("handler runs");
    assert_eq!(got, json!({"pane": "%9", "args": {"id": "d1"}}));
}

#[test]
fn h_whoami_matches_python() {
    let (_dir, flock) = flock();
    let mut d = Dispatcher::new();
    d.register_status_whoami_turn_start();

    let got = dispatch(
        &d,
        &flock,
        &FixedRole("anonymous", false),
        "whoami",
        None,
        None,
    )
    .expect("anonymous whoami");

    assert_eq!(
        got,
        json!({
            "pane_id": null,
            "role": "anonymous",
            "registered": null,
            "via_token": false,
            "caller": {"pid": null, "start": null, "name": null, "source": null},
            "bound": null,
            "binding_note": null,
        })
    );
}

#[test]
fn h_status_matches_python() {
    let (_dir, flock) = flock();
    let env = ParityEnv::new(None);
    let request = RequestCtx {
        self_pane_id: Some("w1:p1".into()),
        ..RequestCtx::default()
    };
    let mut d = Dispatcher::new();
    d.register_status_whoami_turn_start();

    let cached = dispatch_with_env(
        &d,
        &flock,
        &FixedRole("anonymous", false),
        "status",
        None,
        &request,
        &env,
    )
    .expect("cached status");
    assert_eq!(cached["version"], "2.99.0");
    assert_eq!(cached["self_pane_id"], "w1:p1");
    assert_eq!(cached["herdr_available"], Value::Null);
    assert_eq!(cached["herdr_hint"], Value::Null);
    assert_eq!(
        cached.get("inbox_unread"),
        None,
        "cached view is the cheap status"
    );
    assert_eq!(env.probes.get(), 0, "default status skips backend probing");

    let full = dispatch_with_env(
        &d,
        &flock,
        &FixedRole("anonymous", false),
        "status",
        args(json!({"full": true})),
        &request,
        &env,
    )
    .expect("full status");
    assert_eq!(full["self_pane_id"], "w1:p1");
    assert_eq!(full["herdr_available"], true);
    assert_eq!(full["inbox_unread"], 7);
    assert_eq!(full["tasks"], json!({"open": 2}));
    assert_eq!(env.probes.get(), 1, "full Python status probes the backend");

    let checked = dispatch_with_env(
        &d,
        &flock,
        &FixedRole("anonymous", false),
        "status",
        args(json!({"check_herdr": "yes", "fresh": true})),
        &request,
        &env,
    )
    .expect("explicit availability probe");
    assert_eq!(checked["herdr_available"], true);
    assert_eq!(env.probes.get(), 2);
}

#[test]
fn h_turn_start_matches_python() {
    let (_dir, flock) = flock();
    let pane_id = "w1:p1";
    let pid = std::process::id() as i64;
    let caller = Caller::new(
        Some(pid),
        crate::proc::process_start(pid),
        None,
        Some(pane_id.into()),
        "peer",
    );
    let request = RequestCtx {
        self_pane_id: Some(pane_id.into()),
        caller: Some(caller),
        ..RequestCtx::default()
    };
    let env = ParityEnv::new(Some(pid));
    let mut d = Dispatcher::new();
    d.register_status_whoami_turn_start();

    let result = dispatch_with_env(
        &d,
        &flock,
        &FixedRole("worker", false),
        "turn_start",
        args(json!({
            "ttl_s": "60",
            "source": "pi",
            "handoff_id": format!("nj-{}", "a".repeat(32)),
        })),
        &request,
        &env,
    )
    .expect("authenticated turn start");

    assert_eq!(result["pane_id"], pane_id);
    assert_eq!(result["ttl_s"], 60.0);
    assert_eq!(result["source"], "pi");
    assert_eq!(result["turn_id"].as_str().unwrap().len(), 32);
    assert_eq!(env.turn_calls.lock().unwrap().len(), 2);

    for falsey_ttl in [json!(0), json!(false), json!("")] {
        let default_ttl = dispatch_with_env(
            &d,
            &flock,
            &FixedRole("worker", false),
            "turn_start",
            args(json!({"ttl_s": falsey_ttl})),
            &request,
            &env,
        )
        .expect("falsey Python ttl_s uses the default");
        assert_eq!(
            default_ttl["ttl_s"],
            crate::flock::commands_turns::turn_ttl_s()
        );
    }
    let hook_calls_before_refusals = env.turn_calls.lock().unwrap().len();

    let wrong_pane = dispatch_with_env(
        &d,
        &flock,
        &FixedRole("worker", false),
        "turn_start",
        args(json!({"pane": "w1:p9"})),
        &request,
        &env,
    );
    assert!(matches!(
        wrong_pane,
        Err(DunkError::Refused(message)) if message == "turn event must target the caller's own pane"
    ));
    let invalid_handoff = dispatch_with_env(
        &d,
        &flock,
        &FixedRole("worker", false),
        "turn_start",
        args(json!({"handoff_id": "nj-nope"})),
        &request,
        &env,
    );
    assert!(matches!(
        invalid_handoff,
        Err(DunkError::Refused(message)) if message == "invalid next-job handoff id"
    ));

    let non_string_handoff = dispatch_with_env(
        &d,
        &flock,
        &FixedRole("worker", false),
        "turn_start",
        args(json!({"handoff_id": 123})),
        &request,
        &env,
    );
    assert!(matches!(
        non_string_handoff,
        Err(DunkError::Refused(message))
            if message == "expected string or bytes-like object, got 'int'"
    ));
    assert_eq!(
        env.turn_calls.lock().unwrap().len(),
        hook_calls_before_refusals,
        "refusals have no hooks"
    );
    let invalid_ttl = dispatch_with_env(
        &d,
        &flock,
        &FixedRole("worker", false),
        "turn_start",
        args(json!({"ttl_s": "not-a-number"})),
        &request,
        &env,
    );
    assert!(matches!(
        invalid_ttl,
        Err(DunkError::Refused(message))
            if message == "could not convert string to float: 'not-a-number'"
    ));

    let string_zero_ttl = dispatch_with_env(
        &d,
        &flock,
        &FixedRole("worker", false),
        "turn_start",
        args(json!({"ttl_s": "0"})),
        &request,
        &env,
    );
    assert!(matches!(
        string_zero_ttl,
        Err(DunkError::Refused(message)) if message == "ttl_s must be positive"
    ));
}

#[test]
fn api_handlers_refuse_when_required_adapters_are_not_wired() {
    let (_dir, flock) = flock();
    let request = RequestCtx::default();
    let env = TestEnv::default();
    let mut d = Dispatcher::new();
    d.register_status_whoami_turn_start();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let context = HandlerContext {
        flock: &flock,
        request: &request,
        env: &env,
        runtime: runtime.handle(),
    };
    assert!(matches!(
        d.dispatch(&context, &FixedRole("anonymous", false), "status", None),
        Err(DunkError::Refused(message)) if message == "status command adapter is not wired"
    ));
    assert!(matches!(
        h_status(&context, Args::new()),
        Err(DunkError::Refused(message)) if message == "status command adapter is not wired"
    ));
    assert!(matches!(
        env.view_cache(),
        Err(DunkError::Refused(message)) if message == "status view cache is not wired"
    ));
    assert!(matches!(
        env.turn_hooks(),
        Err(DunkError::Refused(message)) if message == "turn hooks are not wired"
    ));

    let pane_id = "w1:p1";
    let pid = std::process::id() as i64;
    let caller = Caller::new(
        Some(pid),
        crate::proc::process_start(pid),
        None,
        Some(pane_id.into()),
        "peer",
    );
    let request = RequestCtx {
        self_pane_id: Some(pane_id.into()),
        caller: Some(caller),
        ..RequestCtx::default()
    };
    let env = CompletionEnv(TestEnv::default());
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let context = HandlerContext {
        flock: &flock,
        request: &request,
        env: &env,
        runtime: runtime.handle(),
    };
    assert!(matches!(
        d.dispatch(
            &context,
            &FixedRole("worker", false),
            "turn_start",
            None,
        ),
        Err(DunkError::Refused(message)) if message == "turn hooks are not wired"
    ));
}

#[test]
fn test_desk_args_are_clamped_before_the_argument_checks() {
    let (_dir, flock) = flock();
    let mut d = Dispatcher::new();
    d.register("task_list", echo);
    d.register("read_pane", echo);
    let desk = FixedRole("desk", false);
    let got = dispatch(
        &d,
        &flock,
        &desk,
        "task_list",
        args(json!({"limit": 500})),
        None,
    )
    .expect("ok");
    assert_eq!(got["args"]["limit"], 5);
    let got = dispatch(&d, &flock, &desk, "task_list", None, None).expect("ok");
    assert_eq!(got["args"]["limit"], 5);
    let got = dispatch(
        &d,
        &flock,
        &desk,
        "task_list",
        args(json!({"limit": "junk"})),
        None,
    )
    .expect("ok");
    assert_eq!(got["args"]["limit"], 5);
    let got = dispatch(
        &d,
        &flock,
        &desk,
        "task_list",
        args(json!({"limit": -3})),
        None,
    )
    .expect("ok");
    assert_eq!(got["args"]["limit"], 1);
    let got = dispatch(
        &d,
        &flock,
        &desk,
        "read_pane",
        args(json!({"target": "%1", "lines": 2000})),
        None,
    )
    .expect("ok");
    assert_eq!(got["args"]["lines"], 40);
    let got = dispatch(
        &d,
        &flock,
        &desk,
        "read_pane",
        args(json!({"target": "%1"})),
        None,
    )
    .expect("ok");
    assert_eq!(got["args"]["lines"], 40);
}

#[test]
fn test_desk_results_are_clamped_and_other_roles_are_not() {
    let (_dir, flock) = flock();
    fn big(_: &HandlerContext<'_>, _: Args) -> Result<Value, DunkError> {
        Ok(json!({"blob": "x".repeat(10_000)}))
    }
    let mut d = Dispatcher::new();
    d.register("task_list", big);
    let desk = dispatch(
        &d,
        &flock,
        &FixedRole("desk", false),
        "task_list",
        None,
        None,
    )
    .expect("ok");
    assert_eq!(desk["truncated"], true);
    let owner = dispatch(
        &d,
        &flock,
        &FixedRole("owner", true),
        "task_list",
        None,
        None,
    )
    .expect("ok");
    assert_eq!(owner["blob"].as_str().map(str::len), Some(10_000));
}

#[test]
fn concurrent_requests_keep_caller_identity_on_their_own_thread() {
    let (_dir, flock) = flock();
    let flock = std::sync::Arc::new(flock);
    let mut dispatcher = Dispatcher::new();
    dispatcher.register("get_dunk", current_caller);
    let dispatcher = std::sync::Arc::new(dispatcher);
    let start = std::sync::Arc::new(std::sync::Barrier::new(2));

    let handles: Vec<_> = ["first", "second"]
        .into_iter()
        .map(|name| {
            let flock = std::sync::Arc::clone(&flock);
            let dispatcher = std::sync::Arc::clone(&dispatcher);
            let start = std::sync::Arc::clone(&start);
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .expect("runtime");
                let env = TestEnv::default();
                let request = RequestCtx {
                    caller: Some(crate::proc::Caller {
                        name: Some(name.into()),
                        source: "test",
                        ..crate::proc::Caller::default()
                    }),
                    socket_peer: Some(true),
                    ..RequestCtx::default()
                };
                let context = request_context(&flock, &request, &env, &runtime);
                start.wait();
                dispatcher
                    .dispatch(
                        &context,
                        &FixedRole("owner", true),
                        "get_dunk",
                        args(json!({"id": "d1"})),
                    )
                    .expect("handler runs")
            })
        })
        .collect();

    let mut names: Vec<_> = handles
        .into_iter()
        .map(|handle| {
            handle
                .join()
                .expect("request thread")
                .get("name")
                .and_then(Value::as_str)
                .expect("caller name")
                .to_string()
        })
        .collect();
    names.sort();
    assert_eq!(names, ["first", "second"]);
}

#[test]
fn request_caller_fields_are_cleared_after_handler_error() {
    let (_dir, flock) = flock();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register("get_dunk", fail);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let env = TestEnv::default();
    let request = RequestCtx {
        caller: Some(crate::proc::Caller {
            name: Some("owner".into()),
            source: "test",
            ..crate::proc::Caller::default()
        }),
        service_token: Some("secret".into()),
        socket_peer: Some(true),
        ..RequestCtx::default()
    };
    let context = request_context(&flock, &request, &env, &runtime);
    let error = dispatcher
        .dispatch(
            &context,
            &FixedRole("owner", true),
            "get_dunk",
            args(json!({"id": "d1"})),
        )
        .expect_err("handler refusal");
    assert!(matches!(error, DunkError::Refused(text) if text == "handler failed"));
    assert_eq!(flock.caller_state(), crate::flock::CallerState::default());
    assert_eq!(flock.socket_peer_ok(), None);
}

#[test]
fn socket_peer_verdict_stays_distinct_from_trusted_in_process_dispatch() {
    let (_dir, flock) = flock();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register("get_dunk", completion_binding_probe);
    let pane = Some("w1:p1".to_string());
    let pid = std::process::id() as i64;
    let caller = crate::proc::Caller::new(
        Some(pid),
        crate::proc::process_start(pid),
        None,
        pane.clone(),
        "peer",
    );

    let unverified = RequestCtx {
        self_pane_id: pane.clone(),
        socket_peer: Some(false),
        ..RequestCtx::default()
    };
    let error = dispatch_request(&dispatcher, &flock, &unverified)
        .expect_err("unverified socket must not complete caller-sensitive work");
    assert!(
        matches!(error, DunkError::Refused(text) if text.contains("socket peer credentials unavailable"))
    );
    assert_eq!(
        flock.socket_peer_ok(),
        None,
        "refusal clears request provenance"
    );

    let verified = RequestCtx {
        caller: Some(caller.clone()),
        self_pane_id: pane.clone(),
        socket_peer: Some(true),
        ..RequestCtx::default()
    };
    let result = dispatch_request(&dispatcher, &flock, &verified).expect("verified socket");
    assert_eq!(result["socket_peer"], true);
    assert_eq!(
        flock.socket_peer_ok(),
        None,
        "success clears request provenance"
    );

    let in_process = RequestCtx {
        caller: Some(caller),
        self_pane_id: pane,
        socket_peer: None,
        ..RequestCtx::default()
    };
    let result = dispatch_request(&dispatcher, &flock, &in_process).expect("trusted in-process");
    assert_eq!(result["socket_peer"], Value::Null);
    assert_eq!(flock.socket_peer_ok(), None);
}

#[test]
fn test_python_repr_escapes() {
    assert_eq!(py_repr("a\\b"), "'a\\\\b'");
    assert_eq!(py_repr("a\nb"), "'a\\nb'");
    assert_eq!(py_repr("both'\""), "'both\\'\"'");
}
