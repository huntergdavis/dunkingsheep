"""t496: one capture, one /proc walk.

Measured on the deployed revision 895c05a (31 lanes, 60 panes): `_proc_children`
was called 62 times per capture - twice per lane - because `session_of()` and the
env lookup both accept a prebuilt pid->children map and neither was given one. In
sampled in-process runs that scan was the largest single cost (5.30s of an 8.0s
capture on one sample, 4.55s of a 6.21s capture on another - separate runs, not a
range).

Scope of that evidence, stated because reviewer t500 was right to draw it: these
are in-process samples with a fresh Flock and a temporary store. They establish
the scan reduction; they do NOT establish that the scan was the sole cause of the
observed "no answer after 15s" failure, nor that no daemon-wide lock was contended
during it.

These tests pin the call COUNT rather than the wall clock, so they are
deterministic on any machine: a regression that reintroduces a per-lane walk
fails here even on a fast box where the timing would look fine.
"""

import os
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import dunk_state  # noqa: E402


class _FakeTasks:
    def leases_for_pane(self, pane_id, now=None):
        return []


class _FakeFlock:
    """Just enough Flock for capture(): lanes, tasks, and no claude status."""

    claude_status = None
    turns = None

    def __init__(self, lanes):
        self._lanes = lanes
        self.tasks = _FakeTasks()

    def resolve_lanes(self):
        return list(self._lanes)


def lane(pane_id, pid):
    return {"pane_id": pane_id, "pane_pid": pid, "agent": "pi",
            "cwd": "/tmp", "project": "p", "harness": "pi"}


class OneProcWalkPerCapture(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.store = dunk_state.StateStore(
            path=os.path.join(self.tmp.name, "state.db"))

    def _capture_counting_proc_walks(self, lanes):
        calls = {"n": 0}
        real = dunk_state._proc_children

        def counting():
            calls["n"] += 1
            return real()

        with mock.patch.object(dunk_state, "_proc_children", counting):
            with mock.patch.object(dunk_state.dunk_activity, "activity",
                                   lambda *a, **k: {"state": None, "source": "test",
                                                    "reason": "test"}):
                result = dunk_state.capture(_FakeFlock(lanes),
                                            source="test", store=self.store)
        return calls["n"], result

    def test_capture_walks_proc_once_not_twice_per_lane(self):
        lanes = [lane("%1", os.getpid()), lane("%2", os.getpid()),
                 lane("%3", os.getpid()), lane("%4", os.getpid())]
        walks, result = self._capture_counting_proc_walks(lanes)
        self.assertEqual(1, walks,
                         "capture rebuilt the /proc map %d times for %d lanes"
                         % (walks, len(lanes)))
        self.assertEqual(len(lanes), result["rows"])

    def test_rows_still_carry_a_session_field_and_their_pane(self):
        """The saving must not change what a row says."""
        lanes = [lane("%7", os.getpid())]
        _, result = self._capture_counting_proc_walks(lanes)
        self.assertEqual(1, result["rows"])
        rows = self.store.latest(limit=5)
        row = rows and rows[0]
        self.assertIsNotNone(row)
        for key in ("pane_id", "project", "harness", "session_id",
                    "session_reason", "state", "state_source"):
            self.assertIn(key, row)
        self.assertEqual("%7", row["pane_id"])

    def test_a_pane_without_a_pid_still_costs_exactly_one_walk(self):
        """The map is built eagerly, so a capture with no pid still scans once.

        That is a deliberate tradeoff (reviewer t500): one scan keeps the map
        identity shared by both consumers, at the cost of a scan even when no
        lane has a pid. It is not an empirical claim that this beats every lazy
        implementation - only that it is the tradeoff this fix chose.
        """
        lanes = [lane("%8", None)]
        walks, result = self._capture_counting_proc_walks(lanes)
        self.assertEqual(1, walks)
        self.assertEqual(1, result["rows"])

    def test_session_of_accepts_a_prebuilt_map(self):
        """The parameter existed and was ignored; it is the whole fix."""
        kids = {1: [2], 2: [3]}
        self.assertEqual([2, 3], dunk_state._descendants(1, kids))
        with mock.patch.object(dunk_state, "_proc_children",
                               side_effect=AssertionError("walked /proc")):
            self.assertEqual([2, 3], dunk_state._descendants(1, kids))


if __name__ == "__main__":
    unittest.main(verbosity=2)
