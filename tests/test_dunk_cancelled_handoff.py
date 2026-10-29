"""t461: a CANCELLED historical handoff must not veto live work.

Root census: pane %28 had a historical next_job_handoff for t106 whose latest
state was CANCELLED, while t106 was still open and owned. advance_next_job built
`active` from every historical handoff naming an owned open task, without
looking at its state, so `pending_delivery` stayed true and the pane could never
advance to its next eligible job - with no lease, no outbox row and no held work.

The fix excludes ONLY terminal cancelled intents. Every other state still
blocks, and an unrecognised state blocks too (fail closed), because a state this
code does not understand might still become a typed prompt.
"""

import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
from dunk_tasks import GLOBAL, Tasks  # noqa: E402

PANE = "%28"          # the census pane
CALLER = {"pid": 4321, "start": 1.0, "name": "t461"}


class CancelledHandoffTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.store = Tasks(os.path.join(self.tmp.name, "tasks.db"))

    def _task(self, title, priority="high"):
        task = self.store.add(title=title, body="acceptance", scope=GLOBAL,
                              assignees=[PANE], priority=priority)
        self.store.db.commit()
        return task

    def _handoff(self, task_id, state, message_id="m-hist"):
        """Record a handoff and leave its LATEST state as `state`."""
        self.store.record_next_job_handoff(
            task_id, pane_id=PANE, from_task="t0", message_id=message_id,
            attempt=1, reason=state, details={"state": state})
        self.store.db.commit()

    def _advance(self, from_task):
        return self.store.advance_next_job(
            PANE, trigger="completion", from_task=from_task["id"], caller=CALLER)

    # -- the defect ---------------------------------------------------------

    def test_cancelled_historical_intent_does_not_block_the_next_job(self):
        """Old behaviour: skip/pending_delivery forever. New: the eligible job
        is claimed and a fresh handoff is journalled."""
        finished = self._task("A finish me", "urgent")
        waiting = self._task("B next job", "high")
        old = self._task("H historical intent", "low")
        self._handoff(old["id"], "cancelled")
        plan = self._advance(finished)
        self.assertEqual("claim", plan["action"], plan)
        self.assertEqual(waiting["id"], plan["task_id"])
        self.assertEqual("in_progress", self.store.get(waiting["id"])["status"])

    def test_cancelled_intent_on_the_completed_task_does_not_block(self):
        """The cancelled intent can name the very task being completed."""
        finished = self._task("A finish me", "urgent")
        waiting = self._task("B next job", "high")
        self._handoff(finished["id"], "cancelled")
        plan = self._advance(finished)
        self.assertEqual("claim", plan["action"], plan)
        self.assertEqual(waiting["id"], plan["task_id"])

    def test_control_without_any_historical_intent_also_claims(self):
        """The cancelled case must match the clean case, not beat it."""
        finished = self._task("A finish me", "urgent")
        waiting = self._task("B next job", "high")
        plan = self._advance(finished)
        self.assertEqual("claim", plan["action"], plan)
        self.assertEqual(waiting["id"], plan["task_id"])

    # -- retained blockers (must NOT be relaxed) ----------------------------

    def test_pending_intent_still_blocks(self):
        finished = self._task("A finish me", "urgent")
        self._task("B next job", "high")
        old = self._task("H historical intent", "low")
        self._handoff(old["id"], "pending")
        plan = self._advance(finished)
        self.assertEqual("skip", plan["action"], plan)
        self.assertEqual("pending_delivery", plan["reason"])

    def test_sent_intent_still_blocks(self):
        finished = self._task("A finish me", "urgent")
        self._task("B next job", "high")
        old = self._task("H historical intent", "low")
        self._handoff(old["id"], "sent")
        plan = self._advance(finished)
        self.assertEqual("pending_delivery", plan["reason"])

    def test_started_intent_still_blocks(self):
        finished = self._task("A finish me", "urgent")
        self._task("B next job", "high")
        old = self._task("H historical intent", "low")
        self._handoff(old["id"], "started")
        plan = self._advance(finished)
        self.assertEqual("pending_delivery", plan["reason"])

    def test_failed_intent_still_blocks(self):
        """'failed' means 'this attempt failed, retrying' - not decided."""
        finished = self._task("A finish me", "urgent")
        self._task("B next job", "high")
        old = self._task("H historical intent", "low")
        self._handoff(old["id"], "failed")
        plan = self._advance(finished)
        self.assertEqual("pending_delivery", plan["reason"])

    def test_uncertain_intent_still_blocks(self):
        finished = self._task("A finish me", "urgent")
        self._task("B next job", "high")
        old = self._task("H historical intent", "low")
        self._handoff(old["id"], "uncertain")
        plan = self._advance(finished)
        self.assertEqual("pending_delivery", plan["reason"])

    def test_case_variant_of_cancelled_still_blocks(self):
        """Reviewer t462 [P2]: the delivery worker (dunk_next_job.tick/deliver)
        skips only the exact string 'cancelled'. A 'CANCELLED' state is
        therefore still EXECUTABLE there, so the claim gate must not treat it
        as terminal - that mismatch typed the old prompt AND claimed new work
        (two sends for one intent)."""
        finished = self._task("A finish me", "urgent")
        self._task("B next job", "high")
        old = self._task("H historical intent", "low")
        self._handoff(old["id"], "CANCELLED")
        plan = self._advance(finished)
        self.assertEqual("skip", plan["action"], plan)
        self.assertEqual("pending_delivery", plan["reason"])

    def test_whitespace_padded_cancelled_still_blocks(self):
        finished = self._task("A finish me", "urgent")
        self._task("B next job", "high")
        old = self._task("H historical intent", "low")
        self._handoff(old["id"], " cancelled ")
        plan = self._advance(finished)
        self.assertEqual("pending_delivery", plan["reason"])

    def test_mixed_case_cancelled_still_blocks(self):
        finished = self._task("A finish me", "urgent")
        self._task("B next job", "high")
        old = self._task("H historical intent", "low")
        self._handoff(old["id"], "Cancelled")
        plan = self._advance(finished)
        self.assertEqual("pending_delivery", plan["reason"])

    def test_empty_state_still_blocks(self):
        finished = self._task("A finish me", "urgent")
        self._task("B next job", "high")
        old = self._task("H historical intent", "low")
        self._handoff(old["id"], "")
        plan = self._advance(finished)
        self.assertEqual("pending_delivery", plan["reason"])

    def test_active_handoffs_matches_the_delivery_workers_skip_set(self):
        """The claim gate and dunk_next_job must agree on what is terminal."""
        import inspect
        import dunk_next_job
        source = inspect.getsource(dunk_next_job.NextJobs.tick)
        # the worker skips exactly this tuple; our non-blocking set must be the
        # subset of it that we are allowed to stop blocking on.
        self.assertIn('("started", "undeliverable", "cancelled")', source)
        self.assertEqual({"cancelled"}, set(self.store.NON_BLOCKING_HANDOFF_STATES))

    def test_unknown_state_blocks_because_it_is_not_proven_terminal(self):
        finished = self._task("A finish me", "urgent")
        self._task("B next job", "high")
        old = self._task("H historical intent", "low")
        self._handoff(old["id"], "delivered-to-a-place-we-do-not-know")
        plan = self._advance(finished)
        self.assertEqual("pending_delivery", plan["reason"])

    def test_held_lease_still_blocks(self):
        held = self._task("B held by me", "high")
        self.store.claim_next(scope=GLOBAL, pane_id=PANE, caller=CALLER)
        self.store.db.commit()
        self.assertEqual("in_progress", self.store.get(held["id"])["status"])
        finished = self._task("A finish me", "urgent")
        plan = self._advance(finished)
        self.assertEqual("skip", plan["action"], plan)
        self.assertEqual("in_progress", self.store.get(held["id"])["status"])

    def test_no_assigned_work_is_still_an_honest_skip(self):
        finished = self._task("A finish me", "urgent")
        plan = self._advance(finished)
        self.assertEqual("skip", plan["action"], plan)
        self.assertIn(plan["reason"], ("no_assigned_work", "no_work"), plan)

    # -- the helper itself --------------------------------------------------

    def test_active_handoffs_excludes_only_cancelled(self):
        keep = self._task("keep", "high")
        drop = self._task("drop", "high")
        self._handoff(keep["id"], "sent", message_id="m-keep")
        self._handoff(drop["id"], "cancelled", message_id="m-drop")
        owned = {keep["id"], drop["id"]}
        active = self.store.active_handoffs(PANE, owned)
        self.assertEqual(["m-keep"], [h["message_id"] for h in active])

    def test_active_handoffs_ignores_tasks_the_pane_does_not_own(self):
        other = self.store.add(title="not mine", body="x", scope=GLOBAL,
                               assignees=["%99"], priority="high")
        self.store.db.commit()
        self._handoff(other["id"], "sent")
        self.assertEqual([], self.store.active_handoffs(PANE, set()))


if __name__ == "__main__":
    unittest.main(verbosity=2)
