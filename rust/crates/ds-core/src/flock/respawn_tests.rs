use super::*;
use crate::flock::{
    commands_inbox::{InboxEnv, Nudge, RouteEntry},
    mailbox::MailboxEnv,
    panes::{PaneInfoEnv, Stores},
    sends::SendEnv,
};
use crate::{db::ReadPool, inbox_store::Store, roles::Roles, terminal::tmux::TmuxBackend};
use std::{
    os::unix::fs::PermissionsExt,
    process::{Child, Command},
    sync::Arc,
};
const P: &str = "%2";
const OWNER: &str = "%1";
struct Children(Vec<Child>);
impl Drop for Children {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
struct Fixture {
    flock: Flock,
    terminal: Mutex<Box<dyn Terminal>>,
    pool: ReadPool,
    store: Store,
    runtime: tokio::runtime::Runtime,
    env: Env,
    children: Children,
    dir: tempfile::TempDir,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("claude");
        std::fs::copy("/bin/sleep", &binary).unwrap();
        let mut children = Children(Vec::new());
        for _ in 0..3 {
            children
                .0
                .push(Command::new(&binary).arg("300").spawn().unwrap());
        }
        let pids: Vec<_> = children.0.iter().map(Child::id).collect();
        std::fs::write(dir.path().join("pid"), pids[0].to_string()).unwrap();
        std::fs::write(dir.path().join("screen"), "› Ask Codex to do anything\n").unwrap();
        let script = dir.path().join("tmux");
        let root = crate::terminal::tmux::shell_join(&[dir.path().to_str().unwrap()]);
        let body = format!(
            r#"#!/bin/sh
cd {root} || exit 1
printf '%s\n' "$*" >> calls
case "$1" in
list-panes)
  reads=0; test ! -f reads || reads=$(cat reads); reads=$((reads+1)); echo "$reads" > reads
  if test -f race && test "$reads" -ge 2; then echo {new} > pid; fi
  p=$(cat pid)
  printf '%%2\037$1\037@1\037/tmp\037claude\037worker\0371\037ws\037tab\0370\0370\037%s\03780\03724\n' "$p"
  ;;
capture-pane) cat screen ;;
respawn-pane)
  n=0; test ! -f count || n=$(cat count); n=$((n+1)); echo "$n" > count
  if test ! -f same_pid; then
    if test "$n" = 1; then echo {new} > pid; else echo {rollback} > pid; fi
  fi
  if test -f bad_start; then
    if test "$n" = 1 || test -f bad_rollback; then printf 'sh: claude: not found\n' > screen; else printf 'ready agent\n' > screen; fi
  fi
  if test -f fail; then exit 1; fi
  ;;
