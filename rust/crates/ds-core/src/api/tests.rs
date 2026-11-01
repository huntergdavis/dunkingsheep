//! Tests for `api.rs`, ported from tests/test_command_smoke.py (the gate-order cases) plus the
//! `dispatch` gate sequence in dunk_api.py. The allowlist tests of
//! tests/test_dunk_dispatch_allowlist.py exercise `_dispatch_allowlist`, not `dispatch`, and
//! belong to the dispatch-command slice.

use super::*;
use serde_json::json;
use std::sync::Mutex;

use crate::flock::panes::PaneInfoEnv;
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

fn dispatch<G: CallerGate + ?Sized>(
    dispatcher: &Dispatcher,
    flock: &Flock,
    gate: &G,
    name: &str,
    args: Option<Args>,
    self_pane_id: Option<&str>,
) -> Result<Value, DunkError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("runtime");
    let env = TestEnv::default();
    let request = RequestCtx {
        self_pane_id: self_pane_id.map(str::to_owned),
        ..RequestCtx::default()
    };
    let context = HandlerContext {
        flock,
        request: &request,
        env: &env,
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
