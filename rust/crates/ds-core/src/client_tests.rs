//! `client` decision tests: t696's no-spawn rules, from `tests/test_dunk_client_busy.py`
//! plus the owner-only spawn guard. No socket and no real sleep.

use std::cell::{Cell, RefCell};
use std::time::Duration;

use super::*;

const SOCKET: &str = "/run/user/1000/dunkingsheep.sock";

/// Probe answers in order; the last repeats.
fn probes(answers: Vec<Probe>) -> impl FnMut() -> Probe {
    let mut index = 0;
    move || {
        let answer = answers[index.min(answers.len() - 1)];
        index += 1;
        answer
    }
}

fn run(
    answers: Vec<Probe>,
    owns: bool,
    start_allowed: bool,
    spawned: &Cell<u32>,
    slept: &RefCell<Vec<Duration>>,
) -> Result<(), ConnectError> {
    connect_or_start(
        SOCKET,
        probes(answers),
        || owns,
        &mut |delay| slept.borrow_mut().push(delay),
        start_allowed,
        || {
            spawned.set(spawned.get() + 1);
            Ok(())
        },
        3,
        "/tmp/dunkingsheep.log",
    )
}

#[test]
fn a_running_daemon_is_used_without_sleeping_or_spawning() {
    let (spawned, slept) = (Cell::new(0), RefCell::new(vec![]));
    assert!(run(vec![Probe::Running], true, true, &spawned, &slept).is_ok());
    assert_eq!((0, 0), (spawned.get(), slept.borrow().len()));
}

#[test]
fn no_spawn_when_the_socket_file_exists_even_if_the_probe_says_absent() {
    // t696: a refused connect on a socket that EXISTS is a saturated daemon, not an
    // absent one. Six strays at once were measured when this started a second `serve`.
    let (spawned, slept) = (Cell::new(0), RefCell::new(vec![]));
    let error = run(vec![Probe::Absent], true, true, &spawned, &slept).unwrap_err();
    assert!(matches!(error, ConnectError::Busy(_)), "{error}");
    assert_eq!(0, spawned.get(), "a busy daemon is never started");
    assert_eq!(BUSY_RETRY_DELAYS.to_vec(), *slept.borrow());
}

#[test]
fn a_busy_daemon_is_retried_with_the_bounded_backoff_then_refused() {
    let (spawned, slept) = (Cell::new(0), RefCell::new(vec![]));
    let error = run(vec![Probe::Busy], false, true, &spawned, &slept).unwrap_err();
    assert_eq!(0, spawned.get());
    assert!(error
        .to_string()
        .contains("Do NOT restart it or start a second one"));
    assert!(error.to_string().contains(SOCKET));
    assert_eq!(3, slept.borrow().len());
}

#[test]
fn a_busy_daemon_that_recovers_inside_the_backoff_is_used() {
    let (spawned, slept) = (Cell::new(0), RefCell::new(vec![]));
    let answers = vec![Probe::Busy, Probe::Busy, Probe::Running];
    assert!(run(answers, false, true, &spawned, &slept).is_ok());
    assert_eq!(0, spawned.get());
    assert_eq!(2, slept.borrow().len());
}

#[test]
fn only_a_truly_absent_daemon_is_started() {
    let (spawned, slept) = (Cell::new(0), RefCell::new(vec![]));
    let answers = vec![Probe::Absent, Probe::Absent, Probe::Running];
    assert!(run(answers, false, true, &spawned, &slept).is_ok());
    assert_eq!(1, spawned.get());
}

#[test]
fn start_false_reports_no_daemon_and_never_spawns() {
    let (spawned, slept) = (Cell::new(0), RefCell::new(vec![]));
    let error = run(vec![Probe::Absent], false, false, &spawned, &slept).unwrap_err();
    assert_eq!(
        ConnectError::Unavailable(format!("no dunkingsheep daemon on {SOCKET}")),
        error
    );
    assert_eq!(0, spawned.get());
}

#[test]
fn a_started_daemon_that_never_answers_is_unavailable_or_busy_by_the_last_probe() {
    let (spawned, slept) = (Cell::new(0), RefCell::new(vec![]));
    let error = run(vec![Probe::Absent], false, true, &spawned, &slept).unwrap_err();
    assert!(
        error.to_string().starts_with("daemon did not come up on"),
        "{error}"
    );
    let busy = run(
        vec![Probe::Absent, Probe::Busy],
        false,
        true,
        &spawned,
        &slept,
    )
    .unwrap_err();
    assert!(matches!(busy, ConnectError::Busy(_)));
    assert!(busy
        .to_string()
        .contains("it is alive, so do NOT start another one"));
}

