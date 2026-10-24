"""Retired panes: silence with a reason, not deletion.

A superseded pane's mail is the record of what was tried. Retiring stops the
alerts and the feeding while keeping every row, and the count is still reported so
a lane cannot vanish from the fleet unnoticed.
"""
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _bootstrap  # noqa: E402,F401

import dunk_panes  # noqa: E402


class RetireTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = os.path.join(self.tmp.name, "retired.json")

    def tearDown(self):
        self.tmp.cleanup()

    def test_a_reason_is_required(self):
        with self.assertRaises(dunk_panes.RetireError):
            dunk_panes.retire("wW:p4", "", by="w9:pH", path=self.path)
        with self.assertRaises(dunk_panes.RetireError):
            dunk_panes.retire("", "superseded", by="w9:pH", path=self.path)

    def test_all_spellings_of_the_mailbox_are_retired(self):
        dunk_panes.retire("wW:p4", "superseded-by:wW:p7", by="w9:pH",
                          labels=["par-d"], path=self.path)
        for mailbox in ("wW:p4", "par-d (wW:p4)", "par-d"):
            self.assertTrue(dunk_panes.mailbox_retired(mailbox, path=self.path), mailbox)
        self.assertFalse(dunk_panes.mailbox_retired("par-d2 (wW:p7)", path=self.path))
        self.assertFalse(dunk_panes.mailbox_retired("wW:p7", path=self.path))

    def test_retiring_a_pane_that_no_longer_exists_still_records_it(self):
        """The case retire exists FOR: the pane is superseded, often already gone.

        A retirement that needs the pane to be resolvable would fail on exactly the
        panes we retire - superseded ones, whose replacement lives in another pane.
        """
        record = dunk_panes.retire("wW:p4", "superseded-by:wW:p7", by="w9:pH",
                                   labels=["par-d"], path=self.path)
        self.assertEqual("wW:p4", record["pane_id"])
        self.assertTrue(dunk_panes.mailbox_retired("par-d (wW:p4)", path=self.path))

    def test_a_mailbox_name_that_outlives_its_pane_is_retired_too(self):
        """The fifth name-vs-id instance: mail filed under 'crackdown-codex' kept
        alerting after the pane was retired, because retire recorded the pane id and
        the label, not the mailbox name."""
        dunk_panes.retire("wS:p2", "shelved", by="w9:pH",
                          labels=["crackdown-codex"], mailboxes=["crackdown-codex"],
                          path=self.path)
        self.assertTrue(dunk_panes.mailbox_retired("crackdown-codex", path=self.path))
        self.assertTrue(dunk_panes.mailbox_retired("wS:p2", path=self.path))

    def test_unretire_brings_a_pane_back(self):
        dunk_panes.retire("wW:p1", "replaced-by:dispatcher", by="w9:pH", path=self.path)
        self.assertTrue(dunk_panes.is_retired("wW:p1", path=self.path))
        dunk_panes.unretire("wW:p1", path=self.path)
        self.assertFalse(dunk_panes.is_retired("wW:p1", path=self.path))

    def test_the_file_is_written_atomically_and_reads_back(self):
        dunk_panes.retire("wW:p4", "superseded", by="w9:pH", path=self.path)
        self.assertFalse(os.path.exists(self.path + ".tmp"), "the temp file must not be left behind")
        records = dunk_panes.all_retired(path=self.path)
        self.assertEqual(["wW:p4"], [record["pane_id"] for record in records])
        self.assertEqual("superseded", records[0]["reason"])

    def test_an_absent_file_means_nothing_is_retired(self):
        self.assertEqual([], dunk_panes.all_retired(path=self.path))
        self.assertFalse(dunk_panes.mailbox_retired("wW:p4", path=self.path))


if __name__ == "__main__":
    unittest.main()
