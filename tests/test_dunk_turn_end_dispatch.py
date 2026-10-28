"""t276: dispatch on turn.end, not on timers.

The measurement behind this file: the last 20 turns lasted 33-448 s while the keep-moving
dunks fire every 60-120 MINUTES, so a lane was unprompted ~95% of the time BY CONSTRUCTION.
The harness already reports turn.end; the daemon received it and did nothing.

Two things are pinned here, and they are different claims:

1. THE POLICY (pure, `dunk_dispatch.turn_end_plan`): a turn end earns one prompt, and
   every skip names itself - no open work, unread mail already waiting, an unreadable
   mailbox, a cooldown, the same task prompted recently.
2. THE CHAIN (a real `Flock.turn_end` call): the handler is actually INVOKED from turn_end,
   the prompt reaches the pane, and a HELD delivery is reported as not delivered rather
   than counted as sent.

What is NOT proved here: that the timer cadence is untouched (it is: no timer code is
called from this path and `flock.list()` stays empty), that codex/opencode emit anything
(they do not - that hook is t264), and anything about a live pane on the real backend.
"""
import os
import json
import sys
import tempfile
import threading
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _bootstrap  # noqa: E402,F401

import dunk_core  # noqa: E402
import dunk_api  # noqa: E402
import dunk_dispatch as dd  # noqa: E402
from dunk_inbox import Inbox  # noqa: E402
from dunk_tasks import Tasks  # noqa: E402
from dunk_server import FlockServer  # noqa: E402
from dunk_callers import caller_from_peer  # noqa: E402
from fakes import FakeHerdr  # noqa: E402

PANE = "w1:p1"
CLEAN_BOX = "\u276f\u00a0\n"
HUMAN_DRAFT = "\u276f half a sentence the human is writing\n"


def task(task_id, *, status="in_progress", priority="medium", assignees=(),
         claimed_by=(), depends=(), block_reason=None, title=None):
    """A task as the board decorates it. `claimed_by` is a live lease held by that pane."""
    return {"id": task_id, "status": status, "priority": priority,
            "assignees": list(assignees) or list(claimed_by),
            "claimed_by": list(claimed_by),
            "leases": [{"pane_id": pane, "expires_at": 9e9} for pane in claimed_by],
            "depends_on": list(depends),
            "block_reason": block_reason,
            "title": title or f"task {task_id}", "created_at": 1.0}


class ClaimedWorkTests(unittest.TestCase):
    """CLAIMED, not assigned (CoS ruling): a live lease held by that pane, or nothing."""

    def test_a_task_the_lane_has_claimed_is_the_one_it_is_carried(self):
        self.assertEqual(["t9"], [t["id"] for t in
                                  dd.claimed_tasks([task("t9", claimed_by=[PANE])], PANE)])
        self.assertEqual("t9", dd.claimed_work([task("t9", claimed_by=[PANE])], PANE)["id"])

    def test_an_assigned_open_programme_task_earns_no_nudge(self):
        """The ruling's case: an umbrella sits assigned-and-open for days."""
        programme = task("programme", status="open", assignees=[PANE], priority="urgent")
        self.assertEqual([], dd.claimed_tasks([programme], PANE))
        plan = dd.turn_end_plan(pane_id=PANE, tasks=[programme], unread=0, now=10_000.0)
        self.assertEqual(("skip", "no_open_work"), (plan["action"], plan["reason"]))

    def test_in_progress_without_a_lease_is_not_claimed_either(self):
        """`claimed_by == pane` is the condition; setting status alone is not claiming."""
        unclaimed = task("t1", status="in_progress", assignees=[PANE])
        self.assertEqual([], dd.claimed_tasks([unclaimed], PANE))

    def test_priority_then_age_orders_the_claimed_tasks(self):
        claimed = dd.claimed_tasks([task("low", priority="low", claimed_by=[PANE]),
                                    task("urgent", priority="urgent", claimed_by=[PANE])], PANE)
        self.assertEqual(["urgent", "low"], [t["id"] for t in claimed])

    def test_blocked_unfinished_done_and_foreign_tasks_are_not_claimed_work(self):
        for label, candidate in {
            "blocked": task("b", claimed_by=[PANE], block_reason="HELD FOR OWNER DECISION"),
            "dep-gated": task("d", claimed_by=[PANE], depends=["unfinished"]),
            "done": task("x", status="done", claimed_by=[PANE]),
            "cancelled": task("y", status="cancelled", claimed_by=[PANE]),
            "another lane's lease": task("z", claimed_by=["w1:p2"]),
            "assigned to me, leased to another lane":
                task("zz", assignees=[PANE], claimed_by=["w1:p2"]),
        }.items():
            self.assertEqual([], dd.claimed_tasks([candidate], PANE), label)

    def test_a_finished_dependency_no_longer_blocks(self):
        self.assertEqual("second", dd.claimed_work([
            task("first", status="done", claimed_by=[PANE]),
            task("second", claimed_by=[PANE], depends=["first"])], PANE)["id"])