#[test]
fn the_service_owns_only_the_production_socket() {
    assert!(
        !service_owns_socket(false, true, || true),
        "a private socket is never owned"
    );
    assert!(
        service_owns_socket(true, true, || false),
        "the socket file exists"
    );
    assert!(
        service_owns_socket(true, false, || true),
        "the unit is active"
    );
    assert!(!service_owns_socket(true, false, || false));
    // the unit is only asked when the file does not already answer it
    let asked = Cell::new(false);
    assert!(service_owns_socket(true, true, || {
        asked.set(true);
        false
    }));
    assert!(!asked.get());
}

#[test]
fn spawning_on_the_production_socket_is_owner_only() {
    let refused = check_spawn_allowed(true, None).unwrap_err();
    assert!(refused
        .to_string()
        .contains("refusing to spawn a daemon on the production socket"));
    assert!(refused.to_string().contains("DUNKINGSHEEP_OWNER_START=1"));
    assert!(check_spawn_allowed(true, Some("0")).is_err());
    assert!(check_spawn_allowed(true, Some("1")).is_ok());
    assert!(
        check_spawn_allowed(false, None).is_ok(),
        "a private socket needs no owner flag"
    );
}

// ---- identity tests, ported from tests/test_dunk_identity_tty.py (T41 slice 2) -------------

use std::collections::HashMap;

fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |name: &str| map.get(name).cloned()
}

fn lookup(pane: Option<&str>, note: &str) -> impl Fn() -> PaneAnswer {
    let (pane, note) = (pane.map(String::from), note.to_string());
    move || (pane.clone(), note.clone())
}

fn no_detection() -> Option<String> {
    None
}

fn resolve(
    env: &[(&str, &str)],
    pane: Option<&str>,
    note: &str,
    allow: bool,
) -> Result<(Option<String>, String), DunkError> {
    resolve_identity(
        None,
        &env_of(env),
        &lookup(pane, note),
        &no_detection,
        allow,
    )
}

#[test]
fn a_stripped_env_resolves_by_the_terminal() {
    let (pane, note) = resolve(&[], Some("%17"), "resolved by /dev/pts/66", false).unwrap();
    assert_eq!(Some("%17".to_string()), pane);
    assert!(note.contains("terminal"), "{note}");
}

#[test]
fn a_forged_claim_that_disagrees_with_the_terminal_is_refused() {
    let error = resolve(
        &[("TMUX_PANE", "%9")],
        Some("%4"),
        "resolved by /dev/pts/58",
        false,
    )
    .unwrap_err();
    assert!(matches!(error, DunkError::Identity(_)));
    assert!(
        error
            .to_string()
            .starts_with("identity refused: TMUX_PANE/HERDR_PANE_ID says"),
        "{error}"
    );
}

#[test]
fn an_agreeing_claim_is_accepted_and_said_so() {
    let (pane, note) = resolve(
        &[("TMUX_PANE", "%4")],
        Some("%4"),
        "resolved by /dev/pts/58",
        false,
    )
    .unwrap();
    assert_eq!(Some("%4".to_string()), pane);
    assert!(note.contains("confirmed"), "{note}");
}

#[test]
fn an_ambiguous_terminal_is_refused_not_guessed() {
    let error = resolve(&[], None, "/dev/pts/70 is claimed by 2 panes", false).unwrap_err();
    assert!(error.to_string().contains("identity refused"), "{error}");
}

#[test]
fn a_systemd_service_is_anonymous_not_refused() {
    // A unit has no pane and no pty. Raising there took every unit down (VP i24050).
    let (pane, note) = resolve(&[], None, "no controlling terminal", false).unwrap();
    assert_eq!(None, pane);
    assert!(note.contains("anonymous"), "{note}");
}

#[test]
fn a_terminal_with_no_pane_is_refused_unless_unresolved_is_allowed() {
    let refused = resolve(&[], None, "no tmux pane owns /dev/pts/70", false).unwrap_err();
    assert!(refused.to_string().contains("identity refused"));
    let (pane, note) = resolve(&[], None, "no tmux pane owns /dev/pts/70", true).unwrap();
    assert_eq!(None, pane);
    assert!(note.contains("anonymous"), "{note}");
    assert!(
        note.contains("/dev/pts/70"),
        "the note must name the terminal it came from"
    );
}

