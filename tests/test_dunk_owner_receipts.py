"""t66: a read-only arrival receipt for owner pushes.

The desk can see that a push is pending or that counters moved, but neither proves a PARTICULAR message
arrived. created_at/taken_at do - and a receipt must never consume the message it reports on, so this
endpoint reads and never calls take().
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
import dunk_owner  # noqa: E402
import dunk_api  # noqa: E402


class OwnerReceiptTests(unittest.TestCase):
    def setUp(self):
        self.store = dunk_owner.OwnerPush(":memory:")

    def test_an_untaken_push_reports_created_at_and_null_taken_at(self):
        pushed = self.store.push("%9", "hello", subject="s")
        rows = self.store.receipts([pushed["id"]])
        self.assertEqual(1, len(rows))
        self.assertEqual(pushed["id"], rows[0]["id"])
        self.assertEqual("%9", rows[0]["pane_id"])
        self.assertIsNotNone(rows[0]["created_at"])
        self.assertIsNone(rows[0]["taken_at"], "not yet arrived is null, not absent")

    def test_a_taken_push_reports_the_arrival_time(self):
        pushed = self.store.push("%9", "hello")
        taken = self.store.take("%9")
        self.assertEqual(1, len(taken))
        rows = self.store.receipts([pushed["id"]])
        self.assertIsNotNone(rows[0]["taken_at"])
        self.assertGreaterEqual(rows[0]["taken_at"], rows[0]["created_at"])

    def test_reading_a_receipt_does_NOT_consume_the_message(self):
        pushed = self.store.push("%9", "hello")
        self.store.receipts([pushed["id"]])
        # still pending: a receipt must not be a take
        self.assertEqual(1, len(self.store.take("%9")),
                         "reading a receipt consumed the message - that is a mutation, not a receipt")

    def test_unknown_ids_are_absent_not_null(self):
        pushed = self.store.push("%9", "hello")
        rows = self.store.receipts([pushed["id"], 999999])
        self.assertEqual([pushed["id"]], [r["id"] for r in rows],
                         "an unknown id must be absent so 'no such push' is distinguishable")

    def test_no_body_or_subject_is_returned(self):
        pushed = self.store.push("%9", "SECRET BODY", subject="SECRET SUBJECT")
        row = self.store.receipts([pushed["id"]])[0]
        self.assertEqual({"id", "pane_id", "created_at", "taken_at"}, set(row))

    def test_the_batch_is_bounded_and_rejects_junk(self):
        ids = [self.store.push("%9", f"m{i}")["id"] for i in range(3)]
        self.assertEqual(3, len(self.store.receipts(ids)))
        self.assertEqual([], self.store.receipts([]))
        self.assertEqual([], self.store.receipts(["not-an-int", -4, 0]))
        many = self.store.receipts(list(range(1, 500)))
        self.assertLessEqual(len(many), 100, "the endpoint is bounded to 100 ids")

    def test_the_handler_is_registered_and_read_only(self):
        self.assertIn("owner_receipts", {c.name for c in dunk_api.COMMANDS})
        source = open(os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                                   "dunk_api.py")).read()
        handler = source.split("def h_owner_receipts")[1].split("\ndef ")[0]
        code = "\n".join(line for line in handler.splitlines()
                         if not line.strip().startswith(("#", "A receipt must", "taken_at and")))
        self.assertNotIn(".take(", code, "the receipt handler must never call take()")


if __name__ == "__main__":
    unittest.main()


class TruncationTests(unittest.TestCase):
    """t319: an oversized request must not look like 'those ids do not exist'.

    The cap stays (it protects the store), but the RESPONSE says how many ids were asked for and whether the
    answer was cut, so a caller can distinguish 'I asked too much' from 'unknown ids'.
    """

    def setUp(self):
        self.store = dunk_owner.OwnerPush(":memory:")

    def test_a_small_request_is_not_truncated(self):
        ids = [self.store.push("%9", f"m{i}")["id"] for i in range(3)]
        self.assertEqual(3, len(self.store.receipts(ids)))
        self.assertFalse(self.store.last_receipts_meta["truncated"])
        self.assertEqual(3, self.store.last_receipts_meta["requested"])

    def test_an_oversized_request_says_it_was_truncated(self):
        ids = list(range(1, 251))
        self.store.receipts(ids)
        meta = self.store.last_receipts_meta
        self.assertTrue(meta["truncated"], "a silent cap is indistinguishable from unknown ids")
        self.assertEqual(250, meta["requested"])
        self.assertLessEqual(meta["returned"], 100)

    def test_the_handler_reports_requested_returned_and_truncated(self):
        source = open(os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                                   "dunk_api.py")).read()
        handler = source.split("def h_owner_receipts")[1].split("\ndef ")[0]
        for key in ('"requested"', '"returned"', '"truncated"'):
            self.assertIn(key, handler, "the response must state complete-vs-truncated explicitly")

    def test_the_cap_still_protects_the_store(self):
        ids = list(range(1, 251))
        self.store.receipts(ids)
        self.assertLessEqual(self.store.last_receipts_meta["returned"], 100)
