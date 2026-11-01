//! Ported from tests/test_dunk_state_session.py (SessionFromProcess, LiveFileOutranksAnEnvironment-
//! Claim) and tests/test_dunk_state_capture_cost.py (StalePidBindingTests, CheckToReadRaceTests,
//! LaterReadBindingTests), same names and order. The Python monkey-patches module functions; here
//! each test builds a [`Fake`] reader. NOT in this slice: `ParsePanesKeepsPanePid` (terminal
//! parsing), `test_capture_never_reads_env_of_a_recycled_pid` and `OneProcWalkPerCapture`
//! (`capture`, slice 6c).

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use serde_json::json;

use super::proc::{
    descendants, read_bound, still_the_same_process, Kids, LiveProc, ProcReader, Starts,
};
use super::session::session_of;

type Env = BTreeMap<String, String>;

struct Fake {
    ticks: Box<dyn Fn(i64) -> Option<i64>>,
    cmdline: Box<dyn Fn(i64) -> Vec<String>>,
    env: Box<dyn Fn(i64) -> Env>,
    open: Box<dyn Fn(i64) -> Option<String>>,
    proc_start: Box<dyn Fn(i64) -> Option<f64>>,
    active: Box<dyn Fn(Option<&str>, Option<f64>) -> (Option<String>, usize)>,
}

impl Fake {
    fn new() -> Fake {
        Fake {
            ticks: Box::new(|_| None),
            cmdline: Box::new(|_| vec![]),
            env: Box::new(|_| Env::new()),
            open: Box::new(|_| None),
            proc_start: Box::new(|_| Some(1000.0)),
            active: Box::new(|_, _| (None, 0)),
        }
    }
}

impl ProcReader for Fake {
    fn proc_children(&self) -> Kids {
        Kids::new()
    }
    fn proc_snapshot(&self) -> (Kids, Starts) {
        (Kids::new(), Starts::new())
    }
    fn proc_start_ticks(&self, pid: i64) -> Option<i64> {
        (self.ticks)(pid)
    }
    fn cmdline(&self, pid: i64) -> Vec<String> {
        (self.cmdline)(pid)
    }
    fn open_session_file(&self, pid: i64) -> Option<String> {
        (self.open)(pid)
    }
    fn env_of(&self, pid: i64) -> Env {
        (self.env)(pid)
    }
    fn proc_start(&self, pid: i64) -> Option<f64> {
        (self.proc_start)(pid)
    }
    fn active_session_file(
        &self,
        cwd: Option<&str>,
        since: Option<f64>,
    ) -> (Option<String>, usize) {
        (self.active)(cwd, since)
    }
}

