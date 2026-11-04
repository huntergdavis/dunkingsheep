use super::*;
use crate::api::{CallerGate, CommandEnv, Dispatcher};
use crate::flock::panes::PaneInfoEnv;
use crate::flock::project_summary::SummaryEnv;
use crate::flock::views::PROJECT_REPOS;
use crate::flock::Flock;
use crate::server::RequestCtx;
use crate::terminal::{fakes::FakeTerminal, Terminal};
use serde_json::json;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

struct Env {
    terminal: Mutex<Box<dyn Terminal>>,
    root: PathBuf,
    home: PathBuf,
    lanes: Vec<Value>,
    tasks: Vec<Value>,
    calls: RefCell<Vec<String>>,
    fail: Option<&'static str>,
}

impl Env {
    fn new(root: PathBuf) -> Self {
        Self {
            terminal: Mutex::new(Box::new(FakeTerminal::default())),
            root,
            home: PathBuf::from("/fixture-home"),
            lanes: Vec::new(),
            tasks: Vec::new(),
            calls: RefCell::new(Vec::new()),
            fail: None,
        }
    }

    fn call(&self, name: &str) -> Result<(), DunkError> {
        self.calls.borrow_mut().push(name.into());
        if self.fail == Some(name) {
            Err(DunkError::Retryable(format!("actual {name} failure")))
        } else {
            Ok(())
        }
    }
}

impl PaneInfoEnv for Env {
    fn list_panes_grouped(&self) -> Vec<Value> {
        Vec::new()
    }
    fn get_pane(&self, _: &str) -> Option<Value> {
        None
    }
}

impl CommandEnv for Env {
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
        &self.terminal
    }
    fn now_s(&self) -> f64 {
        86_400.0
    }
    fn project_summary_env(&self) -> Result<&dyn SummaryEnv, DunkError> {
        self.call("adapter")?;
        Ok(self)
    }
}

impl SummaryEnv for Env {
    fn global_context_root(&self) -> Result<PathBuf, DunkError> {
        self.call("root")?;
        Ok(self.root.clone())
    }
    fn project_dir(&self, raw: &str) -> Result<PathBuf, DunkError> {
        self.call("path")?;
        Ok(match raw.strip_prefix("~/") {
            Some(rest) => self.home.join(rest),
            None => PathBuf::from(raw),
        })
    }
    fn lanes(&self) -> Result<Vec<Value>, DunkError> {
        self.call("lanes")?;
        Ok(self.lanes.clone())
    }
    fn task_list(&self, include_done: bool) -> Result<Vec<Value>, DunkError> {
        self.call(if include_done {
            "tasks:true"
        } else {
            "tasks:false"
        })?;
        Ok(self
            .tasks
            .iter()
            .filter(|t| include_done || t["status"] != "done")
            .cloned()
            .collect())
    }
    fn merges(&self, repo: &str) -> Result<Value, DunkError> {
        self.call(&format!("merges:{repo}"))?;
        Ok(json!({"repo": repo, "merges": [], "as_of": null, "reason": "fixture"}))
    }
}

