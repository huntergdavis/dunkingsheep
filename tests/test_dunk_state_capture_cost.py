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


class StalePidBindingTests(unittest.TestCase):
    """t505 [P2]: a pid from the snapshot is not an identity.

    The reviewer's reproducer, reduced: the snapshot records pane -> wrapper ->
    agent, then the agent pid is recycled under an unrelated root. The cached
    pid's CURRENT argv names a foreign session. Trusting it attributes another
    lane's session, model and effort to this pane with every reason NULL.
    """

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.store = dunk_state.StateStore(
            path=os.path.join(self.tmp.name, "state.db"))

    def test_recycled_descendant_is_not_adopted(self):
        kids = {1001: [1101], 1101: [1201], 9000: [1201]}
        starts = {1001: 1, 1101: 2, 1201: 3}          # snapshot saw 1201 start=3

        def fake_start(pid):
            # 1201 was recycled: it now belongs to a process that started later
            return 999 if int(pid) == 1201 else starts.get(int(pid))

        def fake_cmdline(pid):
            if int(pid) == 1201:
                return ["pi", "--session", "foreign-session"]
            return []

        with mock.patch.object(dunk_state, "_proc_start_ticks", fake_start), \
                mock.patch.object(dunk_state, "_cmdline", fake_cmdline):
            session, reason = dunk_state.session_of(1001, kids=kids, starts=starts)
        self.assertIsNone(session, "adopted a recycled pid's session")
        self.assertIn("pid reuse", reason)

    def test_a_live_descendant_is_still_adopted(self):
        """The control: same shape, process NOT recycled, session is read."""
        kids = {1001: [1101], 1101: [1201]}
        starts = {1001: 1, 1101: 2, 1201: 3}

        def fake_cmdline(pid):
            if int(pid) == 1201:
                return ["pi", "--session", "our-session"]
            return []

        with mock.patch.object(dunk_state, "_proc_start_ticks",
                               lambda pid: starts.get(int(pid))), \
                mock.patch.object(dunk_state, "_cmdline", fake_cmdline):
            session, _reason = dunk_state.session_of(1001, kids=kids, starts=starts)
        self.assertEqual("our-session", session)

    def test_unknown_identity_is_not_the_same_process(self):
        """A pid absent from the snapshot, or with a different start, is NOT it.

        `starts` of None/{} is the documented legacy path (a caller that holds a
        map but no snapshot); capture always supplies a real snapshot, so the
        check is live on the path that matters.
        """
        self.assertFalse(dunk_state._still_the_same_process(4242, {4242: 7}))
        self.assertFalse(dunk_state._still_the_same_process(4242, {9999: 1}))
        self.assertTrue(dunk_state._still_the_same_process(4242, {}))

    def test_capture_never_reads_env_of_a_recycled_pid(self):
        """End to end: the foreign environment must not reach the row."""
        lanes = [lane("%1", os.getpid())]
        real_snapshot = dunk_state._proc_snapshot

        def snapshot_with_recycled_child():
            kids, starts = real_snapshot()
            # pretend our own pid's child is a pid that has since been reused
            kids.setdefault(os.getpid(), []).append(999999)
            starts[999999] = 1                      # a start time it does not have
            return kids, starts

        read = {"env": 0}
        real_env = dunk_state._env_of

        def counting_env(pid):
            # The pane's own wrapper carries no agent environment, so the loop
            # must walk to the descendant - that is what makes this control
            # discriminating: with the identity check removed, the recycled pid
            # is read and its foreign session reaches the row.
            if int(pid) == 999999:
                read["env"] += 1
                return {"PI_SESSION_ID": "foreign", "PI_MODEL": "foreign-model"}
            return {}

        with mock.patch.object(dunk_state, "_proc_snapshot",
                               snapshot_with_recycled_child), \
                mock.patch.object(dunk_state, "_env_of", counting_env), \
                mock.patch.object(dunk_state.dunk_activity, "activity",
                                  lambda *a, **k: {"state": None, "source": "t",
                                                   "reason": "t"}):
            result = dunk_state.capture(_FakeFlock(lanes), source="test",
                                        store=self.store)
        self.assertEqual(1, result["rows"])
        self.assertEqual(0, read["env"], "read the environment of a recycled pid")
        row = self.store.latest(limit=1)[0]
        self.assertNotEqual("foreign", row.get("session_id"))


class OneProcWalkPerCapture(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.store = dunk_state.StateStore(
            path=os.path.join(self.tmp.name, "state.db"))

    def _capture_counting_proc_walks(self, lanes):
        calls = {"n": 0}
        real = dunk_state._proc_snapshot

        def counting():
            calls["n"] += 1
            return real()

        with mock.patch.object(dunk_state, "_proc_snapshot", counting):
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

    def test_one_snapshot_feeds_both_maps(self):
        kids, starts = dunk_state._proc_snapshot()
        self.assertIsInstance(kids, dict)
        self.assertIsInstance(starts, dict)
        # the snapshot records start times for the same pids it lists as children
        for children in kids.values():
            for child in children:
                self.assertIn(child, starts)


if __name__ == "__main__":
    unittest.main(verbosity=2)
