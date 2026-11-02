use super::*;
use crate::flock::{panes::Stores, sends::Message, status_runtime::StatusState, Flock};
use crate::inbox_store::{Outgoing, Store};
use crate::terminal::{fakes::FakeTerminal, Terminal};
use crate::{config::Env, server::RequestCtx, tasks::Tasks};
use std::sync::{Arc, Mutex};

struct Fixture {
    flock: Flock,
    runtime: tokio::runtime::Runtime,
    terminal: Mutex<Box<dyn Terminal>>,
    request: RequestCtx,
    inbox: Arc<Store>,
    dir: tempfile::TempDir,
}
impl Fixture {
    fn new(persist: bool, env: Env) -> Self {
        let dir = crate::testsupport::dir("status-runtime");
        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            persist,
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let inbox =
            Arc::new(Store::open(dir.path().join("inbox.db"), dir.path().join("blobs")).unwrap());
        let tasks = Arc::new(
            runtime
                .block_on(Tasks::open(dir.path().join("tasks.db")))
                .unwrap(),
        );
        runtime
            .block_on(tasks.add("global", "real task", "body"))
            .unwrap();
        flock.attach_stores(Stores {
            inbox: Some(Arc::clone(&inbox)),
            tasks: Some(tasks),
            ..Stores::default()
        });
        let executable = dir.path().join("daemon");
        std::fs::write(&executable, b"running binary").unwrap();
        flock
            .attach_status_state(StatusState::at_start(
                "release-2.71.4",
                dir.path().join("whole-state.json"),
                executable,
                env,
            ))
            .unwrap();
        Self {
            flock,
            runtime,
            terminal: Mutex::new(Box::new(FakeTerminal::new())),
            request: RequestCtx {
                self_pane_id: Some("w1:p1".into()),
                ..RequestCtx::default()
            },
            inbox,
            dir,
        }
    }
    fn adapter(&self) -> RequestRuntimeAdapter<'_> {
        RequestRuntimeAdapter::new(
            &self.flock,
            &self.request,
            self.runtime.handle(),
            &self.terminal,
        )
    }
    fn send(&self, mailbox: &str, sender: Option<&str>) {
        self.inbox
            .send(&Outgoing {
                mailbox,
                sender,
                body: Some("real mail"),
                ..Outgoing::default()
            })
            .unwrap();
    }
}