#[test]
fn a_lying_claim_is_still_refused_even_when_unresolved_is_allowed() {
    let error = resolve(
        &[("TMUX_PANE", "%9")],
        Some("%4"),
        "resolved by /dev/pts/66",
        true,
    )
    .unwrap_err();
    assert!(error.to_string().contains("identity refused"));
}

#[test]
fn an_unreadable_claim_with_no_terminal_keeps_the_claim() {
    let (pane, note) = resolve(
        &[("TMUX_PANE", "%5")],
        None,
        "no controlling terminal",
        false,
    )
    .unwrap();
    assert_eq!(Some("%5".to_string()), pane);
    assert!(note.contains("env only"), "{note}");
}

#[test]
fn the_tty_fallback_confers_no_capability() {
    let (_, note) = resolve(&[], Some("%17"), "resolved by /dev/pts/66", false).unwrap();
    assert!(
        !note.contains("superuser") && !note.contains("drains"),
        "{note}"
    );
}

#[test]
fn an_explicit_pane_wins() {
    let (pane, note) = resolve_identity(
        Some("%2"),
        &env_of(&[("TMUX_PANE", "%9")]),
        &lookup(Some("%4"), "x"),
        &no_detection,
        false,
    )
    .unwrap();
    assert_eq!(
        (Some("%2".to_string()), "explicit".to_string()),
        (pane, note)
    );
}

#[test]
fn the_backend_detection_is_used_only_when_there_is_no_terminal_and_no_claim() {
    let found = || Some("%33".to_string());
    let (pane, note) = resolve_identity(
        None,
        &env_of(&[]),
        &lookup(None, "no controlling terminal"),
        &found,
        false,
    )
    .unwrap();
    assert_eq!(Some("%33".to_string()), pane);
    assert!(note.contains("detected by the backend"));
    // VERIFY_SELF asks the backend first and its answer beats the environment
    let (pane, note) = resolve_identity(
        None,
        &env_of(&[("DUNKINGSHEEP_VERIFY_SELF", "1"), ("TMUX_PANE", "%9")]),
        &lookup(Some("%9"), "x"),
        &found,
        false,
    )
    .unwrap();
    assert_eq!(
        (Some("%33".to_string()), true),
        (pane, note.contains("VERIFY_SELF"))
    );
}

#[test]
fn tmux_pane_beats_herdr_pane_id_and_tty_check_can_be_turned_off() {
    assert_eq!(
        Some("%4".to_string()),
        exported_pane_id(&env_of(&[("TMUX_PANE", "%4"), ("HERDR_PANE_ID", "w9:pH")]))
    );
    assert_eq!(
        Some("w9:pH".to_string()),
        exported_pane_id(&env_of(&[("HERDR_PANE_ID", "w9:pH")]))
    );
    assert_eq!(None, exported_pane_id(&env_of(&[])));
    let (pane, note) = resolve(
        &[("DUNKINGSHEEP_TTY_IDENTITY", "0"), ("TMUX_PANE", "%9")],
        Some("%4"),
        "x",
        false,
    )
    .unwrap();
    assert_eq!(
        (
            Some("%9".to_string()),
            "env only (tty check off)".to_string()
        ),
        (pane, note)
    );
}

#[test]
fn pane_for_tty_refuses_zero_and_many_matches() {
    let listing = "%1 /dev/pts/5\n%2 /dev/pts/6\n%3 /dev/pts/6\nmalformed\n";
    let ok = || Ok(listing.to_string());
    assert_eq!(
        (Some("%1".to_string()), "resolved by /dev/pts/5".to_string()),
        pane_for_tty(Some("/dev/pts/5"), &ok)
    );
    let (none, why) = pane_for_tty(Some("/dev/pts/9"), &ok);
    assert_eq!(
        (None, "no tmux pane owns /dev/pts/9".to_string()),
        (none, why)
    );
    let (none, why) = pane_for_tty(Some("/dev/pts/6"), &ok);
    assert_eq!(None, none);
    assert_eq!("/dev/pts/6 is claimed by 2 panes (%2, %3)", why);
    let (none, why) = pane_for_tty(None, &ok);
    assert_eq!(
        (
            None,
            "no controlling terminal found in this process chain".to_string()
        ),
        (none, why)
    );
    let broken = || Err("could not ask tmux for panes: boom".to_string());
    assert_eq!(
        Some("could not ask tmux for panes: boom".to_string()),
        Some(pane_for_tty(Some("/dev/pts/5"), &broken).1)
    );
}

