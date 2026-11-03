use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use super::*;
use crate::flock::{panes::Stores, Flock};
use crate::metrics::MetricsStore;
use crate::terminal::{
    fakes::FakeTerminal, AgentSpec, Pane, Resize, Tab, Terminal, TerminalError, Workspace,
};

#[derive(Debug)]
struct RecordingTerminal {
    inner: FakeTerminal,
    batches: Arc<Mutex<Vec<(String, Vec<String>)>>>,
}
impl Terminal for RecordingTerminal {
    fn version(&self) -> String {
        self.inner.version()
    }
    fn is_available(&self) -> bool {
        self.inner.is_available()
    }
    fn server_error_hint(&self) -> String {
        self.inner.server_error_hint()
    }
    fn list_panes(&self) -> Vec<Pane> {
        self.inner.list_panes()
    }
    fn get_pane(&self, id: &str) -> Option<Pane> {
        self.inner.get_pane(id)
    }
    fn list_tabs(&self) -> Vec<Tab> {
        self.inner.list_tabs()
    }
    fn list_workspaces(&self) -> Vec<Workspace> {
        self.inner.list_workspaces()
    }
    fn list_panes_grouped(&self) -> BTreeMap<String, Vec<Pane>> {
        self.inner.list_panes_grouped()
    }
    fn agent_child_pids(&self, pid: i64) -> Result<Vec<i64>, TerminalError> {
        self.inner.agent_child_pids(pid)
    }
    fn create_workspace(
        &mut self,
        label: Option<&str>,
        cwd: Option<&str>,
        focus: bool,
    ) -> Result<Workspace, TerminalError> {
        self.inner.create_workspace(label, cwd, focus)
    }
    fn create_tab(
        &mut self,
        workspace: Option<&str>,
        label: Option<&str>,
        cwd: Option<&str>,
        focus: bool,
    ) -> Result<Tab, TerminalError> {
        self.inner.create_tab(workspace, label, cwd, focus)
    }
    fn split_pane(
        &mut self,
        id: &str,
        direction: &str,
        cwd: Option<&str>,
    ) -> Result<Pane, TerminalError> {
        self.inner.split_pane(id, direction, cwd)
    }
    fn start_agent(&mut self, spec: &AgentSpec) -> Result<Pane, TerminalError> {
        self.inner.start_agent(spec)
    }
    fn rename(&mut self, kind: &str, id: &str, label: &str) -> Result<(), TerminalError> {
        self.inner.rename(kind, id, label)
    }
    fn close(&mut self, kind: &str, id: &str) -> Result<(), TerminalError> {
        self.inner.close(kind, id)
    }
    fn focus(&mut self, kind: &str, id: &str) -> Result<(), TerminalError> {
        self.inner.focus(kind, id)
    }
    fn run_command(&mut self, id: &str, command: &str) -> Result<(), TerminalError> {
        self.inner.run_command(id, command)
    }
    fn send_text(&mut self, id: &str, text: &str) -> Result<(), TerminalError> {
        self.inner.send_text(id, text)
    }
    fn send_text_and_enter(&mut self, id: &str, text: &str) -> Result<(), TerminalError> {
        self.inner.send_text_and_enter(id, text)
    }
    fn send_keys(&mut self, id: &str, keys: &[&str]) -> Result<(), TerminalError> {
        self.inner.send_keys(id, keys)?;
        self.batches
            .lock()
            .unwrap()
            .push((id.into(), keys.iter().map(|s| (*s).into()).collect()));
        Ok(())
    }
    fn read_pane(&self, id: &str, lines: i64, ansi: bool) -> Result<Option<String>, TerminalError> {
        self.inner.read_pane(id, lines, ansi)
    }
    fn agent_status(&self, id: &str) -> Option<String> {
        self.inner.agent_status(id)
    }
    fn resize(&mut self, id: &str, cols: i64, rows: i64) -> Result<Resize, TerminalError> {
        self.inner.resize(id, cols, rows)
    }
}

