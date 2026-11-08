//! t3714 (t3592 and the t3711 requirement): the owner conversation through `OwnerConversations::conversation`.
//! More new routine rows than the limit must not hide older owner decisions, legacy decisions, or replies,
//! with and without a `before` boundary. The checks are on the window's rows, count, waiting list and order.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use super::{Conversation, OwnerConversations};
use crate::config::Env;
use crate::flock::mailbox::OWNER_MAILBOX;
use crate::flock::panes::{PaneInfoEnv, Stores};
use crate::flock::Flock;
use crate::inbox_store::{Outgoing, Store};
use crate::roles::Roles;

const DESK: &str = "w1:p1";
const OWNER: &str = "w1:p2";
const WORKER: &str = "w1:p3";

struct NoPanes;

impl PaneInfoEnv for NoPanes {
    fn list_panes_grouped(&self) -> Vec<Value> {
        Vec::new()
    }

    fn get_pane(&self, _: &str) -> Option<Value> {
        None
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    flock: Flock,
    store: Arc<Store>,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let flock = Flock::new(
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
        for (pane, role) in [(DESK, "desk"), (OWNER, "owner"), (WORKER, "worker")] {
            roles
                .register(pane, role, None, None, None, None, None, None)
                .unwrap();
        }
        flock.attach_roles(roles, None);
        let store = Arc::new(
            Store::open(
                dir.path().join(format!("{tag}-inbox.db")),
                dir.path().join(format!("{tag}-blobs")),
            )
            .unwrap(),
        );
        flock.attach_stores(Stores {
            inbox: Some(store.clone()),
            ..Stores::default()
        });
        Self {
            _dir: dir,
            flock,
            store,
        }
    }

    fn service(&self) -> OwnerConversations<'_> {
        OwnerConversations::new(&self.flock, &NoPanes)
    }

    /// Send one owner-mailbox row. A short pause keeps created_at distinct, so the boundary is deterministic.
    fn send(
        &self,
        subject: &str,
        body: &str,
        transport: Option<&str>,
        reply_to: Option<&str>,
    ) -> String {
        std::thread::sleep(Duration::from_millis(2));
        self.store
            .send(&Outgoing {
                mailbox: OWNER_MAILBOX,
                subject: Some(subject),
                body: Some(body),
                mailbox_label: Some(OWNER_MAILBOX),
                transport,
                reply_to,
                ..Outgoing::default()
            })
            .unwrap()
    }

    fn created(&self, id: &str) -> f64 {
        self.store.get_message(id).unwrap().summary.created_at
    }
}

fn ids(window: &Conversation) -> Vec<String> {
    window
        .messages
        .iter()
        .map(|row| row["id"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn routine_rows_newer_than_the_limit_do_not_hide_older_decisions_legacy_or_replies() {
    let fixture = Fixture::new("conversation-routine");
    // Oldest first: a decision, a legacy decision, a blocker that a reply answers, and the reply.
    let decision = fixture.send("[owner-decision] older decision", "decide X", None, None);
    let legacy = fixture.send("Decision needed on the deploy", "yes or no", None, None);
    let question = fixture.send(
        "[owner-blocker] PR #810 blocked",
        "stuck",
        Some("telegram"),
        None,
    );
    let reply = fixture.send("answer", "ok", Some("telegram"), Some(&question));
    // More newer routine verdicts than the limit below.
    let verdicts: Vec<String> = (0..5)
        .map(|n| {
            fixture.send(
                "[review-verdict] PR #900 accept",
                &format!("verdict {n}"),
                None,
                None,
            )
        })
        .collect();

    // Limit 3: the newest three visible rows are legacy, question, reply. No verdict is listed.
    let window = fixture
        .service()
        .conversation(Some(3), None, Some(DESK))
        .unwrap();
    assert_eq!(
        vec![legacy.clone(), question.clone(), reply.clone()],
        ids(&window)
    );
    assert_eq!(3, window.count);
    assert_eq!(
        vec![legacy.clone()],
        window.waiting,
        "legacy is queued and unanswered; the blocker is answered; the reply is never waiting"
    );
    assert!(verdicts.iter().all(|id| !ids(&window).contains(id)));

    // A generous limit lists every visible row, oldest first, and still no verdict.
    let full = fixture
        .service()
        .conversation(Some(10), None, Some(DESK))
        .unwrap();
    assert_eq!(
        vec![
            decision.clone(),
            legacy.clone(),
            question.clone(),
            reply.clone()
        ],
        ids(&full)
    );
    assert_eq!(4, full.count);
    assert_eq!(vec![decision.clone(), legacy.clone()], full.waiting);
    assert!(verdicts.iter().all(|id| !ids(&full).contains(id)));
}

#[test]
fn the_before_boundary_reaches_older_decisions_past_routine_rows() {
    let fixture = Fixture::new("conversation-routine-before");
    let decision = fixture.send("[owner-decision] older decision", "decide X", None, None);
    let legacy = fixture.send("Decision needed on the deploy", "yes or no", None, None);
    let question = fixture.send(
        "[owner-blocker] PR #810 blocked",
        "stuck",
        Some("telegram"),
        None,
    );
    let boundary = fixture.created(&question);
    for n in 0..5 {
        fixture.send(
            "[review-note] t3641 evidence",
            &format!("note {n}"),
            None,
            None,
        );
    }

    // Older than the boundary: the decision and the legacy decision. Both are listed with a generous limit.
    let older = fixture
        .service()
        .conversation(Some(10), Some(boundary), Some(DESK))
        .unwrap();
    assert_eq!(vec![decision.clone(), legacy.clone()], ids(&older));
    assert_eq!(2, older.count);
    assert_eq!(vec![decision.clone(), legacy.clone()], older.waiting);

    // With a limit of one, the newest visible row older than the boundary is the legacy decision.
    let newest_before = fixture
        .service()
        .conversation(Some(1), Some(boundary), Some(DESK))
        .unwrap();
    assert_eq!(vec![legacy.clone()], ids(&newest_before));
    assert_eq!(1, newest_before.count);
    assert_eq!(vec![legacy], newest_before.waiting);
}
