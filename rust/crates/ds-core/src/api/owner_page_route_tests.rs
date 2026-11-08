//! t3700: the owner_page route. A critical emergency page (security, outage or money) is delivered to
//! Telegram: it sits in the Telegram outbox. Any other owner page is recorded to the owner mailbox only
//! and never appears in the Telegram outbox.

use super::*;
use crate::config::Env;
use crate::flock::panes::Stores;
use crate::inbox_store::Store;
use crate::roles::Roles;
use crate::server::RequestCtx;
use crate::terminal::{fakes::FakeTerminal, Terminal};
use serde_json::json;
use std::sync::{Arc, Mutex};

struct PageRouteEnv {
    terminal: Mutex<Box<dyn Terminal>>,
    owner_push: crate::owner::OwnerPush,
}

impl PaneInfoEnv for PageRouteEnv {
    fn list_panes_grouped(&self) -> Vec<Value> {
        Vec::new()
    }

    fn get_pane(&self, pane_id: &str) -> Option<Value> {
        Some(json!({"pane_id": pane_id, "pane_pid": std::process::id()}))
    }
}

impl CommandEnv for PageRouteEnv {
    fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
        &self.terminal
    }

    fn now_s(&self) -> f64 {
        1_800_000_100.0
    }

    fn owner_push(&self) -> Result<&crate::owner::OwnerPush, DunkError> {
        Ok(&self.owner_push)
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    flock: crate::flock::Flock,
    env: PageRouteEnv,
}

fn fixture(tag: &str) -> Fixture {
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
    let env = PageRouteEnv {
        terminal: Mutex::new(Box::new(FakeTerminal::default())),
        owner_push: crate::owner::OwnerPush::open(dir.path().join(format!("{tag}-owner-push.db")))
            .unwrap(),
    };
    Fixture {
        _dir: dir,
        flock,
        env,
    }
}

/// Record one page through the owner_page handler, as the owner pane, with an optional class.
fn record_page(fixture: &Fixture, body: &str, class: Option<&str>) -> Value {
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
    let mut args: Args = json!({"body": body}).as_object().unwrap().clone();
    if let Some(class) = class {
        args.insert("class".into(), json!(class));
    }
    h_owner_page(&context, args).unwrap()
}

fn telegram_outbox(fixture: &Fixture) -> Vec<String> {
    crate::owner::OwnerConversations::new(&fixture.flock, &fixture.env)
        .outbox(Some("telegram"), Some(50), Some("w1:p1"))
        .unwrap()
        .messages
        .into_iter()
        .map(|message| message.body)
        .collect()
}

#[test]
fn each_emergency_class_reaches_telegram() {
    for class in ["security", "outage", "money"] {
        let fixture = fixture(&format!("emergency-{class}"));
        let body = format!("PAGE OWNER {class}: critical");
        let out = record_page(&fixture, &body, Some(class));
        assert_eq!(out["transport"], "telegram", "{class} is an emergency");
        assert_eq!(
            telegram_outbox(&fixture),
            vec![body],
            "{class} is in the Telegram outbox"
        );
    }
}

#[test]
fn a_routine_page_is_recorded_to_the_mailbox_and_never_to_telegram() {
    let fixture = fixture("routine");
    let out = record_page(&fixture, "PAGE OWNER routine status", Some("status"));
    assert_eq!(out["transport"], "report", "a routine page is mailbox-only");
    assert_eq!(
        out["message"]["mailbox"], "owner",
        "the page is in the owner mailbox"
    );
    assert!(
        telegram_outbox(&fixture).is_empty(),
        "a routine page never reaches Telegram"
    );
}

#[test]
fn a_page_with_no_class_is_mailbox_only() {
    let fixture = fixture("no-class");
    let out = record_page(&fixture, "PAGE OWNER unclassified", None);
    assert_eq!(out["transport"], "report");
    assert!(telegram_outbox(&fixture).is_empty());
}

#[test]
fn an_emergency_class_match_ignores_case_and_spaces() {
    let fixture = fixture("case");
    let out = record_page(&fixture, "PAGE OWNER Money: cap", Some(" MONEY "));
    assert_eq!(out["transport"], "telegram");
}
