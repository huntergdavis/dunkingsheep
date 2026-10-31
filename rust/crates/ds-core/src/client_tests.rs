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
