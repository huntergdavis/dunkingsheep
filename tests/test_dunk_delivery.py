"""Can this lane receive a message, and if not why.

The VP's defect: a nudge that never submitted leaves a draft in the input box,
every later send is held as "someone is typing", and the lane is alive, has
unread mail, and can never receive anything - indistinguishable from busy. So
"blocked" is a state of its own, with a reason and a way out.
"""
import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _bootstrap  # noqa: E402,F401  (isolate live state)

import dunk_core  # noqa: E402
from fakes import FakeHerdr  # noqa: E402

LIVE = """\
│ ● wrote the phrase "server connection could not be restored" into a test file
│ ── Working ──────────────────────────────────────────────
│ ~/workspace/dunkingsheep (main)
"""

DEAD = """\
│ ■ Server connection could not be restored
│ • Reconnect failed - check the endpoint, then relaunch (41h 22m)
│ › [inbox] new message
│   ^c quit
"""

PAUSED = """\
│ ✻ Reading files
│ GPT-6-Astra high · ~/workspace/crackdown-switch · Port Crackdown   Goal paused (/goal resume)
"""

CONTEXT_FULL = """\
│ ✻ Working
│ › new task? /clear to save 497.4k tokens
"""


class ClassifyDeliveryTests(unittest.TestCase):
    def test_a_live_pane_that_quotes_the_marker_is_not_dead(self):
        # The false positive that made my own pane look dead: the phrase was in
        # its own output. A banner starts its line; prose does not.
        self.assertEqual(("ready", None), dunk_core.classify_delivery(LIVE, "idle"))

    def test_a_banner_line_is_a_dead_session(self):
        state, reason = dunk_core.classify_delivery(DEAD, "idle")
        self.assertEqual("dead", state)
        self.assertIn("server connection could not be restored", reason)

    def test_escape_sequences_do_not_hide_a_banner(self):
        state, _ = dunk_core.classify_delivery("\x1b[31m■ Server connection could not be restored\x1b[0m\n", "idle")
        self.assertEqual("dead", state, "a colour code before the glyph hid the banner")

    def test_a_paused_goal_blocks_delivery_and_says_so(self):
        state, reason = dunk_core.classify_delivery(PAUSED, "idle")
        self.assertEqual("blocked", state)
        self.assertIn("paused goal", reason)

    def test_a_full_context_blocks_delivery_and_names_the_fix(self):
        state, reason = dunk_core.classify_delivery(CONTEXT_FULL, "idle")
        self.assertEqual("blocked", state)
        self.assertIn("/clear", reason)

    def test_a_draft_in_the_box_blocks_delivery(self):
        state, reason = dunk_core.classify_delivery("│ › half a sentence", "idle",
                                                    typed="half a sentence")
        self.assertEqual("blocked", state)
        self.assertIn("unsubmitted draft", reason)

    def test_working_is_not_ready_and_not_blocked(self):
        self.assertEqual(("working", "working"),
                         dunk_core.classify_delivery("│ ✻ Working", "working"))


class ClearBoxTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.herdr = FakeHerdr()
        self.superusers = dunk_core.Superusers(path=os.path.join(self.tmp.name, "su.json"))
        self.superusers.add("w1:p1")
        self.flock = dunk_core.Flock(herdr=self.herdr, persist=False, restore=False,
                                     superusers=self.superusers)

    def tearDown(self):
        self.tmp.cleanup()

    def test_a_non_superuser_cannot_clear_someone_elses_box(self):
        with self.assertRaises(dunk_core.DunkError):
            self.flock.clear_box("w1:p2", self_pane_id="w1:p2", reason="test")
        self.assertEqual([], self.herdr.keys, "a refused clear must press nothing")

    def test_a_superuser_clear_sends_ctrl_u_and_escape_and_reports_both_states(self):
        # The VP's stuck pane: dead session, draft left in the box, nothing can
        # be delivered. Clearing is what unblocks it.
        self.herdr.screens["w1:p2"] = DEAD
        self.herdr.statuses["w1:p2"] = "idle"
        result = self.flock.clear_box("w1:p2", self_pane_id="w1:p1", reason="test")
        self.assertTrue(result["flushed"])
        self.assertEqual([("w1:p2", ["C-u"]), ("w1:p2", ["Escape"])], self.herdr.keys)
        self.assertIn("before", result)
        self.assertIn("after", result)
        self.assertEqual("test", result["reason"])

    def test_the_clear_outcome_prints_without_a_label(self):
        # The live crash: two lanes had no 'target_label', the CLI printer
        # indexed it, and the one command that unblocks a lane could not run.
        from importlib.machinery import SourceFileLoader
        cli = SourceFileLoader(
            "dunkcli_print", str(Path(__file__).resolve().parent.parent / "dunkingsheep")).load_module()
        result = {"pane_id": "wW:p1", "flushed": True, "reason": "context full",
                  "before": {"state": "blocked", "reason": "context full"},
                  "after": {"state": "ready", "reason": None}}
        import contextlib, io
        with contextlib.redirect_stdout(io.StringIO()) as buffer:
            cli.print_clear(result)
        self.assertIn("cleared wW:p1", buffer.getvalue())
        self.assertIn("blocked -> ready", buffer.getvalue())
        self.assertIn("context full", buffer.getvalue())

    def test_a_pane_record_without_a_label_falls_back_to_the_pane_id(self):
        self.herdr.screens["w1:p9"] = DEAD
        self.herdr.statuses["w1:p9"] = "idle"
        result = self.flock.clear_box({"pane_id": "w1:p9"}, self_pane_id="w1:p1", reason="probe")
        self.assertEqual("w1:p9", result["target_label"])

    def test_a_working_lane_is_not_interrupted_by_a_clear(self):
        self.herdr.screens["w1:p2"] = "│ ✻ Working"
        self.herdr.statuses["w1:p2"] = "working"
        # `statuses` is what the pane reports; a clear must never interrupt work.
        result = self.flock.clear_box("w1:p2", self_pane_id="w1:p1", reason="test")
        self.assertFalse(result["flushed"])
        self.assertIn("working", result["reason"])
        self.assertEqual([], self.herdr.keys)


if __name__ == "__main__":
    unittest.main()
