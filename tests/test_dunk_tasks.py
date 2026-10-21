"""Global and per-pane tasks: the store, atomic claims, leases, daemon push."""

import os
import sys
import tempfile
import threading
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401  (isolate live state)
import dunk_api  # noqa: E402
from dunk_core import DunkError, Flock  # noqa: E402
from dunk_inbox import Inbox  # noqa: E402
from dunk_tasks import GLOBAL, TaskError, Tasks  # noqa: E402
from fakes import FakeHerdr  # noqa: E402


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
                task = tasks.claim_next(GLOBAL, pane, lease_seconds=300)["task"]
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
        self.assertEqual([], self.store.get(task["id"])["assignees"])

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
        )

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def test_self_assign_and_global_completion(self):
        task = self.flock.task_add("work", self_pane_id="w2:p1")
        assigned = self.flock.task_assign(task["id"], ["self"], self_pane_id="w2:p1")
        self.assertEqual(["w2:p1"], assigned["assignees"])
        done = self.flock.task_complete(task["id"], self_pane_id="w2:p1")
        self.assertEqual("done", done["status"])
        self.assertEqual("w2:p1", done["completed_by"])

    def test_per_pane_privacy_is_enforced(self):
        with self.assertRaises(DunkError):
            self.flock.task_add("secret", scope="w2:p2", self_pane_id="w9:p9")
        private = self.flock.task_add("mine", scope="self", self_pane_id="w2:p1")
        with self.assertRaises(DunkError):
            self.flock.task_read(private["id"], self_pane_id="w9:p9")

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
        task = self.flock.task_add("work", self_pane_id="w2:p1")
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

