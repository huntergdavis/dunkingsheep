"""The delivery hold: one predicate, an expiry, and a queue that survives a restart.

Four defects from one incident (2026-10-23):

1. a "busy" report from another process held every send for 8.5 hours while the
   pane showed an empty prompt and no turn - a remembered state with no expiry;
2. `delivery` said ready while the queue held the mail: two checks, two answers;
3. restarting the daemon discarded every held message, silently;
4. the recovery (`send --ignore-typing`) was undocumented.

This module pins 1-3. 4 is a sentence in docs/DELIVERY.md.
"""
import json
import os
import sys
import tempfile
import time
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _bootstrap  # noqa: E402,F401

import dunk_core  # noqa: E402
from fakes import FakeHerdr  # noqa: E402


class BusyReportExpiryTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.herdr = FakeHerdr()
        self.pane = self.herdr.panes[0]["pane_id"]
        self.herdr.panes[0]["agent"] = "claude"
        self.herdr.screens[self.pane] = "❯\u00a0\n"          # an empty, clean prompt
        self.flock = dunk_core.Flock(herdr=self.herdr, persist=False, restore=False)

        class AlwaysBusy:
            def status_for_pane(self, pane, fresh=False):
                return "busy"

        self.flock.claude_status = AlwaysBusy()

    def tearDown(self):
        self.tmp.cleanup()

    def test_a_busy_report_expires_when_the_pane_shows_nothing_happening(self):
        self.assertTrue(self.flock._claude_busy(self.pane), "a fresh busy report holds")
        # Age the belief past the TTL: a remembered state must not be permanent.
        self.flock._claude_busy_since[self.pane] = time.time() - (dunk_core.CLAUDE_BUSY_TTL_S + 1)
        self.assertFalse(self.flock._claude_busy(self.pane),
                         "a stale busy report must be released by the pane's own evidence")

    def test_a_busy_report_is_believed_while_the_pane_is_actually_busy(self):
        self.flock._claude_busy_since[self.pane] = time.time() - (dunk_core.CLAUDE_BUSY_TTL_S + 1)
        self.herdr.screens[self.pane] = "❯ half a sentence\n"
        self.assertTrue(self.flock._claude_busy(self.pane),
                        "draft text in the box means something IS happening")
        self.flock.turn_start(pane_id=self.pane)
        self.herdr.screens[self.pane] = "❯\u00a0\n"
        self.assertTrue(self.flock._claude_busy(self.pane),
                        "a declared turn means something IS happening")

    def test_the_hold_is_forgotten_once_the_report_clears(self):
        self.assertTrue(self.flock._claude_busy(self.pane))

        class Idle:
            def status_for_pane(self, pane, fresh=False):
                return "idle"

        self.flock.claude_status = Idle()
        self.assertFalse(self.flock._claude_busy(self.pane))
        self.assertEqual({}, self.flock._claude_busy_since)


class OnePredicateTests(unittest.TestCase):
    """delivery and the queue must not disagree; they now ask the same function."""

    def setUp(self):
        self.herdr = FakeHerdr()
        self.pane = self.herdr.panes[0]["pane_id"]
        self.herdr.panes[0]["agent"] = "claude"
        self.flock = dunk_core.Flock(herdr=self.herdr, persist=False, restore=False)

        class Busy:
            def status_for_pane(self, pane, fresh=False):
                return "busy"

        self.flock.claude_status = Busy()

    def test_delivery_reports_the_reason_the_queue_would_hold_for(self):
        state = self.flock.delivery_state(self.pane)
        self.assertEqual("blocked", state["state"])
        self.assertIn("busy", state["reason"])
        blocker = self.flock.send_blocker(self.pane)
        self.assertIsNotNone(blocker)
        self.assertEqual(blocker[1], state["reason"],
                         "the two checks must give the same answer, in the same words")

    def test_a_declared_turn_is_visible_to_delivery_too(self):
        self.flock.claude_status.status_for_pane = lambda pane, fresh=False: "idle"
        self.flock.turn_start(pane_id=self.pane)
        state = self.flock.delivery_state(self.pane)
        self.assertEqual("blocked", state["state"])
        self.assertIn("turn", state["reason"])


class OutboxRestartTests(unittest.TestCase):
    """Losing a queued notice is worse than delaying it."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.state = os.path.join(self.tmp.name, "state.json")
        self.herdr = FakeHerdr()
        for pane in self.herdr.panes:
            self.herdr.statuses[pane["pane_id"]] = "working"
        self.pane = self.herdr.panes[0]["pane_id"]

    def tearDown(self):
        self.tmp.cleanup()

    def test_a_held_message_survives_a_restart_and_is_logged(self):
        first = dunk_core.Flock(herdr=self.herdr, state_file=self.state,
                                persist=True, restore=False)
        message = first.send_now(self.pane, "held through a restart",
                                 hold_while_typing=True, wait_s=0.2)
        self.assertEqual("queued", message["status"])
        with self.assertLogs("dunkingsheep", level="WARNING") as captured:
            restarted = dunk_core.Flock(herdr=self.herdr, state_file=self.state,
                                        persist=True, restore=True)
        self.assertIn("resumed 1 held message(s) after restart: " + message["message_id"],
                      "\n".join(captured.output),
                      "a restart must say what it resumed instead of losing it silently")
        held = [m for queue in restarted.outbox.values() for m in queue]
        self.assertEqual(1, len(held))
        self.assertEqual(message["message_id"], held[0].id)
        self.assertEqual("held through a restart", held[0].text)
        self.assertEqual("queued", held[0].status)

    def test_a_delivered_message_is_not_resurrected(self):
        first = dunk_core.Flock(herdr=self.herdr, state_file=self.state,
                                persist=True, restore=False)
        first.herdr.statuses[self.pane] = "idle"
        first.send_now(self.pane, "this one lands", hold_while_typing=True, wait_s=3)
        first._save()
        restarted = dunk_core.Flock(herdr=self.herdr, state_file=self.state,
                                    persist=True, restore=True)
        self.assertEqual([], [m for queue in restarted.outbox.values() for m in queue],
                         "only QUEUED messages are held; a sent one must not come back")


if __name__ == "__main__":
    unittest.main()
