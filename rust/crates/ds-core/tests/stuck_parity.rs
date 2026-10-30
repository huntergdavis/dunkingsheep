//! T17: the stuck-message state machine, ported 1:1 from `tests/test_dunk_stuck.py`.
//!
//! Every case is a sentence from the owner's order turned into an assertion: before
//! the threshold nothing happens, at the threshold exactly ONE guarded attempt, the
//! clock resets on any edit, every uncertain state fails closed, and a restart cannot
//! produce a second Enter.

use ds_core::stuck::{self, Config, Decision, Observation, RawConfig, Tracker};

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

#[test]
fn the_threshold_defaults_to_the_cos_bound_and_is_configurable() {
    assert_eq!(120.0, Config::default().threshold_s);
    let raw = RawConfig { threshold_s: Some(45.0), ..RawConfig::default() };
    assert_eq!(45.0, Config::from_raw(&raw).unwrap().threshold_s);
}

#[test]
fn nonsense_config_is_refused_not_coerced() {
    let interval_gt_threshold = RawConfig { threshold_s: Some(10.0),
                                            observation_interval_s: Some(20.0),
                                            ..RawConfig::default() };
    assert!(Config::from_raw(&interval_gt_threshold).is_err());
    let zero_attempts = RawConfig { max_attempts: Some(0), ..RawConfig::default() };
    assert!(Config::from_raw(&zero_attempts).is_err());
    let negative = RawConfig { threshold_s: Some(-1.0), ..RawConfig::default() };
    assert!(Config::from_raw(&negative).is_err());
}

#[test]
fn the_composer_wrapping_is_not_content() {
    assert_eq!(stuck::collapse("a  b\n c "), stuck::collapse("a b c"));
    assert_eq!(stuck::fingerprint("a  b\n c "), stuck::fingerprint("a b c"));
}

#[test]
fn one_appended_word_changes_the_fingerprint() {
    assert_ne!(stuck::fingerprint(OWNED), stuck::fingerprint(&format!("{OWNED} x")));
}

#[test]
fn whitespace_only_is_the_same_as_empty() {
    assert_eq!("", stuck::collapse("   \n\t "));
}

#[test]
fn nothing_happens_below_the_threshold() {
    let mut tracker = Tracker::default();
    assert_eq!("wait", decide(&mut tracker, &observed(), T0).action);
    let decision = decide(&mut tracker, &observed(), T0 + 60.0);
    assert_eq!("wait", decision.action);
    assert_eq!(0, decision.attempts);
}

#[test]
fn at_the_threshold_exactly_one_attempt_is_decided() {
    let mut tracker = Tracker::default();
    decide(&mut tracker, &observed(), T0);
    let decision = decide(&mut tracker, &observed(), T0 + 120.0);
    assert_eq!("recover", decision.action);
    assert_eq!(1, decision.attempts);
    assert!(decision.recovers());
}

#[test]
fn stable_text_produces_exactly_one_recover_then_a_cooldown() {
    let mut tracker = Tracker::default();
    decide(&mut tracker, &observed(), T0);
    assert_eq!("recover", decide(&mut tracker, &observed(), T0 + 120.0).action);
    let cooling = decide(&mut tracker, &observed(), T0 + 130.0);
    assert_eq!("cooling", cooling.action);
    assert!(cooling.reason.contains("cooldown"));
}

#[test]
fn the_clock_starts_at_the_first_sighting_not_at_the_message_age() {
    let mut tracker = Tracker::default();
    decide(&mut tracker, &observed(), T0);
    assert_eq!("wait", decide(&mut tracker, &observed(), T0 + 119.0).action);
    assert_eq!("recover", decide(&mut tracker, &observed(), T0 + 120.0).action);
}

#[test]
fn an_edited_draft_resets() {
    let mut tracker = Tracker::default();
    decide(&mut tracker, &observed(), T0);
    let mut edited = observed();
    edited.text = Some(format!("{OWNED} and more"));
    let decision = decide(&mut tracker, &edited, T0 + 10.0);
    assert_eq!("reset", decision.action);
}

#[test]
fn text_that_is_not_provably_ours_resets() {
    let mut tracker = Tracker::default();
    decide(&mut tracker, &observed(), T0);
    let mut foreign = observed();
    foreign.owned = false;
    let decision = decide(&mut tracker, &foreign, T0 + 10.0);
    assert_eq!("reset", decision.action);
    assert!(decision.reason.contains("not provably ours"));
}

