//! t3483 (t3355 group D remainder): T55 wiring rows 15 and 17 (t820 proof D). The oracle is
//! tests/test_dunk_recovery_wiring.py at 79bc92fa: `test_negative_controls_never_press` (row 15)
//! and `test_final_boundary_native_start_or_edit_prevents_key` (row 17). Tests only. A real
//! divergence is reported, not fixed, so this file changes no production code.
//!
//! A child of `sends_tests`, so the parent's `frame`, `persistent_recovery_flock` and
//! `pane_generation` are shared. The spawned `codex` process, the hook record and `ChildGuard`
//! are copied from `group_d.rs`, a sibling module whose helpers are private to it.

use super::*;
use crate::flock::panes::PaneInfoEnv;
use crate::flock::sends::SendEnv;
use std::fmt::Write as _;

const SESSION: &str = "01a173ee-beed-7461-a560-e5f6c1f666a7";
const TEXT: &str = "stuck body";

struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The native binding that both rows use, built as Python's `RecoveryWiringTests.setUp` builds it:
/// a real process named `codex` whose hook record binds it to `%1`, a thread row and a rollout for
/// `SESSION`. `_child` is first, so the process dies before its directory is removed.
struct NativeFiles {
    _child: ChildGuard,
    _dir: tempfile::TempDir,
    home: std::path::PathBuf,
    cwd: std::path::PathBuf,
    pid: i64,
}

impl NativeFiles {
    fn rollout(&self) -> std::path::PathBuf {
        self.home.join(format!("rollout-{SESSION}.jsonl"))
    }
}

/// Row 15's fixture: the native binding, the pane it is bound to, and a failed candidate that
/// the observer can see.
struct Native {
    files: NativeFiles,
    env: Pane,
    flock: Flock,
    _state: tempfile::TempDir,
}

impl Native {
    fn set_screen(&mut self, screen: String) {
        self.env.screens.insert("%1".into(), screen);
    }
}

type Mutation = fn(&mut Native);

/// A live process named `codex`, bound to the environment given. It is a copy of `/bin/sh` that
/// blocks reading its stdin: a copy of `/bin/sleep` can exit at once under this name, and an
/// exited process has no native binding to observe.
fn spawn_codex(
    dir: &std::path::Path,
    env: &[(&str, &std::path::Path)],
) -> (ChildGuard, i64, String) {
    let binary = dir.join("codex");
    std::fs::copy("/bin/sh", &binary).unwrap();
    let mut command = std::process::Command::new(&binary);
    command
        .arg("-c")
        .arg("read line")
        .stdin(std::process::Stdio::piped());
    command.env_remove("CODEX_HOME");
    command.env_remove("DUNKINGSHEEP_CODEX_STATE_DIR");
    for (key, value) in env {
        command.env(key, value);
    }
    let mut attempts = 0;
    let mut child = loop {
        match command.spawn() {
            Ok(child) => break child,
            // Another test thread can hold the copied binary open for an instant (ETXTBSY).
            Err(error) if error.raw_os_error() == Some(26) && attempts < 100 => {
                attempts += 1;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => panic!("spawn the codex stand-in: {error}"),
        }
    };
    assert!(
        child.try_wait().unwrap().is_none(),
        "the codex stand-in must stay alive"
    );
    let pid = i64::from(child.id());
    let start = crate::proc::process_start(pid).unwrap();
    (ChildGuard(child), pid, start)
}

/// A fake pane `%1` running `codex` at `pid`, idle, showing `screen`.
fn bound_pane(pid: i64, cwd: &std::path::Path, screen: &str) -> Pane {
    let mut env = Pane::default();
    env.panes
        .push(serde_json::json!({"pane_id":"%1","pane_pid":pid,"agent":"codex","cwd":cwd}));
    env.statuses.insert("%1".into(), "idle".into());
    env.screens.insert("%1".into(), screen.to_string());
    env
}

fn write_thread_row(home: &std::path::Path, cwd: &std::path::Path) {
    let rollout = home.join(format!("rollout-{SESSION}.jsonl"));
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
}

/// The rollout text for `events`, one JSON object per line.
fn rollout_text(events: &[serde_json::Value]) -> String {
    let mut lines = String::new();
    for event in events {
        writeln!(lines, "{event}").unwrap();
    }
    lines
}

fn write_rollout(home: &std::path::Path, events: &[serde_json::Value]) {
    std::fs::write(
        home.join(format!("rollout-{SESSION}.jsonl")),
        rollout_text(events),
    )
    .unwrap();
}

fn session_meta(cwd: &std::path::Path) -> serde_json::Value {
    serde_json::json!({"type":"session_meta","payload":{"session_id":SESSION,"cwd":cwd}})
}

fn assistant_output() -> serde_json::Value {
    serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant",
        "content":[{"type":"output_text","text":"synthetic transcript"}]}})
}

