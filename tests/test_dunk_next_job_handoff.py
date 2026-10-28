"""t372: the durable handoff record clauses 1 and 4 are checked against.

It is read from OUTSIDE by the acceptance instrument, so the contract is the event name plus
the shape, not the code that writes it.
"""

import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
from dunk_tasks import Tasks  # noqa: E402

PANE = "%4"


class NextJobHandoffTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="handoff-")
        self.store = Tasks(os.path.join(self.tmp, "tasks.db"))
        self.a = self.store.add("global", "A: the task that finished")
        self.b = self.store.add("global", "B: the next job")
        self.store.claim_next(scope="global", pane_id=PANE)

    def _events(self, task_id, event):
        rows = self.store.db.execute(
            "SELECT * FROM task_events WHERE task_id = ? AND event = ? ORDER BY at",
            (task_id, event)).fetchall()
        return [dict(r) for r in rows]

    def test_the_record_uses_a_unique_event_name_and_carries_the_shape(self):
        out = self.store.record_next_job_handoff(
            self.b["id"], pane_id=PANE, from_task=self.a["id"],
            message_id="m8454", attempt=1, reason="delivered")
        rows = self._events(self.b["id"], "next_job_handoff")
        self.assertEqual(1, len(rows), "the event name is unique, so a PR handoff cannot produce it")
        import json as _json
        meta = _json.loads(rows[0].get("meta") or "{}")
        self.assertEqual(self.a["id"], meta.get("from_task"))
        self.assertEqual("m8454", meta.get("message_id"))
        self.assertEqual(1, meta.get("attempt"))
        self.assertEqual(PANE, rows[0].get("actor"))
        self.assertEqual("m8454", out["message_id"])

    def test_a_record_without_the_shape_does_not_count(self):
        """Clause 4: a note with the right name and no shape is not a handoff."""
        self.store.record_next_job_handoff(self.b["id"], pane_id=PANE, from_task=None,
                                           message_id=None, attempt=0, reason="pending")
        rows = self._events(self.b["id"], "next_job_handoff")
        import json as _json
        meta = _json.loads(rows[0].get("meta") or "{}")
        valid = bool(meta.get("from_task")) and bool(meta.get("message_id"))
        self.assertFalse(valid, "shape-validated count must ignore a record with no ids")

    def test_the_plain_handoff_name_is_untouched(self):
        """The PR-handoff meaning of event='handoff' must stay available to other lanes."""
        self.store._record(self.a["id"], "handoff", actor="%35",
                           meta={"reason": "PR handed to reviewer"})
        self.assertEqual(1, len(self._events(self.a["id"], "handoff")))
        self.assertEqual([], self._events(self.a["id"], "next_job_handoff"))


if __name__ == "__main__":
    unittest.main()
