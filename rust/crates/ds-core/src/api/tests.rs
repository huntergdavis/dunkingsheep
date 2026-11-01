//! Tests for `api.rs`, ported from tests/test_command_smoke.py (the gate-order cases) plus the
//! `dispatch` gate sequence in dunk_api.py. The allowlist tests of
//! tests/test_dunk_dispatch_allowlist.py exercise `_dispatch_allowlist`, not `dispatch`, and
//! belong to the dispatch-command slice.

use super::*;
use serde_json::json;

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

fn echo(_: &Flock, ctx: &RequestCtx, args: Args) -> Result<Value, DunkError> {
    Ok(json!({"pane": ctx.self_pane_id, "args": args}))
}

#[test]
fn test_every_command_runs_or_refuses_but_never_crashes() {
    let (_dir, flock) = flock();
    let dispatcher = Dispatcher::new();
    for command in registry::COMMANDS {
        let text = refusal(dispatcher.dispatch(
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
    let text =
        refusal(Dispatcher::new().dispatch(&flock, &FixedRole("owner", true), "nope", None, None));
    assert_eq!(text, "unknown command 'nope'");
    let text =
        refusal(Dispatcher::new().dispatch(&flock, &FixedRole("owner", true), "it's", None, None));
    assert_eq!(text, "unknown command \"it's\"");
}

#[test]
fn test_role_gate_names_the_pane_or_the_missing_credential() {
    let (_dir, flock) = flock();
    let d = Dispatcher::new();
    let with = refusal(d.dispatch(
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
    let without = refusal(d.dispatch(
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
    let denied = refusal(d.dispatch(
        &flock,
        &FixedRole("worker", false),
        "close",
        None,
        Some("%4"),
    ));
    assert_eq!(denied, "close needs a superuser");
    // system and web skip the owner gate and fall to the per-role table (the Python elif):
    // system is denied `shutdown` there; web is allowed whatever WEB_DENIED does not name.
    let text = refusal(d.dispatch(&flock, &FixedRole("system", false), "shutdown", None, None));
    assert_eq!(
        text,
        "shutdown is not permitted for role system (no pane, no token); see docs/FLAT_FLEET.md section 4.3"
    );
    let open = registry::COMMANDS
        .iter()
        .find(|c| c.owner_only && c.required.is_empty() && roles::allowed("web", c.name))
        .expect("an owner-only command web may call");
    let text = refusal(d.dispatch(&flock, &FixedRole("web", false), open.name, None, None));
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
    let text = refusal(d.dispatch(&flock, &owner, "get_dunk", args(json!({"id": ""})), None));
    assert_eq!(text, "get_dunk: missing required argument(s): id");
    let text = refusal(d.dispatch(&flock, &owner, "get_dunk", args(json!({"id": null})), None));
    assert_eq!(text, "get_dunk: missing required argument(s): id");
    // missing wins over unknown (the Python order), and unknown names are sorted.
    let text = refusal(d.dispatch(
        &flock,
        &owner,
        "get_dunk",
        args(json!({"zed": 1, "bar": 2})),
        None,
    ));
    assert_eq!(text, "get_dunk: missing required argument(s): id");
    let text = refusal(d.dispatch(
        &flock,
        &owner,
        "get_dunk",
        args(json!({"id": "d1", "zed": 1, "bar": 2})),
        None,
    ));
    assert_eq!(text, "get_dunk: unknown argument(s): bar, zed");
    // a falsy-but-present value (0, false) is NOT missing.
    let text = refusal(d.dispatch(&flock, &owner, "get_dunk", args(json!({"id": 0})), None));
    assert_eq!(text, "command get_dunk is not ported yet");
}

#[test]
fn test_a_registered_handler_gets_the_pane_and_args() {
    let (_dir, flock) = flock();
    let mut d = Dispatcher::new();
    d.register("get_dunk", echo);
    let got = d
        .dispatch(
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
    let got = d
        .dispatch(
            &flock,
            &desk,
            "task_list",
            args(json!({"limit": 500})),
            None,
        )
        .expect("ok");
    assert_eq!(got["args"]["limit"], 5);
    let got = d
        .dispatch(&flock, &desk, "task_list", None, None)
        .expect("ok");
    assert_eq!(got["args"]["limit"], 5);
    let got = d
        .dispatch(
            &flock,
            &desk,
            "task_list",
            args(json!({"limit": "junk"})),
            None,
        )
        .expect("ok");
    assert_eq!(got["args"]["limit"], 5);
    let got = d
        .dispatch(&flock, &desk, "task_list", args(json!({"limit": -3})), None)
        .expect("ok");
    assert_eq!(got["args"]["limit"], 1);
    let got = d
        .dispatch(
            &flock,
            &desk,
            "read_pane",
            args(json!({"target": "%1", "lines": 2000})),
            None,
        )
        .expect("ok");
    assert_eq!(got["args"]["lines"], 40);
    let got = d
        .dispatch(
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
    fn big(_: &Flock, _: &RequestCtx, _: Args) -> Result<Value, DunkError> {
        Ok(json!({"blob": "x".repeat(10_000)}))
    }
    let mut d = Dispatcher::new();
    d.register("task_list", big);
    let desk = d
        .dispatch(&flock, &FixedRole("desk", false), "task_list", None, None)
        .expect("ok");
    assert_eq!(desk["truncated"], true);
    let owner = d
        .dispatch(&flock, &FixedRole("owner", true), "task_list", None, None)
        .expect("ok");
    assert_eq!(owner["blob"].as_str().map(str::len), Some(10_000));
}

#[test]
fn test_python_repr_escapes() {
    assert_eq!(py_repr("a\\b"), "'a\\\\b'");
    assert_eq!(py_repr("a\nb"), "'a\\nb'");
    assert_eq!(py_repr("both'\""), "'both\\'\"'");
}