esac
"#,
            new = pids[1],
            rollback = pids[2]
        );
        std::fs::write(&script, body).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let backend = TmuxBackend::with_registry(&script, dir.path().join("registry.json"));
        backend
            .registry()
            .remember("pane", P, json!({"name":"worker"}))
            .unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let tasks = runtime
            .block_on(crate::tasks::Tasks::open(dir.path().join("tasks.db")))
            .unwrap();
        let store = Store::open(dir.path().join("inbox.db"), dir.path().join("blobs")).unwrap();
        let pool = ReadPool::open(dir.path().join("inbox.db"), 2).unwrap();
        let env = Env::from_pairs(&[("HOME", dir.path().to_str().unwrap())]);
        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            true,
        );
        let mut roles = Roles::new(
            &env,
            Some(dir.path().join("roles.json")),
            Some(dir.path().join("tokens.json")),
            None,
        )
        .unwrap();
        roles
            .register(OWNER, "owner", None, None, None, None, None, None)
            .unwrap();
        roles
            .register(P, "worker", None, None, Some("claude"), None, None, None)
            .unwrap();
        flock.attach_roles(roles, None);
        flock.attach_stores(Stores {
            tasks: Some(Arc::new(tasks)),
            ..Default::default()
        });
        flock.wire_observability(dir.path(), None).unwrap();
        flock.record_launch(
            P,
            &[
                ("agent", json!("claude")),
                ("argv", json!(["claude", "old"])),
                ("cwd", json!("/tmp")),
            ],
        );
        Self {
            flock,
            terminal: Mutex::new(Box::new(backend)),
            pool,
            store,
            runtime,
            env,
            children,
            dir,
        }
    }
    fn run(&self, confirm: bool, pid: Value, start: Value) -> Result<Value, DunkError> {
        let services = RespawnServices {
            terminal: &self.terminal,
            delivery: self,
            inbox: self,
            runtime: self.runtime.handle(),
            env: &self.env,
            verify_seconds: 0.01,
            before_native_recheck: None,
        };
        self.flock.respawn_pane(
            &services,
            RespawnRequest {
                pane_id: &json!(P),
                argv: &["claude".into(), "new".into()],
                rollback_argv: None,
                expected_pid: Some(&pid),
                expected_start: Some(&start),
                confirm_idle: confirm,
                reason: None,
                self_pane_id: Some(OWNER),
            },
        )
    }
    fn original(&self) -> (Value, Value) {
        let pid = i64::from(self.children.0[0].id());
        (json!(pid), json!(crate::proc::process_start(pid).unwrap()))
    }
    fn calls(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("calls")).unwrap_or_default()
    }
}
impl PaneInfoEnv for Fixture {
    fn list_panes_grouped(&self) -> Vec<Value> {
        self.terminal
            .lock()
            .unwrap()
            .list_panes()
            .iter()
            .map(|p| super::super::lanes::pane_to_json(p, None))
            .collect()
    }
    fn get_pane(&self, id: &str) -> Option<Value> {
        self.terminal
            .lock()
            .unwrap()
            .get_pane(id)
            .map(|p| super::super::lanes::pane_to_json(&p, None))
    }
}
impl SendEnv for Fixture {
    fn read_pane(&self, id: &str, lines: i64, ansi: bool) -> Option<String> {
        self.terminal
            .lock()
            .unwrap()
            .read_pane(id, lines, ansi)
            .ok()
            .flatten()
    }
    fn pane_harness(&self, id: &str) -> Option<String> {
        PaneInfoEnv::get_pane(self, id).and_then(|p| p["agent"].as_str().map(str::to_owned))
    }
    fn session_process_gone(&self, id: &str) -> bool {
        self.terminal
            .lock()
            .unwrap()
            .get_pane(id)
            .and_then(|p| p.pane_pid)
            .is_some_and(|pid| !crate::proc::process_alive(pid, None))
    }
    fn read_status(&self, _: &str) -> Option<String> {
        Some("idle".into())
    }
    fn send_key(&mut self, _: &str, _: &str) -> (bool, String) {
        (false, "respawn fixture never sends keys".into())
    }
    fn federated_input(&self) -> bool {
        false
    }
}
impl ClearBoxEnv for Fixture {
    fn server_error_hint(&self) -> String {
        "fixture unavailable".into()
    }
}
impl MailboxEnv for Fixture {
    fn list_panes(&self) -> Vec<Value> {
        PaneInfoEnv::list_panes_grouped(self)
    }
    fn list_panes_grouped(&self) -> Vec<Value> {
        PaneInfoEnv::list_panes_grouped(self)
    }
    fn registered_pane(&self, _: &str) -> (bool, bool, Option<String>) {
        (false, false, None)
    }
}
impl InboxEnv for Fixture {
    fn caller_role(&self, id: &str) -> Result<String, DunkError> {
        self.flock.caller_role(Some(id), self)
    }
    fn mailbox_override(&self) -> Option<String> {
        None
    }
    fn backend_name(&self) -> Result<String, DunkError> {
        Ok("tmux".into())
    }
    fn lane_panes(&self) -> Vec<String> {
        vec![P.into()]
    }
    fn pool(&self) -> &ReadPool {
        &self.pool
    }
    fn store(&self) -> &Store {
        &self.store
    }
    fn now(&self) -> f64 {
        crate::clock::now_s()
    }
    fn record_metric(
        &self,
        _: &str,
        _: Option<&str>,
        _: &str,
        _: Option<&str>,
        _: Option<i64>,
        _: Option<i64>,
    ) {
    }
    fn pm_routing(&self) -> Vec<RouteEntry> {
        vec![]
    }
    fn fold_into_queued_notice(&self, _: &str, _: &str, _: &str) -> Option<String> {
        None
    }
    fn send_now(&self, _: &str, _: &str, _: &str) -> Result<Nudge, DunkError> {
        Err(refusal("fixture never sends"))
    }
    fn record_sent(&self, _: Option<&str>, _: &str, _: Option<&str>, _: usize, _: usize) {}
    fn tasks_for_pane(&self, id: &str) -> Result<Value, DunkError> {
        let tasks = self.flock.stores().tasks.unwrap();
        Ok(json!(self
            .runtime
            .block_on(tasks.leases_for_pane(id, None))?
            .iter()
            .map(|l| l.task_id.clone())
            .collect::<Vec<_>>()))
    }
}
#[test]
fn requires_positive_idle_confirmation() {
    let f = Fixture::new();
    let (p, s) = f.original();
    assert!(f
        .run(false, p, s)
        .unwrap_err()
        .to_string()
        .contains("positive evidence"));
    assert!(!f.calls().contains("respawn-pane"));
}
#[test]
fn wrong_pid_and_start_refuse_without_mutation() {
    let f = Fixture::new();
    let (p, s) = f.original();
    assert!(f.run(true, json!(1), s).is_err());
    assert!(f.run(true, p, json!("wrong")).is_err());
    assert!(!f.calls().contains("respawn-pane"));
}
#[test]
fn verified_launch_records_previous_and_real_replacement() {
    let f = Fixture::new();
    let (p, s) = f.original();
    let result = f.run(true, p.clone(), s).unwrap();
    assert_eq!(result["old_pid"], p);
    assert_eq!(result["pane_pid"], f.children.0[1].id());
    let launch = f.flock.launch(P).unwrap();
    assert_eq!(launch["previous"]["argv"], json!(["claude", "old"]));
    assert_eq!(launch["argv"], json!(["claude", "new"]));
    let events = f
        .flock
        .stores()
        .metrics
        .unwrap()
        .events_between(0.0, f64::MAX)
        .unwrap();
    assert!(events
        .iter()
        .any(|e| e.kind == "pane.respawn" && e.outcome.as_deref() == Some("ok")));
}
#[test]
fn failed_start_rolls_back_to_rollback_argv() {
    let f = Fixture::new();
    std::fs::write(f.dir.path().join("bad_start"), "").unwrap();
    let (p, s) = f.original();
    let error = f.run(true, p, s).unwrap_err().to_string();
    assert!(error.ends_with(" (rolled back)"), "{error}");
    assert!(f.calls().contains("claude old"));
    assert_eq!(
        std::fs::read_to_string(f.dir.path().join("count"))
            .unwrap()
            .trim(),
        "2"
    );
    assert_eq!(f.flock.launch(P).unwrap()["argv"], json!(["claude", "old"]));
}
#[test]
fn rollback_failure_is_not_reported_as_verified() {
    let f = Fixture::new();
    for marker in ["bad_start", "bad_rollback"] {
        std::fs::write(f.dir.path().join(marker), "").unwrap();
    }
    let (p, s) = f.original();
    let error = f.run(true, p, s).unwrap_err().to_string();
    assert!(error.ends_with(" (rollback not verified)"), "{error}");
}
#[test]
fn unreadable_and_foreign_composer_refuse_without_mutation() {
    for screen in [
        "› old transcript\n› live draft\n\n\n  gpt · ~/repo\n",
        "│ ❯ human work\n────────\n",
    ] {
        let f = Fixture::new();
        std::fs::write(f.dir.path().join("screen"), screen).unwrap();
        let (p, s) = f.original();
        assert!(f.run(true, p, s).is_err());
        assert!(!f.calls().contains("respawn-pane"));
    }
    let f = Fixture::new();
    std::fs::remove_file(f.dir.path().join("screen")).unwrap();
    let (p, s) = f.original();
    let error = f.run(true, p, s).unwrap_err().to_string();
    assert!(error.contains("pane unreadable"), "{error}");
    assert!(!f.calls().contains("respawn-pane -k"));
}

