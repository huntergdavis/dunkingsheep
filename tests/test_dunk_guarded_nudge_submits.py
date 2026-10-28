"""t130: the NUDGE path must try the key that submits, not just Enter.

The six idle codex lanes had our nudges typed and unsubmitted. The retry ladder was
fixed in 2.70.35 and changed nothing, because a nudge is delivered through the GUARDED
branch of submit_text - one press, one check, give up. Codex ignores Enter and submits on
the carriage return, which that branch never tried.
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
from dunk_core import Flock  # noqa: E402
from fakes import FakeHerdr, claude_box_empty_ansi  # noqa: E402


class GuardedNudgeTests(unittest.TestCase):
    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)
        self.herdr.cm_only.add("w2:p1")        # Enter does nothing; C-m submits

    def tearDown(self):
        self.flock.close()

    def test_a_nudge_is_submitted_by_the_guarded_path(self):
        result = self.flock.send_now("w2:p1", "a nudge", wait_s=3)
        self.assertIn(result.get("outcome"), ("ok",), result)
        pressed = [keys for pane, keys in self.herdr.keys if pane == "w2:p1"]
        self.assertIn(["C-m"], pressed,
                      "the guarded path never tried the key that submits")

    def test_every_guarded_delivery_leaves_one_line_naming_the_outcome(self):
        """VP i25315: a guarded nudge must leave its own trace, not only the generic
        'message sent' line. The reachable outcomes are: the box was already clear at the
        first check, a key took at round N, the lane started a turn, or the warning."""
        with self.assertLogs("dunkingsheep", level="INFO") as captured:
            self.flock.send_now("w2:p1", "enter takes", wait_s=3)
        self.assertTrue(any("guarded nudge:" in line for line in captured.output),
                        captured.output)

        self.herdr.cm_only.add("w2:p1")
        with self.assertLogs("dunkingsheep", level="INFO") as captured:
            self.flock.send_now("w2:p1", "cm takes", wait_s=3)
        self.assertTrue(any("C-m took at round" in line for line in captured.output),
                        captured.output)

    def test_a_failure_is_visible_from_the_senders_side(self):
        """The observable the VP asked for: a recurrence appears in the journal, not as
        an idle lane somebody infers."""
        self.herdr.cm_only.discard("w2:p1")
        self.herdr.enters_needed["w2:p1"] = 99
        with self.assertLogs("dunkingsheep", level="WARNING") as captured:
            result = self.flock.send_now("w2:p1", "a nudge", wait_s=3)
        self.assertNotEqual("ok", result.get("outcome"), result)
        self.assertTrue(any("typed, not submitted" in line for line in captured.output),
                        captured.output)

    def test_a_nudge_to_a_composer_that_takes_enter_still_works(self):
        self.herdr.cm_only.discard("w2:p1")
        result = self.flock.send_now("w2:p1", "a nudge", wait_s=3)
        self.assertEqual("ok", result.get("outcome"), result)

    def transient_empty_frame(self):
        """One redraw looks empty even though the fake composer still holds text."""
        original = self.herdr.read_pane
        emitted = False

        def read(pane_id, lines=40, source=None, ansi=False):
            nonlocal emitted
            if ansi and self.herdr.box.get(pane_id) and not emitted:
                emitted = True
                return claude_box_empty_ansi()
            return original(pane_id, lines=lines, source=source, ansi=ansi)

        self.herdr.read_pane = read

    def test_transient_empty_frame_does_not_strand_the_nudge(self):
        self.transient_empty_frame()
        result = self.flock.send_now("w2:p1", "redraw nudge", wait_s=5)
        self.assertEqual("ok", result.get("outcome"), result)
        self.assertEqual([("w2:p1", "redraw nudge")], self.herdr.sends)
        self.assertFalse(self.herdr.box.get("w2:p1"))

    def test_transient_empty_frame_cannot_confirm_an_unsubmitted_nudge(self):
        self.herdr.cm_only.clear()
        self.herdr.enters_needed["w2:p1"] = 99
        self.transient_empty_frame()
        result = self.flock.send_now("w2:p1", "still pending", wait_s=5)
        self.assertNotEqual("ok", result.get("outcome"), result)
        self.assertEqual([], self.herdr.sends)
        self.assertEqual("still pending", self.herdr.box["w2:p1"])


if __name__ == "__main__":
    unittest.main()
