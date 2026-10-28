"""t372: the decision a lane makes the moment it goes free, table-tested on its own.

The trigger changes, not the eligibility rule (pi-review-1 i31321): on completion the lane
looks for its next job and takes it through the ATOMIC claim; it is not nudged for work it has
not claimed. Clause 5 is the boundary - a lane with nothing assigned records explicit no-work.

Restart is a PURE input here (same task/lease state, fresh process, no in-memory history) and
overload is a RATE input, because a test that depends on the host's scheduler is not evidence.
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
import dunk_dispatch as dd  # noqa: E402

PANE = "%4"


def task(tid, status="open", priority="high", assignees=(PANE,), created=1000.0, **extra):
    row = {"id": tid, "status": status, "priority": priority, "assignees": list(assignees),
           "created_at": created}
    row.update(extra)
    return row


def plan(**kw):
    base = dict(trigger="completion", pane_id=PANE, tasks=[task("t1")], unread=0, now=2000.0)
    base.update(kw)
    return dd.next_job_plan(**base)


class NextJobPlanTests(unittest.TestCase):
    # -- the happy path ---------------------------------------------------------------
    def test_a_completion_with_assigned_open_work_claims_the_next_task(self):
        out = plan()
        self.assertEqual("claim", out["action"])
        self.assertEqual("t1", out["task_id"])
        self.assertEqual("completion", out["trigger"])

    def test_the_same_decision_applies_to_a_turn_end(self):
        self.assertEqual("claim", plan(trigger="turn.end")["action"])

    def test_priority_then_age_orders_the_choice(self):
        out = plan(tasks=[task("t-low", priority="low", created=1.0),
                          task("t-urgent", priority="urgent", created=5.0),
                          task("t-high", priority="high", created=2.0)])
        self.assertEqual("t-urgent", out["task_id"])

    # -- clause 5: the quiet lanes stay quiet ----------------------------------------
    def test_no_assigned_work_records_explicit_no_work(self):
        out = plan(tasks=[task("t1", assignees=("%15",))])
        self.assertEqual("skip", out["action"])
        self.assertEqual("no_assigned_work", out["reason"],
                         "clause 5: a lane with nothing assigned must record no-work, not poll")

    def test_blocked_and_dependency_gated_work_is_not_eligible(self):
        self.assertEqual("no_assigned_work",
                         plan(tasks=[task("t1", block_reason="waiting on owner")])["reason"])
        self.assertEqual("no_assigned_work",
                         plan(tasks=[task("t1", blocked_by=["t0"])])["reason"])

    def test_completed_or_cancelled_work_is_not_eligible(self):
        for status in ("done", "cancelled", "in_progress"):
            self.assertEqual("no_assigned_work",
                             plan(tasks=[task("t1", status=status)])["reason"], status)

    # -- the guards that already existed ---------------------------------------------
    def test_unread_mail_is_a_skip(self):
        self.assertEqual("unread_mail", plan(unread=2)["reason"])

    def test_unknown_unread_is_a_skip_not_a_zero(self):
        self.assertEqual("unread_unknown", plan(unread=None)["reason"])

    def test_a_pending_delivery_is_a_skip(self):
        self.assertEqual("pending_delivery", plan(pending_delivery=True)["reason"])

    def test_no_pane_is_a_skip(self):
        self.assertEqual("no_pane", plan(pane_id=None)["reason"])

    def test_dispatch_controls_are_inputs_to_the_shared_decision(self):
        for trigger in ("completion", "turn.end", "restart"):
            for reason in ("disabled", "not_allowed", "readiness_unknown"):
                out = plan(trigger=trigger, dispatch_reason=reason)
                self.assertEqual(("skip", reason), (out["action"], out["reason"]))

    # -- rate inputs, not load tests -------------------------------------------------
    def test_a_recent_prompt_holds_the_lane(self):
        self.assertEqual("cooldown", plan(last_nudge_at=1999.0, min_gap_s=60)["reason"])

    def test_a_task_prompted_recently_is_not_offered_again(self):
        out = plan(task_nudges={"t1": 1999.0}, task_cooldown_s=300)
        self.assertEqual("task_cooldown", out["reason"])

    def test_overload_is_a_rate_not_a_scheduler(self):
        # 50 completions in one second on the same lane: the cooldown is what bounds it, and the
        # decision is the same for the first and the fiftieth - no dependence on host timing.
        first = plan(last_nudge_at=None, now=2000.0)
        second = plan(last_nudge_at=2000.0, now=2000.0, min_gap_s=60)
        self.assertEqual("claim", first["action"])
        self.assertEqual("cooldown", second["reason"])

    # -- restart is a pure input -----------------------------------------------------
    def test_a_restart_reaches_the_same_decision_from_the_same_state(self):
        """Same tasks/leases, fresh process, no in-memory history: same answer."""
        state = dict(tasks=[task("t1")], unread=0, now=2000.0)
        before = plan(**state)
        after = plan(**state)          # nothing carried over but the inputs
        self.assertEqual(before, after)


if __name__ == "__main__":
    unittest.main()
