"""Global and per-pane tasks: the store, atomic claims, leases, daemon push."""

import os
import sys
import tempfile
import logging
import threading
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401  (isolate live state)
import dunk_api  # noqa: E402
from dunk_core import DunkError, Flock  # noqa: E402
from dunk_inbox import Inbox  # noqa: E402
import dunk_tasks  # noqa: E402
from dunk_roles import Roles  # noqa: E402
from dunk_tasks import GLOBAL, TaskError, Tasks  # noqa: E402
from fakes import FakeHerdr  # noqa: E402


class CancelledDependencyTests(unittest.TestCase):
    """t878: a cancelled dependency must SURFACE, not silently brick the dependents.

    Two duplicate rows were cancelled on 2026-10-30 and seven dependents became
    permanently unclaimable: the queue satisfies an edge only on 'done', a cancelled
    dependency can never be satisfied, and nothing said so - the rows simply never
    came back from `task next`.
    """

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.store = Tasks(os.path.join(self.tmp.name, "tasks.db"))

    def tearDown(self):
        self.store.close()
        self.tmp.cleanup()

    def _pair(self):
        dep = self.store.add(GLOBAL, "the dependency that gets cancelled")
        row = self.store.add(GLOBAL, "the row that depends on it")
        self.store.update(row["id"], depends_on=[dep["id"]])
        return dep, self.store.get(row["id"])

    def test_a_cancelled_dependency_is_named_and_blocks_the_row(self):
        dep, row = self._pair()
        self.store.cancel(dep["id"], reason="duplicate")
        after = self.store.get(row["id"])
        self.assertEqual([dep["id"]], after["dead_dependencies"])
        self.assertIn(dep["id"], after["blocked_by"])
        self.assertFalse(after["eligible"], "an unsatisfiable edge keeps it out of the queue")

    def test_cancel_names_the_dependents_it_strands(self):
        dep, row = self._pair()
        cancelled = self.store.cancel(dep["id"], reason="duplicate")
        self.assertEqual([row["id"]], cancelled["blocked_dependents"])

    def test_repointing_the_edge_frees_the_row_without_cancelling_it(self):
        """The repair path %107 used on all seven rows: --depends-on REPLACES the list."""
        dep, row = self._pair()
        self.store.cancel(dep["id"], reason="duplicate")
        self.assertFalse(self.store.get(row["id"])["eligible"])
        good = self.store.add(GLOBAL, "the canonical task")
        self.store.complete(good["id"])
        repaired = self.store.update(row["id"], depends_on=[good["id"]])
        self.assertEqual([good["id"]], repaired["depends_on"],
                         "the list is replaced, not appended to")
        self.assertEqual([], repaired["dead_dependencies"])
        self.assertTrue(repaired["eligible"], "a repointed row is claimable again")

    def test_a_done_dependency_is_not_dead(self):
        dep, row = self._pair()
        self.store.complete(dep["id"])
        after = self.store.get(row["id"])
        self.assertEqual([], after["dead_dependencies"])
        self.assertTrue(after["eligible"])


class CancelledDependencyTests(unittest.TestCase):
    """t878: a cancelled dependency must SURFACE, not silently brick the dependents.

    Two duplicate rows were cancelled on 2026-10-30 and seven dependents became
    permanently unclaimable: the queue satisfies an edge only on 'done', a cancelled
    dependency can never be satisfied, and nothing said so - the rows simply never
    came back from `task next`.
    """

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.store = Tasks(os.path.join(self.tmp.name, "tasks.db"))

    def tearDown(self):
        self.store.close()
        self.tmp.cleanup()

    def _pair(self):
        dep = self.store.add(GLOBAL, "the dependency that gets cancelled")
        row = self.store.add(GLOBAL, "the row that depends on it")
        self.store.update(row["id"], depends_on=[dep["id"]])
        return dep, self.store.get(row["id"])

    def test_a_cancelled_dependency_is_named_and_blocks_the_row(self):
        dep, row = self._pair()
        self.store.cancel(dep["id"], reason="duplicate")
        after = self.store.get(row["id"])
        self.assertEqual([dep["id"]], after["dead_dependencies"])
        self.assertIn(dep["id"], after["blocked_by"])
        self.assertFalse(after["eligible"], "an unsatisfiable edge keeps it out of the queue")

    def test_cancel_names_the_dependents_it_strands(self):
        dep, row = self._pair()
        cancelled = self.store.cancel(dep["id"], reason="duplicate")
        self.assertEqual([row["id"]], cancelled["blocked_dependents"])

    def test_repointing_the_edge_frees_the_row_without_cancelling_it(self):
        """The repair path %107 used on all seven rows: --depends-on REPLACES the list."""
        dep, row = self._pair()
        self.store.cancel(dep["id"], reason="duplicate")
        self.assertFalse(self.store.get(row["id"])["eligible"])
        good = self.store.add(GLOBAL, "the canonical task")
        self.store.complete(good["id"])
        repaired = self.store.update(row["id"], depends_on=[good["id"]])
        self.assertEqual([good["id"]], repaired["depends_on"],
                         "the list is replaced, not appended to")
        self.assertEqual([], repaired["dead_dependencies"])
        self.assertTrue(repaired["eligible"], "a repointed row is claimable again")

    def test_a_done_dependency_is_not_dead(self):
        dep, row = self._pair()
        self.store.complete(dep["id"])
        after = self.store.get(row["id"])
        self.assertEqual([], after["dead_dependencies"])
        self.assertTrue(after["eligible"])


class TaskStoreTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = os.path.join(self.tmp.name, "tasks.db")
        self.store = Tasks(self.path)

    def tearDown(self):
        self.store.close()
        self.tmp.cleanup()

    def test_priority_order_then_age(self):
        low = self.store.add(GLOBAL, "low", priority="low")
        urgent = self.store.add(GLOBAL, "urgent", priority="urgent")
        high = self.store.add(GLOBAL, "high", priority="high")
        medium = self.store.add(GLOBAL, "medium", priority="medium")
        order = [task["id"] for task in self.store.list(scope=GLOBAL)]
        self.assertEqual([urgent["id"], high["id"], medium["id"], low["id"]], order)

    def test_default_low_oldest_first_and_explicit_priority_preserved(self):
        older = self.store.add(GLOBAL, "older", assignees=['w1:p1'])
        newer = self.store.add(GLOBAL, "newer", assignees=['w1:p1'])
        explicit = self.store.add(GLOBAL, "explicit medium", priority="medium", assignees=['w1:p1'])
        self.assertEqual('low', older['priority'])
        self.assertEqual('low', newer['priority'])
        self.assertEqual('medium', explicit['priority'])
        self.assertEqual([explicit['id'], older['id'], newer['id']],
                         [t['id'] for t in self.store.list(scope=GLOBAL)])
        self.assertEqual(explicit['id'], self.store.claim_next('mine', 'w1:p1')['task']['id'])

    def test_dependencies_gate_next_until_done(self):
        first = self.store.add(GLOBAL, "first", priority="low")
        second = self.store.add(GLOBAL, "second", priority="urgent",
                                depends_on=[first["id"]])
        claimed = self.store.claim_next(GLOBAL, "w1:p1", lease_seconds=60)
        self.assertEqual(first["id"], claimed["task"]["id"])
        self.assertIsNone(self.store.claim_next(GLOBAL, "w2:p2")["task"])
        self.store.complete(first["id"], by="w1:p1")
        claimed = self.store.claim_next(GLOBAL, "w2:p2", lease_seconds=60)
        self.assertEqual(second["id"], claimed["task"]["id"])

    def test_global_completion_is_final_for_everyone(self):
        task = self.store.add(GLOBAL, "shared", assignees=["a", "b"])
        done = self.store.complete(task["id"], by="a")
        self.assertEqual("done", done["status"])
        self.assertEqual("a", done["completed_by"])
        # b cannot reopen it by claiming; it is no longer eligible.
        self.assertIsNone(self.store.claim_next(GLOBAL, "b")["task"])

    def test_a_tmux_pane_id_is_a_valid_scope(self):
        """The backend flipped from herdr ids (w8:p8) to tmux ids (%4); a scope
        validator that only knew the first spelling made every per-pane task
        unusable on the new backend (2026-10-24)."""
        from dunk_tasks import validate_scope
        self.assertEqual("%4", validate_scope("%4"))
        self.assertEqual("w8:p8", validate_scope("w8:p8"))
        with self.assertRaisesRegex(Exception, "scope must be"):
            validate_scope("not a pane")

    def test_scope_is_private_and_global_is_visible(self):
        pane = self.store.add("w2:p2", "local")
        glob = self.store.add(GLOBAL, "fleet")
        own = {task["id"] for task in self.store.list(self_pane_id="w2:p2")}
        other = {task["id"] for task in self.store.list(self_pane_id="w9:p9")}
        self.assertIn(pane["id"], own)
        self.assertIn(glob["id"], own)
        self.assertNotIn(pane["id"], other)
        self.assertIn(glob["id"], other)

    def test_subtasks_link_and_delete_guard(self):
        parent = self.store.add(GLOBAL, "parent")
        child = self.store.add(GLOBAL, "child", parent_id=parent["id"])
        full = self.store.get(parent["id"])
        self.assertEqual([child["id"]], [item["id"] for item in full["children"]])
        with self.assertRaises(TaskError):
            self.store.delete(parent["id"])
        self.store.delete(child["id"])
        self.store.delete(parent["id"])

    def test_dependency_prevents_delete(self):
        first = self.store.add(GLOBAL, "first")
        self.store.add(GLOBAL, "second", depends_on=[first["id"]])
        with self.assertRaises(TaskError):
            self.store.delete(first["id"])

    def test_atomic_claim_never_double_assigns(self):
        for index in range(20):
            self.store.add(GLOBAL, f"task {index}")
        first, second = Tasks(self.path), Tasks(self.path)
        claimed = []
        barrier = threading.Barrier(2)

        def worker(tasks, pane):
            found = []
            barrier.wait()
            while True:
                task = tasks.claim_next(GLOBAL, pane, lease_seconds=300,
                                        allow_busy=True)["task"]
                if task is None:
                    break
                found.append(task["id"])
            claimed.append((pane, found))

        threads = [
            threading.Thread(target=worker, args=(first, "w1:p1")),
            threading.Thread(target=worker, args=(second, "w2:p2")),
        ]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(timeout=30)
        try:
            ids = [task_id for _pane, found in claimed for task_id in found]
            self.assertEqual(20, len(ids))
            self.assertEqual(20, len(set(ids)), "a task was claimed twice")
        finally:
            first.close()
            second.close()

    def test_lease_expiry_requeues(self):
        task = self.store.add(GLOBAL, "work")
        self.store.claim_next(GLOBAL, "w1:p1", lease_seconds=0.05)
        self.assertEqual([], self.store.reap(alive_source="live", alive_panes={"w1:p1"}))
        import time
        time.sleep(0.1)
        requeued = self.store.reap(alive_source="live", alive_panes={"w1:p1"})
        self.assertEqual([(task["id"], "expired")],
                         [(item["task_id"], item["reason"]) for item in requeued])
        self.assertEqual("open", self.store.get(task["id"])["status"])
        # t145/owner2317: the claim made w1:p1 the owner, and expiry reopens the
        # task for that owner instead of deleting the assignment. A task that
        # should change hands needs an explicit assign/complete.
        self.assertEqual(["w1:p1"], self.store.get(task["id"])["assignees"])

    def test_busy_lane_is_refused_a_second_task(self):
        """t145: a lane already holding in-progress work is not handed more.

        The incident behind this task was a busy lane (codex-parity) being given
        another lane's reaped task. A refusal must be a refusal, not a silent
        empty result: the caller gets a reason and the second task stays open
        and unassigned.
        """
        first = self.store.add(GLOBAL, "mine", priority="urgent",
                               assignees=["w1:p1"])
        second = self.store.add(GLOBAL, "second", priority="low")
        claimed = self.store.claim_next(GLOBAL, "w1:p1", lease_seconds=300)
        self.assertEqual(first["id"], claimed["task"]["id"])
        refused = self.store.claim_next(GLOBAL, "w1:p1", lease_seconds=300)
        self.assertIsNone(refused["task"])
        self.assertEqual("busy", refused["refused"]["reason"])
        self.assertEqual([first["id"]], refused["refused"]["task_ids"])
        still = self.store.get(second["id"])
        self.assertEqual("open", still["status"])
        self.assertEqual([], still["assignees"])
        self.assertEqual([], self.store.leases_for(second["id"]))

    def test_busy_lane_may_take_parallel_work_only_with_the_explicit_flag(self):
        first = self.store.add(GLOBAL, "first")
        second = self.store.add(GLOBAL, "second", priority="urgent")
        self.store.claim_next(GLOBAL, "w1:p1", lease_seconds=300)
        self.assertIsNone(self.store.claim_next(GLOBAL, "w1:p1")["task"])
        allowed = self.store.claim_next(GLOBAL, "w1:p1", lease_seconds=300,
                                        allow_busy=True)
        self.assertEqual(first["id"], allowed["task"]["id"])
        self.assertIsNone(allowed.get("refused"))
        held = {lease["task_id"] for lease in self.store.leases_for_pane("w1:p1")}
        self.assertEqual({first["id"], second["id"]}, held)

    def test_refusal_reason_names_why_nothing_was_claimable(self):
        self.assertEqual(
            "empty",
            self.store.claim_next(GLOBAL, "w1:p1")["refused"]["reason"])
        task = self.store.add(GLOBAL, "owned", assignees=["w2:p2"])
        refused = self.store.claim_next(GLOBAL, "w1:p1")
        self.assertEqual("owned_elsewhere", refused["refused"]["reason"])
        self.assertEqual(["w2:p2"], refused["refused"]["owned_by"])
        self.assertEqual(task["id"], refused["refused"]["task_ids"][0])
        blocked = self.store.add(GLOBAL, "blocked", depends_on=[task["id"]])
        refused = self.store.claim_next(GLOBAL, "w1:p1")
        self.assertEqual("owned_elsewhere", refused["refused"]["reason"],
                         "an open task owned elsewhere still explains the refusal")
        only_blocked = Tasks(os.path.join(self.tmp.name, "blocked.db"))
        try:
            gate = only_blocked.add(GLOBAL, "gate")
            only_blocked.block(gate["id"], reason="paused")
            waiting = only_blocked.add(GLOBAL, "waiting", depends_on=[gate["id"]])
            refused = only_blocked.claim_next(GLOBAL, "w1:p1")
            self.assertEqual("blocked", refused["refused"]["reason"])
            self.assertEqual([waiting["id"]], refused["refused"]["task_ids"])
        finally:
            only_blocked.close()

    def test_every_refusal_is_logged_with_the_pane_and_reason(self):
        with self.assertLogs("dunkingsheep.tasks", level="INFO") as logs:
            self.store.claim_next(GLOBAL, "w1:p1")
        self.assertIn("refused", logs.output[0])
        self.assertIn("w1:p1", logs.output[0])
        self.assertIn("empty", logs.output[0])

    def test_expired_lease_keeps_the_durable_assignee_and_blocks_other_lanes(self):
        """t145/owner2317: a lease is ACTIVITY, an assignment is OWNERSHIP.

        Owner2317 found ten global tasks that lost their assignees 30 minutes
        after a claim expired, which let the next global `task next` take work
        that was never released. Expiry must reopen the task for its owner and
        leave the ownership alone.
        """
        task = self.store.add(GLOBAL, "owned work", assignees=["w1:p1"])
        self.store.claim_next(GLOBAL, "w1:p1", lease_seconds=0.05)
        import time
        time.sleep(0.1)
        requeued = self.store.reap(alive_source="live", alive_panes={"w1:p1"})
        self.assertEqual([(task["id"], "expired")],
                         [(item["task_id"], item["reason"]) for item in requeued])
        reopened = self.store.get(task["id"])
        self.assertEqual("open", reopened["status"])
        self.assertEqual(["w1:p1"], reopened["assignees"],
                         "expiry erased the durable owner")
        stolen = self.store.claim_next(GLOBAL, "w2:p2")
        self.assertIsNone(stolen["task"], "another lane took expired owned work")
        self.assertEqual("owned_elsewhere", stolen["refused"]["reason"])
        self.assertEqual(["w1:p1"], stolen["refused"]["owned_by"])
        back = self.store.claim_next(GLOBAL, "w1:p1")
        self.assertEqual(task["id"], back["task"]["id"], "the owner could not reclaim")

    def test_reap_advisory_names_the_sole_owner_when_that_owner_is_gone(self):
        """The advisory must fire for the common case: the owner IS the pane
        whose lease ended, and it is not in the live pane list. Filtering the
        expiring claimant out of the owner set hides exactly that case (t145
        review i32091)."""
        task = self.store.add(GLOBAL, "dead owner work", assignees=["w1:p1"])
        self.store.claim_next(GLOBAL, "w1:p1", lease_seconds=0.05)
        import time
        time.sleep(0.1)
        with self.assertLogs("dunkingsheep.tasks", level="INFO") as logs:
            self.store.reap(alive_source="live", alive_panes={"w2:p2"})
        self.assertTrue(any("reopened while owned" in line and "w1:p1" in line
                            for line in logs.output),
                        f"no advisory for a sole dead owner: {logs.output}")
        self.assertEqual(["w1:p1"], self.store.get(task["id"])["assignees"])

    def test_reap_advisory_is_silent_when_the_owner_is_alive(self):
        self.store.add(GLOBAL, "live owner work", assignees=["w1:p1"])
        self.store.claim_next(GLOBAL, "w1:p1", lease_seconds=0.05)
        import time
        time.sleep(0.1)
        logging.getLogger("dunkingsheep.tasks").setLevel(logging.INFO)
        with self.assertNoLogs("dunkingsheep.tasks", level="INFO"):
            self.store.reap(alive_source="live", alive_panes={"w1:p1"})

    def test_release_keeps_the_durable_assignee_and_blocks_other_lanes(self):
        task = self.store.add(GLOBAL, "owned work", assignees=["w1:p1"])
        self.store.claim_next(GLOBAL, "w1:p1", lease_seconds=300)
        self.store.release(task["id"], "w1:p1")
        released = self.store.get(task["id"])
        self.assertEqual("open", released["status"])
        self.assertEqual(["w1:p1"], released["assignees"],
                         "release erased the durable owner")
        self.assertIsNone(self.store.claim_next(GLOBAL, "w2:p2")["task"])
        self.assertEqual(task["id"], self.store.claim_next(GLOBAL, "w1:p1")["task"]["id"])

    def test_pane_gone_alone_does_not_requeue(self):
        task = self.store.add(GLOBAL, "work")
        # Legacy lease: no caller identity -> unattributable, not dead.
        self.store.claim_next(GLOBAL, "w1:p1", lease_seconds=300)
        self.assertEqual([], self.store.reap(alive_source="live", alive_panes={"w2:p2"}))
        # A live agent behind a vanished pane is still a live claim.
        self.store.release(task["id"], "w1:p1")
        self.store.claim_next(GLOBAL, "w1:p1", lease_seconds=300,
                              caller={"pid": 42, "start": "42", "name": "pi"})
        self.assertEqual([], self.store.reap(
            alive_source="live", alive_panes={"w2:p2"}, caller_alive=lambda pid, start: True))

    def test_pane_gone_with_a_dead_caller_requeues(self):
        task = self.store.add(GLOBAL, "work")
        self.store.claim_next(GLOBAL, "w1:p1", lease_seconds=300,
                              caller={"pid": 42, "start": "42", "name": "pi"})
        requeued = self.store.reap(
            alive_source="live", alive_panes={"w2:p2"}, caller_alive=lambda pid, start: False)
        self.assertEqual([(task["id"], "caller_dead")],
                         [(item["task_id"], item["reason"]) for item in requeued])

    def test_heartbeat_extends_and_requires_a_lease(self):
        task = self.store.add(GLOBAL, "work")
        claimed = self.store.claim_next(GLOBAL, "w1:p1", lease_seconds=30)
        extended = self.store.heartbeat(task["id"], "w1:p1", lease_seconds=600)
        self.assertEqual(600, extended["lease_seconds"])
        with self.assertRaises(TaskError):
            self.store.heartbeat(task["id"], "w2:p2")

    def test_multi_assignee_allows_parallel_claims(self):
        task = self.store.add(GLOBAL, "shared", multi_assignee=True)
        one = self.store.claim_next(GLOBAL, "w1:p1", lease_seconds=60)["task"]
        two = self.store.claim_next(GLOBAL, "w2:p2", lease_seconds=60)["task"]
        self.assertEqual(task["id"], one["id"])
        self.assertEqual(task["id"], two["id"])
        self.assertEqual(["w1:p1", "w2:p2"], two["assignees"])
        # Both leases are live, so neither is reaped.
        self.assertEqual([], self.store.reap(alive_source="live", alive_panes={"w1:p1", "w2:p2"}))

    def test_claim_requires_a_pane(self):
        self.store.add(GLOBAL, "work")
        with self.assertRaises(TaskError):
            self.store.claim_next(GLOBAL, None)

    def test_metrics_track_throughput_and_latency(self):
        one = self.store.add(GLOBAL, "first", priority="high",
                             created_by="w1:p1")
        two = self.store.add(GLOBAL, "second", priority="urgent")
        self.store.add(GLOBAL, "third")
        claimed = self.store.claim_next(GLOBAL, "w1:p1", lease_seconds=60)["task"]
        self.store.complete(claimed["id"], by="w1:p1")
        self.store.block(one["id"], reason="waiting")
        metrics = self.store.metrics(scopes=[GLOBAL])
        self.assertEqual(3, metrics["throughput"]["created"])
        self.assertEqual(1, metrics["throughput"]["claimed"])
        self.assertEqual(1, metrics["throughput"]["completed"])
        self.assertEqual(1, metrics["throughput"]["blocked"])
        self.assertEqual(1, metrics["latency_ms"]["completion"]["count"])
        self.assertGreaterEqual(metrics["latency_ms"]["completion"]["p50"], 0)
        self.assertEqual([{"actor": "w1:p1", "count": 1}],
                         metrics["by_actor"]["completed"])
        self.assertEqual(1, metrics["current"]["by_status"]["done"])
        self.assertEqual(1, metrics["current"]["by_status"]["blocked"])
        self.assertGreaterEqual(metrics["coverage"]["events_total"], 5)

    def test_leader_held_is_the_needs_owner_queue(self):
        # FLAT_FLEET.md 5: no leader. `leader_held` keeps its name for the board
        # and now counts the questions lanes asked the owner (blocked with a
        # 'needs-owner:' reason), nothing else: unassigned work is the queue's.
        real_now = dunk_tasks._now
        state = {"calls": 0}
        def fake_now():
            state["calls"] += 1
            return 1000.0 if state["calls"] == 1 else 2000.0
        dunk_tasks._now = fake_now
        try:
            asked = self.store.add(GLOBAL, "which port?")
            self.store.block(asked["id"], reason="needs-owner: which port do we target?")
            self.store.add(GLOBAL, "unassigned work")
            self.store.add(GLOBAL, "assigned", assignees=["w1:p1"])
            other = self.store.add(GLOBAL, "blocked on a dependency")
            self.store.block(other["id"], reason="waiting on PR 12")
        finally:
            dunk_tasks._now = real_now
        metrics = self.store.metrics(window_s=3600, until=5000.0, scopes=[GLOBAL])
        held = metrics["current"]["leader_held"]
        self.assertEqual(1, held["count"])
        self.assertEqual(1000.0, held["oldest_created_at"])
        self.assertEqual(4000.0, held["oldest_age_s"])
        self.assertIsNone(held["leader_pane"])

    def test_leader_held_empty_when_nobody_asked_the_owner(self):
        self.store.add(GLOBAL, "unassigned")
        self.store.add(GLOBAL, "assigned", assignees=["w1:p1"])
        metrics = self.store.metrics(window_s=3600, until=5000.0, scopes=[GLOBAL])
        held = metrics["current"]["leader_held"]
        self.assertEqual(0, held["count"])
        self.assertIsNone(held["oldest_created_at"])
        self.assertIsNone(held["oldest_age_s"])

    def test_metrics_bucket_throughput_by_hour(self):
        self.store.add(GLOBAL, "one")
        self.store.add(GLOBAL, "two")
        metrics = self.store.metrics(window_s=3600, scopes=[GLOBAL], bucket="hour")
        self.assertEqual(3600, metrics["window"]["bucket_seconds"])
        self.assertEqual(1, len(metrics["throughput"]["buckets"]))
        self.assertEqual(2, metrics["throughput"]["buckets"][0]["created"])

    def test_concurrent_add_never_collides(self):
        first, second = Tasks(self.path), Tasks(self.path)
        ids, errors = [], []
        barrier = threading.Barrier(2)
        lock = threading.Lock()

        def worker(tasks):
            barrier.wait()
            for index in range(50):
                try:
                    task = tasks.add(GLOBAL, f"concurrent {index}")
                    with lock:
                        ids.append(task["id"])
                except Exception as error:  # noqa: BLE001 - reported below
                    with lock:
                        errors.append(error)

        threads = [threading.Thread(target=worker, args=(first,)),
                   threading.Thread(target=worker, args=(second,))]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(timeout=30)
        try:
            self.assertEqual([], errors)
            self.assertEqual(100, len(ids))
            self.assertEqual(100, len(set(ids)), "duplicate task id allocated")
        finally:
            first.close()
            second.close()

    def test_assigned_agent_can_claim_its_task(self):
        task = self.store.add(GLOBAL, "assigned", assignees=["w1:p1"])
        other = self.store.list(scope=GLOBAL, self_pane_id="w2:p2")[0]
        mine = self.store.list(scope=GLOBAL, self_pane_id="w1:p1")[0]
        self.assertFalse(other["eligible"])
        self.assertTrue(mine["eligible"])
        self.assertIsNone(self.store.claim_next(GLOBAL, "w2:p2")["task"])
        claimed = self.store.claim_next(GLOBAL, "w1:p1")
        self.assertEqual(task["id"], claimed["task"]["id"])

    def test_reap_records_actual_status_when_another_lease_lives(self):
        task = self.store.add(GLOBAL, "shared", multi_assignee=True)
        self.store.claim_next(GLOBAL, "w1:p1", lease_seconds=0.05)
        self.store.claim_next(GLOBAL, "w2:p2", lease_seconds=300)
        import time as _time
        _time.sleep(0.1)
        reaped = self.store.reap(alive_source="live", alive_panes={"w1:p1", "w2:p2"})
        self.assertEqual([], reaped,
                         "the task was not requeued; another lease lives")
        self.assertEqual(["w2:p2"],
                         [lease["pane_id"]
                          for lease in self.store.leases_for(task["id"])])
        self.assertEqual("in_progress", self.store.get(task["id"])["status"])
        metrics = self.store.metrics(scopes=[GLOBAL])
        self.assertEqual(0, metrics["throughput"]["requeued"],
                         "a task still held by another lease is not a requeue")


    def test_mine_claims_owned_global_and_skips_unowned_backlog(self):
        """t425: 'mine' selects ownership, not scope. An older, higher-
        priority unowned global backlog task must not shadow the caller's
        own assigned global task."""
        backlog = self.store.add(GLOBAL, "leader backlog", priority="urgent")
        owned = self.store.add(GLOBAL, "my work", priority="medium",
                               assignees=["w1:p1"])
        claimed = self.store.claim_next("mine", "w1:p1", lease_seconds=300)
        self.assertEqual(owned["id"], claimed["task"]["id"])
        self.assertEqual("in_progress", claimed["task"]["status"])
        self.assertEqual("mine", claimed["scope"])
        still = self.store.get(backlog["id"])
        self.assertEqual("open", still["status"])
        self.assertEqual([], still["assignees"])

    def test_mine_claims_own_pane_assigned_task(self):
        """t425: own-pane-scoped tasks naming the caller are eligible."""
        local = self.store.add("w1:p1", "local work", assignees=["w1:p1"])
        claimed = self.store.claim_next("mine", "w1:p1", lease_seconds=300)
        self.assertEqual(local["id"], claimed["task"]["id"])

    def test_mine_excludes_other_workers_tasks(self):
        """t425: another lane's assigned task is never a 'mine' candidate,
        and the refusal is honest empty rather than a silent skip."""
        other = self.store.add(GLOBAL, "their work", assignees=["w9:p9"])
        refused = self.store.claim_next("mine", "w1:p1", lease_seconds=300)
        self.assertIsNone(refused["task"])
        self.assertEqual("empty", refused["refused"]["reason"])
        still = self.store.get(other["id"])
        self.assertEqual("open", still["status"])
        self.assertEqual(["w9:p9"], still["assignees"])

    def test_mine_empty_when_no_owned_work(self):
        """t425: no owned work is an honest empty, not an error."""
        self.store.add(GLOBAL, "leader backlog")
        refused = self.store.claim_next("mine", "w1:p1", lease_seconds=300)
        self.assertIsNone(refused["task"])
        self.assertEqual("empty", refused["refused"]["reason"])
        self.assertEqual([], refused["refused"]["task_ids"])

    def test_mine_busy_refusal_preserved(self):
        """t425: holding in-progress work refuses a second 'mine' claim
        unless allow_busy is set, exactly like every other scope."""
        first = self.store.add(GLOBAL, "first", assignees=["w1:p1"])
        second = self.store.add(GLOBAL, "second", assignees=["w1:p1"])
        claimed = self.store.claim_next("mine", "w1:p1", lease_seconds=300)
        self.assertEqual(first["id"], claimed["task"]["id"])
        refused = self.store.claim_next("mine", "w1:p1", lease_seconds=300)
        self.assertIsNone(refused["task"])
        self.assertEqual("busy", refused["refused"]["reason"])
        allowed = self.store.claim_next("mine", "w1:p1", lease_seconds=300,
                                        allow_busy=True)
        self.assertEqual(second["id"], allowed["task"]["id"])

    def test_mine_requires_a_pane(self):
        """t425: ownership is relative to a caller; anonymous 'mine'
        is refused, not treated as global."""
        with self.assertRaises(TaskError):
            self.store.claim_next("mine", None, lease_seconds=300)

    def test_pane_id_scope_does_not_take_anothers_assigned_global(self):
        """t425 regression pin of the observed %36-vs-%34 friction: a pane
        id as the positional reads that pane's private queue (scope), so it
        never takes another lane's assigned GLOBAL work. That behaviour is
        preserved; 'mine' is the explicit tool for assigned work."""
        owned = self.store.add(GLOBAL, "their work", assignees=["%34"])
        result = self.store.claim_next("%34", "%36", lease_seconds=300)
        self.assertIsNone(result["task"])
        self.assertEqual("empty", result["refused"]["reason"])
        still = self.store.get(owned["id"])
        self.assertEqual("open", still["status"])
        self.assertEqual(["%34"], still["assignees"])
        mine = self.store.claim_next("mine", "%34", lease_seconds=300)
        self.assertEqual(owned["id"], mine["task"]["id"])


class FlockTaskTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.herdr = FakeHerdr()
        self.flock = Flock(
            herdr=self.herdr,
            state_file=os.path.join(self.tmp.name, "dunks.json"),
            persist=False,
            restore=False,
            inbox=Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                        blob_dir=os.path.join(self.tmp.name, "blobs")),
            tasks=Tasks(path=os.path.join(self.tmp.name, "tasks.db")),
            roles=Roles(path=os.path.join(self.tmp.name, "roles.json"),
                        tokens_path=os.path.join(self.tmp.name, "tokens.json")),
        )

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def test_task_read_shows_the_latest_note_reasons(self):
        """t605: notes live in task_events, so `task read` used to show a fully
        reviewed task as if nobody had looked at it - and the queue re-nudged it."""
        task = self.flock.task_add("reviewed work", self_pane_id="w2:p1")
        self.flock.task_note(task["id"], reason="first: ran ctest 635/635",
                             self_pane_id="w2:p1")
        self.flock.task_note(task["id"],
                             reason="second: head abc123; did NOT verify media",
                             self_pane_id="w2:p1")
        read = self.flock.task_read(task["id"], self_pane_id="w2:p1")
        reasons = [row["reason"] for row in read["notes"]]
        self.assertEqual(["first: ran ctest 635/635",
                          "second: head abc123; did NOT verify media"], reasons,
                         "oldest first, both visible")
        self.assertEqual("w2:p1", read["notes"][0]["actor"])

    def test_task_read_without_notes_is_empty_not_an_error(self):
        task = self.flock.task_add("no notes yet", self_pane_id="w2:p1")
        read = self.flock.task_read(task["id"], self_pane_id="w2:p1")
        self.assertEqual([], read["notes"])

    def test_task_read_keeps_only_the_last_three_notes(self):
        task = self.flock.task_add("many notes", self_pane_id="w2:p1")
        for index in range(5):
            self.flock.task_note(task["id"], reason=f"note {index}",
                                 self_pane_id="w2:p1")
        read = self.flock.task_read(task["id"], self_pane_id="w2:p1")
        self.assertEqual(["note 2", "note 3", "note 4"],
                         [row["reason"] for row in read["notes"]])

    def test_task_read_is_still_read_only(self):
        """The fix must not turn a read into a write: no new events, same state."""
        task = self.flock.task_add("read me", self_pane_id="w2:p1")
        self.flock.task_note(task["id"], reason="evidence", self_pane_id="w2:p1")
        before = self.flock.tasks.db.execute(
            "SELECT COUNT(*) FROM task_events WHERE task_id = ?",
            (task["id"],)).fetchone()[0]
        self.flock.task_read(task["id"], self_pane_id="w2:p1")
        self.flock.task_read(task["id"], self_pane_id="w2:p1")
        after = self.flock.tasks.db.execute(
            "SELECT COUNT(*) FROM task_events WHERE task_id = ?",
            (task["id"],)).fetchone()[0]
        self.assertEqual(before, after)
        self.assertEqual("open", self.flock.task_read(
            task["id"], self_pane_id="w2:p1")["status"])

    def test_self_assign_and_global_completion(self):
        task = self.flock.task_add("work", self_pane_id="w2:p1")
        assigned = self.flock.task_assign(task["id"], ["self"], self_pane_id="w2:p1")
        self.assertEqual(["w2:p1"], assigned["assignees"])
        done = self.flock.task_complete(task["id"], self_pane_id="w2:p1")
        self.assertEqual("done", done["status"])
        self.assertEqual("w2:p1", done["completed_by"])

    def test_api_task_default_low_and_explicit_high(self):
        plain = self.flock.task_add('default', self_pane_id='w2:p1')
        high = self.flock.task_add('manual', priority='high', self_pane_id='w2:p1')
        self.assertEqual('low', plain['priority'])
        self.assertEqual('high', high['priority'])

    def test_per_pane_privacy_is_enforced(self):
        with self.assertRaises(DunkError):
            self.flock.task_add("secret", scope="w2:p2", self_pane_id="w9:p9")
        private = self.flock.task_add("mine", scope="self", self_pane_id="w2:p1")
        with self.assertRaises(DunkError):
            self.flock.task_read(private["id"], self_pane_id="w9:p9")

    def test_anonymous_caller_cannot_create_in_another_panes_private_queue(self):
        # t278: a caller with no pane (human CLI, a unit) could silently create a
        # task in another pane's PRIVATE queue; it showed in task list but no lane
        # could read or complete it. Refuse, naming the workaround.
        with self.assertRaises(DunkError) as caught:
            self.flock.task_add("phantom", scope="w2:p2", self_pane_id=None)
        message = str(caught.exception)
        self.assertIn("--scope global", message)   # the workaround is named
        # The refusal is not a silent private creation: nothing landed anywhere.
        self.assertEqual([], self.flock.tasks.list(scope="w2:p2"))
        self.assertEqual([], self.flock.tasks.list(scope=GLOBAL))

    def test_anonymous_global_add_with_assign_is_unchanged(self):
        # The legitimate path the guard must NOT break: a no-pane caller creating
        # a GLOBAL task and assigning a lane (scope defaults to global).
        task = self.flock.task_add("fleet work", assignees=["w2:p2"],
                                   self_pane_id=None)
        self.assertEqual(GLOBAL, task["scope"])
        self.assertEqual(["w2:p2"], task["assignees"])
        read = self.flock.task_read(task["id"], self_pane_id="w2:p2")
        self.assertEqual("fleet work", read["title"])

    def test_pane_caller_assigning_another_lane_on_global_needs_the_owner_role(self):
        # The old VP-assigns workflow is gone (FLAT_FLEET.md 5): a lane pulls
        # work with `task next`; only the owner hands work to a named lane.
        with self.assertRaisesRegex(DunkError, "only to yourself"):
            self.flock.task_add("assigned work", assignees=["w9:p9"], self_pane_id="w2:p1")
        self.flock.roles.register("w2:p1", "owner")
        task = self.flock.task_add("assigned work", assignees=["w9:p9"],
                                   self_pane_id="w2:p1")
        self.assertEqual(GLOBAL, task["scope"])
        self.assertEqual(["w9:p9"], task["assignees"])

    def test_t145_characterisation_pane_id_as_scope_claims_nothing(self):
        """CHARACTERISATION of the observed CLI trap, NOT a desired-behaviour
        assertion (t145). `task next <pane id>` reads that argument as a SCOPE
        (Flock.task_next -> _task_scope), so an agent passing its own pane id
        scans its empty private queue, gets no task, and is silently subscribed
        to follow that empty scope while the global task it is assigned stays
        open. The documented call (no argument = scope global) claims it.

        This pins today's behaviour so the report is reproducible; a fix that
        refuses a pane id as a scope (or reinterprets it as the caller) must
        REPLACE this test rather than delete it, and the replacement fails
        against this revision."""
        task = self.flock.task_add("assigned work", priority="high",
                                   assignees=["w2:p1"], self_pane_id="w2:p1")
        trap = self.flock.task_next(scope="w2:p1", self_pane_id="w2:p1")
        self.assertIsNone(trap["task"],
                          "the pane's own id was read as a scope, not a caller")
        self.assertTrue(trap.get("following"))
        self.assertEqual("w2:p1", self.flock.task_waiters["w2:p1"],
                         "the pane now follows its empty private scope")
        still = self.flock.task_read(task["id"], self_pane_id="w2:p1")
        self.assertEqual("open", still["status"], "the assigned task was never claimed")
        self.assertEqual([], still["leases"])
        claimed = self.flock.task_next(self_pane_id="w2:p1")
        self.assertEqual(task["id"], claimed["task"]["id"],
                         "the documented call claims the task it is assigned")
        self.assertEqual("in_progress", claimed["task"]["status"])

    def test_t145_cli_handler_reads_the_positional_as_a_scope(self):
        """The exact CLI shape that produced the report: `task next %58`
        arrives at h_task_next as {"scope": "%58"}, so the pane id is a scope.
        This is the handler-level reproduction of the characterisation above."""
        task = self.flock.task_add("assigned work", assignees=["%58"],
                                   self_pane_id="%58")
        trap = dunk_api.h_task_next(self.flock, {"scope": "%58"}, "%58")
        self.assertIsNone(trap["task"])
        self.assertTrue(trap.get("following"))
        self.assertEqual("open",
                         self.flock.task_read(task["id"], self_pane_id="%58")["status"])
        claimed = dunk_api.h_task_next(self.flock, {}, "%58")
        self.assertEqual(task["id"], claimed["task"]["id"])

    def test_cli_handler_refuses_a_busy_pane_and_honours_allow_busy(self):
        """The plumbing, not just the store: `task next` from a pane that holds
        work is refused with a reason, keeps the documented auto-follow, and
        `allow_busy` is what a lane must pass to take parallel work."""
        first = self.flock.task_add("first", self_pane_id="w2:p1")
        second = self.flock.task_add("second", self_pane_id="w2:p1")
        claimed = dunk_api.h_task_next(self.flock, {}, "w2:p1")
        self.assertEqual(first["id"], claimed["task"]["id"])
        refused = dunk_api.h_task_next(self.flock, {}, "w2:p1")
        self.assertIsNone(refused["task"])
        self.assertEqual("busy", refused["refused"]["reason"])
        self.assertEqual([first["id"]], refused["refused"]["task_ids"])
        self.assertTrue(refused.get("following"),
                        "an empty task_next still subscribes the pane for a push")
        allowed = dunk_api.h_task_next(self.flock, {"allow_busy": True}, "w2:p1")
        self.assertEqual(second["id"], allowed["task"]["id"])

    def test_an_unleased_task_assigned_to_another_pane_is_not_claimable(self):
        """t145 guard: after a lease is reaped the task has no lease row but
        still names its owner, so a different pane must not be handed it."""
        task = self.flock.task_add("owned elsewhere", assignees=["w9:p9"],
                                   self_pane_id="w9:p9")
        self.assertIsNone(self.flock.task_next(self_pane_id="w2:p1")["task"],
                          "a foreign pane claimed work owned by w9:p9")
        mine = self.flock.task_next(self_pane_id="w9:p9")
        self.assertEqual(task["id"], mine["task"]["id"],
                         "the named assignee could not claim its own task")


    def test_inbox_read_appends_global_and_pane_tasks(self):
        self.flock.inbox.send("Codex Site", "hello", sender="pm")
        self.flock.task_add("global work", self_pane_id="w2:p1")
        self.flock.task_add("local work", scope="self", self_pane_id="w2:p1")
        result = self.flock.inbox_read(self_pane_id="w2:p1")
        self.assertEqual(1, len(result["messages"]))
        self.assertIn("tasks", result)
        titles = [task["title"] for task in result["tasks"]["global"]]
        titles += [task["title"] for task in result["tasks"]["pane"]]
        self.assertIn("global work", titles)
        self.assertIn("local work", titles)

    def test_next_claims_and_auto_follows_when_empty(self):
        # t110: an unassigned add is owned by its CREATOR, so the claimer here has
        # to be the creator (a task owned by w2:p1 is not w2:p2's to pull).
        task = self.flock.task_add("work", self_pane_id="w2:p2")
        claimed = self.flock.task_next(self_pane_id="w2:p2")
        self.assertEqual(task["id"], claimed["task"]["id"])
        self.assertEqual("in_progress", claimed["task"]["status"])
        self.assertEqual(["w2:p2"], claimed["task"]["assignees"])
        empty = self.flock.task_next(self_pane_id="w2:p2")
        self.assertIsNone(empty["task"])
        self.assertTrue(empty.get("following"))
        self.assertEqual([{"pane_id": "w2:p2", "scope": "global"}],
                         self.flock.task_followers()["followers"])

    def test_push_to_idle_follower(self):
        task = self.flock.task_add("pushed work", priority="high",
                                   self_pane_id="w2:p1")
        self.flock.task_follow(self_pane_id="w2:p1")
        self.flock._push_tasks()
        claimed = self.flock.task_read(task["id"], self_pane_id="w2:p1")
        self.assertEqual("in_progress", claimed["status"])
        self.assertEqual(["w2:p1"], claimed["claimed_by"])
        messages = self.flock.inbox_list(unread_only=True,
                                         self_pane_id="w2:p1")["messages"]
        self.assertEqual("TASK {}".format(task["id"]), messages[0]["subject"].split(":")[0])
        # A follower already holding a lease is not given a second task.
        self.flock.task_add("another", self_pane_id="w2:p1")
        self.flock._push_tasks()
        self.assertEqual(1, len(self.flock.tasks.leases_for_pane("w2:p1")))

    def test_busy_follower_is_not_pushed(self):
        self.flock.task_add("work", self_pane_id="w2:p1")
        # w2:p3 is a working claude pane.
        self.flock.task_follow(self_pane_id="w2:p3")
        self.flock._push_tasks()
        self.assertEqual([], self.flock.tasks.leases_for_pane("w2:p3"))
        self.assertEqual([], [task for task in self.flock.tasks.list(scope=GLOBAL)
                              if task["status"] == "in_progress"])

    def test_mcp_exposes_task_tools(self):
        names = [command.name for command in dunk_api.mcp_commands()]
        for expected in ("task_add", "task_list", "task_read", "task_update",
                         "task_assign", "task_complete", "task_block",
                         "task_unblock", "task_next", "task_heartbeat",
                         "task_follow", "task_stats", "task_metrics"):
            self.assertIn(expected, names)
        self.assertNotIn("task_reap", names)  # admin-only, socket/CLI

    def test_mine_end_to_end_claim_and_follow_subscription(self):
        """t425: `task next mine` claims the caller's assigned global work
        through the full Flock path and subscribes on empty, like others."""
        task = self.flock.task_add("assigned work", priority="high",
                                   assignees=["w2:p1"], self_pane_id="w2:p1")
        claimed = self.flock.task_next(scope="mine", self_pane_id="w2:p1")
        self.assertEqual(task["id"], claimed["task"]["id"])
        self.assertEqual("in_progress", claimed["task"]["status"])
        busy = self.flock.task_next(scope="mine", self_pane_id="w2:p1")
        self.assertIsNone(busy["task"])
        self.assertEqual("busy", busy["refused"]["reason"])
        self.assertTrue(busy.get("following"))
        self.assertEqual("mine", self.flock.task_waiters["w2:p1"])

    def test_mine_empty_subscribes_mine_follow(self):
        """t425: no owned work is an honest empty that still subscribes the
        pane for a later push of assigned work (not the global backlog)."""
        # Backlog owned by another lane (t110: creator-owned), not by us.
        self.flock.task_add("leader backlog", assignees=["%99"],
                            self_pane_id="%99")
        empty = self.flock.task_next(scope="mine", self_pane_id="w2:p1")
        self.assertIsNone(empty["task"])
        self.assertEqual("empty", empty["refused"]["reason"])
        self.assertTrue(empty.get("following"))
        self.assertEqual("mine", self.flock.task_waiters["w2:p1"])

    def test_follow_mine_scope_accepted(self):
        """t425: `task follow mine` subscribes the pane for pushed assigned
        work; the push loop claims through the same selection."""
        result = self.flock.task_follow(scope="mine", self_pane_id="w2:p1")
        self.assertTrue(result["following"])
        self.assertEqual("mine", result["scope"])
        task = self.flock.task_add("assigned work", priority="high",
                                   assignees=["w2:p1"], self_pane_id="w2:p1")
        self.flock._push_tasks()
        claimed = self.flock.task_read(task["id"], self_pane_id="w2:p1")
        self.assertEqual("in_progress", claimed["status"])
        self.assertEqual(["w2:p1"], claimed["claimed_by"])

    def test_mine_needs_a_calling_pane(self):
        """t425: anonymous 'mine' is refused, not treated as global."""
        with self.assertRaises(DunkError):
            self.flock.task_next(scope="mine", self_pane_id=None)
        with self.assertRaises(DunkError):
            self.flock.task_follow(scope="mine", self_pane_id=None)

    def test_task_next_pane_scope_still_reads_scope_not_owner(self):
        """t425 cross-pane pin of the observed friction: from %36,
        `task next %34` is refused by per-pane privacy (it names %34's
        private queue), so it can never take %34's assigned global work.
        Scope was not reinterpreted as ownership."""
        task = self.flock.task_add("their work", assignees=["%34"],
                                   self_pane_id="%34")
        with self.assertRaises(DunkError) as caught:
            self.flock.task_next(scope="%34", self_pane_id="%36")
        self.assertIn("private", str(caught.exception))
        still = self.flock.task_read(task["id"], self_pane_id="%34")
        self.assertEqual("open", still["status"])

    def test_task_metrics_privacy_and_scope(self):
        self.flock.task_add("global", self_pane_id="w2:p1")
        self.flock.task_add("local", scope="self", self_pane_id="w2:p1")
        metrics = self.flock.task_metrics(self_pane_id="w2:p1")
        self.assertEqual("visible", metrics["scope"])
        self.assertEqual(2, metrics["current"]["total"])
        with self.assertRaises(DunkError):
            self.flock.task_metrics(scope="w9:p9", self_pane_id="w2:p1")
        global_only = self.flock.task_metrics(scope="global", self_pane_id="w2:p1")
        self.assertEqual(1, global_only["current"]["total"])
        # A caller with no pane (the CTO CLI) may query everything.
        everything = self.flock.task_metrics(scope="all", self_pane_id=None)
        self.assertEqual("all", everything["scope"])

    def test_dispatch_round_trip(self):
        added = dunk_api.dispatch(self.flock, "task_add",
                                  {"title": "round trip", "priority": "high"},
                                  self_pane_id="w2:p1")
        self.assertEqual("global", added["scope"])
        listed = dunk_api.dispatch(self.flock, "task_list", {},
                                   self_pane_id="w2:p1")
        self.assertEqual([added["id"]], [task["id"] for task in listed["tasks"]])
        claimed = dunk_api.dispatch(self.flock, "task_next", {},
                                    self_pane_id="w2:p1")
        self.assertEqual(added["id"], claimed["task"]["id"])
        heartbeat = dunk_api.dispatch(self.flock, "task_heartbeat",
                                      {"id": added["id"]}, self_pane_id="w2:p1")
        self.assertGreater(heartbeat["expires_at"], 0)