fn argv(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

fn env(pairs: &[(&str, &str)]) -> Env {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

fn kids(pairs: &[(i64, &[i64])]) -> Kids {
    pairs.iter().map(|(k, v)| (*k, v.to_vec())).collect()
}

fn starts(pairs: &[(i64, i64)]) -> Starts {
    pairs.iter().copied().collect()
}

fn reason_has(reason: &Option<String>, needle: &str) -> bool {
    reason.as_deref().is_some_and(|r| r.contains(needle))
}

// -- SessionFromProcess ---------------------------------------------------------------------

#[test]
fn test_missing_pid_reports_a_reason_instead_of_a_guess() {
    let (session, reason) = session_of(
        &Fake::new(),
        None,
        Some(&json!({"session": "recorded-id"})),
        None,
        None,
        None,
    );
    assert_eq!(session.as_deref(), Some("recorded-id"));
    assert!(reason_has(&reason, "no pid"), "{reason:?}");
}

#[test]
fn test_shell_pane_says_it_has_no_agent() {
    // A pane running only a shell: no agent process, and it must say so.
    let mut child = std::process::Command::new("sleep")
        .arg("5")
        .spawn()
        .expect("spawn sleep");
    let live = LiveProc::new("/nonexistent-home");
    let (session, reason) = session_of(&live, Some(&json!(child.id())), None, None, None, None);
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(session, None);
    assert!(reason_has(&reason, "no agent process"), "{reason:?}");
}

#[test]
fn test_inherited_session_from_another_workspace_is_reported_not_used() {
    // The real defect: an env that names another workspace's session must not be trusted.
    let mut fake = Fake::new();
    fake.cmdline = Box::new(|_| argv(&["/usr/bin/pi", "--flag"]));
    fake.env = Box::new(|_| {
        env(&[
            ("PI_SESSION_ID", "01a15560-dead-beef"),
            ("PI_SESSION_FILE", "/home/hunter/.pi/agent/sessions/--home-hunter-workspace-vp-of-agentry--/x_01a15560.jsonl"),
        ])
    });
    let (session, reason) = session_of(
        &fake,
        Some(&json!(4242)),
        None,
        Some(&Kids::new()),
        Some("/home/hunter/workspace/chief-of-staff"),
        None,
    );
    assert_eq!(session, None);
    assert!(reason_has(&reason, "inherited claim"), "{reason:?}");
    let (session, reason) = session_of(
        &fake,
        Some(&json!(4242)),
        None,
        Some(&Kids::new()),
        Some("/home/hunter/workspace/vp-of-agentry"),
        None,
    );
    assert_eq!(session.as_deref(), Some("01a15560-dead-beef"));
    assert_eq!(reason, None);
}

// -- LiveFileOutranksAnEnvironmentClaim -----------------------------------------------------

#[test]
fn test_stale_env_claim_is_flagged_and_the_live_file_is_recorded() {
    // Same workspace, different conversation: the env can still be an inherited claim. %1's
    // environment named the 27.9 MB pre-incident session while the file being written for that
    // workspace was a different one, so the live file wins and the claim is flagged.
    let mut fake = Fake::new();
    fake.cmdline = Box::new(|_| argv(&["/usr/bin/pi"]));
    fake.env = Box::new(|_| {
        env(&[
            ("PI_SESSION_ID", "aaaaaaaa-old"),
            (
                "PI_SESSION_FILE",
                "/home/hunter/.pi/agent/sessions/--ws--/file_aaaaaaaa-old.jsonl",
            ),
        ])
    });
    fake.active = Box::new(|_, _| {
        (
            Some("/home/hunter/.pi/agent/sessions/--ws--/file_bbbbbbbb-live.jsonl".into()),
            1,
        )
    });
    let (session, reason) = session_of(
        &fake,
        Some(&json!(4242)),
        None,
        Some(&Kids::new()),
        Some("/ws"),
        None,
    );
    assert_eq!(session.as_deref(), Some("bbbbbbbb-live"));
    assert!(reason_has(&reason, "inherited"), "{reason:?}");
}

// -- StalePidBindingTests -------------------------------------------------------------------

#[test]
fn test_recycled_descendant_is_not_adopted() {
    let kids = kids(&[(1001, &[1101]), (1101, &[1201]), (9000, &[1201])]);
    let snapshot = starts(&[(1001, 1), (1101, 2), (1201, 3)]); // snapshot saw 1201 start=3
    let mut fake = Fake::new();
    // 1201 was recycled: it now belongs to a process that started later.
    let known = snapshot.clone();
    fake.ticks = Box::new(move |pid| {
        if pid == 1201 {
            Some(999)
        } else {
            known.get(&pid).copied()
        }
    });
    fake.cmdline = Box::new(|pid| {
        if pid == 1201 {
            argv(&["pi", "--session", "foreign-session"])
        } else {
            vec![]
        }
    });
    let (session, reason) = session_of(
        &fake,
        Some(&json!(1001)),
        None,
        Some(&kids),
        None,
        Some(&snapshot),
    );
    assert_eq!(session, None, "adopted a recycled pid's session");
    assert!(reason_has(&reason, "pid reuse"), "{reason:?}");
}

#[test]
fn test_a_live_descendant_is_still_adopted() {
    // The control: same shape, process NOT recycled, session is read.
    let kids = kids(&[(1001, &[1101]), (1101, &[1201])]);
    let snapshot = starts(&[(1001, 1), (1101, 2), (1201, 3)]);
    let mut fake = Fake::new();
    let known = snapshot.clone();
    fake.ticks = Box::new(move |pid| known.get(&pid).copied());
    fake.cmdline = Box::new(|pid| {
        if pid == 1201 {
            argv(&["pi", "--session", "our-session"])
        } else {
            vec![]
        }
    });
    let (session, _reason) = session_of(
        &fake,
        Some(&json!(1001)),
        None,
        Some(&kids),
        None,
        Some(&snapshot),
    );
    assert_eq!(session.as_deref(), Some("our-session"));
}

#[test]
fn test_unknown_identity_is_not_the_same_process() {
    // A pid absent from the snapshot, or with a different start, is NOT it. `starts=None` is the
    // documented legacy path; a SUPPLIED but EMPTY snapshot fails closed too (t500 addendum P2).
    let fake = Fake::new(); // an unreadable stat: proc_start_ticks answers None
    assert!(!still_the_same_process(
        &fake,
        4242,
        Some(&starts(&[(4242, 7)]))
    ));
    assert!(!still_the_same_process(
        &fake,
        4242,
        Some(&starts(&[(9999, 1)]))
    ));
    assert!(!still_the_same_process(&fake, 4242, Some(&Starts::new())));
    assert!(still_the_same_process(&fake, 4242, None));
}

// -- CheckToReadRaceTests -------------------------------------------------------------------

#[test]
fn test_a_recycle_inside_the_read_is_rejected() {
    let snapshot = starts(&[(1201, 3)]);
    let calls = Rc::new(Cell::new(0));
    let counter = calls.clone();
    let mut fake = Fake::new();
    // first check: same process; second (post-read): recycled
    fake.ticks = Box::new(move |_| {
        counter.set(counter.get() + 1);
        Some(if counter.get() == 1 { 3 } else { 999 })
    });
    let (value, held) = read_bound(&fake, 1201, Some(&snapshot), |_| {
        env(&[("PI_SESSION_ID", "foreign")])
    });
    assert!(!held);
    assert_eq!(value, None, "used metadata read across a recycled pid");
}

#[test]
fn test_a_stable_process_is_read() {
    let mut fake = Fake::new();
    fake.ticks = Box::new(|_| Some(3));
    let (value, held) = read_bound(&fake, 1201, Some(&starts(&[(1201, 3)])), |_| {
        env(&[("PI_SESSION_ID", "ours")])
    });
    assert!(held);
    assert_eq!(value, Some(env(&[("PI_SESSION_ID", "ours")])));
}

#[test]
fn test_empty_snapshot_does_not_authorize_a_read() {
    let (value, held) = read_bound(&Fake::new(), 1201, Some(&Starts::new()), |_| {
        env(&[("PI_SESSION_ID", "unverified")])
    });
    assert!(!held);
    assert_eq!(value, None);
}

#[test]
fn test_session_of_refuses_when_the_pid_is_recycled_during_the_env_read() {
    let kids = kids(&[(1001, &[1101])]);
    let snapshot = starts(&[(1001, 1), (1101, 2)]);
    let mut fake = Fake::new();
    fake.cmdline = Box::new(|pid| if pid == 1101 { argv(&["pi"]) } else { vec![] });
    fake.env = Box::new(|_| env(&[("PI_SESSION_ID", "foreign-session")]));
    // the argv read is stable, the env read is not
    let sequence = Rc::new(RefCell::new(vec![2, 2, 999, 999, 999, 999]));
    let known = snapshot.clone();
    fake.ticks = Box::new(move |pid| {
        if pid == 1101 {
            let mut seq = sequence.borrow_mut();
            return Some(if seq.is_empty() { 999 } else { seq.remove(0) });
        }
        known.get(&pid).copied()
    });
    let (session, reason) = session_of(
        &fake,
        Some(&json!(1001)),
        None,
        Some(&kids),
        Some("/tmp/ws"),
        Some(&snapshot),
    );
    assert_eq!(
        session, None,
        "persisted a session read across a recycled pid"
    );
    assert!(
        reason_has(&reason, "pid reuse") || reason_has(&reason, "replaced"),
        "{reason:?}"
    );
}

// -- LaterReadBindingTests ------------------------------------------------------------------

fn later_kids() -> Kids {
    kids(&[(1001, &[1101])])
}

fn later_starts() -> Starts {
    starts(&[(1001, 1), (1101, 2)])
}

fn stable_ticks() -> Box<dyn Fn(i64) -> Option<i64>> {
    Box::new(|pid| later_starts().get(&pid).copied())
}

/// `_session_with`: argv defaults to `["pi"]`, env to `{}`, no open file, the given active file.
fn session_with(fake: Fake) -> (Option<String>, Option<String>) {
    session_of(
        &fake,
        Some(&json!(1001)),
        None,
        Some(&later_kids()),
        Some("/tmp/ws"),
        Some(&later_starts()),
    )
}

fn base_fake(ticks: Box<dyn Fn(i64) -> Option<i64>>) -> Fake {
    let mut fake = Fake::new();
    fake.cmdline = Box::new(|_| argv(&["pi"]));
    fake.ticks = ticks;
    fake
}

#[test]
fn test_identity_change_during_the_start_read_refuses() {
    // The start time IS the cutoff: a foreign one adopts a foreign file.
    let reads = Rc::new(Cell::new(0));
    let (for_ticks, for_start) = (reads.clone(), reads.clone());
    let mut fake = base_fake(Box::new(move |pid| {
        Some(if for_ticks.get() == 0 {
            if pid == 1101 {
                2
            } else {
                1
            }
        } else {
            999
        })
    }));
    fake.active = Box::new(|_, _| (Some("/x/foreign-eeee.jsonl".into()), 1));
    // the identity holds before the read and moves during it
    fake.proc_start = Box::new(move |_| {
        for_start.set(for_start.get() + 1);
        Some(if for_start.get() == 1 { 1000.0 } else { 2000.0 })
    });
    let (session, reason) = session_with(fake);
    assert_ne!(
        session.as_deref(),
        Some("foreign-eeee"),
        "adopted the start-dependent fallback after a changed start read"
    );
    assert!(reason_has(&reason, "pid reuse"), "{reason:?}");
}

#[test]
fn test_identity_change_during_the_active_file_read_refuses() {
    let reads = Rc::new(Cell::new(0));
    let (for_ticks, for_active) = (reads.clone(), reads.clone());
    // Correct start ticks per pid, until the read under test starts; then ONLY 1101 is recycled.
    // (Reviewer t500 caveat: a constant initial value would make the ROOT look replaced on its
    // first check, so the test would never reach the read it is about.)
    let mut fake = base_fake(Box::new(move |pid| {
        if for_ticks.get() > 0 && pid == 1101 {
            return Some(999);
        }
        later_starts().get(&pid).copied()
    }));
    fake.active = Box::new(move |_, _| {
        for_active.set(for_active.get() + 1);
        (Some("/x/foreign-ffff.jsonl".into()), 1)
    });
    let (session, reason) = session_with(fake);
    assert_ne!(
        session.as_deref(),
        Some("foreign-ffff"),
        "adopted a foreign active session file"
    );
    assert!(reason_has(&reason, "pid reuse"), "{reason:?}");
}

#[test]
fn test_a_stable_derivation_still_uses_the_fallback() {
    let mut fake = base_fake(stable_ticks());
    fake.active = Box::new(|_, _| (Some("/x/active-cccc.jsonl".into()), 1));
    assert_eq!(session_with(fake).0.as_deref(), Some("active-cccc"));
}

#[test]
fn test_a_stable_process_reaches_each_source() {
    // Control half: with no recycle, every source still answers.
    let mut fake = base_fake(stable_ticks());
    fake.env = Box::new(|_| env(&[("PI_SESSION_ID", "foreign-session")]));
    fake.open = Box::new(|_| Some("/x/ours-aaaa.jsonl".into()));
    assert_eq!(session_with(fake).0.as_deref(), Some("foreign-session"));

    let mut fake = base_fake(stable_ticks());
    fake.open = Box::new(|_| Some("/x/foreign-bbbb.jsonl".into()));
    assert_eq!(session_with(fake).0.as_deref(), Some("foreign-bbbb"));

    let mut fake = base_fake(stable_ticks());
    fake.active = Box::new(|_, _| (Some("/x/active-cccc.jsonl".into()), 1));
    assert_eq!(session_with(fake).0.as_deref(), Some("active-cccc"));

    let mut fake = base_fake(stable_ticks());
    fake.cmdline = Box::new(|_| argv(&["pi", "--session", "argv-dddd"]));
    assert_eq!(session_with(fake).0.as_deref(), Some("argv-dddd"));
}

// -- beyond the Python tests ----------------------------------------------------------------

#[test]
fn test_descendants_follow_the_pythons_stack_order() {
    let tree = kids(&[(1, &[2, 3]), (2, &[4]), (3, &[5])]);
    assert_eq!(descendants(1, &tree), [2, 3, 5, 4]);
}

#[test]
fn test_session_id_from_path_and_slug() {
    use super::proc::{cwd_slug, session_id_from_path};
    assert_eq!(
        session_id_from_path("/a/2026-10-24T18-04-46-250Z_01a16f16-x.jsonl"),
        "01a16f16-x"
    );
    assert_eq!(session_id_from_path("/a/abcd-1234.jsonl"), "abcd-1234");
    assert_eq!(session_id_from_path("noext"), "noext");
    assert_eq!(
        cwd_slug(Some("/home/x/ws/")).as_deref(),
        Some("--home-x-ws--")
    );
    assert_eq!(cwd_slug(None), None);
}

#[test]
fn test_resume_subcommand_and_session_path_forms() {
    let mut fake = base_fake(stable_ticks());
    fake.cmdline = Box::new(|_| argv(&["codex", "resume", "uuid-1"]));
    assert_eq!(session_with(fake).0.as_deref(), Some("uuid-1"));
    let mut fake = base_fake(stable_ticks());
    fake.cmdline = Box::new(|_| argv(&["pi", "--session=/s/2026_abc-9.jsonl"]));
    assert_eq!(session_with(fake).0.as_deref(), Some("abc-9"));
}