#[test]
fn active_and_falsey_ended_turns_refuse_without_mutation() {
    for ended_at in [None, Some(0.0)] {
        let f = Fixture::new();
        f.flock.turn_states.lock().unwrap().insert(
            P.into(),
            crate::flock::commands_turns::TurnState {
                started_at: 1.0,
                expires_at: 2.0,
                ended_at,
                ttl_s: 1.0,
                turn_id: "turn".into(),
                source: None,
                caller: None,
                handoff_id: None,
            },
        );
        let (p, s) = f.original();
        assert!(f
            .run(true, p, s)
            .unwrap_err()
            .to_string()
            .contains("active turn"));
        assert!(!f.calls().contains("respawn-pane"));
    }
}
#[test]
fn queued_outbox_and_live_task_lease_refuse_without_mutation() {
    let f = Fixture::new();
    f.flock.outbox.lock().unwrap().insert(
        P.into(),
        vec![crate::flock::sends::Message::new(
            "m1", P, "worker", "text", "test",
        )],
    );
    let (p, s) = f.original();
    assert!(f
        .run(true, p, s)
        .unwrap_err()
        .to_string()
        .contains("queued outbox"));
    assert!(!f.calls().contains("respawn-pane"));
    f.flock.outbox.lock().unwrap().clear();
    let db = crate::db::open_writer(&f.dir.path().join("tasks.db")).unwrap();
    db.execute("INSERT INTO task_leases(task_id,pane_id,claimed_at,heartbeat_at,expires_at) VALUES('t1',?,0,0,?)",rusqlite::params![P,crate::clock::now_s()+300.0]).unwrap();
    drop(db);
    let (p, s) = f.original();
    assert!(f
        .run(true, p, s)
        .unwrap_err()
        .to_string()
        .contains("live task lease (t1)"));
    assert!(!f.calls().contains("respawn-pane"));
}
#[test]
fn unread_mail_and_count_errors_refuse_without_mutation() {
    let f = Fixture::new();
    let mailbox = f
        .flock
        .mailbox_or_self(&f, None, Some(P), false, false)
        .unwrap();
    f.store
        .send(&crate::inbox_store::Outgoing {
            mailbox: &mailbox,
            body: Some("unread"),
            subject: None,
            sender: None,
            mailbox_label: None,
            pane_id: Some(P),
            attachments: &[],
            transport: None,
            reply_to: None,
            external_id: None,
        })
        .unwrap();
    let (p, s) = f.original();
    assert!(f
        .run(true, p, s)
        .unwrap_err()
        .to_string()
        .contains("unread message"));
    assert!(!f.calls().contains("respawn-pane"));
    let db = crate::db::open_writer(&f.dir.path().join("inbox.db")).unwrap();
    db.execute_batch("DROP TABLE messages").unwrap();
    drop(db);
    let (p, s) = f.original();
    assert!(f
        .run(true, p, s)
        .unwrap_err()
        .to_string()
        .contains("unread count failed"));
    assert!(!f.calls().contains("respawn-pane"));
}
#[test]
fn unowned_pane_refuses_after_backend_reconstruction() {
    let f = Fixture::new();
    std::fs::write(f.dir.path().join("registry.json"), "{}").unwrap();
    let (p, s) = f.original();
    assert!(f
        .run(true, p, s)
        .unwrap_err()
        .to_string()
        .contains("did not create it"));
    assert!(!f.calls().contains("respawn-pane -k"));
}

