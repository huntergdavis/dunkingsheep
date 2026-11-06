//! t3355 group D (t820 proof D), the rows this branch owns. Each test names its row and its
//! oracle test at commit 79bc92fa (tests/test_dunk_recovery_wiring.py and
//! tests/test_dunk_scheduled_recovery.py). A child of `sends_tests`, so the parent's fixtures
//! (`Pane`, `recovery_candidate`) are shared without editing the parent's tests.

use super::*;

struct ChildGuard(std::process::Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

const SESSION: &str = "01a173ee-beed-7461-a560-e5f6c1f666a7";
const TEXT: &str = "Scheduled check d7: read your inbox and finish the assigned work.";
const FOOTER: &str = "  GPT-6-Astra high · ~/workspace · task\n";

/// A real process named `codex`, started with exactly the given native-state environment. The
/// Codex home and the hook state directory are removed first, so a case with no pairs reads the
/// same empty environment as the Python fixture does.
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

/// Writes the rollout and thread row that let `SESSION` resolve to `cwd`.
fn write_thread(home: &std::path::Path, cwd: &std::path::Path) {
    let rollout = home.join(format!("rollout-{SESSION}.jsonl"));
    std::fs::write(
        &rollout,
        format!(
            "{}\n",
            serde_json::json!({"type":"session_meta", "payload":{"session_id":SESSION,"cwd":cwd}})
        ),
    )
    .unwrap();
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

/// A fake pane `%1` running `codex` at `pid`, idle, showing `screen`.
fn bound_pane(pid: i64, cwd: &std::path::Path, screen: &str) -> Pane {
    let mut env = Pane::default();
    env.panes
        .push(serde_json::json!({"pane_id":"%1","pane_pid":pid,"agent":"codex","cwd":cwd}));
    env.statuses.insert("%1".into(), "idle".into());
    env.screens.insert("%1".into(), screen.to_string());
    env
}

/// A failed (or sent) scheduled reminder for `%1`, created at the real clock, as the observer
/// currency check reads it (ticks are offsets from `created`). The dunk row carries the same text,
/// so the schedule is current.
fn scheduled_fixture(status: &str, pid: i64) -> (Flock, Pane, f64) {
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        false,
    );
    flock.dunks.load(
        serde_json::json!({"dunkers":[{"id":"d7","target_pane_id":"%1","text":TEXT,"running":true}]})
            .as_object()
            .unwrap(),
    );
    let created = crate::clock::now_s();
    let mut env = bound_pane(pid, dir.path(), "");
    let mut message = Message::new("m7", "%1", "writer", TEXT, "d7");
    message.created_at = created;
    message.status = status.into();
    message.scheduled = true;
    message.schedule_template = Some(TEXT.into());
    message.attempt_generation = pane_generation(&env, "%1");
    message.typed_at = Some(created);
    flock.note_stuck_candidate(&message);
    env.screens.remove("%1");
    (flock, env, created)
}

fn observe(flock: &Flock, env: &mut Pane, now: f64) {
    crate::flock::loops_stuck::strict_stuck_observer_scan(flock, env, now).unwrap();
}

/// Oracle test_hook_session_bound_to_root_pid_start_and_pane (wiring row 24): a hook record binds a
/// session to a root pid, root start and pane. A record for another pane, or a stale root start,
/// is not evidence for this pane.
#[test]
fn wiring_24_hook_record_binds_the_root_pid_start_and_pane() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let hooks = dir.path().join("hooks");
    let cwd = dir.path().join("cwd");
    for path in [&home, &hooks, &cwd] {
        std::fs::create_dir_all(path).unwrap();
    }
    let (_child, pid, start) = spawn_codex(
        dir.path(),
        &[
            ("CODEX_HOME", home.as_path()),
            ("DUNKINGSHEEP_CODEX_STATE_DIR", hooks.as_path()),
        ],
    );
    write_thread(&home, &cwd);
    let hook = hooks.join(format!("{SESSION}.json"));
    let record = |pane: &str, root_start: &str| {
        serde_json::json!({"session_id": SESSION, "pane_id": pane, "root_pid": pid,
            "root_start": root_start, "cwd": cwd})
        .to_string()
    };
    std::fs::write(&hook, record("%1", &start)).unwrap();

    let screen = "› stuck body\n\n  model · /tmp/test\n";
    let env = bound_pane(pid, &cwd, screen);
    let flock = Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        false,
    );
    let mut message = Message::new("m1", "%1", "writer", "stuck body", "send_text");
    message.created_at = 100.0;
    message.status = "failed".into();
    message.typed_at = Some(100.0);
    message.attempt_generation = pane_generation(&env, "%1");
    flock.note_stuck_candidate(&message);

    assert!(
        flock.owned_recovery_evidence(&env, "%1", screen).is_some(),
        "the record binds this pid, start and pane"
    );
    std::fs::write(&hook, record("%2", &start)).unwrap();
    assert!(
        flock.owned_recovery_evidence(&env, "%1", screen).is_none(),
        "a record bound to another pane is not this pane's evidence"
    );
    std::fs::write(&hook, record("%1", "stale")).unwrap();
    assert!(
        flock.owned_recovery_evidence(&env, "%1", screen).is_none(),
        "a stale root start is not evidence"
    );
}

