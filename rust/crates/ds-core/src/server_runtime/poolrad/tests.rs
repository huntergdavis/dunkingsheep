use super::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use crate::api::{CommandEnv, Dispatcher};
use crate::config::{CapturedPythonHome, Env};
use crate::flock::{panes::Stores, status_runtime::StatusState, Flock};
use crate::roles::Roles;
use crate::server::RequestCtx;
use crate::server_runtime::{RuntimeObservability, RuntimeServiceInputs, RuntimeServices};
use crate::terminal::{fakes::FakeTerminal, Terminal};
use serde_json::json;

struct Fixture {
    system_token: String,
    services: Arc<RuntimeServices>,
    tasks: Arc<Tasks>,
    repo: PathBuf,
    config: PathBuf,
    dir: tempfile::TempDir,
}

fn row(n: usize) -> Value {
    json!({"id": format!("OVL_0001_{n:04X}"), "rank": n, "score": 50,
        "bytes": 100, "grade": "A", "unit": 1, "offset": n, "ovr_offset": 3,
        "instructions": 40, "behaviour": "leaf", "classification": "game_evidenced",
        "confidence": 0.9, "aliases": [], "prior_translation": null,
        "callee_readiness": {"composable": true, "ready": 2, "counts": {"strict": 2}}})
}

fn git(repo: &Path, args: &[&str]) {
    let result = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

impl Fixture {
    fn new() -> Self {
        let dir = crate::testsupport::dir("poolrad-provider");
        let repo = dir.path().join("repo");
        let config = dir.path().join("config");
        fs::create_dir_all(repo.join("tools/overlay")).unwrap();
        fs::write(repo.join("tools/overlay/strict_candidate_queue.py"),
            "import json, pathlib, sys\nqueue = json.loads(pathlib.Path('fixture.json').read_text())\npathlib.Path(sys.argv[sys.argv.index('--json')+1]).write_text(json.dumps(queue))\n").unwrap();
        fs::write(
            repo.join("fixture.json"),
            json!({"rows": (1..=8).map(row).collect::<Vec<_>>(),
            "summary": {"composable_bodies": 8, "candidates": 8}})
            .to_string(),
        )
        .unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["add", "."]);
        git(
            &repo,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ],
        );
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let tasks = Arc::new(
            runtime
                .block_on(Tasks::open(dir.path().join("tasks.db")))
                .unwrap(),
        );
        let inbox = Arc::new(
            crate::inbox_store::Store::open(dir.path().join("inbox.db"), dir.path().join("blobs"))
                .unwrap(),
        );
        let env = Env::from_pairs(&[
            ("HOME", dir.path().to_str().unwrap()),
            ("DUNKINGSHEEP_DIR", config.to_str().unwrap()),
            ("DUNKINGSHEEP_POOLRAD_REPO", repo.to_str().unwrap()),
        ]);
        let mut roles = Roles::new(
            &env,
            Some(dir.path().join("roles.json")),
            Some(dir.path().join("tokens.json")),
            None,
        )
        .unwrap();
        roles
            .register(
                "w1:p1",
                "worker",
                None,
                None,
                Some("codex"),
                None,
                None,
                None,
            )
            .unwrap();
        roles
            .register("w2:p2", "owner", None, None, None, None, None, None)
            .unwrap();
        let system_token = roles
            .issue_token("system", Some("isolated-fixture"), None)
            .unwrap();
        let executable = dir.path().join("daemon");
        fs::write(&executable, "isolated executable identity").unwrap();
        let services = RuntimeServices::new(RuntimeServiceInputs {
            launches_file: dir.path().join("launches.json"),
            lane_registry_file: dir.path().join("lanes.json"),
            persist_enabled: false,
            stores: Stores {
                tasks: Some(Arc::clone(&tasks)),
                inbox: Some(inbox),
                ..Stores::default()
            },
            roles,
            legacy_superusers: None,
            release_version: "test".into(),
            state_file: dir.path().join("state.json"),
            executable,
            env,
            observability: Some(RuntimeObservability {
                state_dir: dir.path().join("observability"),
                memory_db: None,
            }),
            runtime,
            terminal_factory: Arc::new(|| Box::new(FakeTerminal::new())),
        })
        .unwrap();
        Self {
            system_token,
            services,
            tasks,
            repo,
            config,
            dir,
        }
    }

    fn call(&self, cmd: &str, args: Value, pane: &str) -> Result<Value, DunkError> {
        let mut handlers = Dispatcher::new();
        handlers.register_poolrad_sync_progress();
        self.services.dispatcher(handlers).dispatch(
            cmd,
            &args,
            &RequestCtx {
                caller: Some(crate::proc::Caller::new(
                    Some(123),
                    Some("start".into()),
                    Some("codex".into()),
                    Some(pane.into()),
                    "agent",
                )),
                self_pane_id: Some(pane.into()),
                socket_peer: Some(true),
                ..RequestCtx::default()
            },
        )
    }

    fn with_adapter<T>(&self, f: impl FnOnce(&RequestRuntimeAdapter<'_>) -> T) -> T {
        let request = RequestCtx::default();
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let adapter = RequestRuntimeAdapter::new(
            &self.services.flock,
            &request,
            self.services.runtime.handle(),
            &terminal,
        );
        f(&adapter)
    }
}

