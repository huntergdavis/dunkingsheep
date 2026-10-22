"""Per-caller CPU sampling: the pid+starttime guard, process trees, honest GPU.

The central rule: a recycled pid produces NO sample. A wrong attribution is
worse than a missing one.
"""

import os
import tempfile
import unittest
from pathlib import Path

from tests import _bootstrap  # noqa: F401 - temp DUNKINGSHEEP_DIR first

import dunk_load


class GuardTests(unittest.TestCase):
    def test_a_recycled_pid_produces_no_sample(self):
        me = os.getpid()
        stat = dunk_load._stat(me)
        self.assertIsNotNone(stat)
        # The caller was registered with a different starttime: pid was reused.
        rows = dunk_load.sample([("lane", me, stat["starttime"] + 1)])
        self.assertEqual([], rows["callers"])
        self.assertEqual("pid reused (starttime differs)", rows["skipped"][0]["reason"])

    def test_a_matching_starttime_is_sampled(self):
        me = os.getpid()
        stat = dunk_load._stat(me)
        rows = dunk_load.sample([("lane", me, stat["starttime"])])
        self.assertEqual(1, len(rows["callers"]))
        self.assertEqual(me, rows["callers"][0]["pid"])

    def test_a_vanished_pid_is_skipped_not_an_error(self):
        rows = dunk_load.sample([("gone", 2**31 - 1, 1)])
        self.assertEqual([], rows["callers"])
        self.assertEqual("process gone", rows["skipped"][0]["reason"])

    def test_the_tree_includes_descendants(self):
        me = os.getpid()
        ppids = {me: 1, me + 1: me, me + 2: me + 1, 999999: 1}
        tree = dunk_load.tree_of(me, ppids)
        self.assertIn(me, tree)
        self.assertIn(me + 1, tree)   # child
        self.assertIn(me + 2, tree)   # grandchild
        self.assertNotIn(999999, tree)

    def test_gpu_is_null_with_a_reason(self):
        gpu = dunk_load.gpu()
        if gpu["value"] is not None:
            self.skipTest("this host exposes a per-process GPU counter")
        self.assertTrue(gpu["reason"])

    def test_unattributed_is_reported_not_dropped(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "load-state.json"
            original = dunk_load.STATE
            dunk_load.STATE = path
            try:
                me = os.getpid()
                start = dunk_load._stat(me)["starttime"]
                dunk_load.collect([("lane", me, start)], now=1000.0)
                result = dunk_load.collect([("lane", me, start)], now=1030.0)
            finally:
                dunk_load.STATE = original
        self.assertIn("unattributed_cores", result)
        self.assertIsNotNone(result["unattributed_cores"])
        self.assertIn("attributed_cores", result)


if __name__ == "__main__":
    unittest.main()
