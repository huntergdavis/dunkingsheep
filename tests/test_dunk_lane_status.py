"""t227: a lane's STATUS comes from evidence, and 'unknown' is not an answer.

The ops board showed 'unknown' for every lane. The tmux backend has no agent-status API and
says "unknown" honestly; the board was rendering that as a status. But the daemon does have a
turn signal - the harness calls turn start/end, and turn_end keeps the ended state because that
is the only evidence of idleness we get - so a lane's status is derivable:

    declared, unexpired turn -> working
    ended turn               -> idle
    no turn signal at all    -> None   (the desk shows an em dash)
"""

import os
import sys
import time
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
from dunk_core import Flock  # noqa: E402
from fakes import FakeHerdr  # noqa: E402


class LaneStatusTests(unittest.TestCase):
    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)

    def tearDown(self):
        self.flock.close()

    def test_a_lane_with_no_turn_signal_is_none_not_unknown(self):
        pane = {"pane_id": "%1", "agent": "pi", "agent_status": "unknown"}
        self.assertIsNone(self.flock.lane_status(pane),
                          "an unanswerable backend must say nothing, not 'unknown'")

    def test_a_declared_turn_is_working(self):
        pane = {"pane_id": "%1", "agent": "pi", "agent_status": "unknown"}
        self.flock.turn_start("%1", source="test")
        self.assertEqual("working", self.flock.lane_status(pane))

    def test_an_ended_turn_is_idle(self):
        pane = {"pane_id": "%1", "agent": "pi", "agent_status": "unknown"}
        self.flock.turn_start("%1", source="test")
        self.flock.turn_end("%1", reason="test")
        self.assertEqual("idle", self.flock.lane_status(pane),
                         "turn_end keeps the ended state precisely so idleness is visible")

    def test_an_expired_turn_with_no_end_is_neither_working_nor_idle(self):
        """The TTL exists so a crashed agent cannot hold mail forever - not to call it idle."""
        pane = {"pane_id": "%1", "agent": "pi", "agent_status": "unknown"}
        self.flock.turn_start("%1", source="test", ttl_s=1)
        self.flock.turns["%1"]["expires_at"] = time.time() - 1
        self.assertIsNone(self.flock.lane_status(pane))

    def test_an_old_end_stops_claiming_idle(self):
        """Idle is a claim about NOW; old evidence cannot support it (reviewer ask)."""
        pane = {"pane_id": "%1", "agent": "pi", "agent_status": "unknown"}
        self.flock.turn_start("%1", source="test")
        self.flock.turn_end("%1", reason="test")
        self.assertEqual("idle", self.flock.lane_status(pane))
        self.flock.turns["%1"]["ended_at"] = time.time() - self.flock.IDLE_CLAIM_S - 60
        self.assertIsNone(self.flock.lane_status(pane))

    def test_an_end_just_inside_the_window_is_still_idle(self):
        pane = {"pane_id": "%1", "agent": "pi", "agent_status": "unknown"}
        self.flock.turn_start("%1", source="test")
        self.flock.turn_end("%1", reason="test")
        self.flock.turns["%1"]["ended_at"] = time.time() - self.flock.IDLE_CLAIM_S + 60
        self.assertEqual("idle", self.flock.lane_status(pane))

    def test_a_backend_that_can_answer_wins(self):
        pane = {"pane_id": "%1", "agent": "pi", "agent_status": "done"}
        self.assertEqual("done", self.flock.lane_status(pane))

    def test_the_pane_list_carries_the_derived_status(self):
        pane_id = self.herdr.panes[0]["pane_id"]
        self.flock.turn_start(pane_id, source="test")
        panes = {p["pane_id"]: p for p in self.flock.list_panes()}
        self.assertEqual("working", panes[pane_id]["agent_status"])


if __name__ == "__main__":
    unittest.main()
