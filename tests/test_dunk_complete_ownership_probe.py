"""t423 REVISE probes derived from independent reviewer t424 probe.py (attributed).

Reviewer source (read-only, not vendored):
  dunkingsheep-review-c-99-1db70dd/resources/review-c-t424/probe.py
  (+socket_probe.py for the AF_UNIX round trip).
Same harness the reviewer used - the real FlockServer.handle_line with a
mocked _stat peer tree, a real AF_UNIX round trip on a temporary socket (no
production socket, no live task), and a wrapper-level interleave through the
public task_complete - with expectations inverted to the repaired behavior:
spoofed labels refused with no effects, bound flows pass, reassignment
interleaving refused. Where the reviewer asserted success on the old head
(the red proof), this file asserts refusal (the green proof).
"""

import json
import os
import sys
import tempfile
import threading
import unittest
from unittest.mock import patch

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
import dunk_callers as callers  # noqa: E402
from dunk_core import DunkError, Flock  # noqa: E402
from dunk_inbox import Inbox  # noqa: E402
from dunk_server import FlockServer  # noqa: E402
from dunk_client import FlockClient  # noqa: E402
from dunk_superusers import Superusers  # noqa: E402
from dunk_tasks import Tasks  # noqa: E402
from fakes import FakeHerdr  # noqa: E402
from test_dunk_complete_ownership import (  # noqa: E402
    CompleteOwnershipTests, OWNER, FOREIGN, OPERATOR)

# Reviewer's mocked peer tree, completed to init so the ancestry walk
# terminates deterministically (their table stopped at pid 1's absence).
PEER_TABLE = {
    1001: {"pid": 1001, "comm": "codex", "state": "S", "ppid": 1000,
           "session": 1000, "start": "7"},
    1000: {"pid": 1000, "comm": "bash", "state": "S", "ppid": 1,
           "session": 1000, "start": "5"},
    1: {"pid": 1, "comm": "init", "state": "S", "ppid": 0,
        "session": 1, "start": "1"},
}
FOREIGN_PEER = 1001
# A pane pid the mocked tree provably excludes (reviewer's binding target).
ABSENT_PANE_PID = 2000


def _snapshot(flock):
    return {"task_dump": "\n".join(flock.tasks.db.iterdump()),
            "handoffs": flock.tasks.next_job_handoffs()}


def _completed_events(flock, task_id):
    return flock.tasks.db.execute(
        "SELECT * FROM task_events WHERE task_id = ? AND event = ?",
        (task_id, "completed")).fetchall()