#[test]
fn h_poolrad_sync_matches_python_real_runtime() {
    let f = Fixture::new();
    let first = f
        .call("poolrad_sync", json!({"limit": 2}), "w2:p2")
        .unwrap();
    assert_eq!(first["errors"], json!([]));
    assert_eq!(first["filed"].as_array().unwrap().len(), 2);
    assert_eq!(first["filed"][0]["id"], "OVL_0001_0001");
    assert_eq!(first["base"].as_str().unwrap().len(), 40);
    let id = first["filed"][0]["task"].as_str().unwrap();
    let full = f
        .services
        .runtime
        .block_on(f.tasks.get_full_wire(id, None))
        .unwrap()
        .wire;
    assert_eq!(full["created_by"], "backlog-sync");
    assert_eq!(full["priority"], "medium");
    assert_eq!(full["kind"], "work");
    assert_eq!(full["status"], "open");
    assert_eq!(full["assignees"], json!([]));
    assert!(full["body"]
        .as_str()
        .unwrap()
        .contains("poolrad-strict:OVL_0001_0001"));
    let conn = rusqlite::Connection::open(f.dir.path().join("tasks.db")).unwrap();
    let actor: String = conn
        .query_row(
            "SELECT actor FROM task_events WHERE task_id=? AND event='created'",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(actor, "backlog-sync");
    f.call("poolrad_sync", json!({"limit": 100}), "w2:p2")
        .unwrap();
    let again = f.call("poolrad_sync", json!({}), "w2:p2").unwrap();
    assert_eq!(again["filed"], json!([]));
    assert_eq!(again["skipped"]["already_filed"], 8);
}

#[test]
fn h_poolrad_progress_matches_python_real_runtime() {
    let f = Fixture::new();
    f.call("poolrad_sync", json!({"dry_run": true}), "w2:p2")
        .unwrap();
    fs::write(
        f.config.join("poolrad/credit-pending.json"),
        r#"{"bodies":{"OVL_0001_0001":{"model":"one.c"}}}"#,
    )
    .unwrap();
    fs::create_dir_all(f.repo.join("docs/re")).unwrap();
    fs::write(
        f.repo.join("docs/re/strict_full_c_addresses.csv"),
        "id\nOVL_0001_0001\n",
    )
    .unwrap();
    let progress = f.call("poolrad_progress", json!({}), "w1:p1").unwrap();
    assert_eq!(progress["credited"], 1);
    assert_eq!(progress["credit_pending"], 1);
    assert_eq!(progress["composable_bodies"], 8);
    assert_eq!(progress["queue_candidates"], 8);
    assert_eq!(progress["ceiling"], 815);
    assert!(progress["ceiling_note"]
        .as_str()
        .unwrap()
        .contains("CEILING"));
    assert!(progress["main"].as_str().unwrap().len() >= 7);
}

#[test]
fn poolrad_runtime_role_limit_bool_and_dry_run() {
    let f = Fixture::new();
    assert!(f
        .call("poolrad_sync", json!({}), "w1:p1")
        .unwrap_err()
        .to_string()
        .contains("requires a superuser"));
    assert!(!f.config.join("poolrad/strict-queue.json").exists());
    for (args, count) in [
        (json!({"dry_run": true}), 6),
        (json!({"limit": 0, "dry_run": "yes"}), 6),
        (json!({"limit": -1, "dry_run": true}), 1),
        (json!({"limit": "2", "dry_run": "true"}), 2),
    ] {
        let report = f.call("poolrad_sync", args, "w2:p2").unwrap();
        assert_eq!(report["filed"].as_array().unwrap().len(), count);
        assert_eq!(report["filed"][0]["dry"], true);
    }
    assert!(f
        .call("poolrad_sync", json!({"limit": "bad"}), "w2:p2")
        .unwrap_err()
        .to_string()
        .contains("invalid literal for int()"));
    assert!(f.config.join("poolrad/strict-queue.json").exists());
    assert!(!f.config.join("poolrad/credit-pending.json").exists());
    assert!(f
        .with_adapter(|a| RuntimePoolradTasks(a)
            .list(None, true, 2000, "work")
            .unwrap())
        .is_empty());
    let mut handlers = Dispatcher::new();
    handlers.register_poolrad_sync_progress();
    let system = f
        .services
        .dispatcher(handlers)
        .dispatch(
            "poolrad_sync",
            &json!({"limit": 1, "dry_run": true}),
            &RequestCtx {
                service_token: Some(f.system_token.clone()),
                ..RequestCtx::default()
            },
        )
        .unwrap();
    assert_eq!(system["filed"][0]["dry"], true);
    // A credit-ready model exercises the actual configured write path; an empty
    // credit set would not prove that dry-run suppresses credit persistence.
    fs::create_dir_all(f.repo.join("src/recovered")).unwrap();
    fs::write(
        f.repo.join("src/recovered/overlay_u1_fixture_0001_cpu.c"),
        "/* isolated model */\n",
    )
    .unwrap();
    fs::write(
        f.repo.join("tools/overlay/check_fixture.py"),
        "CHECKER_ROWS = ['OVL_0001_0001']\n",
    )
    .unwrap();
    git(&f.repo, &["add", "."]);
    git(
        &f.repo,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "model fixture (#123)",
        ],
    );
    let dry = f
        .call(
            "poolrad_sync",
            json!({"limit": 1, "dry_run": true}),
            "w2:p2",
        )
        .unwrap();
    assert!(dry["credit_pending"].get("OVL_0001_0001").is_some());
    assert!(!f.config.join("poolrad/credit-pending.json").exists());
    let written = f
        .call("poolrad_sync", json!({"limit": 1}), "w2:p2")
        .unwrap();
    assert_eq!(
        written["credit_pending_file"],
        f.config
            .join("poolrad/credit-pending.json")
            .to_str()
            .unwrap()
    );
    assert_eq!(written["filed"][0]["id"], "OVL_0001_0002");
    assert_eq!(
        f.call("poolrad_progress", json!({}), "w1:p1").unwrap()["credit_pending"],
        1
    );
}