#[test]
fn the_controlling_tty_walks_up_the_parents_to_the_agent_that_owns_the_pty() {
    // pid 30 (the command runner) has no tty (tty_nr 0, parent 20); pid 20 (the agent) owns
    // /dev/pts/58: tty_nr = major 136 << 8 | minor 58.
    let pts58 = (136u64 << 8) | 58;
    let stat = move |pid: u32| match pid {
        30 => Some("30 (sh) S 20 30 30 0 -1 0".to_string()),
        20 => Some(format!("20 (claude code) S 10 20 20 {pts58} 20 0")),
        _ => None,
    };
    assert_eq!(
        Some("/dev/pts/58".to_string()),
        controlling_tty(30, 12, &stat)
    );
    // a non-pty terminal (major 4 = a console) is not a pane
    let console = |_: u32| Some(format!("5 (bash) S 1 5 5 {} 5 0", (4u64 << 8) | 1));
    assert_eq!(None, controlling_tty(5, 12, &console));
    // reaching init, or an unreadable stat, ends the walk
    let orphan = |_: u32| Some("7 (x) S 1 7 7 0 -1 0".to_string());
    assert_eq!(None, controlling_tty(7, 12, &orphan));
    assert_eq!(None, controlling_tty(99, 12, &stat));
    // a command name with a ')' and spaces does not break the field split
    let tricky = move |_: u32| Some(format!("8 (a) b (c) S 1 8 8 {pts58} 8 0"));
    assert_eq!(
        Some("/dev/pts/58".to_string()),
        controlling_tty(8, 12, &tricky)
    );
}

#[test]
fn read_only_verbs_run_from_a_terminal_that_owns_no_pane() {
    for verb in ["status", "whoami", "task_read", "poll", "health"] {
        assert_eq!(
            None,
            identity_refusal_for(verb, None, false, Some("anonymous: no pane")),
            "{verb}"
        );
    }
}

#[test]
fn a_write_verb_is_refused_and_names_the_labelled_token_exit() {
    let refusal = identity_refusal_for("task_next", None, false, Some("anonymous: no pane"))
        .expect("refused");
    assert!(
        refusal.contains("token issue --role owner --label"),
        "{refusal}"
    );
    assert!(refusal.to_lowercase().contains("read-only"), "{refusal}");
    assert!(
        refusal.starts_with("identity refused: task_next needs a lane"),
        "{refusal}"
    );
    assert!(refusal.contains("(anonymous: no pane)"));
}

#[test]
fn a_token_or_a_pane_is_enough_for_a_write() {
    assert_eq!(None, identity_refusal_for("task_next", None, true, None));
    assert_eq!(
        None,
        identity_refusal_for("task_next", Some("%17"), false, None)
    );
}

#[test]
fn the_deadline_defaults_to_45_seconds_and_the_environment_can_override_it() {
    assert_eq!(Duration::from_secs(45), default_timeout(&env_of(&[])));
    assert_eq!(
        Duration::from_secs(12),
        default_timeout(&env_of(&[("DUNKINGSHEEP_CLIENT_TIMEOUT", "12")]))
    );
    assert_eq!(
        Duration::from_secs(45),
        default_timeout(&env_of(&[("DUNKINGSHEEP_CLIENT_TIMEOUT", "")]))
    );
    assert_eq!(
        Duration::from_secs(45),
        default_timeout(&env_of(&[("DUNKINGSHEEP_CLIENT_TIMEOUT", "soon")]))
    );
    assert!(
        default_timeout(&env_of(&[])) >= Duration::from_secs(30),
        "a 15 s cap fails when p90 is 24.5 s"
    );
}

#[test]
fn the_client_rule_is_the_daemons_own_read_only_list() {
    // If these drifted, a verb could be writable for the daemon and read-only here. The rule
    // reads `roles::READ_ONLY` directly, so every verb on that list passes with no pane and no
    // token, and a verb that is not on it is refused.
    for verb in crate::roles::READ_ONLY {
        assert_eq!(
            None,
            identity_refusal_for(verb, None, false, None),
            "{verb}"
        );
    }
    for verb in [
        "task_next",
        "task_complete",
        "send_text",
        "start_agent",
        "shutdown",
    ] {
        assert!(
            !crate::roles::READ_ONLY.contains(&verb),
            "{verb} must not be read-only"
        );
        assert!(
            identity_refusal_for(verb, None, false, None).is_some(),
            "{verb}"
        );
    }
}

