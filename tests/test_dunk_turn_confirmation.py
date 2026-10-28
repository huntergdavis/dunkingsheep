"""t363: a stale working state cannot confirm delivery of a new message.

The PR63 review reproduced the false success: with the pane already "working"
before the send, `_turn_started` confirmed "sent (confirmed by the turn it
started)" while the composer still held the full text and no send had landed.
The turn evidence is only valid as a TRANSITION during this send.

Acceptance here:
  * negative: pre-existing working + composer holding the text -> NOT sent,
    an unconfirmed verdict naming why;
  * positive: idle -> working DURING the ladder -> confirmed by the turn;
  * the same two shapes on the guarded-nudge ladder (the t130 path).
"""

import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
import dunk_core  # noqa: E402
from dunk_core import Flock  # noqa: E402
from fakes import FakeHerdr  # noqa: E402


class TransitionHerdr(FakeHerdr):
    """FakeHerdr whose status turns "working" after N agent_status reads."""

    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.turn_after_reads = None      # None = never; N = working after N reads

    def agent_status(self, pane_id):
        self.status_calls += 1
        if (self.turn_after_reads is not None
                and self.status_calls > self.turn_after_reads):
            return "working"
        return super().agent_status(pane_id)


class SubmitLadderStaleTurnTests(unittest.TestCase):
    """The _press_until_submitted ladder."""

    def setUp(self):
        self.herdr = TransitionHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)
        self.gaps = mock.patch.object(dunk_core, "SUBMIT_GAPS", (0.0, 0.0, 0.0, 0.0))
        self.gaps.start()
        self.verify = mock.patch.object(dunk_core, "SUBMIT_VERIFY_GAPS", (0.0, 0.0))
        self.verify.start()

    def tearDown(self):
        self.gaps.stop()
        self.verify.stop()
        self.flock.close()

    def test_preexisting_working_cannot_confirm_a_new_message(self):
        """t363 acceptance, negative: the pane was working BEFORE the send; the
        composer keeps showing the text and nothing was ever sent. The old code
        returned "sent (confirmed by the turn it started)" - a turn that predates
        the message. It must be an unconfirmed verdict instead."""
        self.herdr.statuses["w2:p1"] = "working"
        self.herdr.enters_needed["w2:p1"] = 99      # no press ever submits
        with mock.patch.object(type(self.flock), "_box_holds", return_value=True):
            ok, detail = self.flock.submit_text("w2:p1", "a nudge")
        self.assertFalse(ok, detail)
        self.assertNotIn("confirmed by the turn", detail, detail)
        self.assertIn("submission unconfirmed", detail, detail)
        self.assertIn("already working", detail, detail)
        self.assertEqual([], self.herdr.sends)

    def test_a_turn_starting_during_the_send_still_confirms(self):
        """The 2026-10-24 measured case, kept: the lane was idle at send start and
        started working DURING the ladder - that transition is genuine evidence."""
        self.herdr.turn_after_reads = 1             # first read (baseline): idle
        with mock.patch.object(type(self.flock), "_box_holds", return_value=True):
            ok, detail = self.flock.submit_text("w2:p1", "a nudge")
        self.assertTrue(ok, detail)
        self.assertIn("confirmed by the turn", detail)

    def test_an_unreadable_baseline_cannot_confirm_either(self):
        """Silence is not a baseline: if the pre-send probe fails, current
        "working" has no transition to compare against."""
        self.herdr.statuses["w2:p1"] = "working"
        self.herdr.enters_needed["w2:p1"] = 99
        with mock.patch.object(type(self.flock), "_read_status", return_value=None), \
             mock.patch.object(type(self.flock), "_box_holds", return_value=True):
            ok, detail = self.flock.submit_text("w2:p1", "a nudge")
        self.assertFalse(ok, detail)
        self.assertNotIn("confirmed by the turn", detail, detail)


class GuardedNudgeStaleTurnTests(unittest.TestCase):
    """The guarded ladder - the path every t130 nudge takes."""

    def setUp(self):
        self.herdr = TransitionHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)

    def tearDown(self):
        self.flock.close()

    def test_guarded_stale_working_is_unconfirmed_not_sent(self):
        """t363 on the guarded path: pre-existing working, presses swallowed
        (enters_needed=99), composer keeps the text -> NOT "sent (confirmed by
        the turn it started)"; an unconfirmed verdict instead."""
        self.herdr.statuses["w2:p1"] = "working"
        self.herdr.enters_needed["w2:p1"] = 99
        with mock.patch.object(type(self.flock), "_box_holds", return_value=True):
            result = self.flock.send_now("w2:p1", "a nudge", wait_s=3)
        self.assertNotEqual("ok", result.get("outcome"), result)
        self.assertNotIn("confirmed by the turn", result.get("message") or "", result)
        self.assertIn("submission unconfirmed", result.get("message") or "", result)
        self.assertEqual([], self.herdr.sends)

    def test_guarded_transition_to_working_still_confirms(self):
        """Positive on the guarded path: idle at send start, working during the
        ladder, box read ambiguous -> confirmed by the turn."""
        self.herdr.turn_after_reads = 1
        with mock.patch.object(type(self.flock), "_box_holds", return_value=True):
            result = self.flock.send_now("w2:p1", "a nudge", wait_s=3)
        self.assertEqual("ok", result.get("outcome"), result)
        self.assertIn("confirmed by the turn", result.get("message") or "", result)


if __name__ == "__main__":
    unittest.main()
