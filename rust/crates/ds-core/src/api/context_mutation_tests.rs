use super::*;

fn startup(flock: &Flock, root: &Path) {
    startup_with_ledger(flock, root, None);
}

fn startup_with_ledger(flock: &Flock, root: &Path, ledger: Option<&str>) {
    let home = root.join("home");
    let config = root.join("config");
    let mut pairs = vec![
        ("HOME", home.to_str().unwrap()),
        ("DUNKINGSHEEP_DIR", config.to_str().unwrap()),
    ];
    if let Some(ledger) = ledger {
        pairs.push(("DUNKINGSHEEP_CONTEXT_LEDGER", ledger));
    }
    flock
        .attach_status_state(crate::flock::status_runtime::StatusState::at_start(
            "test",
            root.join("state.json"),
            root.join("test-bin"),
            Env::from_pairs(&pairs),
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

#[test]
fn memory_backfill_limit_errors_match_python_digit_boundary() {
    use crate::memory::backfill::limit_reached;
    assert_eq!(limit_reached(&json!("9".repeat(4301)),0).unwrap_err().to_string(), "Exceeds the limit (4300 digits) for integer string conversion: value has 4301 digits; use sys.set_int_max_str_digits() to increase the limit");
    assert_eq!(
        limit_reached(&json!("x".repeat(300)), 0)
            .unwrap_err()
            .to_string(),
        format!(
            "invalid literal for int() with base 10: '{}",
            "x".repeat(199)
        )
    );
    assert!(limit_reached(&json!("  +０_０ "), 0).unwrap());
}

fn recorded_hash(path: &Path, slug: &str) -> String {
    rusqlite::Connection::open(path)
        .unwrap()
        .query_row("SELECT sha256 FROM entries WHERE slug=?1", [slug], |row| {
            row.get(0)
        })
        .unwrap()
}

#[test]
fn context_add_and_promote_honor_captured_ledger_override() {
    let dir = tempfile::tempdir().unwrap();
    let flock = context_flock(dir.path());
    let custom = dir.path().join("custom/provenance.db");
    startup_with_ledger(&flock, dir.path(), Some(custom.to_str().unwrap()));
    let env = TestEnv::default();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_context_mutations();
    let first = dispatch_context_command(&dispatcher,&flock,&env,"worker","w1:p2","context_add",json!({"body":"first\r\nsecond", "cls":"fact", "subject":"override", "stated":"2026-11-04"})).unwrap();
    assert!(custom.exists());
    assert!(!dir.path().join("config/context-ledger.db").exists());
    let article_hash = |article: &Value| {
        let text = std::fs::read_to_string(article["path"].as_str().unwrap())
            .unwrap()
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        crate::context::ledger::sha(&text)
    };
    assert_eq!(
        recorded_hash(&custom, first["slug"].as_str().unwrap()),
        article_hash(&first)
    );
    let promoted = dispatch_context_command(
        &dispatcher,
        &flock,
        &env,
        "owner",
        "w1:p1",
        "context_promote",
        json!({"slug":first["slug"]}),
    )
    .unwrap();
    assert_eq!(
        recorded_hash(&custom, promoted["slug"].as_str().unwrap()),
        article_hash(&promoted)
    );
    // Actual Python oracle: the key is (store, slug), not (store, class, slug).
    // Same-day promotion therefore overwrites the fact's row with the rule hash.
    let rows: i64 = rusqlite::Connection::open(&custom)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM entries", [], |row| row.get(0))
        .unwrap();
    if first["slug"] == promoted["slug"] {
        assert_eq!(rows, 1);
        assert_ne!(
            article_hash(&first),
            recorded_hash(&custom, first["slug"].as_str().unwrap())
        );
    } else {
        assert_eq!(rows, 2);
        assert_eq!(
            recorded_hash(&custom, first["slug"].as_str().unwrap()),
            article_hash(&first)
        );
    }
    assert!(!dir.path().join("config/context-ledger.db").exists());
    // An empty captured override selects the normal configured base.
    let fallback = tempfile::tempdir().unwrap();
    let fallback_flock = context_flock(fallback.path());
    startup_with_ledger(&fallback_flock, fallback.path(), Some(""));
    dispatch_context_command(
        &dispatcher,
        &fallback_flock,
        &env,
        "worker",
        "w1:p2",
        "context_add",
        json!({"body":"fallback", "cls":"fact", "subject":"fallback"}),
    )
    .unwrap();
    assert!(fallback.path().join("config/context-ledger.db").exists());
}

#[test]
fn context_captured_ledger_failure_keeps_add_and_promote_partial_writes() {
    let dir = tempfile::tempdir().unwrap();
    let flock = context_flock(dir.path());
    let blocked = dir.path().join("custom-is-directory");
    std::fs::create_dir(&blocked).unwrap();
    startup_with_ledger(&flock, dir.path(), Some(blocked.to_str().unwrap()));
    let env = TestEnv::default();
    let mut dispatcher = Dispatcher::new();
    dispatcher.register_context_mutations();
    let added = dispatch_context_command(
        &dispatcher,
        &flock,
        &env,
        "worker",
        "w1:p2",
        "context_add",
        json!({"body":"remains written", "cls":"fact", "subject":"blocked", "stated":"2026-11-04"}),
    );
    assert!(added.is_err());
    let old = dir.path().join("config/context/fact/2026-11-04-blocked.md");
    assert!(old.exists());
    assert!(!dir.path().join("config/context-ledger.db").exists());
    let promoted = dispatch_context_command(
        &dispatcher,
        &flock,
        &env,
        "owner",
        "w1:p1",
        "context_promote",
        json!({"slug":"2026-11-04-blocked"}),
    );
    assert!(promoted.is_err());
    let (meta, _) =
        crate::context::store::parse_front(&std::fs::read_to_string(&old).unwrap()).unwrap();
    assert!(
        meta["superseded_by"].is_some(),
        "predecessor write precedes failing configured ledger"
    );
    assert_eq!(
        std::fs::read_dir(dir.path().join("config/context/ground-truth"))
            .unwrap()
            .count(),
        0,
        "ledger failure prevents new promoted article"
    );
    assert!(!dir.path().join("config/context-ledger.db").exists());
}

#[test]
fn context_postwrite_ledger_hashes_match_actual_python_newline_oracle() {
    // Python3.14.4 Store.add + Store._record, isolated real SQLite/file oracle:
    // p175-t1594-evidence/repair-python-newlines.json. Literal expected hashes
    // cover both the first write and the predecessor rewrite, independent of
    // the Rust normalization helper.
    for (name, body, first_sha, old_sha) in [
        (
            "crlf",
            "first\r\nsecond",
            "2a1ca1cde668efa20242bb40f2af229184e6e7367bfbf36b452624ed25d66cd3",
            "ece6e4db6c1632170d5844763587c894f88f01f892e57ab8df1dbfea4dbb2b0c",
        ),
        (
            "cr",
            "first\rsecond",
            "4cf36a9526a508fa03e838edba60eeafa05031d77f160d5cc7c68e4d6bda6eb9",
            "70d077272b0807c973a0885fbbcf0c239310a38a9b89028bc55b5ef5f897673d",
        ),
        (
            "mixed",
            "first\r\nsecond\rthird\nfourth\u{85}\u{2028}\u{2029} café",
            "6251574683cc39d686086b7b429eeaac7cda2191860e5aa6f60f46d97872abcd",
            "301a419ecd96f579b5c57d023ab737ffc739a95aab7aab26ba93f74d70bfb49c",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let flock = context_flock(dir.path());
        let custom = dir.path().join("custom.db");
        startup_with_ledger(&flock, dir.path(), Some(custom.to_str().unwrap()));
        let env = TestEnv::default();
        let mut dispatcher = Dispatcher::new();
        dispatcher.register_context_mutations();
        let first = dispatch_context_command(
            &dispatcher,
            &flock,
            &env,
            "worker",
            "w1:p2",
            "context_add",
            json!({"body":body,"cls":"fact","subject":name,"stated":"2026-11-04"}),
        )
        .unwrap();
        assert_eq!(
            recorded_hash(&custom, first["slug"].as_str().unwrap()),
            first_sha
        );
        let raw = std::fs::read_to_string(first["path"].as_str().unwrap()).unwrap();
        assert!(raw.contains('\r'), "writing itself must not normalize");
        assert_ne!(crate::context::ledger::sha(&raw), first_sha);
        dispatch_context_command(&dispatcher,&flock,&env,"worker","w1:p2","context_add",json!({"body":"replacement","cls":"fact","subject":format!("{name}-replacement"),"stated":"2026-11-04","supersedes":first["slug"]})).unwrap();
        assert_eq!(
            recorded_hash(&custom, first["slug"].as_str().unwrap()),
            old_sha
        );
    }
}
