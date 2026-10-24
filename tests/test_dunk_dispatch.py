"""The deterministic feeder: queue, gate, tick, and the numbers it reports.

No model is involved anywhere, so the tests are the specification: only a lane
that is READY and has no unread mail gets an item, a refused send is recorded
rather than retried, and an empty queue with idle lanes says so.
"""
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _bootstrap  # noqa: E402,F401

import dunk_dispatch as dd  # noqa: E402


def pane(pane_id, name=None, status="idle"):
    return {"pane_id": pane_id, "agent_name": name or pane_id, "agent_status": status}


class QueueTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.queue = dd.Queue(os.path.join(self.tmp.name, "d.db"))

    def tearDown(self):
        self.tmp.cleanup()

    def test_items_are_ordered_and_a_taken_item_leaves_the_queue(self):
        first = self.queue.add("first", task_id="t6")
        self.queue.add("second")
        self.assertEqual([first["id"], first["id"] + 1], [i["id"] for i in self.queue.items()])
        self.queue.take(first["id"], "w1:p1", "i9")
        self.assertEqual(1, len(self.queue.items()))
        self.assertEqual(2, len(self.queue.items(pending_only=False)))
        taken = [i for i in self.queue.items(pending_only=False) if i["id"] == first["id"]][0]
        self.assertEqual(("w1:p1", "i9"), (taken["taken_by"], taken["sent_message"]))

    def test_an_item_cannot_be_taken_twice(self):
        item = self.queue.add("only once")
        self.queue.take(item["id"], "w1:p1")
        with self.assertRaises(dd.DispatchError):
            self.queue.take(item["id"], "w1:p2")

    def test_a_lane_reserved_item_is_not_offered_to_another_lane(self):
        reserved = self.queue.add("for poolrad", lane="wW:p1")
        self.assertIsNone(self.queue.next_for("w9:pM"))
        self.assertEqual(reserved["id"], self.queue.next_for("wW:p1")["id"])

    def test_an_empty_brief_is_refused(self):
        with self.assertRaises(dd.DispatchError):
            self.queue.add("   ")


class LanePlanTests(unittest.TestCase):
    def plan(self, panes, states, unread=None):
        return dd.lane_plan(
            panes,
            delivery=lambda p: {"state": states[p["pane_id"]][0],
                                "reason": states[p["pane_id"]][1]},
            unread=lambda p: (unread or {}).get(p["pane_id"], 0))

    def test_a_ready_lane_with_unread_mail_has_an_assignment_already(self):
        plan = self.plan([pane("w1:p1")], {"w1:p1": ("ready", None)}, unread={"w1:p1": 3})
        self.assertEqual([], plan["ready"])
        self.assertEqual(3, plan["has_mail"][0]["unread"])

    def test_blocked_dead_and_working_are_never_ready(self):
        plan = self.plan([pane("a"), pane("b"), pane("c")],
                         {"a": ("blocked", "context full"), "b": ("dead", "dead session"),
                          "c": ("working", "working")})
        self.assertEqual([], plan["ready"])
        self.assertEqual("context full", plan["blocked"][0]["reason"])
        self.assertEqual("dead session", plan["dead"][0]["reason"])
        self.assertEqual(1, len(plan["working"]))

    def test_an_unknown_state_is_reported_as_blocked_not_ready(self):
        plan = self.plan([pane("a")], {"a": ("unknown", "pane unreadable")})
        self.assertEqual([], plan["ready"])
        self.assertEqual("pane unreadable", plan["blocked"][0]["reason"])


def task(task_id, status="open", priority="medium", depends=(), leases=(),
         assignees=(), block_reason=None, title=None, created=1.0):
    return {"id": task_id, "status": status, "priority": priority,
            "depends_on": list(depends), "leases": list(leases),
            "assignees": list(assignees), "block_reason": block_reason,
            "title": title or f"task {task_id}", "created_at": created}