class PushAndReleaseTests(unittest.TestCase):
    """t595: the push path obeys the same role rules as the pull path, and a
    released claim goes back to the queue instead of stranding behind an
    assignee the claim itself added."""

    WORKER, REVIEWER, OWNER = "w2:p1", "w2:p2", "w9:p9"

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.herdr = FakeHerdr()
        self.roles = Roles(path=os.path.join(self.tmp.name, "roles.json"),
                           tokens_path=os.path.join(self.tmp.name, "tokens.json"))
        self.flock = Flock(
            herdr=self.herdr,
            state_file=os.path.join(self.tmp.name, "dunks.json"),
            persist=False, restore=False,
            inbox=Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                        blob_dir=os.path.join(self.tmp.name, "blobs")),
            tasks=Tasks(path=os.path.join(self.tmp.name, "tasks.db")),
            roles=self.roles)
        self.roles.register(self.OWNER, "owner")
        self.roles.register(self.WORKER, "worker", provider="pi", label="pi-worker-1")
        self.roles.register(self.REVIEWER, "reviewer", provider="claude",
                            label="pi-review-1")

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def review(self, author=None, number=1631):
        return self.flock.tasks.add(
            GLOBAL, f"Review PR #{number}", priority="high", kind="review",
            author=author, repo="huntergdavis/poolrad-decomp", pr=number,
            head="d" * 40, created_by="review-sync")

    def test_a_worker_follower_is_never_pushed_a_review(self):
        task = self.review()                     # the only candidate
        self.flock.task_follow(self_pane_id=self.WORKER)
        self.flock._push_tasks()
        self.assertEqual([], self.flock.tasks.leases_for_pane(self.WORKER),
                         "a worker was pushed a review it cannot complete")
        self.assertEqual("open", self.flock.tasks.get(task["id"])["status"])

    def test_a_reviewer_follower_is_not_pushed_a_review_it_authored(self):
        own = self.review(author=self.REVIEWER, number=1632)
        self.flock.task_follow(self_pane_id=self.REVIEWER)
        self.flock._push_tasks()
        self.assertEqual([], self.flock.tasks.leases_for_pane(self.REVIEWER),
                         "a reviewer was pushed its own PR (no-self-review)")
        self.assertEqual("open", self.flock.tasks.get(own["id"])["status"])
        # And a review by someone else IS pushed to that reviewer.
        other = self.review(author=self.WORKER, number=1633)
        self.flock._push_tasks()
        self.assertEqual([other["id"]],
                         [lease["task_id"] for lease in
                          self.flock.tasks.leases_for_pane(self.REVIEWER)])

    def test_a_released_unassigned_task_is_claimable_by_another_pane(self):
        task = self.flock.tasks.add(GLOBAL, "free work")
        self.assertEqual([], task["assignees"])
        self.flock.task_next(self_pane_id=self.WORKER)
        held = self.flock.tasks.get(task["id"])
        self.assertEqual([self.WORKER], held["assignees"],
                         "the claim adds the claimant as assignee")
        released = self.flock.task_release(task["id"], self_pane_id=self.WORKER)
        self.assertEqual([], released["assignees"],
                         "the release left the claim's own assignee behind")
        self.assertEqual("open", released["status"])
        self.assertTrue(self.flock.tasks._claimable(
            self.flock.tasks.get(task["id"]), self.REVIEWER),
            "another pane still cannot claim a released task")

    def test_an_owner_assignment_survives_claim_and_release(self):
        task = self.flock.tasks.add(GLOBAL, "owner-owned work",
                                    assignees=[self.OWNER])
        self.flock.task_next(self_pane_id=self.OWNER)
        self.assertEqual([self.OWNER], self.flock.tasks.get(task["id"])["assignees"])
        released = self.flock.task_release(task["id"], self_pane_id=self.OWNER)
        self.assertEqual([self.OWNER], released["assignees"],
                         "release deleted an assignment the claim did not make")