/// Oracle test_missing_native_binding_has_one_named_diagnostic (wiring row 26): with no native
/// binding and no state directory (the Python fixture's empty environment), the observer presses
/// nothing, and the named refusal is recorded once.
#[test]
fn wiring_26_missing_native_binding_records_one_named_refusal_and_presses_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("cwd");
    std::fs::create_dir_all(&cwd).unwrap();
    let (_child, pid, _start) = spawn_codex(dir.path(), &[]);
    let screen = "› stuck body\n\n  model · /tmp/test\n";
    let mut env = bound_pane(pid, &cwd, screen);
    let flock = Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        false,
    );
    let mut message = Message::new("m1", "%1", "writer", "stuck body", "send_text");
    message.created_at = 100.0;
    message.status = "failed".into();
    message.typed_at = Some(100.0);
    message.attempt_generation = pane_generation(&env, "%1");
    flock.note_stuck_candidate(&message);

    observe(&flock, &mut env, 101.0);
    observe(&flock, &mut env, 220.0);
    assert!(env.keys.is_empty(), "no binding, no key");
    let refusal = flock.stuck_candidates.lock().unwrap()["%1"]
        .native_refusal
        .clone();
    assert!(
        refusal
            .as_deref()
            .is_some_and(|text| text.contains("no process-bound native session id")),
        "the refusal is named: {refusal:?}"
    );
    assert!(flock.owned_recovery_evidence(&env, "%1", screen).is_none());
    assert_eq!(
        refusal,
        flock.stuck_candidates.lock().unwrap()["%1"].native_refusal,
        "the named refusal is recorded once, not per tick"
    );
}

/// Oracle test_unbound_failure_has_one_named_diagnostic_and_never_recovers (wiring row 25).
/// Divergence: Python sets `unobservable_logged` and logs "attempt generation unknown" once when
/// the candidate is noted (dunk_core.py:6203-6206). Rust never sets `unobservable_logged`
/// (sends.rs:2833; the only copy is sends.rs:4525), so no diagnostic is recorded. Ignored until the
/// owner rules on the divergence; no production code is changed here.
#[test]
#[ignore = "divergence tracked by t3468: Python logs 'attempt generation unknown' once; Rust never sets unobservable_logged"]
fn wiring_25_unbound_failure_has_one_named_diagnostic_and_never_recovers() {
    let mut env = Pane::default();
    env.panes
        .push(serde_json::json!({"pane_id":"%1","agent":"codex"}));
    env.statuses.insert("%1".into(), "idle".into());
    env.screens
        .insert("%1".into(), "› stuck body\n\n  model · /tmp/test\n".into());
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        false,
    );
    let mut message = Message::new("m1", "%1", "writer", "stuck body", "send_text");
    message.created_at = 100.0;
    message.status = "failed".into();
    message.attempt_generation = None;
    flock.note_stuck_candidate(&message);
    flock.note_stuck_candidate(&message);
    assert_eq!(
        Some(true),
        flock.stuck_candidates.lock().unwrap()["%1"].unobservable_logged,
        "the unknown attempt generation is named once"
    );
    observe(&flock, &mut env, 101.0);
    observe(&flock, &mut env, 220.0);
    assert!(env.keys.is_empty());
}

/// Oracle test_styled_schedule_survives_restart_with_provenance_for_echo (scheduled row 40), the
/// pre-restart half: an echo of an earlier message in this pane's sent provenance, above a bright
/// schedule, presses exactly once at two minutes and not before.
#[test]
#[ignore = "fixture not reproduced: the Rust observer needs the message-history provenance that Python's _remember writes; this scenario presses nothing even before a restart"]
fn scheduled_40_styled_echo_with_provenance_presses_once_at_two_minutes() {
    let (flock, mut env, created) = scheduled_fixture("failed", i64::from(std::process::id()));
    let echo = "[inbox] new message for Codex. Read it with: dunkingsheep inbox read";
    flock
        .sent_text_by_pane
        .lock()
        .unwrap()
        .insert("%1".into(), vec![echo.into()]);
    env.screens.insert(
        "%1".into(),
        format!("\x1b[1;2m›\x1b[0m {echo}\n\n\x1b[1m›\x1b[0m {TEXT}\n\n\n{FOOTER}"),
    );
    observe(&flock, &mut env, created + 1.0);
    assert!(env.keys.is_empty(), "no press before two minutes");
    observe(&flock, &mut env, created + 120.0);
    assert_eq!(1, env.keys.len(), "one recovery at two minutes");
}