class TurnEndPolicyTests(unittest.TestCase):
    def plan(self, **kwargs):
        kwargs.setdefault("pane_id", PANE)
        kwargs.setdefault("now", 10_000.0)
        return dd.turn_end_plan(**kwargs)

    def test_a_lane_holding_work_and_holding_no_mail_is_prompted(self):
        plan = self.plan(tasks=[task("t7", claimed_by=[PANE])], unread=0)
        self.assertEqual("nudge", plan["action"])
        self.assertEqual("turn.end", plan["trigger"])
        self.assertEqual("t7", plan["item"]["id"])

    def test_a_lane_with_no_open_work_is_left_alone(self):
        plan = self.plan(tasks=[task("t7", assignees=[PANE], status="done")], unread=0)
        self.assertEqual(("skip", "no_open_work"), (plan["action"], plan["reason"]))

    def test_a_lane_that_already_has_unread_mail_is_not_fed_twice(self):
        plan = self.plan(tasks=[task("t7", claimed_by=[PANE])], unread=2)
        self.assertEqual(("skip", "unread_mail"), (plan["action"], plan["reason"]))

    def test_an_unreadable_mailbox_is_a_skip_not_a_guess(self):
        plan = self.plan(tasks=[task("t7", claimed_by=[PANE])], unread=None)
        self.assertEqual(("skip", "unread_unknown"), (plan["action"], plan["reason"]))

    def test_the_same_pane_is_not_prompted_twice_in_a_row(self):
        plan = self.plan(tasks=[task("t7", claimed_by=[PANE])], unread=0,
                         last_nudge_at=9_990.0, min_gap_s=30)
        self.assertEqual(("skip", "cooldown"), (plan["action"], plan["reason"]))

    def test_the_same_task_is_not_re_prompted_on_every_turn(self):
        plan = self.plan(tasks=[task("t7", claimed_by=[PANE])], unread=0,
                         task_nudges={"t7": 9_000.0}, task_cooldown_s=1800)
        self.assertEqual(("skip", "task_cooldown"), (plan["action"], plan["reason"]))

    def test_another_held_task_is_still_prompted_when_one_was_just_mentioned(self):
        plan = self.plan(tasks=[task("t7", claimed_by=[PANE]),
                                task("t8", claimed_by=[PANE])], unread=0,
                         task_nudges={"t7": 9_000.0}, task_cooldown_s=1800)
        self.assertEqual("nudge", plan["action"])
        self.assertEqual("t8", plan["item"]["id"])

    def test_a_different_task_is_prompted_once_the_pane_gap_has_passed(self):
        # t7 was prompted 1000 s ago; the lane now holds t8, which was never prompted.
        plan = self.plan(tasks=[task("t8", claimed_by=[PANE])], unread=0,
                         last_nudge_at=9_000.0, min_gap_s=30,
                         task_nudges={"t7": 9_000.0}, task_cooldown_s=1800)
        self.assertEqual("nudge", plan["action"])
        self.assertEqual("t8", plan["item"]["id"])

    def test_the_prompt_says_why_it_arrived_and_which_task_it_is(self):
        text = dd.turn_end_text(task("t7", title="vitals row colour", claimed_by=[PANE]))
        self.assertTrue(text.startswith("[turn-end t7] vitals row colour"))
        self.assertIn("trigger: turn.end", text)
        self.assertIn("task complete t7", text)