class ServerProbeTests(unittest.TestCase):
    """Reviewer handle_line harness, green expectations."""

    def setUp(self):
        self.case = CompleteOwnershipTests("test_owner_completion_passes")
        self.case.setUp()
        self.addCleanup(self.case.tearDown)
        self.flock = self.case.flock
        self.stat_patch = patch.object(
            callers, "_stat", side_effect=lambda pid: PEER_TABLE.get(int(pid)))
        self.stat_patch.start()
        self.addCleanup(self.stat_patch.stop)

    def _set_pane_pid(self, pane_id, pid):
        for pane in self.case.herdr.panes:
            if pane["pane_id"] == pane_id:
                pane["pane_pid"] = pid
                return
        raise AssertionError(f"no such fake pane {pane_id}")

    def _handle(self, task_id, requester, peer=FOREIGN_PEER, by=None):
        server = FlockServer(self.flock,
                             socket_path=self.case.tmp.name + "/unused.sock")
        request = {"id": "probe", "cmd": "task_complete",
                   "args": {"id": task_id}, "self_pane_id": requester}
        if by is not None:
            request["args"]["by"] = by
        return server.handle_line(json.dumps(request), peer_pid=peer)

    def test_server_spoofed_owner_label_refused_with_no_effects(self):
        """Reviewer probe 1 through the real server path. The task IS owned by
        the claimed pane, so label policy alone would complete it: only the
        kernel binding refusal stops it."""
        task = self.flock.task_add("bound work", self_pane_id="w1:p1")
        self.flock.task_assign(task["id"], ["w1:p1"], self_pane_id="w1:p1")
        self._set_pane_pid("w1:p1", ABSENT_PANE_PID)
        before = _snapshot(self.flock)
        reply, _ = self._handle(task["id"], "w1:p1")
        self.assertFalse(reply["ok"], reply)
        self.assertIn("not inside pane", reply.get("error") or "")
        current = self.flock.tasks.get(task["id"])
        self.assertNotEqual("done", current["status"])
        self.assertEqual([], _completed_events(self.flock, task["id"]))
        self.assertEqual(before, _snapshot(self.flock))

    def test_server_spoofed_operator_label_refused(self):
        """Reviewer probe 2: the allowlist is not consulted for unbound panes."""
        self.case.superusers.add("w1:p1")
        task = self.flock.task_add("other work", self_pane_id="w2:p1")
        self.flock.task_assign(task["id"], ["w2:p1"], self_pane_id="w2:p1")
        self._set_pane_pid("w1:p1", ABSENT_PANE_PID)
        before = _snapshot(self.flock)
        reply, _ = self._handle(task["id"], "w1:p1")
        self.assertFalse(reply["ok"], reply)
        self.assertIn("not inside pane", reply.get("error") or "")
        self.assertNotEqual("done", self.flock.tasks.get(task["id"])["status"])
        self.assertEqual(before, _snapshot(self.flock))

    def test_server_bound_owner_label_passes(self):
        """Same harness, pane pid inside the mocked ancestry: the full server
        path completes (binding preserves legitimate flows)."""
        task = self.flock.task_add("bound work", self_pane_id="w1:p1")
        self.flock.task_assign(task["id"], ["w1:p1"], self_pane_id="w1:p1")
        self._set_pane_pid("w1:p1", 1000)
        reply, _ = self._handle(task["id"], "w1:p1")
        self.assertTrue(reply["ok"], reply)
        self.assertEqual("done", self.flock.tasks.get(task["id"])["status"])

    def test_wrapper_interleave_reassignment_is_refused(self):
        """Reviewer interleave through the PUBLIC wrapper: the guard passes for
        the owner, the task is reassigned before the store transaction, and the
        transaction must refuse (old head completed as the removed owner)."""
        task = self.flock.task_add("bound work", self_pane_id="w1:p1")
        self.flock.task_assign(task["id"], ["w1:p1"], self_pane_id="w1:p1")
        original = self.flock.tasks.advance_next_job
        observed = {}

        def interleave(*args, **kwargs):
            self.flock.tasks.assign(task["id"], ["w2:p1"], mode="set")
            observed["owners_before_transaction"] = self.flock.tasks.get(
                task["id"])["assignees"]
            return original(*args, **kwargs)

        with patch.object(self.flock.tasks, "advance_next_job",
                          side_effect=interleave):
            with self.assertRaises(DunkError):
                self.flock.task_complete(task["id"], self_pane_id="w1:p1")
        self.assertEqual(["w2:p1"], observed["owners_before_transaction"])
        current = self.flock.tasks.get(task["id"])
        self.assertNotEqual("done", current["status"])
        self.assertEqual([], _completed_events(self.flock, task["id"]))


@unittest.skipUnless(os.path.isdir("/proc"), "needs a process table")
class SocketProbeTests(unittest.TestCase):
    """Reviewer socket_probe.py shape: a real temporary AF_UNIX round trip."""

    def setUp(self):
        self.case = CompleteOwnershipTests("test_owner_completion_passes")
        self.case.setUp()
        self.addCleanup(self.case.tearDown)
        self.flock = self.case.flock
        self.socket_path = os.path.join(self.case.tmp.name, "probe.sock")
        self.server = FlockServer(self.flock, socket_path=self.socket_path)
        self.server.bind()
        self.thread = threading.Thread(target=self.server.serve_forever,
                                       daemon=True)
        self.thread.start()
        self.addCleanup(self.server.shutdown)
        self.addCleanup(lambda: self.thread.join(3))

    def test_socket_unresolvable_pane_claim_is_refused(self):
        """Real peer (this test process) naming a pane no backend can see:
        refused with no effects, over a real socket."""
        task = self.case._owned_task()  # OWNER has no fake pane
        client = FlockClient(socket_path=self.socket_path,
                             self_pane_id=OWNER)
        with self.assertRaises(DunkError):
            client.call("task_complete", id=task["id"])
        self.assertNotEqual("done", self.flock.tasks.get(task["id"])["status"])

    def test_socket_bound_pane_passes(self):
        """Real peer bound to a real pane pid through the real server: the
        legitimate socket flow still completes."""
        caller = callers.caller_from_peer(os.getpid(), None)
        self.assertIsNotNone((caller or {}).get("pid"))
        self.assertIsNotNone((caller or {}).get("start"))
        for pane in self.case.herdr.panes:
            if pane["pane_id"] == "w1:p1":
                pane["pane_pid"] = caller["pid"]
        task = self.flock.task_add("bound work", self_pane_id="w1:p1")
        self.flock.task_assign(task["id"], ["w1:p1"], self_pane_id="w1:p1")
        client = FlockClient(socket_path=self.socket_path,
                             self_pane_id="w1:p1")
        result = client.call("task_complete", id=task["id"])
        self.assertEqual("done", result["status"])


if __name__ == "__main__":
    unittest.main()