// ---- FlockClient tests (T41 slice 3) -------------------------------------------------------

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::rc::Rc;

type Reply = Result<Option<String>, std::io::Error>;

#[derive(Clone, Default)]
struct Fake {
    sent: Rc<RefCell<Vec<String>>>,
    script: Rc<RefCell<VecDeque<Reply>>>,
}

impl Fake {
    fn replying(replies: Vec<Reply>) -> Self {
        Fake {
            sent: Rc::default(),
            script: Rc::new(RefCell::new(replies.into())),
        }
    }
    fn always_timing_out() -> Self {
        Fake::replying((0..8).map(|_| Err(timeout())).collect())
    }
    fn cmds(&self) -> Vec<String> {
        self.sent
            .borrow()
            .iter()
            .map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["cmd"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }
}

impl Transport for Fake {
    fn exchange(&self, line: &str, _timeout: Duration) -> Reply {
        self.sent.borrow_mut().push(line.to_string());
        self.script.borrow_mut().pop_front().unwrap_or(Ok(None))
    }
}

fn timeout() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::TimedOut, "no answer")
}

fn ok(result: Value) -> Reply {
    Ok(Some(
        json!({"ok": true, "result": result}).to_string() + "\n",
    ))
}

fn lane(fake: &Fake) -> FlockClient<Fake> {
    FlockClient::new(
        fake.clone(),
        Some("%1".into()),
        "explicit".into(),
        Duration::from_secs(1),
        None,
    )
}

#[test]
fn a_read_is_retried_once_and_a_write_is_not() {
    let fake = Fake::always_timing_out();
    let mut client = lane(&fake);
    assert!(matches!(client.call("status", Map::new()), Err(ClientError::Io(e)) if is_timeout(&e)));
    assert_eq!(
        vec!["status", "status"],
        fake.cmds(),
        "a read gets one retry"
    );
    let fake = Fake::always_timing_out();
    let mut client = lane(&fake);
    assert!(client.call("task_next", Map::new()).is_err());
    assert_eq!(vec!["task_next"], fake.cmds(), "a write is never retried");
}

#[test]
fn the_retry_decision_is_idempotency_not_the_role_allowlist() {
    // t705: turn_start/turn_end are in READ_ONLY (a pane may CALL them) but are WRITES, and
    // lane_register / superuser_recover / dispatch are bootstrap writes. None may be resent.
    for verb in [
        "turn_start",
        "turn_end",
        "lane_register",
        "superuser_recover",
        "dispatch",
    ] {
        assert!(
            !IDEMPOTENT_READS.contains(&verb),
            "{verb} must not be retryable"
        );
        let fake = Fake::always_timing_out();
        let _ = lane(&fake).call(verb, Map::new());
        assert_eq!(1, fake.cmds().len(), "{verb} was sent more than once");
    }
    let fake = Fake::always_timing_out();
    let _ = lane(&fake).call("status", Map::new());
    assert_eq!(2, fake.cmds().len());
}

#[test]
fn every_retryable_verb_is_a_read_the_daemon_also_allows_without_a_lane() {
    let mut seen = std::collections::HashSet::new();
    for verb in IDEMPOTENT_READS {
        assert!(seen.insert(verb), "{verb} is listed twice");
        assert!(
            crate::roles::READ_ONLY.contains(&verb),
            "{verb} is retried but not read-only"
        );
    }
}

#[test]
fn a_read_that_times_out_then_answers_succeeds_on_the_retry() {
    let fake = Fake::replying(vec![Err(timeout()), ok(json!({"lanes": 3}))]);
    let value = lane(&fake).call("list_panes", Map::new()).expect("retried");
    assert_eq!(json!({"lanes": 3}), value);
    assert_eq!(2, fake.cmds().len());
}

