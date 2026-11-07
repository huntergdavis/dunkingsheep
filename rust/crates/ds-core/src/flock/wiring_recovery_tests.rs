//! t3355 wiring rows (t820 proof D; oracle test_dunk_recovery_wiring.py at 79bc92fa).
//!
//! These tests drive the stuck observer through the native evidence chain: a process named
//! codex, its rollout, state_5.sqlite and bound.json. The test supplies only the process and the
//! pane screen. The screen is the t508 incident frame, and its text and prefix are the
//! byte-identical fixtures the oracle derives from the same frame.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::panes::PaneInfoEnv;
use super::sends::{pane_generation, Message, SendEnv};
use super::Flock;

const T508_FRAME: &str = include_str!("testdata/t508-cos-ambiguous.txt");
const T508_TEXT: &str = include_str!("testdata/t508-text.txt");
const T508_PREFIX: &str = include_str!("testdata/t508-prefix.txt");
const SESSION: &str = "01a173ee-beed-7461-a560-e5f6c1f666a7";

/// Pane screens, statuses and the keys sent to them. Nothing else is faked.
#[derive(Default)]
struct WiringEnv {
    screens: BTreeMap<String, String>,
    statuses: BTreeMap<String, String>,
    panes: Vec<Value>,
    keys: Vec<(String, String)>,
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

fn native_fixture(dir: &Path) -> NativeFixture {
    let home = dir.join("home");
    let hooks = dir.join("hooks");
    let cwd = dir.join("cwd");
    for path in [&home, &hooks, &cwd] {
        std::fs::create_dir_all(path).unwrap();
    }
    let binary = dir.join("codex");
    std::fs::copy("/bin/sleep", &binary).unwrap();
    let child = std::process::Command::new(binary)
        .arg("300")
        .env("CODEX_HOME", &home)
        .env("DUNKINGSHEEP_CODEX_STATE_DIR", &hooks)
        .spawn()
        .unwrap();
    let child = NativeChild(child);
    let pid = i64::from(child.0.id());
    let start = crate::proc::process_start(pid).unwrap();
    let rollout = home.join(format!("rollout-{SESSION}.jsonl"));
    let meta = json!({"type": "session_meta", "payload": {"session_id": SESSION, "cwd": cwd}});
    let answer = json!({"type": "response_item", "payload": {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": T508_PREFIX}]}});
    std::fs::write(&rollout, format!("{meta}\n{answer}\n")).unwrap();
    let db = rusqlite::Connection::open(home.join("state_5.sqlite")).unwrap();
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

fn pane_row(native: &NativeFixture) -> Value {
    json!({"pane_id": "%1", "pane_pid": native.pid, "agent": "codex", "cwd": native.cwd})
}

/// The oracle's wiring config: threshold 120 s, observation 20 s, two attempts, cooldown 60 s.
fn oracle_flock(dir: &Path) -> Flock {
    let flock = Flock::new(dir.join("launches"), dir.join("lanes"), false);
    {
        let mut config = flock.stuck_config.lock().unwrap();
        config.threshold_s = 120.0;
        config.observation_interval_s = 20.0;
        config.max_attempts = 2;
    }
    flock
}

// t3355 wiring row 12, test_dunk_recovery_wiring.py::test_real_628_char_incident_reaches_guarded_
// executor_once (oracle 79bc92fa). A failed non-scheduled send whose composer holds the 628-char
// incident text. The observer sees it once, waits for the threshold, sends exactly one Enter, marks
// the attempt consumed, and sends nothing later.
#[test]
fn a_failed_t508_incident_reaches_the_guarded_executor_once() {
    let dir = tempfile::tempdir().unwrap();
    let native = native_fixture(dir.path());
    let flock = oracle_flock(dir.path());
    let base = 1000.0;
    let mut env = WiringEnv::default();
    env.panes.push(pane_row(&native));
    env.statuses.insert("%1".into(), "idle".into());
    env.screens.insert("%1".into(), T508_FRAME.into());
    let mut message = Message::new("m1", "%1", "CoS", T508_TEXT, "send_text");
    message.created_at = base;
    message.status = "failed".into();
    message.typed_at = Some(base);
    message.attempt_generation = pane_generation(&env, "%1");
    flock.note_stuck_candidate(&message);

    crate::flock::loops_stuck::strict_stuck_observer_scan(&flock, &mut env, base + 1.0).unwrap();
    assert!(env.keys.is_empty(), "no key on the first sighting");
    crate::flock::loops_stuck::strict_stuck_observer_scan(&flock, &mut env, base + 101.0).unwrap();
    assert!(env.keys.is_empty(), "no key before the threshold");
    crate::flock::loops_stuck::strict_stuck_observer_scan(&flock, &mut env, base + 121.0).unwrap();
    assert_eq!(
        vec![("%1".to_string(), "Enter".to_string())],
        env.keys,
        "exactly one guarded Enter at the threshold"
    );
    let entry = flock.stuck_candidates.lock().unwrap()["%1"].clone();
    assert_eq!(Some("consumed"), entry.recovery_outcome.as_deref());
    assert!(
        !entry.eligible,
        "the consumed attempt is not eligible again"
    );
    crate::flock::loops_stuck::strict_stuck_observer_scan(&flock, &mut env, base + 301.0).unwrap();
    assert_eq!(
        1,
        env.keys.len(),
        "no second key after the consumed attempt"
    );
}
