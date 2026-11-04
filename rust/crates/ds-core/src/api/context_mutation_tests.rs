use super::*;

fn startup(flock: &Flock, root: &Path) {
    let home = root.join("home");
    let config = root.join("config");
    flock
        .attach_status_state(crate::flock::status_runtime::StatusState::at_start(
            "test",
            root.join("state.json"),
            root.join("test-bin"),
            Env::from_pairs(&[
                ("HOME", home.to_str().unwrap()),
                ("DUNKINGSHEEP_DIR", config.to_str().unwrap()),
            ]),
        ))
        .unwrap();
}

#[test]
fn h_context_add_matches_python() {
    let dir = tempfile::tempdir().unwrap();
    let flock = context_flock(dir.path());
    startup(&flock, dir.path());
    let env = TestEnv::default();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_context_mutations();
    let call = |input| {
        dispatch_context_command(
            &dispatcher,
            &flock,
            &env,
            "worker",
            "w1:p2",
            "context_add",
            input,
        )
    };
    let result = call(json!({"body":" body \n", "cls":"fact", "subject":["mixed",1], "stated":"2026-11-04", "type":false, "authority":false, "source":{"probe":true}, "recheck_after":0})).unwrap();
    assert_eq!(result["slug"], "2026-11-04-mixed-1");
    let text = std::fs::read_to_string(result["path"].as_str().unwrap()).unwrap();
    assert!(text.contains("subject: ['mixed', 1]\n"));
    assert!(text.contains("source: {'probe': True}\n"));
    assert!(text.contains("recheck_after: 0\n"));
    assert!(text.ends_with("\n\nbody\n"));
    let rule = refusal(call(
        json!({"body":[],"cls":"ground-truth","subject":"bad","authority":"owner"}),
    ));
    assert!(rule.starts_with("only the owner writes rules"));
    assert_eq!(
        refusal(call(
            json!({"body":[],"cls":"fact","subject":"bad","authority":"measured"})
        )),
        "a measured fact needs --source (the command or file it came from)"
    );
    let before = std::fs::read_to_string(result["path"].as_str().unwrap()).unwrap();
    let error = refusal(call(
        json!({"body":[],"cls":"fact","subject":"replacement","supersedes":result["slug"],"stated":"2026-11-04"}),
    ));
    assert_eq!(error, "'list' object has no attribute 'strip'");
    let after = std::fs::read_to_string(result["path"].as_str().unwrap()).unwrap();
    assert_ne!(before, after);
    assert!(after.contains("superseded_by: 2026-11-04-replacement"));
    assert!(!dir
        .path()
        .join("config/context/fact/2026-11-04-replacement.md")
        .exists());
    let mut ledger =
        crate::context::ledger::Ledger::new(Some(&dir.path().join("config/context-ledger.db")));
    assert_eq!(
        ledger
            .status(
                &dir.path().join("config/context"),
                result["slug"].as_str().unwrap(),
                &after
            )
            .unwrap(),
        crate::context::ledger::Status::Recorded
    );
}

#[test]
fn h_context_promote_matches_python() {
    let dir = tempfile::tempdir().unwrap();
    let flock = context_flock(dir.path());
    startup(&flock, dir.path());
    let env = TestEnv::default();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_context_mutations();
    let result = dispatch_context_command(&dispatcher,&flock,&env,"worker","w1:p2","context_add",json!({"body":"measured result","cls":"fact","subject":"promote-me","stated":"2026-11-01","type":"measurement","source":"probe"})).unwrap();
    let denied = dispatch_context_command(
        &dispatcher,
        &flock,
        &env,
        "worker",
        "w1:p2",
        "context_promote",
        json!({"slug":result["slug"]}),
    );
    assert!(denied.is_err());
    let promoted = dispatch_context_command(
        &dispatcher,
        &flock,
        &env,
        "owner",
        "w1:p1",
        "context_promote",
        json!({"slug":result["slug"]}),
    )
    .unwrap();
    assert_eq!(promoted["class"], "ground-truth");
    let text = std::fs::read_to_string(promoted["path"].as_str().unwrap()).unwrap();
    assert!(text.contains("type: principle\n"));
    assert!(text.contains("authority: owner\n"));
    assert!(text.contains("recheck_after: 90d\n"));
    assert!(text.contains("supersedes: 2026-11-01-promote-me\n"));
    assert!(std::fs::read_to_string(result["path"].as_str().unwrap())
        .unwrap()
        .contains(promoted["slug"].as_str().unwrap()));
    assert_eq!(
        refusal(dispatch_context_command(
            &dispatcher,
            &flock,
            &env,
            "owner",
            "w1:p1",
            "context_promote",
            json!({"slug":[1]})
        )),
        "no entry [1]"
    );
}