#[test]
fn the_request_carries_an_id_the_pane_and_the_token_only_when_there_is_one() {
    let fake = Fake::replying(vec![ok(json!(null)), ok(json!(null))]);
    let mut client = lane(&fake);
    let mut args = Map::new();
    args.insert("id".into(), json!("t7"));
    client.call("task_read", args).unwrap();
    client.call("status", Map::new()).unwrap();
    let first: Value = serde_json::from_str(&fake.sent.borrow()[0]).unwrap();
    let second: Value = serde_json::from_str(&fake.sent.borrow()[1]).unwrap();
    assert_eq!(
        (1, "task_read", "%1"),
        (
            first["id"].as_u64().unwrap(),
            first["cmd"].as_str().unwrap(),
            first["self_pane_id"].as_str().unwrap()
        )
    );
    assert_eq!(json!({"id": "t7"}), first["args"]);
    assert_eq!(2, second["id"], "the id increments per call");
    assert!(first.get("service_token").is_none(), "no token, no field");
    let fake = Fake::replying(vec![ok(json!(null))]);
    let mut with_token = FlockClient::new(
        fake.clone(),
        None,
        "anonymous".into(),
        Duration::from_secs(1),
        Some("tok-1".into()),
    );
    with_token.call("task_next", Map::new()).unwrap();
    let sent: Value = serde_json::from_str(&fake.sent.borrow()[0]).unwrap();
    assert_eq!(json!("tok-1"), sent["service_token"]);
    assert_eq!(Value::Null, sent["self_pane_id"]);
}

#[test]
fn a_system_sender_never_carries_a_pane() {
    let fake = Fake::replying(vec![ok(json!(null))]);
    let mut args = Map::new();
    args.insert("system_sender".into(), json!(true));
    lane(&fake).call("status", args).unwrap();
    let sent: Value = serde_json::from_str(&fake.sent.borrow()[0]).unwrap();
    assert_eq!(
        Value::Null,
        sent["self_pane_id"],
        "the daemon refuses a system sender that claims a pane"
    );
}

#[test]
fn a_write_with_no_pane_and_no_token_is_refused_before_anything_is_sent() {
    let fake = Fake::default();
    let mut client = FlockClient::new(
        fake.clone(),
        None,
        "anonymous: no pane".into(),
        Duration::from_secs(1),
        None,
    );
    let error = client.call("task_next", Map::new()).unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("identity refused: task_next needs a lane"),
        "{error}"
    );
    assert!(
        fake.sent.borrow().is_empty(),
        "nothing may reach the daemon"
    );
    assert!(FlockClient::new(
        Fake::replying(vec![ok(json!(1))]),
        None,
        "anonymous".into(),
        Duration::from_secs(1),
        None
    )
    .call("status", Map::new())
    .is_ok());
}

#[test]
fn the_daemons_answer_is_decoded_exactly() {
    let refused = Fake::replying(vec![Ok(Some(
        json!({"ok": false, "error": "no such task"}).to_string(),
    ))]);
    assert_eq!(
        "no such task",
        lane(&refused)
            .call("task_read", Map::new())
            .unwrap_err()
            .to_string()
    );
    let silent = Fake::replying(vec![Ok(Some(json!({"ok": false}).to_string()))]);
    assert_eq!(
        "unknown error",
        lane(&silent)
            .call("task_read", Map::new())
            .unwrap_err()
            .to_string()
    );
    let closed = Fake::replying(vec![Ok(None)]);
    let error = lane(&closed).call("task_read", Map::new()).unwrap_err();
    assert!(
        matches!(&error, ClientError::Unavailable(t) if t == "daemon closed the connection without replying")
    );
    let garbage = Fake::replying(vec![Ok(Some("not json\n".into()))]);
    assert!(matches!(
        lane(&garbage).call("task_read", Map::new()),
        Err(ClientError::Protocol(_))
    ));
    let empty_result = Fake::replying(vec![Ok(Some(json!({"ok": true}).to_string()))]);
    assert_eq!(
        Value::Null,
        lane(&empty_result).call("task_read", Map::new()).unwrap()
    );
}

#[test]
fn the_probe_classifies_running_busy_and_absent() {
    let probe = |reply: Reply| lane(&Fake::replying(vec![reply])).probe();
    assert_eq!(Probe::Running, probe(ok(json!({}))));
    // a REFUSED command still proves the daemon answered
    assert_eq!(
        Probe::Running,
        probe(Ok(Some(json!({"ok": false, "error": "no"}).to_string())))
    );
    assert_eq!(
        Probe::Busy,
        probe(Ok(None)),
        "closed without a reply: alive but silent"
    );
    assert_eq!(Probe::Busy, probe(Ok(Some("garbage".into()))));
    for code in [2, 111, 88, 99] {
        assert_eq!(
            Probe::Absent,
            probe(Err(std::io::Error::from_raw_os_error(code))),
            "errno {code} is nothing listening"
        );
    }
    assert_eq!(
        Probe::Busy,
        probe(Err(std::io::Error::from_raw_os_error(104))),
        "ECONNRESET is stuck, not absent"
    );
    let both = Fake::always_timing_out();
    assert_eq!(Probe::Busy, lane(&both).probe(), "a timeout is busy");
}