class DeriveFromBoardTests(unittest.TestCase):
    """The hand-written queue is a hand-maintained list, so the board is primary."""

    def test_open_unblocked_unclaimed_tasks_become_items_in_priority_order(self):
        items, coarse = dd.derive_board_items([
            task("t1", priority="low", created=3),
            task("t2", priority="high", created=2),
            task("t3", priority="urgent", created=9),
        ])
        self.assertEqual(["t3", "t2", "t1"], [item["task_id"] for item in items])
        self.assertEqual([], coarse)
        self.assertTrue(all(item["derived"] for item in items))
        self.assertEqual("[standby t2] task t2", dd.dispatch_text(items[1]))

    def test_everything_that_is_not_dispatchable_is_skipped(self):
        items, _ = dd.derive_board_items([
            task("held", block_reason="HELD FOR OWNER DECISION: wait for owner"),
            task("blocked", block_reason="waiting on vendor"),
            task("dep", depends=["unfinished"]),
            task("leased", leases=[{"pane_id": "w1:p1"}]),
            task("working", status="in_progress"),
            task("done", status="done"),
            task("recent"),
        ], dispatched_task_ids={"recent"})
        self.assertEqual([], items)

    def test_a_dependency_that_is_done_no_longer_blocks(self):
        items, _ = dd.derive_board_items([
            task("first", status="done"),
            task("second", depends=["first"]),
        ])
        self.assertEqual(["second"], [item["task_id"] for item in items])

    def test_an_umbrella_title_is_flagged_for_splitting_not_queued_by_hand(self):
        items, coarse = dd.derive_board_items([task("big", title="x" * 120)])
        self.assertEqual(["big"], [item["task_id"] for item in items])
        self.assertEqual(["big"], coarse,
                         "a coarse task is a signal to split it, not to keep a hand list")


class TickTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.queue = dd.Queue(os.path.join(self.tmp.name, "d.db"))
        self.state = os.path.join(self.tmp.name, "state.json")
        self.sent = []

    def tearDown(self):
        self.tmp.cleanup()

    def tick(self, panes, states, *, unread=None, now=1000.0, dry_run=False, fail=None,
             derived=()):
        def send(pane_id, text):
            if fail:
                raise dd.DispatchError(fail)
            self.sent.append((pane_id, text))
            return f"i{len(self.sent)}"
        return dd.tick(self.queue, panes,
                       delivery=lambda p: {"state": states[p["pane_id"]][0],
                                           "reason": states[p["pane_id"]][1]},
                       unread=lambda p: (unread or {}).get(p["pane_id"], 0),
                       send=send, now=now, dry_run=dry_run, state_path=self.state,
                       derived=derived)

    def test_only_the_ready_lane_is_fed_and_the_text_carries_the_task(self):
        self.queue.add("advance strict C to 250", task_id="t6")
        state = self.tick([pane("w1:p1"), pane("w1:p2")],
                          {"w1:p1": ("ready", None), "w1:p2": ("blocked", "context full")})
        self.assertEqual([("w1:p1", "[standby t6] advance strict C to 250")], self.sent)
        self.assertEqual(1, state["dispatched_count"])
        self.assertEqual(1, state["dispatched_1h"])

    def test_a_dry_run_sends_nothing_and_takes_nothing(self):
        self.queue.add("do not send me")
        state = self.tick([pane("w1:p1")], {"w1:p1": ("ready", None)}, dry_run=True)
        self.assertEqual([], self.sent)
        self.assertFalse(state["dispatched"][0]["sent"])
        self.assertEqual(1, len(self.queue.items()), "a dry run must not consume the item")

    def test_a_refused_send_is_recorded_and_leaves_the_item_queued(self):
        self.queue.add("keep me")
        state = self.tick([pane("w1:p1")], {"w1:p1": ("ready", None)}, fail="typing hold")
        self.assertIn("typing hold", state["dispatched"][0]["error"])
        self.assertEqual(1, len(self.queue.items()), "a failed send must not lose the assignment")

    def test_a_derived_board_item_fills_an_idle_lane_without_a_hand_item(self):
        derived, _coarse = dd.derive_board_items([task("t44", title="fold the flags")])
        state = self.tick([pane("w1:p1")], {"w1:p1": ("ready", None)}, derived=derived)
        self.assertEqual([("w1:p1", "[standby t44] fold the flags")], self.sent)
        self.assertEqual(1, state["derived_dispatched"])
        self.assertEqual(1, state["derived_available"])
        self.assertFalse(state["queue_empty"],
                         "'queue empty' must mean no open work, not 'nobody topped it up'")

    def test_an_empty_queue_with_idle_lanes_is_stated_not_hidden(self):
        state = self.tick([pane("w1:p1"), pane("w1:p2")],
                          {"w1:p1": ("ready", None), "w1:p2": ("ready", None)})
        self.assertTrue(state["queue_empty"])
        self.assertEqual(["w1:p1", "w1:p2"], state["idle_unfilled"])
        self.assertIn("standby queue is empty", dd.summary(state))

    def test_dispatched_this_hour_counts_real_sends_and_expires(self):
        for index in range(2):
            self.queue.add(f"item {index}")
            self.tick([pane("w1:p1")], {"w1:p1": ("ready", None)}, now=1000.0 + index)
        state = self.tick([pane("w1:p1")], {"w1:p1": ("ready", None)}, now=1100.0)
        self.assertEqual(2, state["dispatched_1h"])
        later = self.tick([pane("w1:p1")], {"w1:p1": ("ready", None)}, now=9999.0)
        self.assertEqual(0, later["dispatched_1h"])


if __name__ == "__main__":
    unittest.main()