#[test]
fn poolrad_real_task_view_preserves_global_visibility_metadata_and_all_assignees() {
    let f = Fixture::new();
    f.services.runtime.block_on(async {
        for (n, status) in [(1, "done"), (2, "cancelled")] {
            let t = f
                .tasks
                .add_with(
                    "global",
                    &format!("Poolrad strict C: OVL_0001_{n:04X} fixture"),
                    "body",
                    AddOptions {
                        status: Some(status.into()),
                        assignees: vec!["%11".into(), "%12".into()],
                        multi_assignee: true,
                        created_by: Some("fixture".into()),
                        ..AddOptions::default()
                    },
                )
                .await
                .unwrap();
            f.tasks
                .note(&t.id, "note", "%11", Some("evidence"))
                .await
                .unwrap();
        }
        f.tasks
            .add("%11", "Poolrad strict C: OVL_0001_0003 private", "private")
            .await
            .unwrap();
    });
    let rows = f.with_adapter(|a| {
        RuntimePoolradTasks(a)
            .list(None, true, 2000, "work")
            .unwrap()
    });
    assert_eq!(rows.len(), 2);
    for r in &rows {
        assert_eq!(r["assignees"], json!(["%11", "%12"]));
        assert_eq!(r["created_by"], "fixture");
        assert!(r.get("depends_on").is_some());
        assert!(r.get("claimed_by").is_some());
        assert!(r.get("notes").is_none());
        let read = f
            .services
            .runtime
            .block_on(
                f.tasks
                    .read_visible_wire_with_notes(r["id"].as_str().unwrap(), None),
            )
            .unwrap();
        assert_eq!(read.wire["notes"][0]["reason"], "evidence");
    }
    let report = f
        .call("poolrad_sync", json!({"limit": 1}), "w2:p2")
        .unwrap();
    assert_eq!(report["skipped"]["already_filed"], 2);
    assert_eq!(report["filed"][0]["id"], "OVL_0001_0003");
}

