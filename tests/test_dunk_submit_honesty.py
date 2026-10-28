"""'sent' must mean the box cleared or a turn started - never a guess.

VP i24194, live: three messages were typed into codex lane %17 and reported 'sent'
while the lane sat idle; at 07:42 the composer held all three as ONE concatenated
draft and a single Enter submitted it. The reader had called the box empty because
its footer-adjacency rule decided the last prompt row was transcript - and a
multi-line draft's blank lines look exactly like the gap an echo leaves.

So "empty by transcript rule" is UNKNOWN, not clear: the submit ladder must wait for a
turn instead of claiming delivery.
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

# The shape from 07:42: three typed messages concatenated in the composer, blank
# lines between them, and the status footer two blank rows below.
CONCATENATED = "\n".join([
    "\u203a VP keep-moving: continue t81",
    "",
    "\u203a VP probe 4 (2.70.24): composer check",
    "",
    "\u203a [inbox] new message for codex-poolrad from %19. Read it with: dunki",
    "",
    "",
    "  GPT-6-Astra high \u00b7 ~/workspace/poolrad-codex-a \u00b7 Implement t81 handoff",
    "",
])


class SubmitHonestyTests(unittest.TestCase):
    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)
        self.herdr.screens["w2:p1"] = CONCATENATED
        self.herdr.statuses["w2:p1"] = "idle"

    def tearDown(self):
        self.flock.close()

    def test_the_ambiguous_frame_is_not_reported_as_sent(self):
        ok, detail = self.flock.submit_text("w2:p1", "another line")
        self.assertFalse(ok, f"an unconfirmed submit was reported as sent: {detail}")
        self.assertIn("unconfirmed", detail.lower())

    def test_a_turn_starting_still_counts_as_delivered(self):
        """t363: "a turn started" means a TRANSITION during this send - idle at
        send start, working after. A pre-existing working state is stale evidence
        (the negative regression lives in test_dunk_turn_confirmation.py)."""
        reads = iter(["idle"])
        with mock.patch.object(type(self.flock), "_read_status",
                               side_effect=lambda pane_id: next(reads, "working")):
            ok, detail = self.flock.submit_text("w2:p1", "another line")
        self.assertTrue(ok, detail)
        self.assertIn("turn", detail)

    def test_the_reader_still_says_why_it_called_the_box_empty(self):
        _text, detail = dunk_core.box_text(CONCATENATED)
        self.assertIn("transcript", detail,
                      "the ambiguity has to be visible in the detail, not inferred")


if __name__ == "__main__":
    unittest.main()
