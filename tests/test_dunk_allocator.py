"""Allocator acceptance: durable exclusivity, contracts, ordering and recovery."""
import os
import json
import sys
import tempfile
import threading
import unittest
from types import SimpleNamespace
from unittest.mock import Mock, patch

sys.path.insert(0, os.path.dirname(__file__))
import _bootstrap  # noqa: F401
from dunk_tasks import Tasks, TaskError
from dunk_allocator import AllocationStore, Allocator


class AllocationTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = os.path.join(self.tmp.name, "tasks.db")
        self.tasks = Tasks(self.path)
        self.a = AllocationStore(self.tasks)

    def tearDown(self):
        self.tasks.close()
        self.tmp.cleanup()

    def worker(self, pane, role="worker", skills=None, pool="code"):
        self.a.register(pane, [pool], [role], skills or [], "%leader")

    def task(self, title="real work", priority="low", **kw):
        return self.tasks.add("global", title, body="concrete acceptance", priority=priority,
                              assignees=["%leader"], **kw)

    def offer(self, t, **kw):
        return self.a.offer(t["id"], kw.get("pool", "code"), kw.get("requires", []), "%leader")

    def test_priority_age_and_id_order(self):
        old, high, later = self.task("old"), self.task("high", "high"), self.task("later")
        for t in (old, high, later):
            self.offer(t)
        for p in ("%1", "%2", "%3"):
            self.worker(p)
        result = self.a.allocate({"%1": {}, "%2": {}, "%3": {}})
        self.assertEqual([high["id"], old["id"], later["id"]], [h["task_id"] for h in result])
        for h in result:
            t = self.tasks.get(h["task_id"])
            self.assertEqual([h["pane_id"]], t["assignees"])
            self.assertEqual("in_progress", t["status"])
            self.assertEqual(h["message_id"], self.tasks.next_job_handoffs(h["pane_id"])[0]["message_id"])

    def test_no_capability_no_idle_no_assignment(self):
        t = self.task(); self.offer(t, requires=["c"])
        self.worker("%wrong"); self.worker("%right", skills=["c"])
        self.assertEqual([], self.a.allocate({"%wrong": {}}))
        self.assertEqual([], self.a.allocate({}))
        self.assertEqual(["%leader"], self.tasks.get(t["id"])["assignees"])
        self.assertEqual("%right", self.a.allocate({"%right": {}})[0]["pane_id"])

    def test_manual_work_and_blocked_dependency_preserved(self):
        dependency = self.task("dep")
        blocked = self.task("held", depends_on=[dependency["id"]]); self.offer(blocked)
        self.worker("%worker")
        self.assertEqual([], self.a.allocate({"%worker": {}}))
        self.tasks.complete(dependency["id"], "%leader")
        manual = self.tasks.add("global", "manual", body="reserved", assignees=["%worker"])
        self.assertEqual([], self.a.allocate({"%worker": {}}))
        self.tasks.complete(manual["id"], "%worker")
        self.assertEqual(blocked["id"], self.a.allocate({"%worker": {}})[0]["task_id"])

    def test_private_ready_queue_blocks_capacity_but_dependency_wait_does_not(self):
        t = self.task(); self.offer(t); self.worker("%1")
        private = self.tasks.add("%1", "private owned", body="private", assignees=["%1"])
        self.assertEqual([], self.a.allocate({"%1": {}}))
        dependency = self.task("dependency")
        self.tasks.update(private["id"], depends_on=[dependency["id"]])
        self.assertEqual(t["id"], self.a.allocate({"%1": {}})[0]["task_id"])

    def test_reassignment_and_block_reason_invalidate_offer(self):
        t = self.task(); self.offer(t); self.worker("%1")
        self.tasks.assign(t["id"], ["%other"], mode="set")
        self.assertEqual([], self.a.allocate({"%1": {}}))

    def test_invalidated_offer_is_manual_work_and_blocks_second_offer(self):
        first = self.task(); self.offer(first); self.worker("%1")
        self.tasks.assign(first["id"], ["%1"], mode="set")
        second = self.task("second", "high"); self.offer(second)
        self.assertFalse(self.tasks.get(first["id"])["allocation_waiting"])
        self.assertEqual([], self.a.allocate({"%1": {}}))
        self.assertEqual("open", self.tasks.get(second["id"])["status"])

    def test_former_coauthor_stays_excluded_from_corrected_review(self):
        source = self.task()
        self.tasks.assign(source["id"], ["%coauthor"], mode="add")
        self.worker("%r", "reviewer"); self.worker("%coauthor", "reviewer")
        review = self.a.review(source["id"], "a"*40, "first", "code", [], "%leader")
        self.a.allocate({"%r": {}})
        self.tasks.complete(review["id"], "%r")
        self.tasks.assign(source["id"], ["%leader"], mode="set")
        self.a.review(source["id"], "b"*40, "second", "code", [], "%leader")
        self.assertEqual([], self.a.allocate({"%coauthor": {}}))
        self.assertEqual("%r", self.a.allocate({"%r": {}})[0]["pane_id"])
        with self.assertRaises(TaskError): self.offer(review)

    def test_canonical_review_and_author_exclusion(self):
        source = self.task()
        self.worker("%leader", "reviewer"); self.worker("%reviewer", "reviewer")
        review = self.a.review(source["id"], "a"*40, "artifact and acceptance", "code", [], "%leader")
        again = self.a.review(source["id"], "a"*40, "artifact and acceptance", "code", [], "%leader")
        self.assertEqual(review["id"], again["id"])
        handoff = self.a.allocate({"%leader": {}, "%reviewer": {}})[0]
        self.assertEqual("%reviewer", handoff["pane_id"])
        with self.assertRaises(TaskError):
            self.a.review(source["id"], "b"*40, "new", "code", [], "%leader")

    def test_pending_review_does_not_block_author_next_job(self):
        source = self.task()
        review = self.a.review(source["id"], "a"*40, "review", "code", [], "%leader")
        successor = self.task("next")
        # Review is older, same priority, but must never be claimed by its author.
        result = self.tasks.advance_next_job("%leader", trigger="completion", from_task=source["id"])
        self.assertEqual(successor["id"], result["task_id"])
        self.assertEqual("open", self.tasks.get(review["id"])["status"])

    def test_legacy_claim_does_not_bypass_review_routing(self):
        source = self.task()
        self.tasks.complete(source["id"], "%leader")
        self.a.review(source["id"], "a"*40, "review", "code", [], "%leader")
        self.assertIsNone(self.tasks.claim_next(scope="mine", pane_id="%leader").get("task"))

    def test_atomic_rollback_retains_owner_when_intent_write_fails(self):
        t = self.task(); self.offer(t); self.worker("%1")
        with patch.object(self.tasks, "record_next_job_handoff", side_effect=RuntimeError("disk error")):
            with self.assertRaises(RuntimeError): self.a.allocate({"%1": {}})
        self.assertEqual(["%leader"], self.tasks.get(t["id"])["assignees"])
        self.assertEqual([], self.tasks.leases_for(t["id"]))
        self.assertEqual("ready", self.a.status()["offers"][0]["state"])

    def runtime(self):
        pane = {"pane_id": "%1", "pane_pid": 123}
        self.worker("%1")
        flock = SimpleNamespace(tasks=self.tasks, _lock=threading.RLock(),
            resolve_lanes=Mock(return_value=[pane]),
            turns={"%1": {"turn_id": "native", "ended_at": 1, "caller": {"pid": 456, "start": "7"}}},
            outbox={}, next_jobs=SimpleNamespace(gate=Mock(return_value=None), materialise=Mock()),
            inbox_count=Mock(return_value={"unread": 0}), delivery_state=Mock(return_value={"state": "ready"}))
        return flock, Allocator(flock)

    def test_runtime_durable_intent_survives_inbox_failure(self):
        t = self.task(); self.offer(t)
        flock, runtime = self.runtime()
        flock.next_jobs.materialise.side_effect = OSError("mail unavailable")
        with patch("dunk_callers.caller_pane_auth_failure", return_value=None), self.assertLogs("dunkingsheep.allocator", level="ERROR"):
            result = runtime.tick()
        self.assertEqual(1, len(result["allocated"]))
        self.assertEqual("pending", self.tasks.next_job_handoffs()[0]["state"])
        self.assertEqual(1, flock.resolve_lanes.call_count)

    def test_runtime_refuses_missing_native_idle_foreign_process_and_draft(self):
        t = self.task(); self.offer(t)
        for mode in ("busy", "foreign", "draft", "unread", "new_turn", "disabled"):
            with self.subTest(mode=mode):
                flock, runtime = self.runtime()
                if mode == "busy": flock.turns["%1"].pop("ended_at")
                if mode == "draft": flock.delivery_state.return_value = {"state": "draft"}
                if mode == "unread": flock.inbox_count.return_value = {"unread": 1}
                if mode == "disabled": flock.next_jobs.gate.return_value = "disabled"
                if mode == "new_turn":
                    def race(*a, **kw):
                        flock.turns["%1"] = {"turn_id": "new", "caller": {"pid": 456}}
                        return {"state": "ready"}
                    flock.delivery_state.side_effect = race
                with patch("dunk_callers.caller_pane_auth_failure", return_value="foreign" if mode == "foreign" else None):
                    self.assertEqual([], runtime.tick()["allocated"])
                flock.next_jobs.materialise.assert_not_called()

    def test_registration_is_dispatch_opt_in_but_not_kill_switch_override(self):
        import dunk_core
        import dunk_api
        from dunk_next_job import NextJobs
        flock, runtime = self.runtime()
        flock.allocator = runtime
        jobs = NextJobs(flock)
        with patch.object(dunk_api, "_dispatch_allowlist", return_value=set()), patch("dunk_panes.is_retired", return_value=False):
            with patch.object(dunk_core, "TURN_END_DISPATCH", True):
                self.assertIsNone(jobs.gate("%1"))
                self.a.register("%1", ["code"], ["worker"], [], "%leader", enabled=False)
                self.assertEqual("not_allowed", jobs.gate("%1"))
            with patch.object(dunk_core, "TURN_END_DISPATCH", False):
                self.assertEqual("disabled", jobs.gate("%1"))

    def test_api_requires_operator_for_registry_and_offer(self):
        import dunk_api
        from dunk_core import DunkError
        flock, runtime = self.runtime()
        flock.allocator = runtime
        flock.require_turn_caller = Mock()
        flock.require_superuser = Mock(side_effect=DunkError("not operator"))
        for action in ("register", "offer"):
            with self.assertRaises(DunkError):
                dunk_api.h_allocation(flock, {"action": action}, "%foreign")
        self.assertEqual([], self.a.status()["offers"])

    def test_foreign_operator_peer_cannot_mutate_registry_or_offers(self):
        import dunk_api
        from dunk_core import DunkError
        flock, runtime = self.runtime()
        flock.allocator = runtime
        flock.require_turn_caller = Mock(side_effect=DunkError("pane_not_in_ancestry"))
        flock.require_superuser = Mock()
        for action in ("register", "offer"):
            with self.assertRaises(DunkError):
                dunk_api.h_allocation(flock, {"action": action}, "%leader")
        flock.require_superuser.assert_not_called()
        self.assertEqual([], self.a.status()["offers"])

    def test_real_server_binds_operator_peer_before_writes(self):
        # Reuses reviewer30 i33922's independent synthetic kernel topology,
        # exercising the real socket handler and policy (no permission mocks).
        import test_dunk_complete_ownership as ownership
        import dunk_callers
        from dunk_server import FlockServer
        table = {
            1001: dict(pid=1001, comm="codex", state="S", ppid=1000, session=1000, start="7"),
            1000: dict(pid=1000, comm="bash", state="S", ppid=1, session=1000, start="5"),
            1: dict(pid=1, comm="init", state="S", ppid=0, session=1, start="1"),
        }
        for pane_pid, peer_pid, accepted in ((2000, 1001, False), (1000, None, False), (1000, 1001, True)):
            fixture = ownership.CompleteOwnershipTests("test_owner_completion_passes")
            fixture.setUp()
            try:
                flock = fixture.flock
                fixture.superusers.add(ownership.OPERATOR)
                source = flock.tasks.add("global", "operator source", body="concrete", assignees=[ownership.OPERATOR])
                server = FlockServer(flock, socket_path=fixture.tmp.name+"/unused.sock")
                request = {"id": "binding", "cmd": "allocation", "self_pane_id": ownership.OPERATOR}
                with patch.object(dunk_callers, "_stat", side_effect=lambda p: table.get(int(p))), \
                     patch.object(dunk_callers, "_cmdline", return_value=["codex"]), \
                     patch.object(flock.herdr, "get_pane", return_value={"pane_id": ownership.OPERATOR, "pane_pid": pane_pid}), \
                     patch.object(flock, "resolve_lanes", return_value=[{"pane_id": "w1:p1", "pane_pid": 3000}]):
                    for args in (
                        {"action": "register", "pane_id": "w1:p1", "pools": ["code"], "roles": ["worker"]},
                        {"action": "offer", "task_id": source["id"], "pool": "code"},
                    ):
                        request["args"] = args
                        result, _ = server.handle_line(json.dumps(request), peer_pid=peer_pid)
                        self.assertEqual(accepted, result["ok"], result)
                status = flock.allocator.store.status()
                self.assertEqual(int(accepted), len(status["workers"]))
                self.assertEqual(int(accepted), len(status["offers"]))
            finally:
                fixture.tearDown()
                fixture.doCleanups()

    def test_api_review_requires_native_ownership(self):
        import dunk_api
        from dunk_core import DunkError
        source = self.task()
        flock, runtime = self.runtime()
        flock.allocator = runtime
        flock._authorize_task_complete = Mock(side_effect=DunkError("foreign caller"))
        with self.assertRaises(DunkError):
            dunk_api.h_allocation(flock, {"action": "review", "task_id": source["id"]}, "%leader")
        self.assertEqual([], self.a.status()["offers"])

    def test_review_new_revision_reuses_task_after_release(self):
        source = self.task(); self.worker("%r", "reviewer")
        review = self.a.review(source["id"], "a"*40, "first", "code", [], "%leader")
        self.a.allocate({"%r": {}})
        self.tasks.complete(review["id"], "%r")
        self.tasks.release(review["id"], "%r")
        new = self.a.review(source["id"], "b"*40, "second", "code", [], "%leader")
        self.assertEqual(review["id"], new["id"])
        self.assertEqual(1, len(self.a.allocate({"%r": {}})))

    def test_restart_and_duplicate_ticks_keep_one_claim(self):
        t = self.task(); self.offer(t); self.worker("%1")
        first = self.a.allocate({"%1": {}})[0]
        self.tasks.close(); self.tasks = Tasks(self.path); self.a = AllocationStore(self.tasks)
        self.assertEqual([], self.a.allocate({"%1": {}}))
        self.assertEqual(first["message_id"], self.tasks.next_job_handoffs()[0]["message_id"])
        self.assertEqual(1, self.tasks.db.execute("SELECT count(*) FROM task_events WHERE event='claimed'").fetchone()[0])

    def test_two_connections_cannot_double_allocate(self):
        t = self.task(); self.offer(t); self.worker("%1"); self.worker("%2")
        barrier, outputs, errors = threading.Barrier(2), [], []
        def race(pane):
            store = Tasks(self.path)
            try:
                allocator = AllocationStore(store)
                barrier.wait(timeout=5)
                outputs.extend(allocator.allocate({pane: {}}))
            except Exception as exc:
                errors.append(exc)
            finally:
                store.close()
        threads = [threading.Thread(target=race, args=(p,)) for p in ("%1", "%2")]
        for thread in threads: thread.start()
        for thread in threads: thread.join(timeout=10)
        self.assertFalse(errors)
        self.assertEqual(1, len(outputs))
        self.assertEqual(1, len(self.tasks.leases_for(t["id"])))

    def test_disabled_registration_and_private_tasks_refused(self):
        self.a.register("%1", ["code"], ["worker"], [], "%leader", enabled=False)
        t = self.task(); self.offer(t)
        self.assertEqual([], self.a.allocate({"%1": {}}))
        private = self.tasks.add("%1", "private", body="body", assignees=["%1"])
        with self.assertRaises(TaskError): self.offer(private)


if __name__ == "__main__":
    unittest.main()