#[test]
fn a_busy_turn_resets_and_is_never_read_as_success() {
    let mut tracker = Tracker::default();
    decide(&mut tracker, &observed(), T0);
    let mut busy = observed();
    busy.status = "working".to_string();
    let decision = decide(&mut tracker, &busy, T0 + 130.0);
    assert_eq!("reset", decision.action);
    assert_eq!("target is working", decision.reason);
}

#[test]
fn an_unknown_a_dead_or_a_blocked_target_resets() {
    for status in ["unknown", "dead", "blocked"] {
        let mut tracker = Tracker::default();
        decide(&mut tracker, &observed(), T0);
        let mut other = observed();
        other.status = status.to_string();
        let decision = decide(&mut tracker, &other, T0 + 130.0);
        assert_eq!("reset", decision.action, "{status}");
        assert_eq!(format!("target is {status}"), decision.reason);
    }
}

#[test]
fn an_open_dialog_resets() {
    let mut tracker = Tracker::default();
    decide(&mut tracker, &observed(), T0);
    let mut dialog = observed();
    dialog.dialog = true;
    let decision = decide(&mut tracker, &dialog, T0 + 130.0);
    assert_eq!("reset", decision.action);
    assert!(decision.reason.contains("dialog"));
}

#[test]
fn a_replaced_pane_resets() {
    let mut tracker = Tracker::default();
    decide(&mut tracker, &observed(), T0);
    let mut replaced = observed();
    replaced.generation = Some("pid:99:start:bb".to_string());
    let decision = decide(&mut tracker, &replaced, T0 + 130.0);
    assert_eq!("reset", decision.action);
    assert!(decision.reason.contains("replaced"));
}

#[test]
fn a_different_tracked_message_resets() {
    let mut tracker = Tracker::default();
    decide(&mut tracker, &observed(), T0);
    let mut other = observed();
    other.message_id = Some("m2".to_string());
    let decision = decide(&mut tracker, &other, T0 + 130.0);
    assert_eq!("reset", decision.action);
    assert!(decision.reason.contains("tracked message changed"));
}

#[test]
fn a_cancelled_message_resets() {
    let mut tracker = Tracker::default();
    decide(&mut tracker, &observed(), T0);
    let mut cancelled = observed();
    cancelled.cancelled = true;
    let decision = decide(&mut tracker, &cancelled, T0 + 130.0);
    assert_eq!("reset", decision.action);
    assert!(decision.reason.contains("cancelled"));
}

#[test]
fn an_empty_composer_resets_instead_of_pressing() {
    let mut tracker = Tracker::default();
    decide(&mut tracker, &observed(), T0);
    let mut empty = observed();
    empty.text = Some(String::new());
    let decision = decide(&mut tracker, &empty, T0 + 130.0);
    assert_eq!("reset", decision.action);
    assert!(decision.reason.contains("empty"));
}

#[test]
fn attempts_are_bounded_and_end_in_a_named_terminal_reason() {
    let mut tracker = Tracker::default();
    decide(&mut tracker, &observed(), T0);
    assert_eq!("recover", decide(&mut tracker, &observed(), T0 + 120.0).action);
    assert_eq!("recover", decide(&mut tracker, &observed(), T0 + 200.0).action);
    let terminal = decide(&mut tracker, &observed(), T0 + 300.0);
    assert_eq!("terminal", terminal.action);
    assert!(terminal.reason.contains("did not submit the text"));
    assert_eq!("terminal", decide(&mut tracker, &observed(), T0 + 400.0).action);
}

#[test]
fn attempts_survive_a_restart_so_no_second_enter_is_pressed() {
    // Mirrors the Python: max_attempts=1, so ONE press is the whole budget, and a
    // restart must not buy the same message a second Enter.
    let config = Config { threshold_s: 120.0, max_attempts: 1, cooldown_s: 60.0,
                          ..Config::default() };
    let mut tracker = Tracker::default();
    tracker.decide(&observed(), &config, T0);
    assert_eq!("recover", tracker.decide(&observed(), &config, T0 + 120.0).action);
    // A restart restores the persisted state - records AND the spent ledger, which is
    // what Tracker.load() keeps (the Python test persists with dump()/load()).
    let mut restored = tracker.clone();
    let decision = restored.decide(&observed(), &config, T0 + 400.0);
    assert_eq!("terminal", decision.action,
               "a restart must not buy the same message a second Enter");
    assert_eq!(1, decision.attempts);
}

