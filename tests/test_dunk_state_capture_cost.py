"""t496: one capture, one /proc walk.

Measured on the deployed revision 895c05a (31 lanes, 60 panes): a capture took
7.98s in-process, of which 5.30s was `_proc_children` - called 62 times, twice
per lane - because `session_of()` and the env lookup both accept a prebuilt
pid->children map and neither was given one.

Through the daemon on a busy fleet that pushed the client's 15s budget over the
edge ("error: no answer after 15s from the daemon", exit 3) while the daemon had
used only ~1.1s of CPU: it was polling /proc, not waiting on a lock. Cheap calls
made during a capture also degraded from ~0.3s to 2.9-3.4s, which is head-of-line
blocking behind that scan, not a second defect.

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

    def test_a_pane_without_a_pid_still_needs_no_walk(self):
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