struct Fixture {
    flock: Flock,
    terminal: Mutex<Box<dyn Terminal>>,
    batches: Arc<Mutex<Vec<(String, Vec<String>)>>>,
    metrics: Arc<MetricsStore>,
    runtime: tokio::runtime::Runtime,
    _dir: tempfile::TempDir,
}
impl Fixture {
    fn new(fake: FakeTerminal) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let flock = Flock::new(dir.path().join("launches"), dir.path().join("lanes"), false);
        let metrics = Arc::new(MetricsStore::open(dir.path().join("metrics.db")).unwrap());
        flock.attach_stores(Stores {
            metrics: Some(Arc::clone(&metrics)),
            ..Stores::default()
        });
        let batches = Arc::new(Mutex::new(Vec::new()));
        Self {
            flock,
            terminal: Mutex::new(Box::new(RecordingTerminal {
                inner: fake,
                batches: Arc::clone(&batches),
            })),
            batches,
            metrics,
            runtime: tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap(),
            _dir: dir,
        }
    }
    fn adapter(&self) -> TerminalCallAdapter<'_> {
        TerminalCallAdapter {
            flock: &self.flock,
            terminal: &self.terminal,
            runtime: self.runtime.handle(),
        }
    }
}

#[test]
fn production_env_keypress_uses_concrete_services() {
    let f = Fixture::new(FakeTerminal::new());
    let mut env = f.adapter();
    assert_eq!(
        KeyPressEnv::send_keys(&mut env, "w1:p1", &["C-x".into(), "Escape".into()]),
        (true, "sent".into())
    );
    assert_eq!(
        *f.batches.lock().unwrap(),
        vec![("w1:p1".into(), vec!["C-x".into(), "Escape".into()])]
    );
    assert_eq!(
        KeyPressEnv::confirm_pending(&mut env, "w1:p1", "雪🐑"),
        (true, "sent".into())
    );
    assert_eq!(f.batches.lock().unwrap().last().unwrap().1, vec!["Enter"]);
    let events = f
        .metrics
        .events_between(0.0, crate::clock::now_s() + 1.0)
        .unwrap();
    assert_eq!(events.last().unwrap().bytes, Some("雪🐑".len() as i64));
    assert!(f.terminal.try_lock().is_ok());
    *f.terminal.lock().unwrap() = Box::new(FakeTerminal::unavailable());
    let (ok, detail) = KeyPressEnv::send_keys(&mut env, "w1:p1", &["Enter".into()]);
    assert!(!ok);
    assert!(detail.contains("tmux server not running"));
}

#[test]
fn production_env_keypress_routes_prefix_and_bare_enter_as_distinct_batches() {
    let fake = FakeTerminal::new();
    let reads = Arc::clone(&fake.reads);
    let f = Fixture::new(fake);
    let mut env = f.adapter();
    let keys = vec!["C-x".into(), "Return".into()];
    assert_eq!(
        crate::flock::send_keys::press_keys_unlocked(&mut env, "w1:p1", &keys),
        (true, "sent".into())
    );
    assert_eq!(
        *f.batches.lock().unwrap(),
        vec![
            ("w1:p1".into(), vec!["C-x".into()]),
            ("w1:p1".into(), vec!["Return".into()])
        ]
    );
    assert_eq!(
        *reads.lock().unwrap(),
        vec![("w1:p1".into(), sends::CONSUMPTION_READ_LINES, true)]
    );
}

#[test]
fn production_env_keypress_prefix_refusal_skips_the_composer_read() {
    let fake = FakeTerminal::unavailable();
    let reads = Arc::clone(&fake.reads);
    let f = Fixture::new(fake);
    let mut env = f.adapter();
    let result = crate::flock::send_keys::press_keys_unlocked(
        &mut env,
        "w1:p1",
        &["C-x".into(), "Enter".into()],
    );
    assert!(!result.0);
    assert!(result.1.contains("tmux server not running"));
    assert!(reads.lock().unwrap().is_empty());
    assert!(f.batches.lock().unwrap().is_empty());
}

