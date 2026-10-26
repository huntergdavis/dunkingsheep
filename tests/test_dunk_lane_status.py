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
import dunk_activity  # noqa: E402
from dunk_core import Flock  # noqa: E402
from dunk_views import roster  # noqa: E402
from fakes import FakeHerdr  # noqa: E402


def tmux_shaped(herdr):
    """A backend that cannot answer a status says 'unknown' - that is the tmux case t227 is
    about. The fake ships herdr-ish values, so the tests impose the shape they mean."""
    for pane in herdr.panes:
        pane["agent_status"] = "unknown"
    return herdr


class LaneStatusTests(unittest.TestCase):
    def setUp(self):
        self.herdr = tmux_shaped(FakeHerdr())
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

    def test_the_roster_and_the_lane_status_agree_on_an_expired_turn(self):
        """Reviewer's bad case: the roster used to say IDLE for a turn that never ended."""
        pane = {"pane_id": "%1", "agent": "pi", "agent_status": "unknown"}
        self.flock.turn_start("%1", source="test", ttl_s=1)
        self.flock.turns["%1"]["expires_at"] = time.time() - 3600
        self.assertIsNone(self.flock.lane_status(pane), "lane_status must not call it idle")
        doing = dunk_activity.activity(pane, turns=self.flock.turns)
        self.assertEqual("unknown", doing["state"], "nor may the roster")
        self.assertIn("expired", doing["reason"])

    def test_the_roster_emits_null_not_unknown_for_a_silent_lane(self):
        """Reviewer's (b): roster() re-materialised 'unknown' on the desk's path."""
        rows = roster(self.flock)["lanes"]
        self.assertTrue(rows, "the fake has lanes")
        for row in rows:
            self.assertIsNone(row["pane_status"],
                              "a lane with no status signal must emit null, not 'unknown'")

    def test_the_pane_list_carries_the_derived_status(self):
        pane_id = self.herdr.panes[0]["pane_id"]
        self.flock.turn_start(pane_id, source="test")
        panes = {p["pane_id"]: p for p in self.flock.list_panes()}
        self.assertEqual("working", panes[pane_id]["agent_status"])


if __name__ == "__main__":
    unittest.main()


class DispatchEligibilityTests(unittest.TestCase):
    """The other half of t227 (reviewer): before this change NO lane was dispatch-eligible.

    The dispatch tick filters `agent_status not in (None, "unknown")`. While every pane reported
    'unknown' that list was empty, so the tick had no lane to feed. This pins the new shape: a
    lane with a turn signal becomes eligible, a silent lane does not.
    """

    def setUp(self):
        self.herdr = tmux_shaped(FakeHerdr())
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)

    def tearDown(self):
        self.flock.close()

    def _eligible(self):
        return [pane["pane_id"] for pane in self.flock.resolve_lanes()
                if pane.get("agent_status") not in (None, "unknown")]

    def test_a_lane_with_no_signal_is_not_dispatch_eligible(self):
        self.assertEqual([], self._eligible())

    def test_a_working_lane_is_dispatch_eligible(self):
        pane_id = self.herdr.panes[0]["pane_id"]
        self.flock.turn_start(pane_id, source="test")
        self.assertEqual([pane_id], self._eligible())