#[test]
fn h_memory_backfill_matches_python() {
    let (dir, flock, env) = memory_api_fixture();
    startup(&flock, dir.path());
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_context_mutations();
    let call = |input| {
        dispatch_context_command(
            &dispatcher,
            &flock,
            &env,
            "owner",
            "w1:p1",
            "memory_backfill",
            input,
        )
    };
    // Invalid limits are lazy: no source means no conversion.
    assert_eq!(call(json!({"limit":"bad"})).unwrap()["enqueued"], 0);
    let home = dir.path().join("home");
    for file in [
        ".codex/sessions/a.jsonl",
        ".claude/projects/demo/b.jsonl",
        ".pi/agent/sessions/c.jsonl",
        ".codex/thread_history_1.sqlite",
        ".local/share/opencode/opencode.db",
        ".codex/sessions/.hidden.jsonl",
        ".codex/sessions/.hidden/nested.jsonl",
    ] {
        let path = home.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "{}").unwrap();
    }
    assert_eq!(
        refusal(call(json!({"limit":"bad"}))),
        "invalid literal for int() with base 10: 'bad'"
    );
    assert_eq!(flock.last_message(), "Queued 0 memory source(s)");
    let paused = call(json!({"action":"PAUSE","limit":{"ignored":true}})).unwrap();
    assert_eq!(paused["paused"], true);
    assert_eq!(paused["status"]["ingestor"]["paused"], true);
    let one = call(json!({"action":"unknown-means-start","limit":"1"})).unwrap();
    assert_eq!(one["discovered"], 5);
    assert_eq!(one["enqueued"], 1);
    assert_eq!(one["limit"], "1");
    assert_eq!(one["paused"], true);
    assert_eq!(flock.last_message(), "Queued 1 memory source(s)");
    assert_eq!(call(json!({"limit":-1})).unwrap()["enqueued"], 0);
    assert_eq!(call(json!({"limit":0})).unwrap()["enqueued"], 5);
    assert_eq!(call(json!({"action":"resume"})).unwrap()["paused"], false);
    assert!(call(json!({"action":"status"}))
        .unwrap()
        .get("ingestor")
        .is_some());
    assert_eq!(
        refusal(call(json!({"action":true}))),
        "'bool' object has no attribute 'lower'"
    );
    let service = flock.memory_service().unwrap();
    assert!(!service
        .enqueue_discovered(home.join("missing.jsonl").to_str().unwrap())
        .unwrap());
    service.stop_and_join().unwrap();
    assert!(
        call(json!({"limit":1})).is_err(),
        "closed writer is not swallowed as an input ValueError"
    );
}

#[test]
fn context_raw_failures_keep_validation_and_store_side_effect_order() {
    let dir = tempfile::tempdir().unwrap();
    let flock = context_flock(dir.path());
    startup(&flock, dir.path());
    let env = TestEnv::default();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_context_mutations();
    for bad in [json!(false), json!(0), json!([]), json!({})] {
        let error = dispatch_context_command(
            &dispatcher,
            &flock,
            &env,
            "worker",
            "w1:p2",
            "context_add",
            json!({"body":bad,"cls":false,"subject":"invalid"}),
        )
        .unwrap_err()
        .to_string();
        assert!(error.starts_with("class must be one of"));
        assert!(!dir.path().join("config/context").exists());
    }
    for (i, bad) in [json!(false), json!(0), json!([]), json!({})]
        .into_iter()
        .enumerate()
    {
        let error = dispatch_context_command(
            &dispatcher,
            &flock,
            &env,
            "worker",
            "w1:p2",
            "context_add",
            json!({"body":bad,"cls":"fact","subject":format!("bad-{i}")}),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("object has no attribute 'strip'"));
        assert!(dir.path().join("config/context/README.md").exists());
    }
    let project = dir.path().join("project-context");
    let result=dispatch_context_command(&dispatcher,&flock,&env,"worker","w1:p2","context_add",json!({"body":"project","cls":"provisional","subject":42,"global_store":false,"project_path":project,"stated":"2026-11-04"})).unwrap();
    assert_eq!(result["slug"], "2026-11-04-42");
    let text = std::fs::read_to_string(result["path"].as_str().unwrap()).unwrap();
    assert!(text.contains("scope: project\n"));
    assert!(text.contains("recheck_after: 7d\n"));
    // A ledger failure happens AFTER writing the new article, and must propagate.
    let other = tempfile::tempdir().unwrap();
    let other_flock = context_flock(other.path());
    startup(&other_flock, other.path());
    std::fs::create_dir_all(other.path().join("config/context-ledger.db")).unwrap();
    assert!(dispatch_context_command(&dispatcher,&other_flock,&env,"worker","w1:p2","context_add",json!({"body":"written before ledger","cls":"fact","subject":"ledger-fail","stated":"2026-11-04"})).is_err());
    assert!(other
        .path()
        .join("config/context/fact/2026-11-04-ledger-fail.md")
        .exists());
}

#[test]
fn memory_backfill_discovery_order_and_numeric_limits() {
    use crate::memory::backfill::{limit_reached, memory_sources};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join(".codex/sessions");
    std::fs::create_dir_all(root.join("nested")).unwrap();
    for (name, second) in [("older.jsonl", 1), ("nested/newer.jsonl", 2)] {
        let file = std::fs::File::create(root.join(name)).unwrap();
        file.set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(second)),
        )
        .unwrap();
    }
    let found = memory_sources(dir.path()).unwrap();
    assert_eq!(
        found,
        vec![root.join("nested/newer.jsonl"), root.join("older.jsonl")]
    );
    for value in [
        json!("0"),
        json!(-1),
        json!("-100000000000000000000000000000"),
        json!(-0.5),
    ] {
        assert!(limit_reached(&value, 0).unwrap());
    }
    for value in [
        json!("100000000000000000000000000000"),
        json!("１２"),
        json!("1_000"),
        json!(1.9),
    ] {
        assert!(!limit_reached(&value, 0).unwrap());
    }
    assert!(limit_reached(&json!(1.9), 1).unwrap());
    assert_eq!(
        limit_reached(&json!([1]), 0).unwrap_err().to_string(),
        "int() argument must be a string, a bytes-like object or a real number, not 'list'"
    );
    assert_eq!(
        limit_reached(&serde_json::from_str("1e400").unwrap(), 0)
            .unwrap_err()
            .to_string(),
        "cannot convert float infinity to integer"
    );
}