#[test]
fn poolrad_real_task_view_keeps_two_thousand_not_one_thousand() {
    let f = Fixture::new();
    f.services.runtime.block_on(async {
        for n in 0..1001 {
            f.tasks
                .add("global", &format!("row {n}"), "body")
                .await
                .unwrap();
        }
    });
    assert_eq!(
        f.with_adapter(|a| RuntimePoolradTasks(a)
            .list(None, true, 2000, "work")
            .unwrap())
            .len(),
        1001
    );
}

#[test]
fn poolrad_partial_insert_failure_keeps_real_commits_and_continues() {
    let f = Fixture::new();
    let conn = rusqlite::Connection::open(f.dir.path().join("tasks.db")).unwrap();
    conn.execute_batch("CREATE TRIGGER fail_second BEFORE INSERT ON tasks WHEN NEW.title LIKE '%OVL_0001_0002%' BEGIN SELECT RAISE(FAIL, 'fixture second insertion failure'); END;").unwrap();
    let report = f
        .call("poolrad_sync", json!({"limit": 3}), "w2:p2")
        .unwrap();
    assert_eq!(report["filed"].as_array().unwrap().len(), 2);
    assert_eq!(report["filed"][1]["id"], "OVL_0001_0003");
    assert!(report["errors"][0]
        .as_str()
        .unwrap()
        .contains("fixture second insertion failure"));
    assert_eq!(
        f.with_adapter(|a| RuntimePoolradTasks(a)
            .list(None, true, 2000, "work")
            .unwrap())
            .len(),
        2
    );
}

#[test]
fn poolrad_exporter_precedes_missing_or_failed_actor_and_progress_needs_no_tasks() {
    let f = Fixture::new();
    let flock = Flock::new(
        f.dir.path().join("detached-launches"),
        f.dir.path().join("detached-lanes"),
        false,
    );
    flock
        .attach_status_state(StatusState::at_start(
            "test",
            f.dir.path().join("detached-state"),
            f.dir.path().join("daemon"),
            f.services.flock.status_state().unwrap().env.clone(),
        ))
        .unwrap();
    let request = RequestCtx::default();
    let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
    let adapter =
        RequestRuntimeAdapter::new(&flock, &request, f.services.runtime.handle(), &terminal);
    assert!(CommandEnv::poolrad_progress(&adapter).is_ok());
    assert!(CommandEnv::poolrad_sync(&adapter, 1, false)
        .unwrap_err()
        .to_string()
        .contains("task store is not attached"));
    fs::write(
        f.repo.join("tools/overlay/strict_candidate_queue.py"),
        "raise RuntimeError('isolated exporter failed')\n",
    )
    .unwrap();
    let report = CommandEnv::poolrad_sync(&adapter, 1, false).unwrap();
    assert!(report["errors"][0]
        .as_str()
        .unwrap()
        .contains("isolated exporter failed"));
    assert_eq!(report["base"], Value::Null);
    let conn = rusqlite::Connection::open(f.dir.path().join("tasks.db")).unwrap();
    conn.execute_batch("DROP TABLE tasks;").unwrap();
    assert!(
        f.call("poolrad_sync", json!({}), "w2:p2").unwrap()["errors"][0]
            .as_str()
            .unwrap()
            .contains("isolated exporter failed")
    );
    fs::write(
        f.repo.join("tools/overlay/strict_candidate_queue.py"),
        "import pathlib, sys\npathlib.Path(sys.argv[-1]).write_text('{\"rows\": []}')\n",
    )
    .unwrap();
    assert!(f
        .call("poolrad_sync", json!({}), "w2:p2")
        .unwrap_err()
        .to_string()
        .contains("no such table"));
}

