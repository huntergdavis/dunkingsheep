"""t377: the deterministic stuck-message state machine, on a fake clock.

Every case here is a sentence from the owner's order turned into an assertion:
before the threshold nothing happens, at the threshold exactly ONE guarded attempt,
the clock resets on any edit, every uncertain state fails closed, and a restart
cannot produce a second Enter.
"""
import json
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import dunk_stuck as S  # noqa: E402

T0 = 1_000_000.0
OWNED = "dunkingsheep send t42 --text 'continue'"


def obs(**kw):
    base = dict(pane_id="w1:p1", message_id="m1", text=OWNED, status="ready",
                owned=True, generation="pid:11:start:aa", dialog=False, cancelled=False)
    base.update(kw)
    return S.Observation(**base)


class ConfigTests(unittest.TestCase):
    def test_the_threshold_defaults_to_the_cos_bound(self):
        self.assertEqual(120.0, S.Config().threshold_s)
        self.assertEqual(2, S.Config().max_attempts)
        self.assertEqual(60.0, S.Config().cooldown_s)

    def test_the_threshold_is_configurable(self):
        config = S.Config.from_mapping({"threshold_s": 45, "max_attempts": 1,
                                        "cooldown_s": 10})
        self.assertEqual((45.0, 1, 10.0),
                         (config.threshold_s, config.max_attempts, config.cooldown_s))

    def test_nonsense_config_is_refused_not_coerced(self):
        for bad in ({"threshold_s": "soon"}, {"threshold_s": 0}, {"threshold_s": -5},
                    {"cooldown_s": 0}, {"max_attempts": 0}, {"max_attempts": "two"}):
            with self.assertRaises(ValueError, msg=bad):
                S.Config.from_mapping(bad)


class FingerprintTests(unittest.TestCase):
    def test_the_composer_wrapping_is_not_content(self):
        self.assertEqual(S.fingerprint("a very long notice that wrapped\n  onto a second line"),
                         S.fingerprint("a very long notice that wrapped onto a second line"))

    def test_one_appended_word_changes_the_fingerprint(self):
        self.assertNotEqual(S.fingerprint(OWNED), S.fingerprint(OWNED + " please"))

    def test_whitespace_only_is_the_same_as_empty(self):
        # A composer holding spaces is empty, not a draft: the reader collapses them.
        self.assertEqual(S.fingerprint(""), S.fingerprint(" \n  "))


class BeforeAndAtThresholdTests(unittest.TestCase):
    def setUp(self):
        self.tracker = S.Tracker()

    def test_nothing_happens_below_the_threshold(self):
        first = self.tracker.decide(obs(), now=T0)
        self.assertEqual("wait", first.action)
        self.assertEqual(0, first.attempts)
        just_before = self.tracker.decide(obs(), now=T0 + 119.9)
        self.assertEqual("wait", just_before.action)
        self.assertEqual(0, just_before.attempts, "no attempt may be recorded before 120 s")

    def test_at_the_threshold_exactly_one_attempt_is_decided(self):
        self.tracker.decide(obs(), now=T0)
        at = self.tracker.decide(obs(), now=T0 + 120)
        self.assertEqual("recover", at.action)
        self.assertEqual(1, at.attempts)
        self.assertTrue(at.recovers)

    def test_stable_text_produces_exactly_one_recover_then_a_cooldown(self):
        self.tracker.decide(obs(), now=T0)
        self.assertEqual("recover", self.tracker.decide(obs(), now=T0 + 130).action)
        cooling = self.tracker.decide(obs(), now=T0 + 131)
        self.assertEqual("cooling", cooling.action)
        self.assertEqual(1, cooling.attempts, "a second tick must not press again")

    def test_the_clock_starts_at_the_first_sighting_not_at_the_message_age(self):
        self.tracker.decide(obs(), now=T0 + 5000)
        self.assertEqual("wait", self.tracker.decide(obs(), now=T0 + 5020).action)