class TaskAddOwnershipTests(unittest.TestCase):
    """FLAT_FLEET.md 5: `task add` creates work the QUEUE owns.

    Owner #1708 ("unowned means leader-owned") is retired with the leader: an
    unassigned task is claimable by any lane whose role matches its kind, an
    explicit `--assign` by the owner is delegation and is kept, and a lane may
    assign only itself. No declared-leader environment variable exists.
    """

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.herdr = FakeHerdr()
        self.flock = Flock(
            herdr=self.herdr,
            state_file=os.path.join(self.tmp.name, "dunks.json"),
            persist=False, restore=False,
            inbox=Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                        blob_dir=os.path.join(self.tmp.name, "blobs")),
            tasks=Tasks(path=os.path.join(self.tmp.name, "tasks.db")),
            roles=Roles(path=os.path.join(self.tmp.name, "roles.json"),
                        tokens_path=os.path.join(self.tmp.name, "tokens.json")))
        self._leader = os.environ.pop("DUNKINGSHEEP_LEADER_PANE", None)

    def tearDown(self):
        if self._leader is not None:
            os.environ["DUNKINGSHEEP_LEADER_PANE"] = self._leader
        else:
            os.environ.pop("DUNKINGSHEEP_LEADER_PANE", None)
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def test_no_assign_is_unassigned_and_claimable(self):
        task = self.flock.task_add("work nobody asked for", self_pane_id="w2:p1")
        self.assertEqual([], task["assignees"], "the queue owns unassigned work")
        self.assertEqual("w2:p1", task["created_by"])
        self.assertTrue(task["claimable"])

    def test_an_explicit_assign_by_the_owner_is_delegation_and_is_kept(self):
        self.flock.roles.register("w2:p1", "owner")
        task = self.flock.task_add("for someone else", assignees=["w1:p2"],
                                   self_pane_id="w2:p1")
        self.assertEqual(["w1:p2"], task["assignees"],
                         "delegation is an explicit write; the default must not overwrite it")

    def test_assign_self_still_names_the_caller(self):
        task = self.flock.task_add("mine", assignees=["self"], self_pane_id="w2:p1")
        self.assertEqual(["w2:p1"], task["assignees"])

    def test_work_created_without_a_pane_is_unassigned_too(self):
        # The pane-less creators: systemd units, the web desk. A declared
        # leader environment variable is ignored: there is no leader.
        os.environ["DUNKINGSHEEP_LEADER_PANE"] = "w9:pV"
        task = self.flock.task_add("raised by the daemon")
        self.assertEqual([], task["assignees"])