class TurnEndChainTests(unittest.TestCase):
    """The handler must be INVOKED BY turn_end, and its outcome must be honest."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(mock.patch.stopall)
        mock.patch.object(dunk_api, "DISPATCH_LANES_PATH",
                          os.path.join(self.tmp.name, "dispatch-lanes.json")).start()
        mock.patch.dict(os.environ, {"DUNKINGSHEEP_DISPATCH_LANES": PANE}).start()
        self.timer = mock.patch.object(dd, "tick", side_effect=AssertionError(
            "turn.end must not depend on a timer tick")).start()
        self.herdr = FakeHerdr()
        self.herdr.panes[0]["agent"] = "pi"
        self.herdr.panes[0]["agent_status"] = "idle"
        self.herdr.panes[0]["pane_pid"] = caller_from_peer(os.getpid())["pid"]
        self.herdr.screens[PANE] = CLEAN_BOX
        self.inbox = Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                           blob_dir=os.path.join(self.tmp.name, "blobs"))
        self.tasks = Tasks(path=os.path.join(self.tmp.name, "tasks.db"))
        self.flock = dunk_core.Flock(herdr=self.herdr, persist=False, restore=False,
                                     inbox=self.inbox, tasks=self.tasks)
        self.server = FlockServer(self.flock)

    def tearDown(self):
        self.flock.close()
        self.tmp.cleanup()

    def hold(self, title="vitals row colour"):
        """Create work and CLAIM it for the pane, the way a producer lane does."""
        added = self.flock.task_add(title, self_pane_id=PANE)
        claimed = self.flock.tasks.claim_next(scope="global", pane_id=PANE)
        self.assertEqual(added["id"], claimed["task"]["id"], "the fixture must claim it")
        return added

    def end_turn(self):
        started = self.rpc("turn_start", {"source": "test"})
        return self.rpc("turn_end", {"turn_id": started["turn_id"]})

    def raw_rpc(self, cmd, args=None, pane=PANE):
        response, _ = self.server.handle_line(json.dumps({
            "cmd": cmd, "args": args or {}, "self_pane_id": pane}), peer_pid=os.getpid())
        return response

    def rpc(self, cmd, args=None):
        response = self.raw_rpc(cmd, args)
        self.assertTrue(response["ok"], response)
        return response["result"]

    def test_foreign_explicit_target_cannot_end_or_dispatch(self):
        self.hold()
        self.rpc("turn_start")
        result = self.raw_rpc("turn_end", {"pane": PANE}, pane="w1:p2")
        self.assertFalse(result["ok"], result)
        self.assertNotIn("ended_at", self.flock.turns[PANE])
        self.assertEqual([], self.herdr.sends)

    def test_forging_both_pane_labels_does_not_authenticate_a_foreign_process(self):
        self.hold()
        started = self.rpc("turn_start")
        self.herdr.panes[0]["pane_pid"] = 99_999_999
        result = self.raw_rpc("turn_end", {"pane": PANE, "turn_id": started["turn_id"]})
        self.assertFalse(result["ok"], result)
        self.assertNotIn("ended_at", self.flock.turns[PANE])
        self.assertEqual([], self.herdr.sends)

    def test_missing_turn_never_dispatches_claimed_work(self):
        self.hold()
        result = self.rpc("turn_end")
        self.assertFalse(result["ended"], result)
        self.assertEqual([], self.herdr.sends)
        self.assertEqual("no_active_turn", result["dispatch"]["reason"])

    def test_missing_token_cannot_end_a_valid_turn(self):
        self.hold()
        self.rpc("turn_start")
        result = self.rpc("turn_end")
        self.assertFalse(result["ended"], result)
        self.assertEqual("turn_mismatch", result["dispatch"]["reason"])
        self.assertNotIn("ended_at", self.flock.turns[PANE])

    def test_ended_token_cannot_be_replayed_even_with_cooldowns_disabled(self):
        self.hold()
        started = self.rpc("turn_start")
        with mock.patch.object(dd, "TURN_END_MIN_GAP_S", 0), \
                mock.patch.object(dd, "TURN_END_TASK_COOLDOWN_S", 0):
            self.assertTrue(self.rpc("turn_end", {"turn_id": started["turn_id"]})["ended"])
            result = self.rpc("turn_end", {"turn_id": started["turn_id"]})
        self.assertFalse(result["ended"])
        self.assertEqual("already_ended", result["dispatch"]["reason"])
        self.assertEqual(1, len(self.herdr.sends))

    def test_old_token_cannot_end_a_new_turn_from_the_same_process(self):
        self.hold()
        first = self.rpc("turn_start")
        second = self.rpc("turn_start")
        result = self.rpc("turn_end", {"turn_id": first["turn_id"]})
        self.assertFalse(result["ended"])
        self.assertEqual("turn_mismatch", result["dispatch"]["reason"])
        self.assertEqual(second["turn_id"], self.flock.turns[PANE]["turn_id"])
        self.assertNotIn("ended_at", self.flock.turns[PANE])
        self.assertEqual([], self.herdr.sends)

    def test_expired_turn_cannot_supply_idle_evidence_or_dispatch(self):
        self.hold()
        started = self.rpc("turn_start")
        self.flock.turns[PANE]["expires_at"] = 1
        result = self.rpc("turn_end", {"turn_id": started["turn_id"]})
        self.assertFalse(result["ended"])
        self.assertEqual("expired_turn", result["dispatch"]["reason"])
        self.assertNotIn("ended_at", self.flock.turns[PANE])
        self.assertEqual([], self.herdr.sends)

    def test_empty_allowlist_never_prompts_a_claimed_lane(self):
        self.hold()
        with mock.patch.dict(os.environ, {"DUNKINGSHEEP_DISPATCH_LANES": ""}):
            result = self.end_turn()
        self.assertEqual([], self.herdr.sends)
        self.assertEqual("not_allowed", result["dispatch"]["reason"])

    def test_another_allowed_lane_does_not_authorize_this_one(self):
        self.hold()
        with mock.patch.dict(os.environ, {"DUNKINGSHEEP_DISPATCH_LANES": "w1:p2"}):
            result = self.end_turn()
        self.assertEqual([], self.herdr.sends)
        self.assertEqual("not_allowed", result["dispatch"]["reason"])

    def test_completed_dependency_is_not_lost_from_open_task_view(self):
        parent = self.flock.task_add("finished prerequisite", status="done",
                                     self_pane_id=PANE)
        added = self.flock.task_add("ready dependent", depends_on=[parent["id"]],
                                    self_pane_id=PANE)
        claimed = self.flock.tasks.claim_next(scope="global", pane_id=PANE)
        self.assertEqual(added["id"], claimed["task"]["id"])
        dispatch = self.end_turn()["dispatch"]
        self.assertTrue(dispatch["delivered"], dispatch)
        self.assertEqual(added["id"], dispatch["task_id"])

    def test_a_lane_that_ends_a_turn_holding_work_is_prompted_at_once(self):
        added = self.hold()
        with mock.patch.object(dunk_core.log, "info", wraps=dunk_core.log.info) as captured:
            result = self.end_turn()
        self.assertEqual(1, len(self.herdr.sends),
                         "turn.end must invoke delivery without any timer tick")
        dispatch = result["dispatch"]
        self.assertEqual("turn.end", dispatch["trigger"])
        self.assertTrue(dispatch["delivered"], dispatch)
        self.assertEqual(added["id"], dispatch["task_id"])
        self.assertEqual(1, len(self.herdr.sends), "the prompt must reach the pane")
        pane_id, text = self.herdr.sends[0]
        self.assertEqual(PANE, pane_id)
        self.assertIn(f"[turn-end {added['id']}] vitals row colour", text)
        self.assertIn("trigger: turn.end", text)
        log = "\n".join(call.args[0] % call.args[1:] for call in captured.call_args_list)
        self.assertIn("trigger=turn.end nudge DELIVERED", log)
        self.assertNotIn("NOT delivered", log)
        # No timer was touched: the trigger is not a dunk.
        self.assertEqual([], self.flock.list())
        self.timer.assert_not_called()

    def test_genuine_end_claims_assigned_work_under_owner_2271(self):
        """t372 supersedes the old assigned-but-unclaimed restriction at turn end."""
        added = self.flock.task_add("assigned next job", status="open",
                                    priority="urgent", assignees=[PANE])
        with self.assertLogs("dunkingsheep", level="INFO") as captured:
            result = self.end_turn()
        self.assertEqual(("claim", None),
                         (result["dispatch"]["action"], result["dispatch"]["reason"]))
        self.assertEqual(added["id"], result["dispatch"]["task_id"])
        self.assertEqual([PANE], self.tasks.get(added["id"])["claimed_by"])
        self.assertEqual(1, len(self.tasks.next_job_handoffs(PANE)))
        self.assertIn("action=claim", "\n".join(captured.output))

    def test_a_task_another_pane_has_claimed_is_not_mine_to_be_prompted_about(self):
        self.flock.task_add("their work", assignees=[PANE, "w1:p2"], self_pane_id=PANE)
        self.flock.tasks.claim_next(scope="global", pane_id="w1:p2")
        result = self.end_turn()
        self.assertEqual(("skip", "no_open_work"),
                         (result["dispatch"]["action"], result["dispatch"]["reason"]))
        self.assertEqual([], self.herdr.sends)

    def test_a_lane_with_nothing_to_do_is_not_prompted(self):
        with self.assertLogs("dunkingsheep", level="INFO") as captured:
            result = self.end_turn()
        self.assertEqual(("skip", "no_open_work"),
                         (result["dispatch"]["action"], result["dispatch"]["reason"]))
        self.assertEqual([], self.herdr.sends)
        self.assertIn("reason=no_open_work", "\n".join(captured.output))

    def test_a_lane_that_already_has_unread_mail_is_not_prompted(self):
        self.hold()
        self.flock.inbox_send(PANE, "read me first", notify=False)
        result = self.end_turn()
        self.assertEqual(("skip", "unread_mail"),
                         (result["dispatch"]["action"], result["dispatch"]["reason"]))
        self.assertEqual([], self.herdr.sends)

    def test_a_held_prompt_is_never_counted_as_delivered(self):
        """A human starts typing after preflight: the guarded send still holds."""
        self.hold()
        original_send = self.flock.send_now
        def draft_then_send(*args, **kwargs):
            self.herdr.screens[PANE] = HUMAN_DRAFT
            return original_send(*args, **kwargs)
        with mock.patch.object(self.flock, "send_now", side_effect=draft_then_send), \
                self.assertLogs("dunkingsheep", level="INFO") as captured:
            result = self.end_turn()
        dispatch = result["dispatch"]
        self.assertEqual("nudge", dispatch["action"])
        self.assertFalse(dispatch["delivered"], "a held delivery is not a delivery")
        self.assertEqual("queued", dispatch["outcome"])
        self.assertEqual([], self.herdr.sends, "nothing may be typed over a human")
        log = "\n".join(captured.output)
        self.assertIn("NOT delivered", log)
        self.assertIn("not counted as delivered", log)
        self.assertNotIn("nudge DELIVERED", log)
        self.assertTrue([m for queue in self.flock.outbox.values() for m in queue
                         if m.status == "queued"],
                        "the prompt must stay queued for when the box clears")

    def test_a_prompt_still_waiting_is_not_stacked_by_the_next_turn_end(self):
        self.hold()
        self.herdr.screens[PANE] = HUMAN_DRAFT
        # Simulate a readiness snapshot taken just before the human began typing.
        with mock.patch.object(self.flock, "delivery_state", return_value={"state": "ready"}):
            self.assertEqual("nudge", self.end_turn()["dispatch"]["action"])
        second = self.end_turn()["dispatch"]
        self.assertEqual(("skip", "held_delivery_pending"),
                         (second["action"], second["reason"]))
        queued = [m for queue in self.flock.outbox.values() for m in queue]
        self.assertEqual(1, len(queued), "one prompt at a time per pane")

    def test_shared_gate_blocks_paused_goal_without_queuing(self):
        self.hold()
        self.herdr.screens[PANE] = "Goal paused (/goal resume)\n"
        result = self.end_turn()["dispatch"]
        self.assertEqual(("skip", "blocked", False),
                         (result["action"], result["reason"], result["delivered"]))
        self.assertEqual([], self.herdr.sends)
        self.assertFalse(any(self.flock.outbox.values()))

    def test_concurrent_turn_ends_cannot_enqueue_twice(self):
        self.hold()
        entered, release = threading.Event(), threading.Event()
        results = []
        def slow_send(*args, **kwargs):
            entered.set()
            if not release.wait(5):
                raise RuntimeError("test did not release send")
            return {"outcome": "ok", "message_id": "fake-confirmed"}
        with mock.patch.object(self.flock, "send_now", side_effect=slow_send) as send:
            first = threading.Thread(target=lambda: results.append(self.end_turn()))
            first.start()
            try:
                self.assertTrue(entered.wait(5))
                second = self.end_turn()["dispatch"]
                self.assertEqual("dispatch_in_progress", second["reason"])
                self.assertFalse(second["delivered"])
            finally:
                release.set()
                first.join(5)
            self.assertFalse(first.is_alive())
            self.assertTrue(results[0]["dispatch"]["delivered"])
            self.assertEqual(1, send.call_count)

    def test_unreadable_lane_fails_closed(self):
        self.hold()
        with mock.patch.object(self.flock, "resolve_lanes", side_effect=RuntimeError("offline")):
            result = self.end_turn()["dispatch"]
        self.assertEqual("readiness_unknown", result["reason"])
        self.assertEqual([], self.herdr.sends)

    def test_one_turn_end_prompts_once(self):
        self.hold()
        self.assertTrue(self.end_turn()["dispatch"]["delivered"])
        second = self.end_turn()["dispatch"]
        self.assertEqual(("skip", "cooldown"), (second["action"], second["reason"]))
        self.assertEqual(1, len(self.herdr.sends))

    def test_the_trigger_can_be_switched_off(self):
        self.hold()
        previous = dunk_core.TURN_END_DISPATCH
        dunk_core.TURN_END_DISPATCH = False
        try:
            result = self.end_turn()
        finally:
            dunk_core.TURN_END_DISPATCH = previous
        self.assertEqual(("skip", "disabled"),
                         (result["dispatch"]["action"], result["dispatch"]["reason"]))
        self.assertEqual([], self.herdr.sends)

    def test_the_nudge_is_recorded_as_a_turn_end_message_and_metric(self):
        self.hold()
        self.end_turn()
        recent = [m for m in self.flock.message_history]
        self.assertEqual(["turn_end"], [m.source for m in recent])
        counts, _durations, _retries = self.flock.metrics.summary(0, 9e9)
        self.assertEqual(1, counts["by_kind"].get("turn.end.dispatch", 0))
        self.assertEqual(1, counts["by_source"].get("turn_end", 0))


if __name__ == "__main__":
    unittest.main()


class CliReportTests(unittest.TestCase):
    """The one line a person reads after `turn end` must not hide the trigger.

    Loaded by path: the CLI is a script, not a module. `run_name` is not "__main__", so
    only its definitions run, not argparse.
    """

    @classmethod
    def setUpClass(cls):
        import runpy
        cls.cli = runpy.run_path(
            os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                         "dunkingsheep"),
            run_name="dunkingsheep_cli_under_test")

    def say(self, result):
        import contextlib
        import io
        buffer = io.StringIO()
        with contextlib.redirect_stdout(buffer):
            self.cli["print_turn_end"](result)
        return buffer.getvalue().strip()

    def test_a_delivered_nudge_is_named(self):
        line = self.say({"pane_id": "%1", "ended": True, "dispatch": {
            "action": "nudge", "task_id": "t7", "delivered": True}})
        self.assertEqual("turn end %1 (ended=True); trigger=turn.end nudged t7: delivered",
                         line)

    def test_a_held_nudge_reads_as_held_not_delivered(self):
        line = self.say({"pane_id": "%1", "ended": True, "dispatch": {
            "action": "nudge", "task_id": "t7", "delivered": False, "outcome": "queued"}})
        self.assertIn("HELD, not delivered (queued)", line)
        self.assertNotIn(": delivered", line)

    def test_a_skip_names_its_reason(self):
        line = self.say({"pane_id": "%1", "ended": True, "dispatch": {
            "action": "skip", "reason": "no_open_work"}})
        self.assertIn("trigger=turn.end no nudge (no_open_work)", line)