fn user_input(text: &str) -> serde_json::Value {
    serde_json::json!({"type":"response_item","payload":{"type":"message","role":"user",
        "content":[{"type":"input_text","text":text}]}})
}

fn task_started() -> serde_json::Value {
    serde_json::json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"turn"}})
}

fn native_files() -> NativeFiles {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let hooks = dir.path().join("hooks");
    let cwd = dir.path().join("cwd");
    for path in [&home, &hooks, &cwd] {
        std::fs::create_dir_all(path).unwrap();
    }
    let (child, pid, start) = spawn_codex(
        dir.path(),
        &[
            ("CODEX_HOME", home.as_path()),
            ("DUNKINGSHEEP_CODEX_STATE_DIR", hooks.as_path()),
        ],
    );
    write_thread_row(&home, &cwd);
    write_rollout(&home, &[session_meta(&cwd), assistant_output()]);
    std::fs::write(
        hooks.join(format!("{SESSION}.json")),
        serde_json::json!({"session_id": SESSION, "pane_id": "%1", "root_pid": pid,
            "root_start": start, "cwd": cwd})
        .to_string(),
    )
    .unwrap();
    NativeFiles {
        _child: child,
        _dir: dir,
        home,
        cwd,
        pid,
    }
}

/// The native pane `%1`: the bound process, idle, showing the owned composer.
fn native_pane(files: &NativeFiles) -> Pane {
    bound_pane(files.pid, &files.cwd, &frame(TEXT, &[]))
}

/// A persisted Flock in `state_dir` holding the failed, typed candidate for `%1`. Its attempt
/// generation is read from `env`, as the observer's currency check reads it.
fn candidate_flock(state_dir: &std::path::Path, env: &Pane) -> Flock {
    let flock = persistent_recovery_flock(state_dir, &state_dir.join("state.json"));
    let mut message = Message::new("m1", "%1", "writer", TEXT, "send_text");
    message.created_at = 100.0;
    message.status = "failed".into();
    message.typed_at = Some(100.0);
    message.attempt_generation = pane_generation(env, "%1");
    flock.note_stuck_candidate(&message);
    flock
}

fn native_fixture() -> Native {
    let files = native_files();
    let env = native_pane(&files);
    let state = tempfile::tempdir().unwrap();
    let flock = candidate_flock(state.path(), &env);
    Native {
        files,
        env,
        flock,
        _state: state,
    }
}

/// Runs the real strict observer tick at `now`, and returns how many keys it acted on.
fn observe<E: SendEnv + PaneInfoEnv>(flock: &Flock, env: &mut E, now: f64) -> usize {
    crate::flock::loops_stuck::strict_stuck_observer_scan(flock, env, now).unwrap()
}