#[test]
fn production_env_status_core_uses_concrete_services() {
    let f = Fixture::new(
        false,
        Env::from_pairs(&[("DUNKINGSHEEP_FEDERATED_INPUT", "1")]),
    );
    let a = f.adapter();
    let env = CommandEnv::status_env(&a).unwrap();
    assert_eq!(env.version(), "release-2.71.4");
    assert_eq!(env.federated_input(), json!(true));
    let before = env.code_status();
    assert_eq!(before["code_stale"], false);
    assert_eq!(before["code_hash"].as_str().unwrap().len(), 12);
    std::fs::write(f.dir.path().join("daemon"), b"different rebuilt binary").unwrap();
    let after = env.code_status();
    assert_eq!(after["code_hash"], before["code_hash"]);
    assert_eq!(after["code_stale"], true);
    assert_eq!(after["code_changed_modules"], json!(["daemon"]));
    assert!(f
        .flock
        .attach_status_state(StatusState::at_start(
            "replacement",
            "wrong",
            "missing",
            Env::default()
        ))
        .is_err());
    assert_eq!(env.federated_input(), true);
}
#[test]
fn production_env_status_view_uses_concrete_services() {
    let f = Fixture::new(false, Env::default());
    let a = f.adapter();
    let env = CommandEnv::status_env(&a).unwrap();
    assert!(std::ptr::eq(env.dunks(), &f.flock.dunks));
    assert_eq!(env.pid(), std::process::id());
    assert!(env.started_at() <= env.now());
    let start = env.started_at();
    assert_eq!(env.started_at(), start);
    assert!(std::ptr::eq(
        CommandEnv::view_cache(&a).unwrap(),
        f.flock.view_cache()
    ));
}
#[test]
fn production_env_status_counts_uses_concrete_services() {
    let f = Fixture::new(false, Env::default());
    f.send("one", None);
    f.send("two", None);
    f.flock.outbox.lock().unwrap().insert(
        "pane".into(),
        vec![
            Message::new("m1", "pane", "lane", "text", "send_text"),
            Message::new("m2", "pane", "lane", "text", "send_text"),
        ],
    );
    let a = f.adapter();
    let env = CommandEnv::status_env(&a).unwrap();
    assert_eq!(env.queued_messages(), 2);
    assert_eq!(env.inbox_unread().unwrap(), 2);
    assert_eq!(env.task_totals().unwrap()["open"], 1);
    f.runtime.block_on(async {
        let error = env.task_totals().unwrap_err().to_string();
        assert!(
            error.contains("dedicated blocking request thread"),
            "{error}"
        );
    });
    f.flock.attach_stores(Stores::default());
    assert!(env
        .inbox_unread()
        .unwrap_err()
        .to_string()
        .contains("inbox store"));
    assert!(env
        .task_totals()
        .unwrap_err()
        .to_string()
        .contains("task store"));
}
#[test]
fn production_env_status_state_uses_concrete_services() {
    let f = Fixture::new(false, Env::default());
    let a = f.adapter();
    let env = CommandEnv::status_env(&a).unwrap();
    assert_eq!(env.task_followers(), 0);
    assert_eq!(env.quarantined_mailboxes(), None);
    assert_eq!(env.last_message(), "");
    f.flock
        .task_waiters
        .lock()
        .unwrap()
        .insert("pane".into(), "global".into());
    f.flock.set_last_message("actual completed operation");
    assert_eq!(env.task_followers(), 1);
    assert_eq!(env.last_message(), "actual completed operation");
    let roles = crate::roles::Roles::new(
        &Env::default(),
        Some(f.dir.path().join("roles.json")),
        Some(f.dir.path().join("tokens.json")),
        None,
    )
    .unwrap();
    f.flock.attach_roles(roles, None);
    f.flock
        .lane_register(
            Some("w9:p9"),
            Some("owner"),
            None,
            None,
            None,
            Some("w9:p9"),
            &a,
        )
        .unwrap();
    assert_eq!(env.last_message(), "Registered w9:p9 as owner");
    f.flock.clear_caller();
    assert!(f
        .flock
        .lane_register(
            Some("w9:p8"),
            Some("owner"),
            None,
            None,
            None,
            Some("w9:p7"),
            &a
        )
        .is_err());
    assert_eq!(env.last_message(), "Registered w9:p9 as owner");
    let mut terminal = FakeTerminal::new();
    terminal.panes[0].command = "codex".into();
    f.send("codex", None);
    f.send("codex", None);
    f.send("private-lane", None);
    f.flock.quarantine_shared_mailboxes(&terminal).unwrap();
    assert_eq!(
        env.quarantined_mailboxes(),
        Some(json!([{"was":"codex","now":"unrouted:codex","messages":2}]))
    );
    assert_eq!(
        crate::inbox::count(f.inbox.read_pool(), "codex", &[], env.now())
            .unwrap()
            .total,
        0
    );
    assert_eq!(
        crate::inbox::count(f.inbox.read_pool(), "unrouted:codex", &[], env.now())
            .unwrap()
            .total,
        2
    );
    assert_eq!(
        crate::inbox::count(f.inbox.read_pool(), "private-lane", &[], env.now())
            .unwrap()
            .total,
        1
    );
}
#[test]
fn production_env_status_backend_uses_concrete_services() {
    let f = Fixture::new(true, Env::default());
    let a = f.adapter();
    let env = CommandEnv::status_env(&a).unwrap();
    assert!(env.herdr_available());
    assert_eq!(
        env.state_file(),
        Some(
            f.dir
                .path()
                .join("whole-state.json")
                .to_string_lossy()
                .into_owned()
        )
    );
    *f.terminal.lock().unwrap() = Box::new(FakeTerminal::unavailable());
    assert!(!env.herdr_available());
    assert!(!env.herdr_hint().is_empty());
    let transient = Fixture::new(false, Env::default());
    assert_eq!(transient.adapter().helper_statusenv_state_file(), None);
}
#[test]
fn production_env_status_inbox_uses_concrete_services() {
    let f = Fixture::new(
        false,
        Env::from_pairs(&[("DUNKINGSHEEP_MAILBOX", " override-lane ")]),
    );
    f.send("override-lane", Some("lane-1"));
    f.send("other", None);
    let routing = f.dir.path().join("routing.json");
    std::fs::write(
        &routing,
        r#"{"p":{"pm":"pm","members":["lane-1","lane-1"]}}"#,
    )
    .unwrap();
    // Separate configured fixture to avoid mutating frozen startup inputs.
    let g = Fixture::new(
        false,
        Env::from_pairs(&[
            ("DUNKINGSHEEP_MAILBOX", " override-lane "),
            ("DUNKINGSHEEP_PM_ROUTING", routing.to_str().unwrap()),
        ]),
    );
    g.send("override-lane", Some("lane-1"));
    g.send("other", None);
    let a = g.adapter();
    let env = CommandEnv::status_env(&a).unwrap();
    let count = env.inbox_count("missing-pane").unwrap();
    assert_eq!(count["mailbox"], "override-lane");
    assert_eq!(count["unread"], 1);
    assert_eq!(count["from_lanes"], 1);
    assert!(env
        .inbox_count("")
        .unwrap_err()
        .contains("no mailbox given"));
    let h = Fixture::new(false, Env::default());
    h.send("absent-pane", None);
    let a = h.adapter();
    assert_eq!(
        a.helper_statusenv_inbox_count("absent-pane").unwrap()["mailbox"],
        "absent-pane"
    );
    assert!(a
        .helper_statusenv_inbox_count("unknown")
        .unwrap_err()
        .contains("cannot resolve this lane"));
}
#[test]
fn production_status_env_is_complete_and_drives_real_handlers() {
    let f = Fixture::new(
        false,
        Env::from_pairs(&[("DUNKINGSHEEP_MAILBOX", "self-mailbox")]),
    );
    f.send("self-mailbox", None);
    f.flock.set_last_message("real handler status");
    let a = f.adapter();
    let context = a.handler_context();
    let full = crate::api::h_status(
        &context,
        serde_json::from_value(json!({"full":true})).unwrap(),
    )
    .unwrap();
    assert_eq!(full["inbox_unread"], 1);
    assert_eq!(full["tasks"]["open"], 1);
    assert_eq!(full["message"], "real handler status");
    let cheap = crate::api::h_status(&context, Map::new()).unwrap();
    assert_eq!(cheap["message"], "real handler status");
    let view = CommandEnv::view_cache(&a).unwrap().view(
        CommandEnv::status_env(&a).unwrap(),
        Some("missing-pane"),
        true,
        5.0,
    );
    assert_eq!(view["inbox_count"]["unread"], 1);
    // poll/view does not require global store scans, unlike full status.
    f.flock.attach_stores(Stores::default());
    let poll = crate::flock::status::poll_status(CommandEnv::status_env(&a).unwrap());
    assert_eq!(poll["message"], "real handler status");
}
#[test]
fn unconfigured_status_is_a_named_refusal() {
    let dir = crate::testsupport::dir("unconfigured-status");
    let flock = Flock::new(dir.path().join("launches"), dir.path().join("lanes"), false);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let request = RequestCtx::default();
    let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
    let a = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);
    let error = CommandEnv::status_env(&a).err().unwrap().to_string();
    assert!(error.contains("status startup state is not configured"));
    assert!(crate::api::h_status(&a.handler_context(), Map::new()).is_err());
}
