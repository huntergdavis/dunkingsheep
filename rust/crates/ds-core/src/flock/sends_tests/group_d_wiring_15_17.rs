//! t3483 (t3355 group D remainder): T55 wiring rows 15 and 17 (t820 proof D). The oracle is
//! tests/test_dunk_recovery_wiring.py at 79bc92fa: `test_negative_controls_never_press` (row 15)
//! and `test_final_boundary_native_start_or_edit_prevents_key` (row 17). Tests only. A real
//! divergence is reported, not fixed, so this file changes no production code.
//!
//! A child of `sends_tests`, so the parent's `frame`, `persistent_recovery_flock` and
//! `pane_generation` are shared. The spawned `codex` process, the hook record and `ChildGuard`
//! are copied from `group_d.rs`, a sibling module whose helpers are private to it.

use super::*;
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

/// A native-bound pane, built as Python's `RecoveryWiringTests.setUp` and `failed()` build one: a
/// real process named `codex` whose hook record binds it to `%1`, a thread row and rollout for
/// `SESSION`, and a failed, typed candidate whose text the composer on screen owns. `_child` is
/// declared first, so the process is killed before its directory is removed.
struct Native {
    _child: ChildGuard,
    _dir: tempfile::TempDir,
    home: std::path::PathBuf,
    cwd: std::path::PathBuf,
    flock: Flock,
    env: Pane,
    message: Message,
}

impl Native {
    fn set_screen(&mut self, screen: String) {
        self.env.screens.insert("%1".into(), screen);
    }

    fn rollout(&self) -> std::path::PathBuf {
        self.home.join(format!("rollout-{SESSION}.jsonl"))
    }
}

type Mutation = fn(&mut Native);

fn spawn_codex(
    dir: &std::path::Path,
    env: &[(&str, &std::path::Path)],
) -> (ChildGuard, i64, String) {
    let binary = dir.join("codex");
    std::fs::copy("/bin/sleep", &binary).unwrap();
    let mut command = std::process::Command::new(binary);
    command.arg("300");
    command.env_remove("CODEX_HOME");
    command.env_remove("DUNKINGSHEEP_CODEX_STATE_DIR");
    for (key, value) in env {
        command.env(key, value);
    }
    let child = ChildGuard(command.spawn().unwrap());
    let pid = i64::from(child.0.id());
    let start = crate::proc::process_start(pid).unwrap();
    (child, pid, start)
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

/// Replaces the rollout with `events`, one JSON object per line.
fn write_rollout(home: &std::path::Path, events: &[serde_json::Value]) {
    let mut lines = String::new();
    for event in events {
        writeln!(lines, "{event}").unwrap();
    }
    std::fs::write(home.join(format!("rollout-{SESSION}.jsonl")), lines).unwrap();
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

fn native_fixture() -> Native {
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
    let env = bound_pane(pid, &cwd, &frame(TEXT, &[]));
    let flock = persistent_recovery_flock(dir.path(), &dir.path().join("state.json"));
    let mut message = Message::new("m1", "%1", "writer", TEXT, "send_text");
    message.created_at = 100.0;
    message.status = "failed".into();
    message.typed_at = Some(100.0);
    message.attempt_generation = pane_generation(&env, "%1");
    flock.note_stuck_candidate(&message);
    Native {
        _child: child,
        _dir: dir,
        home,
        cwd,
        flock,
        env,
        message,
    }
}

fn observe(flock: &Flock, env: &mut Pane, now: f64) {
    crate::flock::loops_stuck::strict_stuck_observer_scan(flock, env, now).unwrap();
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
            let events = [session_meta(&fx.cwd), assistant_output(), user_input(TEXT)];
            write_rollout(&fx.home, &events);
        }),
        ("native active", |fx: &mut Native| {
            let events = [session_meta(&fx.cwd), assistant_output(), task_started()];
            write_rollout(&fx.home, &events);
        }),
        ("native missing auth", |fx: &mut Native| {
            write_rollout(&fx.home, &[assistant_output()]);
        }),
        ("native missing file", |fx: &mut Native| {
            std::fs::remove_file(fx.rollout()).unwrap();
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
            fx.flock.stuck_candidates.lock().unwrap().clear();
        }),
        ("untyped intent", |fx: &mut Native| {
            let mut rows = fx.flock.stuck_candidates.lock().unwrap();
            rows.get_mut("%1").unwrap().typed_at = None;
        }),
    ]
}

/// Oracle test_negative_controls_never_press (wiring row 15). Every control runs on a native-bound
/// pane and is observed at +1 and +121 seconds, as Python ticks it at 1000 and 1120. The fixture is
/// press-ready when nothing is wrong (the next test), so each "no key" below is not vacuous.
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
/// threshold, so the negative controls above can only pass because of the condition each one
/// changes.
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

/// The decision, then the native start (when `race`), then the reservation save, then the
/// press-time revalidation. Python's `race()` writes the native start and then saves, inside the
/// observer tick and before the key. The Rust observer has no seam between its save and its press,
/// so this takes those steps in the same order. The parent's
/// `a_text_edited_after_the_reservation_save_is_not_pressed` models the text half the same way.
fn press_after_the_save(fx: &mut Native, race: bool) -> (bool, String) {
    let screen = fx.env.screens["%1"].clone();
    assert!(
        fx.flock
            .owned_recovery_evidence(&fx.env, "%1", &screen)
            .is_some(),
        "the decision sees the owned native composer"
    );
    let generation = pane_generation(&fx.env, "%1").expect("a live pane generation");
    if race {
        write_rollout(
            &fx.home,
            &[session_meta(&fx.cwd), assistant_output(), task_started()],
        );
    }
    fx.flock.save().unwrap();
    fx.flock.press_stuck_enter(
        &mut fx.env,
        "%1",
        &crate::stuck::fingerprint(TEXT),
        &generation,
        "m1",
        Some(TEXT),
        Some(&fx.message),
        true,
        Some(100.0),
    )
}

/// Oracle test_final_boundary_native_start_or_edit_prevents_key (wiring row 17), native half: a
/// native turn that starts between the decision and the key refuses the press, and no key is sent.
#[test]
fn wiring_17_native_turn_starting_at_the_save_boundary_prevents_the_key() {
    let mut fx = native_fixture();
    observe(&fx.flock, &mut fx.env, 101.0);
    let (pressed, detail) = press_after_the_save(&mut fx, true);
    assert!(
        !pressed,
        "a native turn that starts before the key must refuse it: {detail}"
    );
    assert!(fx.env.keys.is_empty(), "no key was sent");
}

/// The control for row 17: the same path, with no native start, presses once. This shows the
/// refusal above comes from the native start and not from the path itself.
#[test]
fn wiring_17_control_presses_once_when_no_native_turn_starts() {
    let mut fx = native_fixture();
    observe(&fx.flock, &mut fx.env, 101.0);
    let (pressed, detail) = press_after_the_save(&mut fx, false);
    assert!(
        pressed,
        "the same path presses when nothing races it: {detail}"
    );
    assert_eq!(1, fx.env.keys.len(), "exactly one key");
}
