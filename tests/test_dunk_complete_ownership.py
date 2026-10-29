"""t423: completing another lane's task must be refused with no effects.

Measured incident on live 0b6253e: worker %20 completed t399 owned by %71,
which also triggered an automatic t422 claim. task_complete/TaskStore.complete
performed the mutation with no ownership check. These fixtures use invented
panes (w9:p7 owner, w1:p1 foreign worker, w9:p9 operator) through the real
Flock.task_complete wrapper: the owned-task foreign completion must raise
BEFORE any status/event/handoff change.
"""

import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
from dunk_core import DunkError, Flock  # noqa: E402
from dunk_inbox import Inbox  # noqa: E402
from dunk_superusers import Superusers  # noqa: E402
from dunk_tasks import GLOBAL, Tasks  # noqa: E402
from fakes import FakeHerdr  # noqa: E402

OWNER = "w9:p7"      # the task owner (t399's %71)
FOREIGN = "w1:p1"    # the mistaken worker (incident's %20)
OPERATOR = "w9:p9"   # allowlisted CoS/VP operator pane


class CompleteOwnershipTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.herdr = FakeHerdr()
        self.superusers = Superusers(
            path=os.path.join(self.tmp.name, "superusers.json"))
        self.flock = Flock(
            herdr=self.herdr,
            state_file=os.path.join(self.tmp.name, "dunks.json"),
            persist=False,
            restore=False,
            inbox=Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                        blob_dir=os.path.join(self.tmp.name, "blobs")),
            tasks=Tasks(path=os.path.join(self.tmp.name, "tasks.db")),
            superusers=self.superusers,
        )
        self.flock.AGENT_VERIFY_S = 0.1

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def _owned_task(self):
        task = self.flock.task_add("owner work", self_pane_id=OWNER)
        return self.flock.task_assign(task["id"], [OWNER], self_pane_id=OWNER)

    def _completed_events(self, task_id):
        rows = self.flock.tasks.db.execute(
            "SELECT * FROM task_events WHERE task_id = ? AND event = ?",
            (task_id, "completed")).fetchall()
        return rows

    def test_foreign_worker_completion_is_refused_with_no_effects(self):
        """The measured incident shape: must raise, changing nothing."""
        task = self._owned_task()
        before_leases = self.flock.tasks.leases_for(task["id"])
        with self.assertRaisesRegex(DunkError, "not.*complet|owned|owner"):
            self.flock.task_complete(task["id"], self_pane_id=FOREIGN)
        current = self.flock.tasks.get(task["id"])
        self.assertNotEqual("done", current["status"])
        self.assertIsNone(current["completed_by"])
        self.assertEqual(before_leases, self.flock.tasks.leases_for(task["id"]))
        self.assertEqual([], self._completed_events(task["id"]))
        self.assertEqual(
            [], [h for h in self.flock.tasks.next_job_handoffs(FOREIGN)
                 if h.get("from_task") == task["id"]])

    def test_owner_completion_passes(self):
        task = self._owned_task()
        done = self.flock.task_complete(task["id"], self_pane_id=OWNER)
        self.assertEqual("done", done["status"])
        self.assertEqual(OWNER, done["completed_by"])

    def test_lease_holder_completion_passes(self):
        task = self.flock.task_add("claimed work", self_pane_id=FOREIGN)
        claimed = self.flock.tasks.claim_next(GLOBAL, FOREIGN, lease_seconds=300)
        self.assertEqual(task["id"], claimed["task"]["id"])
        done = self.flock.task_complete(task["id"], self_pane_id=FOREIGN)
        self.assertEqual("done", done["status"])

    def test_authorized_operator_completion_passes(self):
        self.superusers.add(OPERATOR)
        task = self._owned_task()
        done = self.flock.task_complete(task["id"], self_pane_id=OPERATOR)
        self.assertEqual("done", done["status"])

    def test_spoofed_by_attribution_is_denied(self):
        """Forging the owner's id in `by` (or a VP-sounding label) grants nothing:
        authorization keys on the requesting pane, never on attribution."""
        task = self._owned_task()
        with self.assertRaises(DunkError):
            self.flock.task_complete(task["id"], by=OWNER, self_pane_id=FOREIGN)
        self.assertNotEqual("done", self.flock.tasks.get(task["id"])["status"])

    def test_anonymous_completion_of_owned_task_is_refused(self):
        task = self._owned_task()
        with self.assertRaises(DunkError):
            self.flock.task_complete(task["id"])
        self.assertNotEqual("done", self.flock.tasks.get(task["id"])["status"])

    def test_anonymous_completion_is_refused_even_unowned(self):
        """Unattributed callers complete nothing: unowned means leader-owned,
        so there is no ownerless completion. The named workaround is --self-pane."""
        task = self.flock.tasks.add(GLOBAL, "legacy unowned work")
        self.assertEqual([], task["assignees"])
        with self.assertRaisesRegex(DunkError, "self-pane"):
            self.flock.task_complete(task["id"])
        self.assertNotEqual("done", self.flock.tasks.get(task["id"])["status"])


if __name__ == "__main__":
    unittest.main()