/// The negative controls of Python's `test_negative_controls_never_press`, one fixture each, in the
/// oracle's order. Each one changes a single condition after the candidate is recorded.
fn negative_controls() -> [(&'static str, Mutation); 15] {
    [
        ("human continuation", |fx: &mut Native| {
            fx.set_screen(frame(TEXT, &["call Hunter"]));
        }),
        ("human edit", |fx: &mut Native| {
            fx.set_screen(frame(&TEXT.replace("stuck", "Stuck"), &[]));
        }),
        ("inserted sigil", |fx: &mut Native| {
            let screen = frame(TEXT, &[]).replace(
                &format!("\u{203a} {TEXT}"),
                &format!("\u{203a} my draft\n\u{203a} {TEXT}"),
            );
            fx.set_screen(screen);
        }),
        ("human line before prompt", |fx: &mut Native| {
            let screen = frame(TEXT, &[]).replace(
                &format!("\u{203a} {TEXT}"),
                &format!("do not send my unfinished draft\n\u{203a} {TEXT}"),
            );
            fx.set_screen(screen);
        }),
        ("native consumed", |fx: &mut Native| {
            let events = [
                session_meta(&fx.files.cwd),
                assistant_output(),
                user_input(TEXT),
            ];
            write_rollout(&fx.files.home, &events);
        }),
        ("native active", |fx: &mut Native| {
            let events = [
                session_meta(&fx.files.cwd),
                assistant_output(),
                task_started(),
            ];
            write_rollout(&fx.files.home, &events);
        }),
        ("native missing auth", |fx: &mut Native| {
            write_rollout(&fx.files.home, &[assistant_output()]);
        }),
        ("native missing file", |fx: &mut Native| {
            std::fs::remove_file(fx.files.rollout()).unwrap();
        }),
        ("busy", |fx: &mut Native| {
            fx.env.statuses.insert("%1".into(), "working".into());
        }),
        ("dialog", |fx: &mut Native| {
            let screen = format!(
                "{}\nDo you want to proceed?\n  1. Yes\n  2. No\n",
                frame(TEXT, &[])
            );
            fx.set_screen(screen);
        }),
        ("consumed attempt", |fx: &mut Native| {
            let mut rows = fx.flock.stuck_candidates.lock().unwrap();
            let entry = rows.get_mut("%1").unwrap();
            entry.eligible = false;
            entry.status = "consumed".into();
        }),
        ("stale state", |fx: &mut Native| {
            let mut rows = fx.flock.stuck_candidates.lock().unwrap();
            rows.get_mut("%1").unwrap().status = "stale".into();
        }),
        ("wrong generation", |fx: &mut Native| {
            let mut rows = fx.flock.stuck_candidates.lock().unwrap();
            rows.get_mut("%1").unwrap().generation = Some("pid:old:start:old".into());
        }),
        ("no durable attempt", |fx: &mut Native| {
            // The fixture holds one candidate, so removing it is the oracle's dict clear.
            fx.flock.stuck_candidates.lock().unwrap().remove("%1");
        }),
        ("untyped intent", |fx: &mut Native| {
            let mut rows = fx.flock.stuck_candidates.lock().unwrap();
            rows.get_mut("%1").unwrap().typed_at = None;
        }),
    ]
}

/// Oracle test_negative_controls_never_press (wiring row 15). Every control runs on a native-bound
/// pane and is observed at +1 and +122 seconds, as Python ticks it at 1000 and 1120. The positive
/// control below shows the same fixture presses when nothing is wrong, so these "no key" results
/// are not vacuous.
#[test]
fn wiring_15_negative_controls_never_press() {
    let mut pressed = Vec::new();
    for (name, mutate) in negative_controls() {
        let mut fx = native_fixture();
        mutate(&mut fx);
        observe(&fx.flock, &mut fx.env, 101.0);
        observe(&fx.flock, &mut fx.env, 222.0);
        if !fx.env.keys.is_empty() {
            pressed.push(format!("{name}: {:?}", fx.env.keys));
        }
    }
    assert!(pressed.is_empty(), "negative controls pressed: {pressed:?}");
}

/// The positive control for row 15: the unmutated native pane presses exactly once, at the
/// threshold.
#[test]
fn wiring_15_control_presses_once_at_the_threshold_when_nothing_is_wrong() {
    let mut fx = native_fixture();
    observe(&fx.flock, &mut fx.env, 101.0);
    assert!(fx.env.keys.is_empty(), "no press before the threshold");
    observe(&fx.flock, &mut fx.env, 222.0);
    assert_eq!(
        1,
        fx.env.keys.len(),
        "the unmutated native pane presses once at the threshold"
    );
}

/// A `Pane` that counts its screen reads and, during one of them, replaces the rollout. That read
/// is one the observer makes while deciding, so the native turn starts before the key. `key_reads`
/// records the read count at each key press, which is how the race run finds its boundary.
struct BoundaryEnv {
    inner: Pane,
    reads: std::cell::Cell<usize>,
    key_reads: Vec<usize>,
    flip: Option<(usize, std::path::PathBuf, String)>,
}

impl BoundaryEnv {
    fn new(inner: Pane, flip: Option<(usize, std::path::PathBuf, String)>) -> Self {
        Self {
            inner,
            reads: std::cell::Cell::new(0),
            key_reads: Vec::new(),
            flip,
        }
    }
}

impl SendEnv for BoundaryEnv {
    fn read_pane(&self, pane_id: &str, lines: i64, ansi: bool) -> Option<String> {
        let read = self.reads.get();
        self.reads.set(read + 1);
        if let Some((at, path, content)) = &self.flip {
            if *at == read {
                std::fs::write(path, content).unwrap();
            }
        }
        SendEnv::read_pane(&self.inner, pane_id, lines, ansi)
    }

    fn pane_harness(&self, pane_id: &str) -> Option<String> {
        SendEnv::pane_harness(&self.inner, pane_id)
    }

    fn session_process_gone(&self, pane_id: &str) -> bool {
        SendEnv::session_process_gone(&self.inner, pane_id)
    }

    fn read_status(&self, pane_id: &str) -> Option<String> {
        SendEnv::read_status(&self.inner, pane_id)
    }

    fn send_key(&mut self, pane_id: &str, key: &str) -> (bool, String) {
        self.key_reads.push(self.reads.get());
        SendEnv::send_key(&mut self.inner, pane_id, key)
    }

    fn federated_input(&self) -> bool {
        SendEnv::federated_input(&self.inner)
    }
}

impl PaneInfoEnv for BoundaryEnv {
    fn list_panes_grouped(&self) -> Vec<serde_json::Value> {
        PaneInfoEnv::list_panes_grouped(&self.inner)
    }

    fn get_pane(&self, pane_id: &str) -> Option<serde_json::Value> {
        PaneInfoEnv::get_pane(&self.inner, pane_id)
    }
}

/// Oracle test_final_boundary_native_start_or_edit_prevents_key (wiring row 17), native half. A
/// native turn starts after the observer's decision and before its key, and the observer must not
/// press. The real strict observer runs throughout. A calibration run with no change finds the read
/// count at the key and presses once, which is the positive control. The race run then replaces the
/// rollout during the last read before that key, so the reservation and the press both run after
/// the native turn has started.
#[test]
fn wiring_17_native_turn_starting_at_the_final_boundary_prevents_the_key() {
    let files = native_files();
    let state = tempfile::tempdir().unwrap();

    let calibration_state = state.path().join("calibration");
    std::fs::create_dir_all(&calibration_state).unwrap();
    let mut calibration = BoundaryEnv::new(native_pane(&files), None);
    let flock = candidate_flock(&calibration_state, &calibration.inner);
    observe(&flock, &mut calibration, 101.0);
    observe(&flock, &mut calibration, 222.0);
    assert_eq!(
        1,
        calibration.inner.keys.len(),
        "the unchanged pane presses once at the threshold"
    );
    let boundary = calibration.key_reads[0] - 1;

    let race_state = state.path().join("race");
    std::fs::create_dir_all(&race_state).unwrap();
    let race_rollout =
        rollout_text(&[session_meta(&files.cwd), assistant_output(), task_started()]);
    let mut race = BoundaryEnv::new(
        native_pane(&files),
        Some((boundary, files.rollout(), race_rollout)),
    );
    let flock = candidate_flock(&race_state, &race.inner);
    assert_eq!(0, observe(&flock, &mut race, 101.0));
    assert_eq!(
        0,
        observe(&flock, &mut race, 222.0),
        "the boundary refuses the key"
    );
    assert!(
        race.inner.keys.is_empty(),
        "no key after a native turn starts"
    );
}

/// The control for row 17 on its own: the same observer path, with no native change, presses once
/// at the threshold.
#[test]
fn wiring_17_control_presses_once_at_the_threshold_without_a_race() {
    let files = native_files();
    let state = tempfile::tempdir().unwrap();
    let mut env = BoundaryEnv::new(native_pane(&files), None);
    let flock = candidate_flock(state.path(), &env.inner);
    observe(&flock, &mut env, 101.0);
    assert!(env.inner.keys.is_empty(), "no press before the threshold");
    observe(&flock, &mut env, 222.0);
    assert_eq!(
        1,
        env.inner.keys.len(),
        "the same path presses once when nothing races it"
    );
}