#[test]
fn production_env_keypress_pending_requires_real_composer_ownership() {
    let mut fake = FakeTerminal::new();
    fake.screens.insert(
        "w1:p1".into(),
        "› owned draft\n\n\n  model · /tmp/work\n".into(),
    );
    let f = Fixture::new(fake);
    let mut env = f.adapter();
    assert_eq!(KeyPressEnv::pending_text(&mut env, "w1:p1"), None);
    f.flock.remember_typed("w1:p1", "owned draft");
    assert_eq!(
        KeyPressEnv::pending_text(&mut env, "w1:p1"),
        Some("owned draft".into())
    );
    let mut unreadable = FakeTerminal::new();
    unreadable.unreadable.insert("w1:p1".into());
    *f.terminal.lock().unwrap() = Box::new(unreadable);
    assert_eq!(KeyPressEnv::pending_text(&mut env, "w1:p1"), None);
}

#[cfg(target_os = "linux")]
#[test]
fn production_env_keypress_pending_uses_authenticated_native_session() {
    use crate::flock::sends::Message;
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let hooks = dir.path().join("hooks");
    let cwd = dir.path().join("cwd");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&hooks).unwrap();
    std::fs::create_dir_all(&cwd).unwrap();
    let binary = dir.path().join("codex");
    std::fs::copy("/bin/sleep", &binary).unwrap();
    let child = std::process::Command::new(binary)
        .arg("300")
        .env("CODEX_HOME", &home)
        .env("DUNKINGSHEEP_CODEX_STATE_DIR", &hooks)
        .spawn()
        .unwrap();
    let child = ChildGuard(child);
    let pid = i64::from(child.0.id());
    let start = crate::proc::process_start(pid).unwrap();
    let session = "01a173ee-beed-7461-a560-e5f6c1f666a7";
    let rollout = home.join(format!("rollout-{session}.jsonl"));
    let meta =
        serde_json::json!({"type":"session_meta", "payload":{"session_id":session,"cwd":cwd}});
    let prior = serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":"previous"}});
    std::fs::write(&rollout, format!("{meta}\n{prior}\n")).unwrap();
    let db = rusqlite::Connection::open(home.join("state_5.sqlite")).unwrap();
    db.execute(
        "create table threads (id text primary key, rollout_path text, cwd text)",
        [],
    )
    .unwrap();
    db.execute(
        "insert into threads values (?,?,?)",
        (session, rollout.to_str().unwrap(), cwd.to_str().unwrap()),
    )
    .unwrap();
    std::fs::write(hooks.join("bound.json"), serde_json::json!({"session_id":session,"pane_id":"w1:p1","root_pid":pid,"root_start":start,"cwd":cwd}).to_string()).unwrap();
    let mut fake = FakeTerminal::new();
    fake.panes[0].command = "codex".into();
    fake.panes[0].pane_pid = Some(pid);
    fake.panes[0].cwd = cwd.to_str().unwrap().into();
    fake.statuses.insert("w1:p1".into(), "idle".into());
    fake.screens.insert(
        "w1:p1".into(),
        "› previous\n› pending 🐑\n\n\n  model · /tmp/test\n".into(),
    );
    let f = Fixture::new(fake);
    let mut message = Message::new("m1", "w1:p1", "writer", "pending 🐑", "send_text");
    message.status = "failed".into();
    message.created_at = 1.0;
    message.typed_at = Some(1.0);
    message.attempt_generation = Some(format!("pid:{pid}:start:{start}"));
    f.flock.note_stuck_candidate(&message);
    let mut env = f.adapter();
    assert_eq!(
        KeyPressEnv::pending_text(&mut env, "w1:p1"),
        Some("pending 🐑  ".into())
    );
    let unrelated = serde_json::json!({"type":"event_msg","payload":{"type":"user_message","message":"other text"}});
    std::fs::write(&rollout, format!("{meta}\n{unrelated}\n")).unwrap();
    assert_eq!(KeyPressEnv::pending_text(&mut env, "w1:p1"), None);
    assert!(f.batches.lock().unwrap().is_empty());
}