/// Oracle test_styled_schedule_survives_restart_with_provenance_for_echo (scheduled row 40), the
/// restart half. Unverified, ignored: the pre-restart control in this fixture presses nothing, so
/// this test cannot show a divergence. The snapshot carries no sent_text_by_pane
/// (flock/mod.rs:249-330). No production code is changed here.
#[test]
#[ignore = "unverified: the pre-restart control in this fixture presses nothing; not a divergence until the fixture is reproduced"]
fn scheduled_40_restart_recovers_once_like_python() {
    let (flock, mut env, created) = scheduled_fixture("failed", i64::from(std::process::id()));
    let echo = "[inbox] new message for Codex. Read it with: dunkingsheep inbox read";
    env.screens.insert(
        "%1".into(),
        format!("\x1b[1;2m›\x1b[0m {echo}\n\n\x1b[1m›\x1b[0m {TEXT}\n\n\n{FOOTER}"),
    );
    let payload = flock.snapshot();
    drop(flock);
    let restarted = Flock::new(
        std::path::PathBuf::from("/nonexistent/launches.json"),
        std::path::PathBuf::from("/nonexistent/lanes.json"),
        false,
    );
    restarted.restore_snapshot(&payload);
    observe(&restarted, &mut env, created + 1.0);
    assert!(env.keys.is_empty(), "no press before two minutes");
    observe(&restarted, &mut env, created + 120.0);
    assert_eq!(1, env.keys.len(), "one recovery after the restart");
}

/// Oracle test_unaccounted_styled_human_line_remains_unknown (scheduled row 41): a human line
/// before the scheduled prompt leaves the composer unknown, so no key is pressed and the box
/// reads as unknown rather than empty.
#[test]
fn scheduled_41_unaccounted_styled_human_line_stays_unknown() {
    let (flock, mut env, created) = scheduled_fixture("failed", i64::from(std::process::id()));
    let screen = format!(
        "\x1b[1;2m›\x1b[0m a human line never sent by us\n\n\x1b[1m›\x1b[0m {TEXT}\n\n\n{FOOTER}"
    );
    env.screens.insert("%1".into(), screen.clone());
    observe(&flock, &mut env, created + 1.0);
    observe(&flock, &mut env, created + 120.0);
    assert!(
        env.keys.is_empty(),
        "an unaccounted human line presses nothing"
    );
    let (text, detail) = crate::terminal::screen::box_text(&screen);
    assert!(
        text.is_empty() && detail.starts_with(AMBIGUOUS_COMPOSER),
        "the box is unknown, not empty: {text:?} {detail:?}"
    );
}

/// Oracle test_reviewed_echo_edits_cannot_authorize_recovery (scheduled row 42): each edit of an
/// approved echo, appended human text, a continuation, a truncated echo, or an ellipsis, leaves the
/// composer not owned, and no key is pressed over two ticks.
#[test]
fn scheduled_42_reviewed_echo_edits_cannot_authorize_recovery() {
    let echo = "Prior approved schedule: check your task and report the result.";
    let truncated = &echo[..5];
    for body in [
        format!("{echo} HUMAN EDIT"),
        format!("{echo}\nhuman continuation"),
        format!("{echo}\n\nhuman continuation"),
        truncated.to_string(),
        format!("{truncated}…"),
    ] {
        let (flock, mut env, created) = scheduled_fixture("failed", i64::from(std::process::id()));
        flock
            .sent_text_by_pane
            .lock()
            .unwrap()
            .insert("%1".into(), vec![echo.into()]);
        let screen = format!("\x1b[1;2m›\x1b[0m {body}\n\n\x1b[1m›\x1b[0m {TEXT}\n\n\n{FOOTER}");
        env.screens.insert("%1".into(), screen.clone());
        observe(&flock, &mut env, created + 1.0);
        observe(&flock, &mut env, created + 120.0);
        assert!(env.keys.is_empty(), "no key for the edit {body:?}");
        assert!(
            flock.owned_recovery_evidence(&env, "%1", &screen).is_none(),
            "the composer is not owned for the edit {body:?}"
        );
    }
}

/// Oracle test_sent_schedule_style_change_before_press_refuses (scheduled row 47): the decision
/// reads the bright (sent) composer, and the press reads the dim one. No key is pressed.
#[test]
fn scheduled_47_sent_schedule_style_change_before_press_refuses() {
    let (flock, mut env, created) = scheduled_fixture("sent", i64::from(std::process::id()));
    let bright = format!("\x1b[1m›\x1b[0m {TEXT}\n\n\n{FOOTER}");
    let dim = format!("\x1b[1;2m›\x1b[0m {TEXT}\n\n\n{FOOTER}");
    env.screens.insert("%1".into(), bright.clone());
    observe(&flock, &mut env, created + 1.0);
    assert!(env.keys.is_empty(), "no press before the window");
    // The second tick's decision reads the bright composer, then every later read sees the dim one.
    env.screen_script.borrow_mut().push_back(Some(bright));
    for _ in 0..16 {
        env.screen_script.borrow_mut().push_back(Some(dim.clone()));
    }
    observe(&flock, &mut env, created + 120.0);
    assert!(
        env.keys.is_empty(),
        "a style change before the press refuses the press"
    );
}
