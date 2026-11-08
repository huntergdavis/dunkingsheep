//! t3625: the owner-mailbox channel through `run`, mirroring tests/test_watchdog_owner_mailbox.py.
//! A critical outage is one owner write; a disabled owner escalation writes nothing; a refused write
//! is recorded UNDELIVERED and is not retried in the same run.

use super::rules_host::HostSample;
use super::run::{run, OwnerTransport, SendError};
use crate::error::DunkError;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::path::Path;

const OUTAGE_KEY: &str = "host.disk_full|/:0.98";

/// The owner channel as the live route builds it: an owner-mailbox write, confirmed on post for a
/// routine page and on the outbox receipt for an emergency page (t3700).
struct OwnerMailboxFake {
    posts: RefCell<Vec<String>>,
    fail: bool,
}

impl OwnerTransport for OwnerMailboxFake {
    fn desk_channel(&self) -> Result<String, DunkError> {
        Ok("owner-mailbox".into())
    }

    fn post(&self, body: &str) -> Result<i64, SendError> {
        self.posts.borrow_mut().push(body.to_owned());
        if self.fail {
            Err(SendError {
                kind: "ClientError".into(),
                message: "fake refused write".into(),
            })
        } else {
            Ok(1065)
        }
    }

    fn confirmed_through(&self) -> Result<Option<Value>, DunkError> {
        Ok(None)
    }

    fn confirms_on_post_for(&self, emergency: bool) -> bool {
        !emergency
    }
}

/// A live config whose only enabled rule is a critical outage: `host.disk_full` at 99% of `/`.
fn outage_config(owner_enabled: bool) -> (tempfile::TempDir, Value) {
    let dir = tempfile::tempdir().unwrap();
    let mut config: Value = serde_json::from_str(include_str!(
        "../../../../../packaging/watchdog.example.json"
    ))
    .unwrap();
    config["stores"]["config_dir"] = json!(dir.path());
    config["state_path"] = json!(dir.path().join("state.json"));
    config["heartbeat_path"] = json!(dir.path().join("heartbeat.json"));
    config["panes"]["source"] = json!("none");
    for rule in config["rules"].as_object_mut().unwrap().values_mut() {
        rule["enabled"] = json!(false);
    }
    config["enabled"] = json!(true);
    config["dry_run"] = json!(false);
    config["escalation"]["owner"]["enabled"] = json!(owner_enabled);
    config["escalation"]["owner"]["approved_by"] = json!("fixture");
    config["escalation"]["owner"]["channel"] = json!("owner-mailbox");
    let disk = &mut config["rules"]["host.disk_full"];
    disk["enabled"] = json!(true);
    disk["must_hold_runs"] = json!(1);
    disk["paths"] = json!(["/"]);
    // A missing state file is a first run, which suppresses owner pages (fail closed); seed it.
    super::state::save_state(&dir.path().join("state.json"), &super::state::empty_state()).unwrap();
    (dir, config)
}

fn read_state(dir: &tempfile::TempDir) -> Value {
    serde_json::from_str(&std::fs::read_to_string(dir.path().join("state.json")).unwrap()).unwrap()
}

#[test]
fn a_critical_outage_is_one_owner_mailbox_write_and_waits_for_the_receipt() {
    let (dir, config) = outage_config(true);
    let owner = OwnerMailboxFake {
        posts: RefCell::new(vec![]),
        fail: false,
    };
    let host = |_path: &Path| {
        Ok(HostSample {
            load1: 0.1,
            load5: Some(0.1),
            cpus: 1,
            used_fraction: 0.99,
            free_bytes: 1_000_000,
            inode_used_fraction: 0.1,
        })
    };
    let mut sender = |_: &str, _: &str, _: &str| Ok(());
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
        owner.posts.borrow().len(),
        1,
        "a critical outage is one owner write; report: {report}"
    );
    assert!(owner.posts.borrow()[0].contains("PAGE OWNER"));
    let state = read_state(&dir);
    assert_eq!(
        state["owner_pages"][OUTAGE_KEY]["desk_channel"],
        "owner-mailbox"
    );
    assert!(
        state["owner_pages"][OUTAGE_KEY]["confirmed_at"].is_null(),
        "an emergency page is confirmed only by the outbox receipt (t3700)"
    );
}

#[test]
fn a_disabled_owner_escalation_writes_nothing() {
    let (_dir, config) = outage_config(false);
    let owner = OwnerMailboxFake {
        posts: RefCell::new(vec![]),
        fail: false,
    };
    let host = |_path: &Path| {
        Ok(HostSample {
            load1: 0.1,
            load5: Some(0.1),
            cpus: 1,
            used_fraction: 0.99,
            free_bytes: 1_000_000,
            inode_used_fraction: 0.1,
        })
    };
    let mut sender = |_: &str, _: &str, _: &str| Ok(());
    run(
        &config,
        100.0,
        false,
        &mut sender,
        &owner,
        None,
        Some(&host),
    )
    .unwrap();

    assert!(
        owner.posts.borrow().is_empty(),
        "owner escalation disabled: no owner write"
    );
}

#[test]
fn a_refused_owner_write_is_undelivered_and_not_retried() {
    let (dir, config) = outage_config(true);
    let owner = OwnerMailboxFake {
        posts: RefCell::new(vec![]),
        fail: true,
    };
    let host = |_path: &Path| {
        Ok(HostSample {
            load1: 0.1,
            load5: Some(0.1),
            cpus: 1,
            used_fraction: 0.99,
            free_bytes: 1_000_000,
            inode_used_fraction: 0.1,
        })
    };
    let mut sender = |_: &str, _: &str, _: &str| Ok(());
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
        owner.posts.borrow().len(),
        1,
        "a refused write is not retried in the same run"
    );
    let problems = report["problems"].as_array().unwrap();
    assert!(
        problems.iter().any(|problem| problem
            .as_str()
            .is_some_and(|text| text.contains("recorded as UNDELIVERED"))),
        "{problems:?}"
    );
    assert!(read_state(&dir)["owner_pages"].get(OUTAGE_KEY).is_none());
}