class ResetTests(unittest.TestCase):
    """Every uncertain path resets the clock and never presses Enter."""

    def setUp(self):
        self.tracker = S.Tracker()
        self.tracker.decide(obs(), now=T0)

    def assertResets(self, observation, reason_part, at=150):
        decision = self.tracker.decide(observation, now=T0 + at)
        self.assertEqual("reset", decision.action, decision)
        self.assertIn(reason_part, decision.reason)
        self.assertNotIn("w1:p1", self.tracker.records)

    def test_an_edited_draft_resets(self):
        self.assertResets(obs(text=OWNED.replace("continue", "continue twice")),
                          "text changed")

    def test_a_human_continuation_appended_to_our_text_resets(self):
        self.assertResets(obs(text=OWNED + "\nand also please look at the flaky test"),
                          "text changed")

    def test_text_that_is_not_provably_ours_resets(self):
        self.assertResets(obs(text="a person's own sentence", owned=False),
                          "not provably ours")

    def test_a_busy_turn_resets_and_is_never_read_as_success(self):
        self.assertResets(obs(status="working"), "target is working")

    def test_an_unknown_state_resets(self):
        self.assertResets(obs(status="unknown"), "target is unknown")

    def test_a_dead_or_blocked_target_resets(self):
        self.assertResets(obs(status="blocked"), "target is blocked")

    def test_an_open_dialog_resets(self):
        self.assertResets(obs(dialog=True), "dialog")

    def test_a_replaced_pane_resets(self):
        self.assertResets(obs(generation="pid:99:start:bb"), "pane was replaced")

    def test_a_different_tracked_message_resets(self):
        self.assertResets(obs(message_id="m2"), "tracked message changed")

    def test_a_cancelled_message_resets(self):
        self.assertResets(obs(cancelled=True), "cancelled")

    def test_an_empty_composer_resets_instead_of_pressing(self):
        self.assertResets(obs(text=""), "empty")


class BoundedRetryTests(unittest.TestCase):
    def test_attempts_are_bounded_and_end_in_a_named_terminal_reason(self):
        tracker = S.Tracker()
        config = S.Config(threshold_s=120, max_attempts=2, cooldown_s=60)
        tracker.decide(obs(), config=config, now=T0)
        self.assertEqual("recover", tracker.decide(obs(), config=config, now=T0 + 120).action)
        self.assertEqual("cooling", tracker.decide(obs(), config=config, now=T0 + 150).action)
        self.assertEqual("recover", tracker.decide(obs(), config=config, now=T0 + 200).action)
        terminal = tracker.decide(obs(), config=config, now=T0 + 300)
        self.assertEqual("terminal", terminal.action)
        self.assertIn("did not submit", terminal.terminal)
        # Sticky: no further Enter, ever, on this record.
        again = tracker.decide(obs(), config=config, now=T0 + 10_000)
        self.assertEqual("terminal", again.action)


class RestartDedupeTests(unittest.TestCase):
    def test_attempts_survive_a_restart_so_no_second_enter_is_pressed(self):
        config = S.Config(threshold_s=120, max_attempts=1, cooldown_s=60)
        tracker = S.Tracker()
        tracker.decide(obs(), config=config, now=T0)
        self.assertEqual("recover", tracker.decide(obs(), config=config, now=T0 + 120).action)
        restored = S.Tracker.load(tracker.dump())
        decision = restored.decide(obs(), config=config, now=T0 + 400)
        self.assertEqual("terminal", decision.action,
                         "a restart must not buy the same message a second Enter")
        self.assertEqual(1, decision.attempts)

    def test_a_corrupt_tracker_resets_the_clock_instead_of_acting(self):
        for garbage in ("{not json", "[]", '"text"'):
            tracker = S.Tracker.load(garbage)
            self.assertEqual({}, tracker.to_dict())
            self.assertEqual("wait", tracker.decide(obs(), now=T0).action)

    def test_the_round_trip_preserves_every_schema_field(self):
        tracker = S.Tracker()
        tracker.decide(obs(), now=T0)
        state = json.loads(tracker.dump())
        payload = state["records"]["w1:p1"]
        self.assertEqual(set(S.SCHEMA_FIELDS), set(payload))
        self.assertEqual(state["records"], S.Tracker.load(tracker.dump()).to_dict())
        self.assertEqual(state["spent"], json.loads(tracker.dump())["spent"])