struct Role(&'static str);
impl CallerGate for Role {
    fn caller_role(&self, _: Option<&str>) -> Result<String, DunkError> {
        Ok(self.0.into())
    }
    fn require_superuser(&self, _: Option<&str>, _: &str) -> Result<(), DunkError> {
        panic!("project_summary is not owner-only")
    }
}

fn dispatch(env: &Env, args: Value, role: &'static str) -> Result<Value, DunkError> {
    let scratch = crate::testsupport::dir("summary-dispatch");
    let flock = Flock::new(
        scratch.path().join("launches.json"),
        scratch.path().join("lanes.json"),
        false,
    );
    let request = RequestCtx {
        self_pane_id: Some("%unrelated".into()),
        ..RequestCtx::default()
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let context = HandlerContext {
        flock: &flock,
        request: &request,
        env,
        runtime: runtime.handle(),
    };
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_project_summary();
    assert_eq!(dispatcher.registration_attempts(), &["project_summary"]);
    dispatcher.dispatch(
        &context,
        &Role(role),
        "project_summary",
        Some(args.as_object().unwrap().clone()),
    )
}

fn entry(root: &Path, class: &str, slug: &str, subject: &str, extra: &str, body: &str) {
    std::fs::create_dir_all(root.join(class)).unwrap();
    std::fs::write(root.join(class).join(format!("{slug}.md")), format!(
        "---\nclass: {class}\nsubject: {subject}\ntype: reference\nscope: project\nstated: 2026-10-01\nauthority: measured\n{extra}---\n{body}\n"
    )).unwrap();
}

#[test]
fn h_project_summary_matches_python() {
    // Sanitized projection of ContextTests.test_chain_head_is_the_only_live_article_across_views.
    let scratch = crate::testsupport::dir("summary-python-chain");
    let root = scratch.path().join(".dunkingsheep");
    entry(
        &root,
        "fact",
        "first",
        "chain",
        "superseded_by: second\n",
        "obsolete-one",
    );
    entry(
        &root,
        "fact",
        "second",
        "chain",
        "superseded_by: head\n",
        "obsolete-two",
    );
    entry(&root, "fact", "head", "chain", "", "current-three");
    entry(
        &root,
        "fact",
        "retired",
        "retired",
        "retired_at: 2026-10-01\n",
        "retired-claim",
    );
    std::fs::write(root.join("fact/broken.md"), "bad front matter").unwrap();
    let env = Env::new(scratch.path().join("absent-global"));
    let result = dispatch(
        &env,
        json!({"project": "synthetic", "path": scratch.path()}),
        "anonymous",
    )
    .unwrap();
    assert_eq!(
        result,
        json!({
            "project": "synthetic", "as_of": "1970-01-02T00:00:00Z", "path": scratch.path(),
            "knowledge": {
                "counts": {"ground-truth": 0, "fact": 1, "provisional": 0, "history": 0},
                "ground_truths": [], "articles_total": 1,
                "articles": [{"store": "project", "class": "fact", "slug": "head", "subject": "chain",
                    "authority": "measured", "stated": "2026-10-01", "first_line": "current-three"}]
            },
            "now": {"agents": [], "tasks_in_flight": []}, "merges": {}, "history": [],
            "history_note": "no completed task has a timestamp yet"
        })
    );
    assert_eq!(
        *env.calls.borrow(),
        [
            "adapter",
            "path",
            "root",
            "lanes",
            "tasks:false",
            "tasks:true"
        ]
    );
}

#[test]
fn project_summary_dispatch_preserves_raw_labels_and_falsey_paths() {
    let scratch = crate::testsupport::dir("summary-raw");
    let env = Env::new(scratch.path().join("absent"));
    // Oracle: /tmp/t1591-raw-wire-oracle.json, AST-extracted dunk_views.project_summary.
    for (label, suffix) in [
        (json!(false), "False"),
        (json!(0), "0"),
        (json!(true), "True"),
        (json!(7), "7"),
        (json!(["x"]), "['x']"),
        (json!({"x": 1}), "{'x': 1}"),
        (json!([]), "[]"),
        (json!({}), "{}"),
        (json!("alpha"), "alpha"),
    ] {
        for path in [
            Value::Null,
            json!(false),
            json!(0),
            json!(""),
            json!([]),
            json!({}),
        ] {
            let result =
                dispatch(&env, json!({"project": label, "path": path}), "anonymous").unwrap();
            assert_eq!(result["project"], label);
            assert_eq!(result["path"], format!("/fixture-home/workspace/{suffix}"));
            assert_eq!(result["merges"], json!({}));
        }
    }
    let omitted = dispatch(&env, json!({"project": "alpha"}), "worker").unwrap();
    assert_eq!(omitted["path"], "/fixture-home/workspace/alpha");
}

#[test]
fn project_summary_dispatch_global_ignores_path_and_selects_all_repos() {
    let scratch = crate::testsupport::dir("summary-global");
    let env = Env::new(scratch.path().join("absent"));
    for project in [Value::Null, json!(""), json!("global")] {
        for path in [json!(true), json!(7), json!(["x"]), json!({"x": 1})] {
            env.calls.borrow_mut().clear();
            let result =
                dispatch(&env, json!({"project": project, "path": path}), "anonymous").unwrap();
            assert_eq!(result["project"], "global");
            assert_eq!(result["path"], Value::Null);
            assert_eq!(result["now"]["agents"], json!([]));
            assert_eq!(
                result["merges"]
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
                PROJECT_REPOS
                    .iter()
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>()
            );
            assert!(!env.calls.borrow().iter().any(|call| call == "path"));
        }
    }
    assert_eq!(
        dispatch(&env, json!({}), "anonymous").unwrap()["project"],
        "global"
    );
    let named = dispatch(&env, json!({"project": "dunkingsheep"}), "worker").unwrap();
    assert_eq!(
        named["merges"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["dunkingsheep"]
    );
}

#[test]
fn project_summary_dispatch_rejects_truthy_wrong_path_before_providers() {
    let scratch = crate::testsupport::dir("summary-path-error");
    let env = Env::new(scratch.path().join("absent"));
    for (path, kind) in [
        (json!(true), "bool"),
        (json!(7), "int"),
        (json!(1.5), "float"),
        (json!(["x"]), "list"),
        (json!({"x": 1}), "dict"),
    ] {
        let error =
            dispatch(&env, json!({"project": "alpha", "path": path}), "worker").unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("expected str, bytes or os.PathLike object, not {kind}")
        );
        assert!(
            env.calls.borrow().is_empty(),
            "validation must precede adapter, root and store access"
        );
    }
}

#[test]
fn project_summary_dispatch_propagates_actual_source_errors() {
    let scratch = crate::testsupport::dir("summary-source-errors");
    for (fail, calls) in [
        (
            "merges:~/workspace/dunkingsheep",
            vec![
                "adapter",
                "root",
                "lanes",
                "tasks:false",
                "tasks:true",
                "merges:~/workspace/dunkingsheep",
            ],
        ),
        ("adapter", vec!["adapter"]),
        ("root", vec!["adapter", "root"]),
        ("lanes", vec!["adapter", "root", "lanes"]),
        (
            "tasks:false",
            vec!["adapter", "root", "lanes", "tasks:false"],
        ),
        (
            "tasks:true",
            vec!["adapter", "root", "lanes", "tasks:false", "tasks:true"],
        ),
    ] {
        let mut env = Env::new(scratch.path().join("absent"));
        env.fail = Some(fail);
        let error = dispatch(&env, json!({}), "anonymous").unwrap_err();
        assert!(
            matches!(error, DunkError::Retryable(_)),
            "preserve the actual error category"
        );
        assert_eq!(error.to_string(), format!("actual {fail} failure"));
        assert_eq!(*env.calls.borrow(), calls);
    }
}

#[test]
fn project_summary_dispatch_role_and_unknown_argument_gates_precede_handler() {
    let scratch = crate::testsupport::dir("summary-role");
    let env = Env::new(scratch.path().join("absent"));
    assert!(dispatch(&env, json!({}), "unrecognized")
        .unwrap_err()
        .to_string()
        .contains("not permitted for role"));
    assert!(env.calls.borrow().is_empty());
    assert_eq!(
        dispatch(&env, json!({"unexpected": true}), "worker")
            .unwrap_err()
            .to_string(),
        "project_summary: unknown argument(s): unexpected"
    );
    assert!(env.calls.borrow().is_empty());
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
        dispatch(&env, json!({}), role).unwrap();
    }
}

#[test]
fn project_summary_dispatch_combines_stores_sorts_and_limits_history() {
    let scratch = crate::testsupport::dir("summary-complete");
    let global = scratch.path().join("global");
    let project = scratch.path().join("demo");
    let root = project.join(".dunkingsheep");
    entry(&global, "ground-truth", "z", "z", "", "global truth");
    entry(&root, "ground-truth", "a", "a", "", "project truth");
    entry(&global, "fact", "zeta", "zeta", "", "zeta body");
    entry(
        &root,
        "fact",
        "alpha",
        "alpha",
        "",
        &format!("{}\nignored", "é".repeat(170)),
    );
    let mut env = Env::new(global);
    env.lanes = vec![
        json!({"agent": "codex", "pane_id": "%2", "agent_name": "in", "agent_status": "working", "cwd": project.join("child")}),
        json!({"agent": "codex", "pane_id": "%3", "cwd": format!("{}-prefix-sibling", project.display())}),
        json!({"agent": "codex", "pane_id": "%4", "cwd": "/elsewhere"}),
        json!({"agent": "", "pane_id": "%5", "cwd": project}),
    ];
    env.tasks = vec![
        json!({"id": "active", "title": "other pane task", "status": "in_progress", "priority": "high", "assignees": ["%2"]}),
        json!({"id": "open", "status": "open"}),
        json!({"id": "unstamped", "status": "done"}),
    ];
    for at in 1..=55 {
        env.tasks.push(json!({"id": format!("t{at}"), "title": "completed", "status": "done", "completed_at": at, "completed_by": "%2"}));
    }
    let result = dispatch(&env, json!({"project": "demo", "path": project}), "worker").unwrap();
    assert_eq!(
        result["knowledge"]["counts"],
        json!({"ground-truth": 2, "fact": 2, "history": 0, "provisional": 0})
    );
    assert_eq!(result["knowledge"]["ground_truths"][0]["subject"], "a");
    assert_eq!(result["knowledge"]["articles"][0]["subject"], "alpha");
    assert_eq!(
        result["knowledge"]["articles"][0]["first_line"],
        "é".repeat(160)
    );
    assert_eq!(result["knowledge"]["articles"][1]["store"], "global");
    assert_eq!(
        result["now"]["agents"].as_array().unwrap().len(),
        2,
        "Python uses textual startswith, not path containment"
    );
    assert_eq!(
        result["now"]["tasks_in_flight"],
        json!([{"id": "active", "title": "other pane task", "status": "in_progress", "priority": "high", "assignees": ["%2"]}])
    );
    assert_eq!(result["history"].as_array().unwrap().len(), 50);
    assert_eq!(result["history"][0]["id"], "t55");
    assert_eq!(result["history"][49]["id"], "t6");
    assert_eq!(
        result["history_note"],
        "merges and decisions are the repository's record; this timeline is tasks completed here"
    );
    assert_eq!(
        *env.calls.borrow(),
        [
            "adapter",
            "path",
            "root",
            "lanes",
            "tasks:false",
            "tasks:true"
        ]
    );
}
