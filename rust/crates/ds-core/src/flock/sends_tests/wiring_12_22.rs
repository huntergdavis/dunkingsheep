//! t3355 wiring rows 12 and 22 (t820 proof D), the rows this branch owns. Each test names its row
//! and the oracle test at commit 79bc92fa (tests/test_dunk_recovery_wiring.py). The observer runs
//! through the native evidence chain: a live process named codex, its rollout, state_5.sqlite and
//! bound.json. The pane screen is the t508 incident frame. Its text and prefix fixtures are
//! byte-identical to the oracle's derivation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::Connection;
use serde_json::{json, Value};

use crate::flock::panes::{PaneInfoEnv, Stores};
use crate::flock::sends::{pane_generation, Message, SendEnv};
use crate::flock::Flock;
use crate::inbox_store::{Outgoing, Store};

const T508_FRAME: &str = include_str!("../testdata/t508-cos-ambiguous.txt");
const T508_TEXT: &str = include_str!("../testdata/t508-text.txt");
const T508_PREFIX: &str = include_str!("../testdata/t508-prefix.txt");
const SESSION: &str = "01a173ee-beed-7461-a560-e5f6c1f666a7";
const BASE: f64 = 1000.0;
/// The composer the renderer draws after a submitting Enter, as the oracle's fake draws it.
const EMPTY_COMPOSER: &str = "\u{203a} \n\n  GPT-6-Astra high \u{b7} ~/workspace \u{b7} task\n";

/// Pane screens, statuses and the keys sent to them. The renderer clears the composer on the Enter
/// that submits, as the oracle's fake does. Nothing else is faked.
#[derive(Default)]
struct WiringEnv {
    screens: BTreeMap<String, String>,
    statuses: BTreeMap<String, String>,
    panes: Vec<Value>,
    keys: Vec<(String, String)>,
    /// Enter presses that submit; zero means the first one does.
    enters_needed: usize,
    enters: usize,
}

impl SendEnv for WiringEnv {
    fn read_pane(&self, pane_id: &str, _lines: i64, _ansi: bool) -> Option<String> {
        self.screens.get(pane_id).cloned()
    }
    fn pane_harness(&self, _pane_id: &str) -> Option<String> {
        Some("codex".into())
    }
    fn session_process_gone(&self, _pane_id: &str) -> bool {
        false
    }
    fn read_status(&self, pane_id: &str) -> Option<String> {
        Some(
            self.statuses
                .get(pane_id)
                .cloned()
                .unwrap_or_else(|| "idle".into()),
        )
    }
    fn send_key(&mut self, pane_id: &str, key: &str) -> (bool, String) {
        self.keys.push((pane_id.into(), key.into()));
        if key == "Enter" {
            self.enters += 1;
            if self.enters >= self.enters_needed.max(1) {
                self.screens.insert(pane_id.into(), EMPTY_COMPOSER.into());
            }
        }
        (true, "sent".into())
    }
    fn federated_input(&self) -> bool {
        false
    }
}

impl PaneInfoEnv for WiringEnv {
    fn list_panes_grouped(&self) -> Vec<Value> {
        self.panes.clone()
    }
    fn get_pane(&self, pane_id: &str) -> Option<Value> {
        self.panes
            .iter()
            .find(|pane| pane["pane_id"] == pane_id)
            .cloned()
    }
}

/// A child process named codex, killed when the guard drops.
struct NativeChild(std::process::Child);

impl Drop for NativeChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The native files for pane %1 and the process they are bound to.
struct NativeFixture {
    _child: NativeChild,
    pid: i64,
    cwd: PathBuf,
}

fn spawn_codex(binary: &Path, home: &Path, hooks: &Path) -> std::io::Result<std::process::Child> {
    std::process::Command::new(binary)
        .arg("-c")
        .arg("read line")
        .stdin(std::process::Stdio::piped())
        .env("CODEX_HOME", home)
        .env("DUNKINGSHEEP_CODEX_STATE_DIR", hooks)
        .spawn()
}