class DurableBudgetTests(unittest.TestCase):
    """The review's blocker 2: a spent attempt must never be refunded."""

    CONFIG = S.Config(threshold_s=10, max_attempts=1, cooldown_s=1)

    def test_a_consumed_message_does_not_regain_its_budget_when_it_reappears(self):
        tracker = S.Tracker()
        tracker.decide(obs(), self.CONFIG, now=T0)
        self.assertEqual("recover", tracker.decide(obs(), self.CONFIG, now=T0 + 10).action)
        tracker.forget("w1:p1")                  # consumption forgets the pane record
        tracker.decide(obs(), self.CONFIG, now=T0 + 20)
        self.assertEqual(1, tracker.records["w1:p1"]["attempts"],
                         "the ledger keeps what this message already spent")
        self.assertEqual("terminal", tracker.decide(obs(), self.CONFIG, now=T0 + 40).action)

    def test_a_reset_does_not_refund_either(self):
        tracker = S.Tracker()
        tracker.decide(obs(), self.CONFIG, now=T0)
        tracker.decide(obs(), self.CONFIG, now=T0 + 10)          # reserved 1
        tracker.decide(obs(status="working"), self.CONFIG, now=T0 + 11)   # reset
        tracker.decide(obs(), self.CONFIG, now=T0 + 30)
        self.assertEqual("terminal", tracker.decide(obs(), self.CONFIG, now=T0 + 60).action,
                         "a busy turn must not hand the same message a new attempt")

    def test_the_ledger_crosses_a_restart(self):
        tracker = S.Tracker()
        tracker.decide(obs(), self.CONFIG, now=T0)
        tracker.decide(obs(), self.CONFIG, now=T0 + 10)
        restored = S.Tracker.load(tracker.dump())
        restored.forget("w1:p1")
        restored.decide(obs(), self.CONFIG, now=T0 + 100)
        self.assertEqual(1, restored.records["w1:p1"]["attempts"])

    def test_a_clock_at_epoch_zero_still_measures_the_threshold(self):
        # The falsy-first_seen bug: first_seen 0.0 read as "no value" and the
        # threshold never elapsed.
        tracker = S.Tracker()
        tracker.decide(obs(), self.CONFIG, now=0)
        self.assertEqual("recover", tracker.decide(obs(), self.CONFIG, now=20).action)


class DisabledTests(unittest.TestCase):
    def test_a_disabled_recovery_does_not_act_at_all(self):
        tracker = S.Tracker()
        config = S.Config(enabled=False)
        tracker.decide(obs(), config, now=T0)
        self.assertEqual("reset", tracker.decide(obs(), config, now=T0 + 10_000).action)
        self.assertEqual({}, tracker.records, "a disabled recovery does not even track")


class VerificationTests(unittest.TestCase):
    """One guarded Enter is not a success; only consumption evidence is."""

    def test_a_cleared_composer_is_consumption(self):
        self.assertEqual("consumed", S.verify(obs(), obs(text=""))[0])

    def test_a_new_turn_is_consumption(self):
        before = obs(status="ready")
        after = obs(status="working")
        self.assertEqual("consumed", S.verify(before, after)[0])

    def test_unchanged_text_is_not_consumption_and_says_so(self):
        outcome, detail = S.verify(obs(), obs())
        self.assertEqual("unchanged", outcome)
        self.assertIn("still in the composer", detail)

    def test_a_different_text_after_the_press_is_not_success(self):
        self.assertEqual("modified", S.verify(obs(), obs(text="something else"))[0])

    def test_no_observation_after_the_press_is_unverified_not_success(self):
        self.assertEqual("unverified", S.verify(obs(), None)[0])

    def test_an_unreadable_pane_is_not_a_cleared_composer(self):
        # The review's blocker 3: None read coerced to '' reported success.
        outcome, detail = S.verify(obs(), S.Observation(
            pane_id="w1:p1", message_id="m1", text=None, status="unknown", owned=False))
        self.assertEqual("unverified", outcome)
        self.assertIn("could not be read", detail)

    def test_a_lane_that_was_already_working_never_confirms_our_send(self):
        before = obs(status="working")
        after = obs(status="working")
        self.assertEqual("unchanged", S.verify(before, after)[0])

    def test_a_dialog_after_the_press_is_unverified(self):
        self.assertEqual("unverified", S.verify(obs(), obs(dialog=True))[0])


class ClockIsInjectedTests(unittest.TestCase):
    def test_the_machine_refuses_to_read_a_clock_of_its_own(self):
        with self.assertRaises(ValueError):
            S.Tracker().decide(obs())


if __name__ == "__main__":
    unittest.main()
