//! t3355 wiring row 13 (t820 proof D). The oracle test is test_dunk_recovery_wiring.py::
//! test_schedule_false_sent_with_real_frame_recovers (oracle 79bc92fa): a scheduled message is
//! reported sent while the real t508 frame is on screen, and a bound native codex session
//! authenticates that frame. The observer must press exactly once at the threshold.
//!
//! The fixture is the same as the wiring 12 and 22 module: a live codex stand-in (a copy of
//! /bin/sh, blocking in `read`), its rollout, state_5.sqlite and bound.json, and a persisted flock.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::Connection;
use serde_json::{json, Value};

use crate::flock::panes::PaneInfoEnv;
use crate::flock::sends::{pane_generation, Message, SendEnv};
use crate::flock::Flock;

const T508_FRAME: &str = include_str!("../testdata/t508-cos-ambiguous.txt");
const T508_TEXT: &str = include_str!("../testdata/t508-text.txt");
const T508_PREFIX: &str = include_str!("../testdata/t508-prefix.txt");
const SESSION: &str = "01a173ee-beed-7461-a560-e5f6c1f666a7";
/// The composer the renderer draws after a submitting Enter, as the oracle's fake draws it.
const EMPTY_COMPOSER: &str = "\u{203a} \n\n  GPT-6-Astra high \u{b7} ~/workspace \u{b7} task\n";
/// ETXTBSY: the copied codex binary is still open for writing in another fork.
const ETXTBSY: i32 = 26;

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
    let mut attempts = 0;
    let mut child = loop {
        match spawn_codex(&binary, &home, &hooks) {
            Ok(child) => break NativeChild(child),
            // Another test thread's fork can hold the copy open for an instant after it was
            // written. Retry that one error, then fail on anything else.
            Err(error) if error.raw_os_error() == Some(ETXTBSY) && attempts < 500 => {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("spawn the codex fixture: {error}"),
        }
    };
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

fn enter() -> (String, String) {
    ("%1".to_string(), "Enter".to_string())
}

// t3355 wiring row 13, test_dunk_recovery_wiring.py::test_schedule_false_sent_with_real_frame_
// recovers (oracle 79bc92fa). The schedule's dunk is running at this pane with the message's text.
// The message is reported sent, the real frame stays on screen, and the native session authenticates
// it. The first sighting starts the clock, and the threshold tick presses exactly once.
#[test]
fn a_scheduled_false_sent_real_frame_recovers_once_through_native_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let native = native_fixture(dir.path());
    let flock = persisted_flock(dir.path());
    let mut env = incident_env(&native);
    // The key boundary reads the wall clock for schedule currency, so the message is created at
    // the real clock and the observer ticks run from it.
    let base = crate::clock::now_s();
    let rows = json!({"dunkers": [{"id": "d82", "running": true, "target_pane_id": "%1", "text": T508_TEXT}]});
    flock.dunks.load(rows.as_object().unwrap());
    let mut message = Message::new("m1", "%1", "CoS", T508_TEXT, "d82");
    message.created_at = base;
    message.status = "sent".into();
    message.scheduled = true;
    message.schedule_template = Some(T508_TEXT.into());
    message.typed_at = Some(base);
    message.attempt_generation = pane_generation(&env, "%1");
    flock.note_stuck_candidate(&message);

    scan(&flock, &mut env, base + 1.0);
    assert!(env.keys.is_empty(), "no key on the first sighting");
    scan(&flock, &mut env, base + 121.0);
    assert_eq!(vec![enter()], env.keys, "exactly one key at the threshold");
}
