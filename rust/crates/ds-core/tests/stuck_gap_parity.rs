//! t3354 (t820 proof C, rows the audit left GAP or PARTIAL): state-machine cases from
//! `tests/test_dunk_stuck_recovery.py`, asserted through the public `stuck` API.
//!
//! Each test names the Python case it ports. Times are monotonic and use the
//! default 120 s threshold, the same clock the other `stuck_parity` cases use.

use ds_core::stuck::{Config, Decision, Observation, Tracker};

const T0: f64 = 1_000_000.0;
const OWNED: &str = "dunkingsheep send t42 --text 'continue'";

fn observed() -> Observation {
    let mut observation = Observation::new("w1:p1", Some("m1"), Some(OWNED), "ready");
    observation.generation = Some("pid:11:start:aa".to_string());
    observation
}

fn decide(tracker: &mut Tracker, observation: &Observation, now: f64) -> Decision {
    tracker.decide(observation, &Config::default(), now)
}

// ObserverBoundaryTests.test_an_unreadable_observation_resets_the_clock (row 47):
// an unknown observation resets the clock, and the first readable tick afterwards
// starts a NEW window. No action may use the clock from before the reset.
#[test]
fn an_unreadable_observation_resets_the_clock_and_needs_a_fresh_window() {
    let mut tracker = Tracker::default();
    assert_eq!("wait", decide(&mut tracker, &observed(), T0).action);

    let mut unknown = observed();
    unknown.status = "unknown".to_string();
    assert_eq!("reset", decide(&mut tracker, &unknown, T0 + 60.0).action);

    // Readable again at the old threshold: a first sighting, not a recovery.
    let restarted = decide(&mut tracker, &observed(), T0 + 120.0);
    assert_eq!("wait", restarted.action);
    assert_eq!(0, restarted.attempts);

    // One full window after that first sighting is required before the attempt.
    let acted = decide(&mut tracker, &observed(), T0 + 240.0);
    assert_eq!("recover", acted.action);
    assert_eq!(1, acted.attempts);
}