#[test]
fn poolrad_rejects_nested_and_current_thread_runtime_without_export() {
    let f = Fixture::new();
    f.with_adapter(|a| {
        let _guard = f.services.runtime.enter();
        assert!(a
            .runtime_poolrad_sync(1, true)
            .unwrap_err()
            .to_string()
            .contains("dedicated blocking request thread"));
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let request = RequestCtx::default();
    let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
    let a = RequestRuntimeAdapter::new(&f.services.flock, &request, runtime.handle(), &terminal);
    assert!(a
        .runtime_poolrad_sync(1, true)
        .unwrap_err()
        .to_string()
        .contains("independently driven multithread"));
    assert!(!f.config.join("poolrad/strict-queue.json").exists());
    let services = Arc::clone(&f.services);
    let error = f
        .services
        .runtime
        .block_on(f.services.runtime.spawn_blocking(move || {
            let request = RequestCtx::default();
            let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
            let adapter = RequestRuntimeAdapter::new(
                &services.flock,
                &request,
                services.runtime.handle(),
                &terminal,
            );
            adapter
                .runtime_poolrad_sync(1, true)
                .unwrap_err()
                .to_string()
        }))
        .unwrap();
    assert!(error.contains("dedicated blocking request thread"));
}

#[test]
fn poolrad_captured_home_and_config_present_empty_absent_failure() {
    for (raw, expected) in [
        ("/home/fixture///", "/home/fixture"),
        ("", "/"),
        ("////", "/"),
        ("relative/", "relative"),
    ] {
        let env = Env::from_pairs(&[("HOME", raw)]);
        let home =
            CapturedPythonHome::capture_with(&env, || panic!("present HOME must not query passwd"));
        assert_eq!(home.path().unwrap(), Path::new(expected));
        let config = PoolradConfig::from_env(&env, &home).unwrap();
        assert_eq!(
            config.queue_path,
            Path::new(expected).join(".config/dunkingsheep/poolrad/strict-queue.json")
        );
    }
    let absent = Env::default();
    let home =
        CapturedPythonHome::capture_with(&absent, || Ok(Some(PathBuf::from("/account/home/"))));
    assert_eq!(home.path().unwrap(), Path::new("/account/home"));
    for raw in ["", "  ", "relative/config"] {
        let env = Env::from_pairs(&[("DUNKINGSHEEP_DIR", raw), ("DUNKINGSHEEP_POOLRAD_REPO", "")]);
        let config = PoolradConfig::from_env(&env, &home).unwrap();
        assert_eq!(config.repo, PathBuf::from(""));
        assert_eq!(
            config.queue_path,
            Path::new(raw).join("poolrad/strict-queue.json")
        );
        assert_eq!(
            config.credit_path,
            Path::new(raw).join("poolrad/credit-pending.json")
        );
    }
    let missing = CapturedPythonHome::capture_with(&absent, || Ok(None));
    assert_eq!(missing.expanduser_home().unwrap(), None);
    let explicit = Env::from_pairs(&[("DUNKINGSHEEP_DIR", "/explicit")]);
    assert_eq!(
        PoolradConfig::from_env(&explicit, &missing).unwrap_err(),
        "Could not determine home directory."
    );
    let failed = CapturedPythonHome::capture_with(&absent, || Err("account lookup failed".into()));
    assert_eq!(
        PoolradConfig::from_env(&absent, &failed).unwrap_err(),
        "account lookup failed"
    );
    assert_eq!(
        failed.expanduser_home().unwrap_err(),
        "account lookup failed"
    );
    for raw in ["~", "~unknown"] {
        let env = Env::from_pairs(&[("HOME", raw)]);
        let unexpanded = CapturedPythonHome::capture_with(&env, || panic!("HOME present"));
        assert_eq!(unexpanded.expanduser_home().unwrap(), Some(Path::new(raw)));
        assert_eq!(
            unexpanded.path().unwrap_err(),
            "Could not determine home directory."
        );
    }
}

#[test]
fn poolrad_home_error_is_deferred_and_precedes_export_and_task_lookup() {
    let f = Fixture::new();
    let flock = Flock::new(
        f.dir.path().join("error-launches"),
        f.dir.path().join("error-lanes"),
        false,
    );
    let env = f.services.flock.status_state().unwrap().env.clone();
    let mut state = StatusState::at_start(
        "test",
        f.dir.path().join("error-state"),
        f.dir.path().join("daemon"),
        env,
    );
    state.python_home = CapturedPythonHome::capture_with(&Env::default(), || Ok(None));
    flock.attach_status_state(state).unwrap();
    let roles = Roles::new(
        &Env::default(),
        Some(f.dir.path().join("roles.json")),
        Some(f.dir.path().join("tokens.json")),
        None,
    )
    .unwrap();
    flock.attach_roles(roles, None);
    let request = RequestCtx {
        service_token: Some(f.system_token.clone()),
        ..RequestCtx::default()
    };
    let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
    let a = RequestRuntimeAdapter::new(&flock, &request, f.services.runtime.handle(), &terminal);
    assert_eq!(
        a.runtime_poolrad_sync(1, true).unwrap_err().to_string(),
        "Could not determine home directory."
    );
    assert_eq!(
        a.runtime_poolrad_progress().unwrap_err().to_string(),
        "Could not determine home directory."
    );
    assert!(!f.config.join("poolrad/strict-queue.json").exists());
    let mut handlers = Dispatcher::new();
    handlers.register_poolrad_sync_progress();
    let context = a.handler_context();
    let error = handlers
        .dispatch(
            &context,
            &crate::api::FlockGate(&context),
            "poolrad_sync",
            Some(json!({"limit": "bad"}).as_object().unwrap().clone()),
        )
        .unwrap_err();
    assert_eq!(error.to_string(), "Could not determine home directory.");
}

#[test]
fn poolrad_real_runtime_dry_report_equals_python_oracle() {
    let f = Fixture::new();
    let rust = f
        .call(
            "poolrad_sync",
            json!({"limit": 3, "dry_run": true}),
            "w2:p2",
        )
        .unwrap();
    let output = Command::new("python3").args(["-c", "import json, types, dunk_poolrad\nflock=types.SimpleNamespace(tasks=types.SimpleNamespace(list=lambda **kwargs: []))\nprint(json.dumps(dunk_poolrad.Sync(flock).run(limit=3,dry_run=True)))"])
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.."))
        .env("HOME", f.dir.path()).env("DUNKINGSHEEP_DIR", &f.config)
        .env("DUNKINGSHEEP_POOLRAD_REPO", &f.repo).env("PYTHONDONTWRITEBYTECODE", "1")
        .output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let python: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(rust, python);
}

#[test]
fn poolrad_config_snapshot_and_invalid_output_path_are_not_live_fallbacks() {
    let f = Fixture::new();
    let original = f.with_adapter(|a| a.poolrad_config().unwrap());
    let later = Env::from_pairs(&[
        ("HOME", "/different"),
        ("DUNKINGSHEEP_DIR", "/different/config"),
    ]);
    let later_home = CapturedPythonHome::capture_with(&later, || panic!("not absent"));
    assert_ne!(
        original,
        PoolradConfig::from_env(&later, &later_home).unwrap()
    );
    assert_eq!(original, f.with_adapter(|a| a.poolrad_config().unwrap()));
    fs::write(&f.config, "file, not a config directory").unwrap();
    let report = f.call("poolrad_sync", json!({}), "w2:p2").unwrap();
    assert_eq!(report["base"], Value::Null);
    assert_eq!(report["filed"], json!([]));
    assert_eq!(report["errors"].as_array().unwrap().len(), 1);
    assert_eq!(
        f.call("poolrad_progress", json!({}), "w1:p1").unwrap()["queue_exported_at"],
        Value::Null
    );
}