fn native_fixture(dir: &Path) -> NativeFixture {
    let home = dir.join("home");
    let hooks = dir.join("hooks");
    let cwd = dir.join("cwd");
    for path in [&home, &hooks, &cwd] {
        std::fs::create_dir_all(path).unwrap();
    }
    // Not /bin/sleep: that is a coreutils multi-call binary and exits at once when its file is
    // named codex. The shell stays alive by blocking in `read` on a stdin pipe the guard holds.
    let binary = dir.join("codex");
    std::fs::copy("/bin/sh", &binary).unwrap();
    let mut child = NativeChild(
        crate::testsupport::exec_retrying_busy(|| spawn_codex(&binary, &home, &hooks))
            .expect("spawn the codex fixture"),
    );
    assert!(
        child.0.try_wait().unwrap().is_none(),
        "the codex fixture process exited at once"
    );
    let pid = i64::from(child.0.id());
    let start = crate::proc::process_start(pid).unwrap();
    let rollout = home.join(format!("rollout-{SESSION}.jsonl"));
    std::fs::write(&rollout, rollout_text(&cwd)).unwrap();
    let db = Connection::open(home.join("state_5.sqlite")).unwrap();
    db.execute(
        "create table threads (id text primary key, rollout_path text, cwd text)",
        [],
    )
    .unwrap();
    db.execute(
        "insert into threads values (?,?,?)",
        (SESSION, rollout.to_str().unwrap(), cwd.to_str().unwrap()),
    )
    .unwrap();
    std::fs::write(
        hooks.join("bound.json"),
        json!({"session_id": SESSION, "pane_id": "%1", "root_pid": pid, "root_start": start, "cwd": cwd})
            .to_string(),
    )
    .unwrap();
    NativeFixture {
        _child: child,
        pid,
        cwd,
    }
}

/// The rollout the native reader sees: the authenticated session meta, then the assistant prefix.
fn rollout_text(cwd: &Path) -> String {
    let meta = json!({"type": "session_meta", "payload": {"session_id": SESSION, "cwd": cwd}});
    let answer = json!({"type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": T508_PREFIX}]}});
    format!("{meta}\n{answer}\n")
}

fn pane_row(native: &NativeFixture) -> Value {
    json!({"pane_id": "%1", "pane_pid": native.pid, "agent": "codex", "cwd": native.cwd})
}

/// The pane at the threshold: idle, showing the incident frame, with the native codex process.
fn incident_env(native: &NativeFixture) -> WiringEnv {
    let mut env = WiringEnv::default();
    env.panes.push(pane_row(native));
    env.statuses.insert("%1".into(), "idle".into());
    env.screens.insert("%1".into(), T508_FRAME.into());
    env
}

/// The oracle's wiring config: threshold 120 s, observation 20 s, two attempts, cooldown 60 s.
/// The flock is persisted, as the oracle's make() is, so a durable reservation can be written.
fn persisted_flock(dir: &Path) -> Flock {
    let flock = Flock::new(dir.join("launches.json"), dir.join("lanes.json"), true);
    flock
        .attach_status_state(crate::flock::status_runtime::StatusState::at_start(
            "test",
            dir.join("state.json"),
            std::env::current_exe().unwrap(),
            crate::config::Env::default(),
        ))
        .unwrap();
    let mut config = flock.stuck_config.lock().unwrap();
    config.threshold_s = 120.0;
    config.observation_interval_s = 20.0;
    config.max_attempts = 2;
    drop(config);
    flock
}

fn scan(flock: &Flock, env: &mut WiringEnv, now: f64) -> usize {
    crate::flock::loops_stuck::strict_stuck_observer_scan(flock, env, now).unwrap()
}

/// The failed send of the incident text, as the send path leaves it, under the pane's generation.
fn failed_incident(env: &WiringEnv) -> Message {
    let mut message = Message::new("m1", "%1", "CoS", T508_TEXT, "send_text");
    message.created_at = BASE;
    message.status = "failed".into();
    message.typed_at = Some(BASE);
    message.attempt_generation = pane_generation(env, "%1");
    message
}

fn enter() -> (String, String) {
    ("%1".to_string(), "Enter".to_string())
}

