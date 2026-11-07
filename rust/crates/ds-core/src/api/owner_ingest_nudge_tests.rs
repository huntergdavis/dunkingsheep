//! t1613 / t1352: the desk is nudged once for each new Telegram owner question, and the web route
//! keeps its own nudge. These tests drive the daemon's `owner_ingest` handler directly, with an
//! environment that records every inbox write the handler asks for.

use super::*;
use crate::config::Env;
use crate::flock::panes::Stores;
use crate::inbox_store::Store;
use crate::roles::Roles;
use crate::server::RequestCtx;
use crate::terminal::{fakes::FakeTerminal, Terminal};
use serde_json::json;
use std::cell::Cell;
use std::sync::{Arc, Mutex};

/// Counts the inbox writes the handler attempts. The write itself is refused, so the ingest is the
/// only thing that happens; the handler must still succeed (the mailbox row is the record).
struct NudgeRecorderEnv {
    terminal: Mutex<Box<dyn Terminal>>,
    owner_push: crate::owner::OwnerPush,
    panes: Vec<Value>,
    writes: Cell<usize>,
}

impl PaneInfoEnv for NudgeRecorderEnv {
    fn list_panes_grouped(&self) -> Vec<Value> {
        self.panes.clone()
    }

    fn get_pane(&self, pane_id: &str) -> Option<Value> {
        Some(json!({"pane_id": pane_id, "pane_pid": std::process::id()}))
    }
}

impl CommandEnv for NudgeRecorderEnv {
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
        &self.terminal
    }

    fn now_s(&self) -> f64 {
        1_800_000_100.0
    }

    fn owner_push(&self) -> Result<&crate::owner::OwnerPush, DunkError> {
        Ok(&self.owner_push)
    }

    fn with_inbox_env(
        &self,
        _operation: &mut dyn FnMut(
            &dyn crate::flock::commands_inbox::InboxEnv,
        ) -> Result<Value, DunkError>,
    ) -> Result<Value, DunkError> {
        self.writes.set(self.writes.get() + 1);
        Err(DunkError::Refused("recorded only".into()))
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    flock: crate::flock::Flock,
    env: NudgeRecorderEnv,
}

fn desk_panes() -> Vec<Value> {
    vec![json!({
        "pane_id": "w1:p2",
        "tab_label": "desk",
        "workspace_label": "dunk",
        "cwd": "/home/hunter/workspace/desk",
    })]
}

fn fixture(tag: &str, panes: Vec<Value>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let flock = crate::flock::Flock::new(
        dir.path().join(format!("{tag}-launches.json")),
        dir.path().join(format!("{tag}-lanes.json")),
        false,
    );
    let mut roles = Roles::new(
        &Env::default(),
        Some(dir.path().join(format!("{tag}-roles.json"))),
        Some(dir.path().join(format!("{tag}-tokens.json"))),
        None,
    )
    .unwrap();
    roles
        .register("w1:p1", "owner", None, None, None, None, None, None)
        .unwrap();
    roles
        .register("w1:p2", "worker", None, None, None, None, None, None)
        .unwrap();
    flock.attach_roles(roles, None);
    let inbox = Arc::new(
        Store::open(
            dir.path().join(format!("{tag}-inbox.db")),
            dir.path().join(format!("{tag}-blobs")),
        )
        .unwrap(),
    );
    flock.attach_stores(Stores {
        inbox: Some(inbox),
        ..Stores::default()
    });
    let env = NudgeRecorderEnv {
        terminal: Mutex::new(Box::new(FakeTerminal::default())),
        owner_push: crate::owner::OwnerPush::open(dir.path().join(format!("{tag}-owner-push.db")))
            .unwrap(),
        panes,
        writes: Cell::new(0),
    };
    Fixture {
        _dir: dir,
        flock,
        env,
    }
}

fn ingest(fixture: &Fixture, transport: &str, external_id: &str) -> Result<Value, DunkError> {
    let request = RequestCtx {
        self_pane_id: Some("w1:p1".into()),
        ..RequestCtx::default()
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let context = HandlerContext {
        flock: &fixture.flock,
        request: &request,
        env: &fixture.env,
        runtime: runtime.handle(),
    };
    let args: Args = json!({
        "body": "is the desk up?",
        "transport": transport,
        "external_id": external_id,
        "subject": null,
    })
    .as_object()
    .unwrap()
    .clone();
    h_owner_ingest(&context, args)
}

#[test]
fn a_new_telegram_question_nudges_the_desk_once() {
    let fixture = fixture("nudge-once", desk_panes());
    let out = ingest(&fixture, "telegram", "telegram:7").unwrap();
    assert_eq!(out["duplicate"], json!(false));
    assert_eq!(
        fixture.env.writes.get(),
        1,
        "one nudge to the desk for a new telegram question"
    );
}

#[test]
fn a_redelivered_telegram_question_nudges_nothing_new() {
    let fixture = fixture("nudge-dup", desk_panes());
    ingest(&fixture, "telegram", "telegram:8").unwrap();
    let again = ingest(&fixture, "telegram", "telegram:8").unwrap();
    assert_eq!(again["duplicate"], json!(true));
    assert_eq!(fixture.env.writes.get(), 1, "a redelivery adds no nudge");
}

#[test]
fn a_web_question_is_nudged_by_its_own_route_not_by_the_ingest() {
    let fixture = fixture("nudge-web", desk_panes());
    ingest(&fixture, "web", "web-1").unwrap();
    assert_eq!(
        fixture.env.writes.get(),
        0,
        "web stays single-nudge: the web route sends its own"
    );
}
