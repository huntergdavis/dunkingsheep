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