#[test]
fn the_service_token_comes_from_the_environment_then_the_file() {
    let no_file = |_: &str| Err(std::io::Error::from(std::io::ErrorKind::NotFound));
    assert_eq!(
        Some("direct".to_string()),
        service_token(
            &env_of(&[("DUNKINGSHEEP_SERVICE_TOKEN", " direct ")]),
            &no_file
        )
    );
    let file = |path: &str| {
        assert_eq!("/tok", path);
        Ok("  from-file\n".to_string())
    };
    assert_eq!(
        Some("from-file".to_string()),
        service_token(
            &env_of(&[("DUNKINGSHEEP_SERVICE_TOKEN_FILE", "/tok")]),
            &file
        )
    );
    assert_eq!(
        None,
        service_token(
            &env_of(&[("DUNKINGSHEEP_SERVICE_TOKEN_FILE", "/tok")]),
            &no_file
        ),
        "unreadable is no token"
    );
    let blank = |_: &str| Ok("\n".to_string());
    assert_eq!(
        None,
        service_token(
            &env_of(&[("DUNKINGSHEEP_SERVICE_TOKEN_FILE", "/tok")]),
            &blank
        )
    );
    assert_eq!(None, service_token(&env_of(&[]), &file));
    assert_eq!(
        Some("direct".to_string()),
        service_token(
            &env_of(&[
                ("DUNKINGSHEEP_SERVICE_TOKEN", "direct"),
                ("DUNKINGSHEEP_SERVICE_TOKEN_FILE", "/tok")
            ]),
            &file
        ),
        "the variable wins"
    );
}

// A real socket: the line protocol, a missing socket, a silent close and a silent hang.

fn serve_once(
    path: &std::path::Path,
    behaviour: fn(&mut std::os::unix::net::UnixStream, String),
) -> std::thread::JoinHandle<()> {
    let listener = UnixListener::bind(path).expect("bind");
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut line = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        behaviour(&mut stream, line);
    })
}

fn real_client(path: &std::path::Path, timeout_ms: u64) -> FlockClient<UnixTransport> {
    FlockClient::new(
        UnixTransport {
            path: path.to_path_buf(),
        },
        Some("%1".into()),
        "explicit".into(),
        Duration::from_millis(timeout_ms),
        None,
    )
}

#[test]
fn a_real_round_trip_over_a_unix_socket() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("d.sock");
    let server = serve_once(&path, |stream, line| {
        let request: Value = serde_json::from_str(&line).unwrap();
        assert_eq!("task_read", request["cmd"]);
        assert_eq!("%1", request["self_pane_id"]);
        let reply = json!({"ok": true, "result": {"echo": request["args"]["id"]}});
        stream.write_all(format!("{reply}\n").as_bytes()).unwrap();
    });
    let mut args = Map::new();
    args.insert("id".into(), json!("t42"));
    let value = real_client(&path, 2000)
        .call("task_read", args)
        .expect("round trip");
    server.join().unwrap();
    assert_eq!(json!({"echo": "t42"}), value);
}

#[test]
fn a_missing_socket_probes_absent_and_a_silent_close_probes_busy() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        Probe::Absent,
        real_client(&dir.path().join("none.sock"), 500).probe()
    );
    let path = dir.path().join("closer.sock");
    let server = serve_once(&path, |_stream, _line| { /* accept, read, drop: no reply */
    });
    assert_eq!(Probe::Busy, real_client(&path, 2000).probe());
    server.join().unwrap();
}

#[test]
fn a_listener_that_never_answers_times_out_and_probes_busy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hang.sock");
    let server = serve_once(&path, |_stream, _line| {
        std::thread::sleep(Duration::from_millis(600))
    });
    let started = std::time::Instant::now();
    let mut client = real_client(&path, 150);
    assert_eq!(Probe::Busy, client.probe());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the deadline must bound the wait"
    );
    server.join().unwrap();
}
