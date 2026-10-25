"""t130: an inconclusive box read must not stop the submit ladder.

Live: six idle codex lanes had our nudges TYPED but UNSUBMITTED; a guarded Enter fixed
each in seconds, and reviewers had been silent 30+ min. The composer of an idle codex
lane reads as "transcript" by the footer-adjacency rule, so `_box_holds` returns unknown -
and the ladder used to RETURN on the first unknown instead of trying the next key. Enter
alone does not submit in codex; the carriage return does.

These two tests are the pair t130 asks for:
  - the real idle frame (our text in the composer, footer below, Enter ignored) must
    submit, and
  - a real human draft frame must still be treated as a draft and never submitted.
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
import dunk_core  # noqa: E402
from dunk_core import Flock  # noqa: E402
from fakes import FakeHerdr  # noqa: E402

# An idle codex lane holding our nudge, with the status footer two blank rows below -
# the shape whose box read is inconclusive (transcript rule).
IDLE_WITH_OUR_NUDGE = "\n".join([
    "\u203a an earlier instruction",
    "",
    "\u203a a nudge",
    "",
    "",
    "  GPT-6-Astra high \u00b7 ~/workspace/poolrad-codex-a \u00b7 Implement t81 handoff",
    "",
])


class SubmitWhenUnknownTests(unittest.TestCase):
    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)
        self.herdr.screens["w2:p1"] = IDLE_WITH_OUR_NUDGE
        self.herdr.statuses["w2:p1"] = "idle"
        # Enter does nothing in this composer; the carriage return submits.
        self.herdr.cm_only.add("w2:p1")

    def tearDown(self):
        self.flock.close()

    def test_an_inconclusive_read_keeps_pressing_until_a_key_submits(self):
        ok, detail = self.flock.submit_text("w2:p1", "a nudge")
        self.assertTrue(ok, detail)
        pressed = [keys for pane, keys in self.herdr.keys if pane == "w2:p1"]
        self.assertIn(["C-m"], pressed,
                      "the ladder gave up before trying the key that submits")

    def test_the_reader_still_calls_that_frame_inconclusive(self):
        text, detail = dunk_core.box_text(IDLE_WITH_OUR_NUDGE)
        self.assertEqual("", text)
        self.assertIn("transcript", detail,
                      "the ambiguity must stay visible, not be silently resolved")

    def test_a_human_draft_is_never_submitted(self):
        """The gate t130 asks for: the dangerous direction, pinned first."""
        draft = "my human draft line one"
        self.herdr.screens["w2:p1"] = "\n".join([
            "\u203a " + draft,
            "  draft line two",
            "  draft line three",
            "",
            "  GPT-6-Astra high \u00b7 /tmp/x \u00b7 a task",
            "",
        ])
        # The fake models the composer separately from the screen; a real draft lives in
        # both, so fill the box too or the guard has nothing to protect.
        self.herdr.box["w2:p1"] = draft + "\ndraft line two\ndraft line three"
        # The guard is what protects a draft: the box check is the caller's job
        # (send_guarded sets it), so ask for it explicitly here.
        ok, detail = self.flock.submit_text("w2:p1", "a nudge", check_box=True)
        self.assertFalse(ok, f"a human draft was submitted: {detail}")
        self.assertEqual([], [s for s in self.herdr.sends if s[0] == "w2:p1"],
                         "nothing should have been submitted into a human's draft")


if __name__ == "__main__":
    unittest.main()
