use super::*;
use std::{cell::RefCell, time::Duration};

struct FakeTransport {
    calls: RefCell<Vec<Value>>,
    fail: bool,
}
impl Transport for &FakeTransport {
    fn exchange(&self, line: &str, _: Duration) -> std::io::Result<Option<String>> {
        self.calls
            .borrow_mut()
            .push(serde_json::from_str(line).unwrap());
        if self.fail {
            Err(std::io::Error::from(std::io::ErrorKind::TimedOut))
        } else {
            Ok(Some(json!({"ok":true,"result":{"id":"i1"}}).to_string()))
        }
    }
}
#[test]
fn inbox_sender_uses_system_identity_and_does_not_retry_failed_write() {
    for fail in [false, true] {
        let transport = FakeTransport {
            calls: RefCell::new(vec![]),
            fail,
        };
        let mut client = FlockClient::new(
            &transport,
            Some("fake-pane".into()),
            String::new(),
            Duration::from_secs(1),
            None,
        );
        assert_eq!(
            inbox_sender(&mut client, "fake-vp", "subject", "body").is_err(),
            fail
        );
        let calls = transport.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["cmd"], "inbox_send");
        assert_eq!(calls[0]["self_pane_id"], Value::Null);
        assert_eq!(
            calls[0]["args"],
            json!({"to":"fake-vp","subject":"subject","body":"body","system_sender":"watchdog"})
        );
    }
}
fn setup() -> (tempfile::TempDir, Value, Value) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("superusers.json"),
        r#"{"panes":["%2","%15"]}"#,
    )
    .unwrap();
    let config = json!({"stores":{"config_dir":dir.path()},"state_path":dir.path().join("state.json"),"rules":{"fleet.assignment_starvation":{"superuser_panes":["%2","%15","%2"],"cooldown_minutes":1}}});
    let item = json!({"rule":"fleet.assignment_starvation","key":"fleet","severity":"warning","class":"ops","source":"watchdog-rule","threshold":"1 worker","measured":"2 workers","window":"10m","evidence":{}});
    (dir, config, item)
}
#[test]
fn partial_failure_retries_only_failed_superuser_and_persists_each_acceptance() {
    let (dir, config, item) = setup();
    let mut state = json!({});
    let mut calls = vec![];
    let error = send_starvation_group(&config, &mut state, &item, 100.0, |pane, subject, body| {
        calls.push(pane.to_owned());
        assert_eq!(subject, "watchdog: idle capacity needs backlog allocation");
        assert_eq!(
            body,
            digest(
                "SUPERUSER allocation action",
                &[line("NOTIFY SUPERUSERS", &item, &json!("no"), None, None)]
            )
        );
        if pane == "%2" {
            let saved = read_json(&dir.path().join("state.json")).unwrap();
            assert_eq!(
                saved["superuser_deliveries"][dedupe_key(&item)]["%15"],
                100.0
            );
            Err(SendError {
                kind: "RuntimeError".into(),
                message: "injected delivery failure".into(),
            })
        } else {
            Ok(())
        }
    })
    .unwrap_err();
    assert_eq!(calls, ["%15", "%2"]);
    assert_eq!(
        error.to_string(),
        "%2: RuntimeError: injected delivery failure"
    );
    // Restart from disk: accepted recipient is not re-prompted.
    let mut state = read_json(&dir.path().join("state.json")).unwrap();
    calls.clear();
    send_starvation_group(&config, &mut state, &item, 101.0, |pane, _, _| {
        calls.push(pane.to_owned());
        Ok(())
    })
    .unwrap();
    assert_eq!(calls, ["%2"]);
    calls.clear();
    send_starvation_group(&config, &mut state, &item, 159.0, |pane, _, _| {
        calls.push(pane.to_owned());
        Ok(())
    })
    .unwrap();
    assert!(calls.is_empty());
    send_starvation_group(&config, &mut state, &item, 160.0, |pane, _, _| {
        calls.push(pane.to_owned());
        Ok(())
    })
    .unwrap();
    assert_eq!(
        calls,
        ["%15"],
        "exact cooldown boundary permits that recipient"
    );
}
#[test]
fn unprotected_or_malformed_recipient_list_refuses_whole_group() {
    let (dir, mut config, item) = setup();
    config["rules"]["fleet.assignment_starvation"]["superuser_panes"] = json!(["%2", "%99"]);
    let mut state = json!({});
    let error = send_starvation_group(&config, &mut state, &item, 100.0, |_, _, _| {
        panic!("must not send")
    })
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "configured starvation recipients are not all current superusers"
    );
    assert_eq!(state, json!({}));
    for panes in [json!(null), json!("%2"), json!(["%2", 2])] {
        std::fs::write(
            dir.path().join("superusers.json"),
            json!({"panes":panes}).to_string(),
        )
        .unwrap();
        assert_eq!(
            send_starvation_group(&config, &mut state, &item, 100.0, |_, _, _| panic!(
                "must not send"
            ))
            .unwrap_err()
            .to_string(),
            "superusers.json panes must be a list of pane ids"
        );
        assert_eq!(state, json!({}));
    }
}
#[test]
fn grouping_aggregates_failures_and_save_failure_stops_later_sends() {
    let (dir, mut config, item) = setup();
    let mut state = json!({});
    let error = send_starvation_group(&config, &mut state, &item, 100.0, |_, _, _| {
        Err(SendError {
            kind: "OSError".into(),
            message: "offline".into(),
        })
    })
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "%15: OSError: offline; %2: OSError: offline"
    );
    assert!(!dir.path().join("state.json").exists());
    config["state_path"] = json!(dir.path()); // atomic replace of a directory must fail
    let mut calls = vec![];
    assert!(
        send_starvation_group(&config, &mut state, &item, 100.0, |pane, _, _| {
            calls.push(pane.to_owned());
            Ok(())
        })
        .is_err()
    );
    assert_eq!(calls, ["%15"]);
    assert_eq!(
        state["superuser_deliveries"][dedupe_key(&item)]["%15"],
        100.0
    );
}
#[test]
fn desk_channel_reads_watermark_and_refuses_malformed_or_wrong_route() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("telegram-status.json");
    let channel = DeskBriefChannel::new(path.clone(), |parent: &Path| {
        assert_eq!(parent, dir.path());
        Ok("cos".into())
    });
    for (status, expected) in [
        (
            json!({"channel":"cos","last_brief_id":1065}),
            Some(json!(1065)),
        ),
        (json!({"channel":"cos","last_brief_id":null}), None),
        (json!({"channel":"cos"}), None),
    ] {
        std::fs::write(&path, status.to_string()).unwrap();
        assert_eq!(channel.confirmed_through().unwrap(), expected);
    }
    let large_id: Value = serde_json::from_str("92233720368547758081234").unwrap();
    std::fs::write(
        &path,
        format!(r#"{{"channel":"cos","last_brief_id":{large_id}}}"#),
    )
    .unwrap();
    assert_eq!(channel.confirmed_through().unwrap(), Some(large_id));
    for value in [json!(true), json!("1065"), json!(1.5), json!([]), json!({})] {
        std::fs::write(
            &path,
            json!({"channel":"cos","last_brief_id":value}).to_string(),
        )
        .unwrap();
        assert!(channel
            .confirmed_through()
            .unwrap_err()
            .to_string()
            .starts_with("last_brief_id is not an integer:"));
    }
    for status in [
        json!({"channel":"vp","last_brief_id":999999}),
        json!({"last_brief_id":999999}),
        json!([]),
    ] {
        std::fs::write(&path, status.to_string()).unwrap();
        assert_eq!(
            channel.confirmed_through().unwrap_err().to_string(),
            "Telegram confirmation channel differs from configured route or is missing"
        );
    }
    std::fs::write(&path, "{ trunc").unwrap();
    assert!(channel.confirmed_through().is_err());
    std::fs::remove_file(&path).unwrap();
    assert!(channel.confirmed_through().is_err());
    assert_eq!(
        channel.post("page").unwrap_err().to_string(),
        "desk brief post not wired: desk API token handling awaits VP decision"
    );
}