// t3355 wiring row 12, test_dunk_recovery_wiring.py::test_real_628_char_incident_reaches_guarded_
// executor_once (oracle 79bc92fa). A failed non-scheduled send whose composer holds the 628-char
// incident text. The observer sees it once, waits for the threshold, sends exactly one Enter, marks
// the attempt consumed, and sends nothing later.
#[test]
fn a_failed_t508_incident_reaches_the_guarded_executor_once() {
    let dir = tempfile::tempdir().unwrap();
    let native = native_fixture(dir.path());
    let flock = persisted_flock(dir.path());
    let mut env = incident_env(&native);
    flock.note_stuck_candidate(&failed_incident(&env));

    scan(&flock, &mut env, BASE + 1.0);
    assert!(env.keys.is_empty(), "no key on the first sighting");
    scan(&flock, &mut env, BASE + 101.0);
    assert!(env.keys.is_empty(), "no key before the threshold");
    scan(&flock, &mut env, BASE + 121.0);
    assert_eq!(
        vec![enter()],
        env.keys,
        "exactly one guarded Enter at the threshold"
    );
    let entry = flock.stuck_candidates.lock().unwrap()["%1"].clone();
    assert_eq!(Some("consumed"), entry.recovery_outcome.as_deref());
    assert!(
        !entry.eligible,
        "the consumed attempt is not eligible again"
    );
    scan(&flock, &mut env, BASE + 301.0);
    assert_eq!(
        1,
        env.keys.len(),
        "no second key after the consumed attempt"
    );
}

/// The notification state the inbox holds for one message.
fn notify_state(inbox_path: &Path, id: &str) -> Option<String> {
    Connection::open(inbox_path)
        .unwrap()
        .query_row(
            "SELECT notify_state FROM messages WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap()
}

// t3355 wiring row 22, test_dunk_recovery_wiring.py::test_failed_notification_reconciles_after_
// restart_without_replay (oracle 79bc92fa). The threshold press succeeds, but the receipt write
// fails while the store is busy. After a restart the saved candidate reconciles the notice to
// "sent" on the next tick, and the restart sends no second key.
#[test]
fn a_failed_notification_reconciles_after_restart_without_replay() {
    let dir = tempfile::tempdir().unwrap();
    let native = native_fixture(dir.path());
    let inbox_path = dir.path().join("inbox.db");
    let inbox = Arc::new(Store::open(&inbox_path, dir.path().join("blobs")).unwrap());
    let inbox_id = inbox
        .send(&Outgoing {
            mailbox: "writer",
            body: Some("real notification"),
            ..Outgoing::default()
        })
        .unwrap();
    inbox
        .record_notify(&inbox_id, "failed", Some("text did not submit"), None, None)
        .unwrap();
    let holder = Connection::open(&inbox_path).unwrap();

    let flock = persisted_flock(dir.path());
    flock.attach_stores(Stores {
        inbox: Some(inbox.clone()),
        ..Stores::default()
    });
    let mut env = incident_env(&native);
    let mut message = failed_incident(&env);
    message.notify_for = vec![inbox_id.clone()];
    flock.note_stuck_candidate(&message);

    scan(&flock, &mut env, BASE + 1.0);
    assert!(env.keys.is_empty(), "no key on the first sighting");
    holder.execute_batch("BEGIN EXCLUSIVE").unwrap();
    scan(&flock, &mut env, BASE + 121.0);
    holder.execute_batch("ROLLBACK").unwrap();
    assert_eq!(vec![enter()], env.keys, "the threshold press succeeds");
    assert_eq!(
        Some("failed".to_string()),
        notify_state(&inbox_path, &inbox_id),
        "the receipt write fails while the store is busy"
    );

    let saved = read_state(dir.path());
    drop(flock);
    let restarted = persisted_flock(dir.path());
    restarted.attach_stores(Stores {
        inbox: Some(inbox.clone()),
        ..Stores::default()
    });
    restarted.restore_snapshot(&saved);
    scan(&restarted, &mut env, BASE + 301.0);
    assert_eq!(
        Some("sent".to_string()),
        notify_state(&inbox_path, &inbox_id),
        "the restart reconciles the receipt"
    );
    assert_eq!(1, env.keys.len(), "the restart does not replay the send");
}

fn read_state(dir: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(dir.join("state.json")).unwrap()).unwrap()
}