#[test]
fn final_identity_replacement_refuses_before_kill() {
    let f = Fixture::new();
    std::fs::write(f.dir.path().join("race"), "").unwrap();
    let pid = i64::from(f.children.0[0].id());
    let start = crate::proc::process_start(pid).unwrap();
    let error = f
        .terminal
        .lock()
        .unwrap()
        .guarded_respawn(P, &["claude".into()], pid, &start)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("replaced between check and mutation"),
        "{error}"
    );
    assert!(!f.calls().contains("respawn-pane -k"));
}
#[test]
fn poll_timeout_never_rolls_back_same_pid() {
    let f = Fixture::new();
    std::fs::write(f.dir.path().join("same_pid"), "").unwrap();
    let (p, s) = f.original();
    let error = f.run(true, p, s).unwrap_err().to_string();
    assert!(error.contains("did not show a new process"), "{error}");
    assert!(!error.contains("rollback"));
    assert_eq!(
        std::fs::read_to_string(f.dir.path().join("count"))
            .unwrap()
            .trim(),
        "1"
    );
}
#[test]
fn unsupported_backend_is_explicit() {
    let f = Fixture::new();
    let pane = f.terminal.lock().unwrap().get_pane(P).unwrap();
    let mut fake = crate::terminal::fakes::FakeTerminal::new();
    fake.panes = vec![pane];
    *f.terminal.lock().unwrap() = Box::new(fake);
    let (p, s) = f.original();
    assert!(f
        .run(true, p, s)
        .unwrap_err()
        .to_string()
        .contains("does not support guarded respawn"));
}
#[test]
fn argv_quoting_preserves_literal_shell_syntax_and_exit_wrapper() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("not-created");
    let literal = format!("$(touch {}) ' ; echo nope", marker.display());
    let command = crate::terminal::tmux::shell_join(&["printf", "%s", &literal]);
    let wrapper = crate::terminal::tmux::agent_launch(&command);
    let output = Command::new("sh")
        .arg("-c")
        .arg(wrapper)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.starts_with(&literal), "{text}");
    assert!(text.contains("[agent exited rc=0]"));
    assert!(!marker.exists());
}