struct FakeOwner {
    route: String,
    through: Option<Value>,
    posts: RefCell<Vec<String>>,
    fail: bool,
}
impl OwnerTransport for FakeOwner {
    fn desk_channel(&self) -> Result<String, DunkError> {
        Ok(self.route.clone())
    }
    fn post(&self, body: &str) -> Result<i64, SendError> {
        self.posts.borrow_mut().push(body.to_owned());
        if self.fail {
            Err(SendError {
                kind: "RuntimeError".into(),
                message: "fake post failure".into(),
            })
        } else {
            Ok(1065)
        }
    }
    fn confirmed_through(&self) -> Result<Option<Value>, DunkError> {
        Ok(self.through.clone())
    }
}

fn run_fixture() -> (tempfile::TempDir, Value) {
    let dir = tempfile::tempdir().unwrap();
    let mut config: Value = serde_json::from_str(include_str!(
        "../../../../../packaging/watchdog.example.json"
    ))
    .unwrap();
    config["stores"]["config_dir"] = json!(dir.path());
    config["state_path"] = json!(dir.path().join("state.json"));
    config["heartbeat_path"] = json!(dir.path().join("heartbeat.json"));
    for rule in config["rules"].as_object_mut().unwrap().values_mut() {
        rule["enabled"] = json!(false);
    }
    (dir, config)
}