if __name__ == "__main__":
    unittest.main()

class CallerLivenessTests(unittest.TestCase):
    """A task lease is keyed by pane, liveness-checked by pid + start."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.store = Tasks(os.path.join(self.tmp.name, "tasks.db"))
        self.task = self.store.add(GLOBAL, "caller-liveness")

    def tearDown(self):
        self.store.close()
        self.tmp.cleanup()

    def test_claim_records_the_caller_pid_and_start(self):
        caller = {"pid": os.getpid(), "start": "12345", "name": "pi"}
        self.store.claim_next(GLOBAL, "w1:p1", lease_seconds=60, caller=caller)
        lease = self.store.leases_for(self.task["id"])[0]
        self.assertEqual(os.getpid(), lease["caller_pid"])
        self.assertEqual("12345", lease["caller_start"])
        self.assertEqual("pi", lease["caller_name"])

    def test_heartbeat_refreshes_the_caller_identity(self):
        self.store.claim_next(GLOBAL, "w1:p1", lease_seconds=30,
                              caller={"pid": 1, "start": "1", "name": "old"})
        self.store.heartbeat(self.task["id"], "w1:p1", lease_seconds=60,
                             caller={"pid": 2, "start": "2", "name": "new"})
        lease = self.store.leases_for(self.task["id"])[0]
        self.assertEqual(2, lease["caller_pid"])
        self.assertEqual("new", lease["caller_name"])

    def test_reap_requeues_a_dead_caller_even_with_a_live_pane(self):
        self.store.claim_next(GLOBAL, "w1:p1", lease_seconds=600,
                              caller={"pid": os.getpid(), "start": "1",
                                      "name": "pi"})
        requeued = self.store.reap(
            alive_source="live", alive_panes={"w1:p1"}, caller_alive=lambda pid, start: False)
        self.assertEqual(1, len(requeued))
        self.assertEqual("caller_dead", requeued[0]["reason"])
        self.assertEqual("open", self.store.get(self.task["id"])["status"])
        self.assertEqual([], self.store.leases_for(self.task["id"]))

    def test_reap_keeps_a_live_caller(self):
        self.store.claim_next(GLOBAL, "w1:p1", lease_seconds=600,
                              caller={"pid": os.getpid(), "start": "1",
                                      "name": "pi"})
        self.assertEqual([], self.store.reap(
            alive_source="live", alive_panes={"w1:p1"}, caller_alive=lambda pid, start: True))
        self.assertEqual("in_progress", self.store.get(self.task["id"])["status"])

    def test_reap_does_not_guess_for_a_legacy_lease_without_a_caller(self):
        self.store.claim_next(GLOBAL, "w1:p1", lease_seconds=600)
        self.assertEqual([], self.store.reap(
            alive_source="live", alive_panes={"w1:p1"}, caller_alive=lambda pid, start: False))

    def test_old_lease_table_is_migrated_in_place(self):
        import sqlite3
        path = os.path.join(self.tmp.name, "old.db")
        conn = sqlite3.connect(path)
        conn.execute("CREATE TABLE task_leases (task_id TEXT NOT NULL, "
                     "pane_id TEXT NOT NULL, claimed_at REAL NOT NULL, "
                     "heartbeat_at REAL NOT NULL, expires_at REAL NOT NULL, "
                     "PRIMARY KEY (task_id, pane_id))")
        conn.commit()
        conn.close()
        old = Tasks(path)
        try:
            columns = {row["name"] for row in old.db.execute(
                "PRAGMA table_info(task_leases)")}
            self.assertTrue({"caller_pid", "caller_start", "caller_name"} <= columns)
        finally:
            old.close()



class RowFactoryDriftTests(unittest.TestCase):
    """The guard added in 2.70.21 must FAIL without it.

    Reviewer's requirement (PR #14): a test that fails when dunk_tasks.py is
    reverted is the only evidence the guard does anything. So this clears the
    connection's row factory behind the module's back and asserts a read with ROWS
    still returns rows addressable by column name - which is what raised
    IndexError in the daemon log on 2026-10-24. An empty result cannot show this:
    with no rows there is nothing to index, and the guard has nothing to fix.
    """

    def test_a_cleared_row_factory_does_not_break_a_lease_read(self):
        import sqlite3
        import tempfile
        import time as _time
        from dunk_tasks import Tasks
        with tempfile.TemporaryDirectory() as tmp:
            tasks = Tasks(path=os.path.join(tmp, "tasks.db"))
            try:
                task = tasks.add("global", "a task", priority="high")
                now = _time.time()
                tasks.db.execute(
                    "INSERT INTO task_leases (task_id, pane_id, claimed_at, heartbeat_at, "
                    "expires_at) VALUES (?,?,?,?,?)",
                    (task["id"], "%4", now, now, now + 600))
                tasks.db.commit()
                tasks.db.row_factory = None       # exactly the drift observed
                leases = tasks.leases_for(task["id"])
                self.assertEqual(1, len(leases), "a tuple row must not raise here")
                self.assertEqual("%4", leases[0]["pane_id"],
                                 "the row must be readable by column name")
                self.assertEqual(sqlite3.Row, tasks.db.row_factory,
                                 "the factory is re-asserted so later reads work too")
            finally:
                tasks.close()


class CancelledDependencyTests(unittest.TestCase):
    """t783: a cancelled prerequisite must not block its dependents for ever.

    Measured 2026-10-30: t783 (T04) declares depends_on t781; t781 was CANCELLED, and
    t783 still reported `open, claimable True, blocked_by ['t781'], eligible False`. So
    an owner instruction to "claim t783 specifically" could not be carried out by any
    worker: the task was not a candidate, and a worker cannot assign itself.
    """

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.flock = Flock(
            herdr=FakeHerdr(), persist=False, restore=False,
            state_file=os.path.join(self.tmp.name, "d.json"),
            inbox=Inbox(path=os.path.join(self.tmp.name, "i.db"),
                        blob_dir=os.path.join(self.tmp.name, "b")),
            tasks=Tasks(path=os.path.join(self.tmp.name, "t.db")),
            roles=Roles(path=os.path.join(self.tmp.name, "r.json"),
                        tokens_path=os.path.join(self.tmp.name, "k.json")))

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def test_a_cancelled_dependency_is_satisfied(self):
        pre = self.flock.tasks.add("global", "prerequisite")
        self.flock.tasks.cancel(pre["id"], reason="superseded")
        after = self.flock.tasks.add("global", "dependent", depends_on=[pre["id"]])
        view = self.flock.tasks.get(after["id"])
        self.assertEqual([], view["blocked_by"],
                         "a cancelled prerequisite is not outstanding work")
        self.assertTrue(view["eligible"])

    def test_a_done_dependency_is_satisfied(self):
        pre = self.flock.tasks.add("global", "prerequisite")
        self.flock.tasks.complete(pre["id"])
        after = self.flock.tasks.add("global", "dependent", depends_on=[pre["id"]])
        self.assertEqual([], self.flock.tasks.get(after["id"])["blocked_by"])

    def test_an_open_dependency_still_blocks(self):
        pre = self.flock.tasks.add("global", "prerequisite")
        after = self.flock.tasks.add("global", "dependent", depends_on=[pre["id"]])
        view = self.flock.tasks.get(after["id"])
        self.assertEqual([pre["id"]], view["blocked_by"])
        self.assertFalse(view["eligible"])
