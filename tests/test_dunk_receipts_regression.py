"""t319: the two defects the reviewer measured, pinned by tests that fail on the PRE-FIX head.

A test written after a fix proves the fix compiles; these are written to be SHAPE-AGNOSTIC so they run
against both implementations - the pre-fix head returns a list and stashes counts on the store, the fixed
head returns a dict with the counts in it. The helper below reads whichever is there, so the same test
fails on 4779aa2 and passes on the fixed head, which is the only form of red-first that survives an API
change.
"""

import os
import sys
import threading
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
import dunk_owner  # noqa: E402


def counts(store, push_ids):
    """(requested, returned, truncated) from either implementation shape."""
    out = store.receipts(push_ids)
    if isinstance(out, dict):                      # fixed head: counts travel with the rows
        return out["requested"], out["returned"], out["truncated"]
    meta = getattr(store, "last_receipts_meta", {}) or {}   # pre-fix head: stashed on the store
    return meta.get("requested"), meta.get("returned"), meta.get("truncated")


class ReceiptsRegressionTests(unittest.TestCase):
    def setUp(self):
        self.store = dunk_owner.OwnerPush(":memory:")

    def test_an_empty_request_reports_its_own_counts_not_the_previous_ones(self):
        """DEFECT 1 (measured on 4779aa2: receipts([]) inherited 150/True)."""
        counts(self.store, list(range(1, 151)))
        requested, returned, truncated = counts(self.store, [])
        self.assertEqual((0, 0, False), (requested, returned, truncated),
                         "an empty request must report its own counts, not the previous call's")

    def test_junk_reports_its_own_counts_not_the_previous_ones(self):
        """DEFECT 1, second shape."""
        counts(self.store, list(range(1, 151)))
        requested, returned, truncated = counts(self.store, ["nope", -3, 0])
        self.assertEqual((0, 0, False), (requested, returned, truncated))

    def test_concurrent_callers_do_not_read_each_others_counts(self):
        """DEFECT 2 (measured on 4779aa2: the second call's counts were visible to the first)."""
        results = {}

        def ask(tag, ids):
            results[tag] = counts(self.store, ids)

        a = threading.Thread(target=ask, args=("a", list(range(1, 151))))
        b = threading.Thread(target=ask, args=("b", [1, 2]))
        a.start(); b.start(); a.join(); b.join()
        self.assertEqual(150, results["a"][0], "A asked for 150")
        self.assertTrue(results["a"][2], "A's request was truncated")
        self.assertEqual(2, results["b"][0], "B asked for 2")
        self.assertFalse(results["b"][2], "B's request was not truncated")

    def test_nothing_is_stored_on_the_store_after_a_call(self):
        """The fixed head has no attribute to go stale or race; the pre-fix head does."""
        counts(self.store, [1, 2, 3])
        self.assertFalse(hasattr(self.store, "last_receipts_meta"),
                         "counts must be part of the return, not state on the callee")


if __name__ == "__main__":
    unittest.main()