#[test]
fn run_dry_run_matches_fixture_shape_and_never_writes_or_sends() {
    let (dir, mut config) = run_fixture();
    config["enabled"] = json!(true);
    config["dry_run"] = json!(true);
    let state_path = dir.path().join("state.json");
    std::fs::write(&state_path, r#"{"alerts":{"kept":{"last_seen":1}}}"#).unwrap();
    let owner = FakeOwner {
        route: "cos".into(),
        through: None,
        posts: RefCell::new(vec![]),
        fail: false,
    };
    let mut sends = Vec::new();
    let mut sender = |to: &str, subject: &str, body: &str| -> Result<(), SendError> {
        sends.push((to.to_owned(), subject.to_owned(), body.to_owned()));
        Ok(())
    };
    let report = run(&config, 100.0, false, &mut sender, &owner, None, None).unwrap();
    assert_eq!(report["mode"], "dry-run");
    assert_eq!(report["why_not_live"], "config dry_run=true");
    assert_eq!(
        report["counts"],
        json!({"findings":0,"vp":0,"owner":0,"needs_attention":0})
    );
    assert_eq!(report["lines"], json!([]));
    assert!(sends.is_empty());
    assert!(owner.posts.borrow().is_empty());
    assert_eq!(
        std::fs::read_to_string(state_path).unwrap(),
        r#"{"alerts":{"kept":{"last_seen":1}}}"#
    );
    assert!(!dir.path().join("heartbeat.json").exists());
}

#[test]
fn run_confirms_matching_route_and_reports_channel_mismatch_once() {
    let (dir, mut config) = run_fixture();
    config["enabled"] = json!(true);
    config["dry_run"] = json!(false);
    let mut state = super::super::state::empty_state();
    state["owner_pages"]["cos-page"] =
        json!({"brief_id":1065,"posted_at":90.0,"confirmed_at":null,"desk_channel":"cos"});
    state["owner_pages"]["legacy-page"] =
        json!({"brief_id":9,"posted_at":90.0,"confirmed_at":null});
    save_state(&dir.path().join("state.json"), &state).unwrap();
    let owner = FakeOwner {
        route: "cos".into(),
        through: Some(json!(1065)),
        posts: RefCell::new(vec![]),
        fail: false,
    };
    let mut sender = |_: &str, _: &str, _: &str| Ok(());
    let report = run(&config, 100.0, false, &mut sender, &owner, None, None).unwrap();
    let persisted = read_json(&dir.path().join("state.json")).unwrap();
    assert_eq!(persisted["owner_pages"]["cos-page"]["confirmed_at"], 100.0);
    assert_eq!(
        persisted["owner_pages"]["legacy-page"]["status"],
        "undelivered"
    );
    assert_eq!(
        persisted["owner_pages"]["legacy-page"]["undelivered_reason"],
        "channel-mismatch"
    );
    assert!(report["problems"][0]
        .as_str()
        .unwrap()
        .contains("UNDELIVERED"));
    assert_eq!(report["sent"], json!(["vp"]));
    assert!(dir.path().join("heartbeat.json").exists());

    let retry = run(&config, 101.0, false, &mut sender, &owner, None, None).unwrap();
    assert_eq!(retry["problems"], json!([]));
    assert_eq!(retry["sent"], json!([]));
    assert!(owner.posts.borrow().is_empty());
}

#[test]
fn confirm_pending_sorts_pages_and_accepts_python_sized_ids() {
    let (_dir, config) = run_fixture();
    let large_id: Value = serde_json::from_str("92233720368547758081234").unwrap();
    let mut pages = serde_json::Map::new();
    // Insert in reverse lexical order so the observed problem order proves the sort.
    pages.insert(
        "z-page".into(),
        json!({"brief_id":1,"posted_at":90.0,"confirmed_at":null,"desk_channel":"vp"}),
    );
    pages.insert(
        "a-page".into(),
        json!({"brief_id":1,"posted_at":90.0,"confirmed_at":null,"desk_channel":"vp"}),
    );
    pages.insert(
        "large-page".into(),
        json!({"brief_id":large_id,"posted_at":90.0,"confirmed_at":null,"desk_channel":"cos"}),
    );
    let mut state = super::super::state::empty_state();
    state["owner_pages"] = Value::Object(pages);
    let owner = FakeOwner {
        route: "cos".into(),
        through: Some(large_id),
        posts: RefCell::new(vec![]),
        fail: false,
    };
    let mut problems = Vec::new();
    confirm_pending(&config, &mut state, &owner, 100.0, &mut problems);
    assert!(problems[0]
        .as_str()
        .unwrap()
        .starts_with("owner page a-page"));
    assert!(problems[1]
        .as_str()
        .unwrap()
        .starts_with("owner page z-page"));
    assert_eq!(state["owner_pages"]["large-page"]["confirmed_at"], 100.0);
}

#[test]
fn run_vp_delivery_failure_persists_failure_without_refreshing_heartbeat() {
    let (dir, mut config) = run_fixture();
    config["enabled"] = json!(true);
    config["dry_run"] = json!(false);
    config["rules"]["host.load"]["enabled"] = json!(true);
    super::super::state::save_state(
        &dir.path().join("state.json"),
        &super::super::state::empty_state(),
    )
    .unwrap();
    let owner = FakeOwner {
        route: "cos".into(),
        through: None,
        posts: RefCell::new(vec![]),
        fail: false,
    };
    let host = |_path: &Path| {
        Ok(super::super::rules_host::HostSample {
            load1: 8.0,
            load5: Some(8.0),
            cpus: 1,
            used_fraction: 0.5,
            free_bytes: 8_000_000_000,
            inode_used_fraction: 0.1,
        })
    };
    let mut sender = |_: &str, _: &str, _: &str| {
        Err(SendError {
            kind: "RuntimeError".into(),
            message: "fake transport down".into(),
        })
    };
    let report = run(
        &config,
        100.0,
        false,
        &mut sender,
        &owner,
        None,
        Some(&host),
    )
    .unwrap();
    assert_eq!(
        report["delivery_error"],
        "RuntimeError: fake transport down"
    );
    assert!(!dir.path().join("heartbeat.json").exists());
    let persisted = read_json(&dir.path().join("state.json")).unwrap();
    assert_eq!(persisted["delivery_failure_count"], 1);
    assert_eq!(persisted["last_run"], 100.0);
}

#[test]
fn run_cooldown_dedupes_across_persisted_state_and_reopens_at_boundary() {
    let (dir, mut config) = run_fixture();
    config["enabled"] = json!(true);
    config["dry_run"] = json!(false);
    config["rules"]["host.load"]["enabled"] = json!(true);
    save_state(
        &dir.path().join("state.json"),
        &super::super::state::empty_state(),
    )
    .unwrap();
    let owner = FakeOwner {
        route: "cos".into(),
        through: None,
        posts: RefCell::new(vec![]),
        fail: false,
    };
    let host = |_path: &Path| {
        Ok(super::super::rules_host::HostSample {
            load1: 8.0,
            load5: Some(8.0),
            cpus: 1,
            used_fraction: 0.5,
            free_bytes: 8_000_000_000,
            inode_used_fraction: 0.1,
        })
    };
    let sends = RefCell::new(Vec::new());
    let mut sender = |to: &str, subject: &str, body: &str| {
        sends
            .borrow_mut()
            .push((to.to_owned(), subject.to_owned(), body.to_owned()));
        Ok(())
    };
    let first = run(
        &config,
        100.0,
        false,
        &mut sender,
        &owner,
        None,
        Some(&host),
    )
    .unwrap();
    assert_eq!(first["sent"], json!(["vp"]));
    assert_eq!(sends.borrow().len(), 1);
    let second = run(
        &config,
        101.0,
        false,
        &mut sender,
        &owner,
        None,
        Some(&host),
    )
    .unwrap();
    assert_eq!(second["decisions"][0]["vp"], "dedupe");
    assert_eq!(second["sent"], json!([]));
    assert_eq!(
        sends.borrow().len(),
        1,
        "persisted cooldown prevents a duplicate send"
    );
    let boundary = run(
        &config,
        1900.0,
        false,
        &mut sender,
        &owner,
        None,
        Some(&host),
    )
    .unwrap();
    assert_eq!(boundary["sent"], json!(["vp"]));
    assert_eq!(
        sends.borrow().len(),
        2,
        "exact cooldown boundary permits a new send"
    );
}

#[test]
fn run_retries_confirmation_without_reposting_and_sends_resolution_once() {
    let (dir, mut config) = run_fixture();
    config["enabled"] = json!(true);
    config["dry_run"] = json!(false);
    let mut state = super::super::state::empty_state();
    state["owner_pages"]["host.disk_full|/tmp:0.98"] = json!({
        "brief_id":1065,"posted_at":90.0,"confirmed_at":null,"desk_channel":"cos"
    });
    save_state(&dir.path().join("state.json"), &state).unwrap();
    let owner = FakeOwner {
        route: "cos".into(),
        through: None,
        posts: RefCell::new(vec![]),
        fail: false,
    };
    let mut sender = |_: &str, _: &str, _: &str| Ok(());
    let first = run(&config, 100.0, false, &mut sender, &owner, None, None).unwrap();
    assert_eq!(first["sent"], json!([]));
    assert!(owner.posts.borrow().is_empty());
    assert!(
        read_json(&dir.path().join("state.json")).unwrap()["owner_pages"]
            ["host.disk_full|/tmp:0.98"]["confirmed_at"]
            .is_null()
    );

    // The bridge watermark advances on the next invocation; the runner confirms and resolves
    // the prior page without reposting the original page.
    let owner = FakeOwner {
        route: "cos".into(),
        through: Some(json!(1065)),
        posts: RefCell::new(vec![]),
        fail: false,
    };
    let second = run(&config, 101.0, false, &mut sender, &owner, None, None).unwrap();
    assert_eq!(second["sent"], json!([]));
    assert_eq!(owner.posts.borrow().len(), 1);
    assert!(owner.posts.borrow()[0].contains("Watchdog OWNER resolved"));
    assert!(
        read_json(&dir.path().join("state.json")).unwrap()["owner_pages"]
            .get("host.disk_full|/tmp:0.98")
            .is_none()
    );
}