#[test]
fn a_consumed_message_does_not_regain_its_budget_when_it_reappears() {
    let config = Config { threshold_s: 10.0, max_attempts: 1, cooldown_s: 1.0,
                          ..Config::default() };
    let mut tracker = Tracker::default();
    tracker.decide(&observed(), &config, T0);
    assert_eq!("recover", tracker.decide(&observed(), &config, T0 + 10.0).action);
    tracker.forget("w1:p1");                       // consumption forgets the pane record
    tracker.decide(&observed(), &config, T0 + 20.0);
    assert_eq!(1, tracker.records["w1:p1"].attempts,
               "the ledger keeps what this message already spent");
    assert_eq!("terminal", tracker.decide(&observed(), &config, T0 + 40.0).action);
}

#[test]
fn a_reset_does_not_refund_either() {
    let config = Config { threshold_s: 10.0, max_attempts: 1, cooldown_s: 1.0,
                          ..Config::default() };
    let mut tracker = Tracker::default();
    tracker.decide(&observed(), &config, T0);
    tracker.decide(&observed(), &config, T0 + 10.0);          // reserved 1
    let mut busy = observed();
    busy.status = "working".to_string();
    assert_eq!("reset", tracker.decide(&busy, &config, T0 + 11.0).action);
    tracker.decide(&observed(), &config, T0 + 30.0);
    assert_eq!("terminal", tracker.decide(&observed(), &config, T0 + 60.0).action,
               "a busy turn must not hand the same message a new attempt");
}

#[test]
fn the_ledger_crosses_a_restart() {
    let config = Config { threshold_s: 10.0, max_attempts: 1, cooldown_s: 1.0,
                          ..Config::default() };
    let mut tracker = Tracker::default();
    tracker.decide(&observed(), &config, T0);
    tracker.decide(&observed(), &config, T0 + 10.0);
    let mut restored = tracker.clone();
    restored.forget("w1:p1");
    restored.decide(&observed(), &config, T0 + 100.0);
    assert_eq!(1, restored.records["w1:p1"].attempts,
               "the ledger crosses the restart");
}

#[test]
fn a_disabled_recovery_does_not_act_at_all() {
    let mut tracker = Tracker::default();
    decide(&mut tracker, &observed(), T0);
    let config = Config { enabled: false, ..Config::default() };
    let decision = tracker.decide(&observed(), &config, T0 + 500.0);
    assert_eq!("reset", decision.action);
    assert!(decision.reason.contains("disabled"));
}

#[test]
fn a_cleared_composer_is_consumption() {
    let mut after = observed();
    after.text = Some(String::new());
    assert_eq!("consumed", stuck::verify(&observed(), Some(&after)).0);
}

#[test]
fn a_new_turn_is_consumption() {
    let mut after = observed();
    after.status = "working".to_string();
    assert_eq!("consumed", stuck::verify(&observed(), Some(&after)).0);
}

#[test]
fn unchanged_text_is_not_consumption_and_says_so() {
    let (outcome, detail) = stuck::verify(&observed(), Some(&observed()));
    assert_eq!("unchanged", outcome);
    assert!(detail.contains("same text"));
}

#[test]
fn a_different_text_after_the_press_is_not_success() {
    let mut after = observed();
    after.text = Some("something else entirely".to_string());
    assert_eq!("modified", stuck::verify(&observed(), Some(&after)).0);
}

#[test]
fn no_observation_after_the_press_is_unverified_not_success() {
    assert_eq!("unverified", stuck::verify(&observed(), None).0);
}

#[test]
fn an_unreadable_pane_is_not_a_cleared_composer() {
    let mut after = observed();
    after.text = None;
    let (outcome, detail) = stuck::verify(&observed(), Some(&after));
    assert_eq!("unverified", outcome);
    assert!(detail.contains("could not be read"));
}

#[test]
fn a_lane_that_was_already_working_never_confirms_our_send() {
    let mut before = observed();
    before.status = "working".to_string();
    let mut after = observed();
    after.status = "working".to_string();
    assert_eq!("unchanged", stuck::verify(&before, Some(&after)).0);
}

#[test]
fn a_dialog_after_the_press_is_unverified() {
    let mut after = observed();
    after.dialog = true;
    assert_eq!("unverified", stuck::verify(&observed(), Some(&after)).0);
}

#[test]
fn the_machine_refuses_to_read_a_clock_of_its_own() {
    assert!(stuck::obs_now(None).is_err());
    assert_eq!(T0, stuck::obs_now(Some(T0)).unwrap());
}
