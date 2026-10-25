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
from fakes import FakeHerdr  # noqa: E402


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

    def test_a_nudge_to_a_composer_that_takes_enter_still_works(self):
        self.herdr.cm_only.discard("w2:p1")
        result = self.flock.send_now("w2:p1", "a nudge", wait_s=3)
        self.assertEqual("ok", result.get("outcome"), result)


if __name__ == "__main__":
    unittest.main()