fn codex_request(
    f: &Fixture,
    argv: &[String],
    rollback: Option<&[String]>,
) -> Result<Value, DunkError> {
    let (p, s) = f.original();
    f.flock.respawn_pane(
        &RespawnServices {
            terminal: &f.terminal,
            delivery: f,
            inbox: f,
            runtime: f.runtime.handle(),
            env: &f.env,
            verify_seconds: 0.01,
            before_native_recheck: None,
        },
        RespawnRequest {
            pane_id: &json!(P),
            argv,
            rollback_argv: rollback,
            expected_pid: Some(&p),
            expected_start: Some(&s),
            confirm_idle: true,
            reason: None,
            self_pane_id: Some(OWNER),
        },
    )
}
fn native_fixture(f: &Fixture, active: bool) -> (rusqlite::Connection, Vec<String>) {
    let home = f.dir.path().join("private-codex");
    std::fs::create_dir(&home).unwrap();
    let session = "01a173ee-beed-7461-a560-e5f6c1f666a7";
    let rollout = home.join(format!("rollout-{session}.jsonl"));
    let mut events = vec![
        json!({"timestamp":"2026-10-29T06:06:30Z","type":"session_meta","payload":{"id":session,"session_id":session,"cwd":"/tmp"}}),
        json!({"timestamp":"2026-10-29T06:06:31Z","type":"event_msg","payload":{"type":"task_started","turn_id":"turn","thread_id":session}}),
    ];
    if !active {
        events.push(json!({"timestamp":"2026-10-29T06:06:32Z","type":"event_msg","payload":{"type":"task_complete","turn_id":"turn","thread_id":session}}));
    }
    std::fs::write(
        &rollout,
        events
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n"),
    )
    .unwrap();
    let db = rusqlite::Connection::open(home.join("state_5.sqlite")).unwrap();
    db.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,rollout_path TEXT,cwd TEXT)")
        .unwrap();
    db.execute(
        "INSERT INTO threads VALUES(?,?,?)",
        rusqlite::params![session, rollout.to_str().unwrap(), "/tmp"],
    )
    .unwrap();
    (
        db,
        vec![
            "env".into(),
            format!("CODEX_HOME={}", home.display()),
            "codex".into(),
            "--no-daemon".into(),
            "-C".into(),
            "/tmp".into(),
            "resume".into(),
            session.into(),
        ],
    )
}
#[test]
fn native_active_and_busy_refuse_without_backend_mutation() {
    let f = Fixture::new();
    let (_db, argv) = native_fixture(&f, true);
    let error = codex_request(&f, &argv, None).unwrap_err().to_string();
    assert!(error.contains("unfinished turn(s)"), "{error}");
    assert!(!f.calls().contains("respawn-pane -k"));
    let f = Fixture::new();
    let (db, argv) = native_fixture(&f, false);
    db.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let error = codex_request(&f, &argv, None).unwrap_err();
    assert!(matches!(error, DunkError::Retryable(_)), "{error}");
    assert!(!f.calls().contains("respawn-pane -k"));
}
#[test]
fn shared_codex_home_in_requested_and_rollback_argv_is_refused() {
    for rollback in [false, true] {
        let f = Fixture::new();
        let path = f.dir.path().join("tmux");
        let script = std::fs::read_to_string(&path)
            .unwrap()
            .replace("\\037claude\\037", "\\037codex\\037");
        std::fs::write(&path, script).unwrap();
        let shared = vec![
            "env".into(),
            format!("CODEX_HOME={}/.codex", f.dir.path().display()),
            "codex".into(),
        ];
        let normal = vec!["codex".into()];
        let result = if rollback {
            codex_request(&f, &normal, Some(&shared))
        } else {
            codex_request(&f, &shared, None)
        };
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("isolated per-lane CODEX_HOME"));
        assert!(!f.calls().contains("respawn-pane -k"));
    }
}

#[test]
fn native_rollout_changed_at_final_recheck_refuses_mutation() {
    let f = Fixture::new();
    let (_db, argv) = native_fixture(&f, false);
    let rollout = f
        .dir
        .path()
        .join("private-codex/rollout-01a173ee-beed-7461-a560-e5f6c1f666a7.jsonl");
    let change = || {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .unwrap()
            .write_all(b"\n{}\n")
            .unwrap();
    };
    let (p, s) = f.original();
    let services = RespawnServices {
        terminal: &f.terminal,
        delivery: &f,
        inbox: &f,
        runtime: f.runtime.handle(),
        env: &f.env,
        verify_seconds: 0.01,
        before_native_recheck: Some(&change),
    };
    let error = f
        .flock
        .respawn_pane(
            &services,
            RespawnRequest {
                pane_id: &json!(P),
                argv: &argv,
                rollback_argv: None,
                expected_pid: Some(&p),
                expected_start: Some(&s),
                confirm_idle: true,
                reason: None,
                self_pane_id: Some(OWNER),
            },
        )
        .unwrap_err();
    assert!(error.to_string().contains("changed"), "{error}");
    assert!(!f.calls().contains("respawn-pane -k"));
}
