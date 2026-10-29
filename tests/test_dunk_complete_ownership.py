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
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
from dunk_callers import process_start  # noqa: E402
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

    # -- P1: kernel binding of the pane claim -------------------------------

    def _set_pane_pid(self, pane_id, pid):
        for pane in self.herdr.panes:
            if pane["pane_id"] == pane_id:
                pane["pane_pid"] = pid
                return pane
        raise AssertionError(f"no such fake pane {pane_id}")

    def _caller_for(self, pid):
        return {"pane_id": None, "pid": pid, "start": process_start(pid),
                "name": "test", "source": "test"}

    def test_bound_owner_completion_passes(self):
        """A kernel-bound owner is unaffected by the binding check."""
        task = self.flock.task_add("bound work", self_pane_id="w1:p1")
        self.flock.task_assign(task["id"], ["w1:p1"], self_pane_id="w1:p1")
        self._set_pane_pid("w1:p1", os.getpid())
        self.flock.set_caller(self._caller_for(os.getpid()))
        self.addCleanup(self.flock.clear_caller)
        done = self.flock.task_complete(task["id"], self_pane_id="w1:p1")
        self.assertEqual("done", done["status"])

    def test_spoofed_owner_label_is_refused(self):
        """P1 probe 1: a foreign peer naming the OWNER still fails: the live
        parent process is provably outside this process's ancestry."""
        task = self.flock.task_add("bound work", self_pane_id="w1:p1")
        self.flock.task_assign(task["id"], ["w1:p1"], self_pane_id="w1:p1")
        self._set_pane_pid("w1:p1", os.getpid())
        self.flock.set_caller(self._caller_for(os.getppid()))
        self.addCleanup(self.flock.clear_caller)
        with self.assertRaisesRegex(DunkError, "not inside pane"):
            self.flock.task_complete(task["id"], self_pane_id="w1:p1")
        current = self.flock.tasks.get(task["id"])
        self.assertNotEqual("done", current["status"])
        self.assertEqual([], self._completed_events(task["id"]))

    def test_spoofed_operator_label_is_refused(self):
        """P1 probe 2: a foreign peer naming an ALLOWLISTED operator pane fails
        the binding first - the allowlist is consulted only for bound panes."""
        self.superusers.add("w1:p1")
        task = self.flock.task_add("other work", self_pane_id="w2:p1")
        self.flock.task_assign(task["id"], ["w2:p1"], self_pane_id="w2:p1")
        self._set_pane_pid("w1:p1", os.getpid())
        self.flock.set_caller(self._caller_for(os.getppid()))
        self.addCleanup(self.flock.clear_caller)
        with self.assertRaisesRegex(DunkError, "not inside pane"):
            self.flock.task_complete(task["id"], self_pane_id="w1:p1")
        self.assertNotEqual("done", self.flock.tasks.get(task["id"])["status"])

    def test_unresolvable_pane_claim_is_refused(self):
        """A live peer naming a pane no backend can see is spoof-shaped: no
        live lane hides from its own backend. (Replaces the old inconclusive
        fallback for this shape; in-process callers without a peer still use
        label policy, as the w9:p7 tests pin.)"""
        task = self._owned_task()  # w9:p7 has no fake pane: unresolvable
        self.flock.set_caller(self._caller_for(os.getpid()))
        self.addCleanup(self.flock.clear_caller)
        with self.assertRaisesRegex(DunkError, "not visible"):
            self.flock.task_complete(task["id"], self_pane_id=OWNER)
        self.assertNotEqual("done", self.flock.tasks.get(task["id"])["status"])

    def test_peer_gone_is_refused(self):
        """Hit-and-run shaped: the peer reaped before the check leaves no
        ancestry to verify, so the label cannot authorize. Legitimate clients
        wait for their response and stay resolvable."""
        task = self.flock.task_add("bound work", self_pane_id="w1:p1")
        self.flock.task_assign(task["id"], ["w1:p1"], self_pane_id="w1:p1")
        self._set_pane_pid("w1:p1", os.getpid())
        self.flock.set_caller({"pane_id": None, "pid": None, "start": None,
                               "name": None, "source": "none"})
        self.addCleanup(self.flock.clear_caller)
        with self.assertRaisesRegex(DunkError, "not inside pane"):
            self.flock.task_complete(task["id"], self_pane_id="w1:p1")
        self.assertNotEqual("done", self.flock.tasks.get(task["id"])["status"])

    def test_no_proc_table_falls_back_to_label_policy(self):
        """Non-Linux keeps existing behavior: with no kernel evidence the
        label policy still decides."""
        task = self._owned_task()
        self.flock.set_caller(self._caller_for(os.getpid()))
        self.addCleanup(self.flock.clear_caller)
        with mock.patch("os.path.isdir", return_value=False):
            done = self.flock.task_complete(task["id"], self_pane_id=OWNER)
        self.assertEqual("done", done["status"])

    # -- P2: ownership rechecked inside the transaction ----------------------

    def test_reassignment_before_transaction_is_refused(self):
        """P2 interleaving, deterministic: the guard passed for OWNER while it
        owned the task; the reassignment lands before the store transaction;
        the transaction must refuse with no effects."""
        task = self._owned_task()
        self.flock.tasks.assign(task["id"], [FOREIGN], mode="set")
        from dunk_tasks import TaskError
        with self.assertRaisesRegex(TaskError, "not owned"):
            self.flock.tasks.advance_next_job(
                OWNER, trigger="completion", from_task=task["id"],
                completer_is_operator=False)
        current = self.flock.tasks.get(task["id"])
        self.assertNotEqual("done", current["status"])
        self.assertEqual([], self._completed_events(task["id"]))
        self.assertEqual(
            [], [h for h in self.flock.tasks.next_job_handoffs(OWNER)
                 if h.get("from_task") == task["id"]])

    def test_operator_survives_reassignment(self):
        """Operator privilege is role-based: it does not evaporate when the
        row's assignees change under it."""
        task = self._owned_task()
        self.flock.tasks.assign(task["id"], [FOREIGN], mode="set")
        plan = self.flock.tasks.advance_next_job(
            OPERATOR, trigger="completion", from_task=task["id"],
            completer_is_operator=True)
        self.assertEqual("done", plan["task"]["status"])

    def test_transaction_accepts_unchanged_ownership(self):
        task = self._owned_task()
        plan = self.flock.tasks.advance_next_job(
            OWNER, trigger="completion", from_task=task["id"],
            completer_is_operator=False)
        self.assertEqual("done", plan["task"]["status"])


if __name__ == "__main__":
    unittest.main()
