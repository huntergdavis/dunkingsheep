"""Roles: registry bound to a pane process, service tokens, the per-role
allowlist in dispatch, and the server's identity binding (FLAT_FLEET.md 4).

What these pin: a label grants nothing; a reused pane id grants nothing; a
no-pane request without a token is anonymous; the desk cannot type or mail;
workers cannot assign or flip status; a mismatched claim runs anonymous.
"""

import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401
import dunk_api  # noqa: E402
import dunk_roles  # noqa: E402
from dunk_callers import caller_from_peer  # noqa: E402
from dunk_core import DunkError, Flock  # noqa: E402
from dunk_inbox import Inbox  # noqa: E402
from dunk_roles import Roles  # noqa: E402
from dunk_server import FlockServer  # noqa: E402
from dunk_tasks import Tasks  # noqa: E402
from fakes import FakeHerdr  # noqa: E402

OWNER = "w1:p2"
DESK = "w2:p1"
WORKER = "w2:p3"
OTHER = "w1:p1"


def make_flock(tmp, herdr=None):
    roles = Roles(path=os.path.join(tmp, "roles.json"),
                  tokens_path=os.path.join(tmp, "service-tokens.json"))
    flock = Flock(
        herdr=herdr or FakeHerdr(),
        state_file=os.path.join(tmp, "dunks.json"), persist=False, restore=False,
        inbox=Inbox(path=os.path.join(tmp, "inbox.db"), blob_dir=os.path.join(tmp, "blobs")),
        tasks=Tasks(path=os.path.join(tmp, "tasks.db")),
        roles=roles,
    )
    return flock, roles


class RegistryTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.roles = Roles(path=os.path.join(self.tmp.name, "roles.json"),
                           tokens_path=os.path.join(self.tmp.name, "tokens.json"))

    def tearDown(self):
        self.tmp.cleanup()

    def test_unregistered_pane_is_a_worker_until_the_first_registration(self):
        self.assertEqual("worker", self.roles.role_of("w9:p9"))
        self.assertEqual("anonymous", self.roles.role_of(None))
        self.roles.register(OWNER, "owner")
        self.assertEqual("anonymous", self.roles.role_of("w9:p9"))
        self.assertEqual("anonymous", Roles(path=self.roles.path,
                                            tokens_path=self.roles.tokens_path).default)

    def test_a_label_grants_nothing(self):
        self.roles.register(OWNER, "owner")
        self.assertEqual("anonymous",
                         self.roles.role_of(OTHER, {"label": "owner", "tab_label": "owner"}))

    def test_a_reused_pane_id_with_a_new_root_pid_loses_the_role(self):
        self.roles.register(WORKER, "worker", pane_pid=4100, start="100")
        self.assertEqual("worker", self.roles.role_of(WORKER, {"pane_pid": 4100}))
        self.assertEqual("anonymous", self.roles.role_of(WORKER, {"pane_pid": 9999}))
        # A backend that reports no pid cannot contradict the registration.
        self.assertEqual("worker", self.roles.role_of(WORKER, {}))

    def test_file_is_private_and_reread_when_changed(self):
        self.roles.register(OWNER, "owner")
        self.assertEqual(0o600, os.stat(self.roles.path).st_mode & 0o777)
        other = Roles(path=self.roles.path, tokens_path=self.roles.tokens_path)
        other.register(DESK, "desk", provider="pi")
        self.assertEqual("desk", self.roles.role_of(DESK))

    def test_tokens_hash_at_rest_and_resolve_to_a_role(self):
        token = self.roles.issue_token("web", label="desk-web")
        with open(self.roles.tokens_path, encoding="utf-8") as handle:
            self.assertNotIn(token, handle.read())
        self.assertEqual("web", self.roles.token_role(token))
        self.assertIsNone(self.roles.token_role("nope"))
        self.assertEqual(1, self.roles.revoke_token("desk-web"))
        self.assertIsNone(self.roles.token_role(token))

    def test_reviewers_must_be_on_the_provider_list(self):
        self.roles.register("w3:p1", "reviewer", provider="pi")
        self.roles.register("w3:p2", "reviewer", provider="claude")
        self.assertFalse(self.roles.may_review("w3:p1"))
        self.assertTrue(self.roles.may_review("w3:p2"))

    def test_allowlist_shape(self):
        self.assertTrue(dunk_roles.allowed("owner", "shutdown"))
        self.assertFalse(dunk_roles.allowed("web", "shutdown"))
        self.assertTrue(dunk_roles.allowed("web", "send_text"))
        self.assertFalse(dunk_roles.allowed("desk", "send_text"))
        self.assertFalse(dunk_roles.allowed("desk", "inbox_send"))
        self.assertFalse(dunk_roles.allowed("desk", "task_complete"))
        self.assertTrue(dunk_roles.allowed("desk", "task_add"))
        self.assertFalse(dunk_roles.allowed("planner", "inbox_send"))
        self.assertFalse(dunk_roles.allowed("anonymous", "task_next"))
        self.assertTrue(dunk_roles.allowed("anonymous", "task_list"))
        self.assertFalse(dunk_roles.allowed("worker", "task_assign"))
        self.assertFalse(dunk_roles.allowed("system", "send_text"))


class DispatchGateTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.flock, self.roles = make_flock(self.tmp.name)
        self.roles.register(OWNER, "owner")
        self.roles.register(DESK, "desk", provider="pi")
        self.roles.register(WORKER, "worker", provider="pi")

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def call(self, name, args=None, pane=None, token=None):
        self.flock.clear_caller()
        self.flock.set_service_token(token)
        try:
            return dunk_api.dispatch(self.flock, name, args or {}, self_pane_id=pane)
        finally:
            self.flock.clear_caller()

    def test_whoami_reports_the_bound_role(self):
        self.assertEqual("owner", self.call("whoami", pane=OWNER)["role"])
        self.assertEqual("desk", self.call("whoami", pane=DESK)["role"])
        self.assertEqual("anonymous", self.call("whoami", pane=OTHER)["role"])
        self.assertEqual("anonymous", self.call("whoami")["role"])

    def test_the_desk_cannot_type_mail_or_complete(self):
        for name, args in (("send_text", {"target": OTHER, "text": "hi"}),
                           ("inbox_send", {"to": OTHER, "body": "hi"}),
                           ("task_next", {}),
                           ("add_dunk", {"target": OTHER, "text": "x"}),
                           ("close", {"kind": "pane", "target": OTHER})):
            with self.assertRaisesRegex(DunkError, "not permitted for role desk|requires a superuser"):
                self.call(name, args, pane=DESK)

    def test_the_desk_may_file_work_and_read_the_board(self):
        made = self.call("task_add", {"title": "owner asked for X"}, pane=DESK)
        self.assertEqual("open", made["status"])
        self.assertTrue(self.call("task_list", pane=DESK)["tasks"])

    def test_desk_reads_are_capped(self):
        for index in range(8):
            self.call("task_add", {"title": f"t{index}", "body": "x" * 2000}, pane=OWNER)
        result = self.call("task_list", pane=DESK)
        self.assertTrue(result.get("truncated") or len(result.get("tasks", [])) <= 5)

    def test_a_worker_cannot_assign_or_flip_status(self):
        made = self.call("task_add", {"title": "work"}, pane=OWNER)
        with self.assertRaisesRegex(DunkError, "not permitted for role worker"):
            self.call("task_assign", {"id": made["id"], "assignees": [WORKER]}, pane=WORKER)
        with self.assertRaisesRegex(DunkError, "cannot change status"):
            self.call("task_update", {"id": made["id"], "status": "done"}, pane=WORKER)
        # Title edits are fine.
        self.call("task_update", {"id": made["id"], "title": "work (renamed)"}, pane=WORKER)

    def test_a_no_pane_request_without_a_token_is_anonymous(self):
        with self.assertRaisesRegex(DunkError, "not permitted for role anonymous"):
            self.call("task_add", {"title": "x"})
        with self.assertRaisesRegex(DunkError, "not permitted for role anonymous"):
            self.call("inbox_send", {"to": OTHER, "body": "x", "system_sender": "watchdog"})
        # Over the socket, a registered worker naming a system sender is refused.
        self.flock._caller_state.socket_peer_ok = True
        try:
            with self.assertRaisesRegex(DunkError, "requires the system token"):
                self.call("inbox_send", {"to": OTHER, "body": "x", "system_sender": "watchdog"},
                          pane=WORKER)
        finally:
            self.flock.clear_socket_peer()

    def test_a_system_token_grants_system_and_a_web_token_grants_the_owner_transport(self):
        system = self.call("token_issue", {"role": "system", "label": "watchdog"},
                           pane=OWNER)["token"]
        web = self.call("token_issue", {"role": "web", "label": "desk"}, pane=OWNER)["token"]
        self.assertEqual("system", self.call("whoami", token=system)["role"])
        made = self.call("task_add", {"title": "x"}, token=system)
        self.assertEqual("open", made["status"])
        self.assertEqual("web", self.call("whoami", token=web)["role"])
        with self.assertRaisesRegex(DunkError, "not permitted for role web"):
            self.call("shutdown", {}, token=web)
        with self.assertRaisesRegex(DunkError, "not permitted for role anonymous"):
            self.call("task_add", {"title": "x"}, token="garbage")

    def test_only_the_owner_issues_tokens_and_registers_lanes(self):
        with self.assertRaisesRegex(DunkError, "requires a superuser"):
            self.call("token_issue", {"role": "web"}, pane=WORKER)
        with self.assertRaisesRegex(DunkError, "requires a superuser"):
            self.call("lane_register", {"pane": OTHER, "role": "owner"}, pane=WORKER)

    def test_superuser_add_refuses_names(self):
        with self.assertRaisesRegex(DunkError, "names are no longer matched"):
            self.call("superuser_add", {"entry": "VP of agentry"}, pane=OWNER)

    def test_another_lanes_mailbox_is_not_readable(self):
        with self.assertRaisesRegex(DunkError, "not yours"):
            self.call("inbox_read", {"mailbox": OTHER}, pane=WORKER)

    def test_a_free_model_reviewer_cannot_claim(self):
        self.roles.register("w2:p2", "reviewer", provider="pi")
        self.call("task_add", {"title": "review me"}, pane=OWNER)
        with self.assertRaisesRegex(DunkError, "may not review"):
            self.call("task_next", {}, pane="w2:p2")


class BootstrapTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.flock, self.roles = make_flock(self.tmp.name)

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def call(self, name, args=None, pane=None):
        self.flock.clear_caller()
        try:
            return dunk_api.dispatch(self.flock, name, args or {}, self_pane_id=pane)
        finally:
            self.flock.clear_caller()

    def test_the_first_owner_self_registers_then_nobody_else_can(self):
        with self.assertRaisesRegex(DunkError, "no owner is registered"):
            self.call("close", {"kind": "pane", "target": OTHER}, pane=OWNER)
        self.call("superuser_recover", {}, pane=OWNER)
        self.assertEqual("owner", self.call("whoami", pane=OWNER)["role"])
        with self.assertRaisesRegex(DunkError, "requires the owner role"):
            self.call("superuser_recover", {}, pane=OTHER)


class ServerBindingTests(unittest.TestCase):
    """The server replaces a claim the kernel contradicts."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        caller = caller_from_peer(os.getpid())
        self.caller_pid = caller["pid"]
        panes = [
            {"pane_id": "w5:p1", "tab_id": "w5:t1", "workspace_id": "w5", "cwd": "/tmp",
             "agent": "claude", "agent_status": "idle", "terminal_id": "t51",
             "pane_pid": self.caller_pid},
            {"pane_id": "w5:p2", "tab_id": "w5:t1", "workspace_id": "w5", "cwd": "/tmp",
             "agent": "claude", "agent_status": "idle", "terminal_id": "t52",
             "pane_pid": 1},
        ]
        self.flock, self.roles = make_flock(self.tmp.name, herdr=FakeHerdr(panes=panes))
        self.roles.register("w5:p1", "owner", pane_pid=self.caller_pid)
        self.roles.register("w5:p2", "owner", pane_pid=1)
        self.server = FlockServer(self.flock, socket_path=os.path.join(self.tmp.name, "s.sock"))

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def request(self, pane):
        line = json.dumps({"id": 1, "cmd": "whoami", "args": {}, "self_pane_id": pane})
        reply, _stop = self.server.handle_line(line, peer_pid=os.getpid())
        return reply

    @unittest.skipUnless(os.path.isdir("/proc"), "needs /proc")
    def test_a_claim_matching_the_process_is_bound_and_keeps_its_role(self):
        reply = self.request("w5:p1")
        self.assertTrue(reply["ok"], reply)
        self.assertEqual("owner", reply["result"]["role"])
        self.assertTrue(reply["result"]["bound"])

    @unittest.skipUnless(os.path.isdir("/proc"), "needs /proc")
    def test_a_claim_the_process_does_not_belong_to_runs_anonymous(self):
        reply = self.request("w5:p2")
        self.assertTrue(reply["ok"], reply)
        self.assertEqual("anonymous", reply["result"]["role"])
        self.assertIsNone(reply["result"]["pane_id"])
        self.assertIn("refused", reply["result"]["binding_note"])


if __name__ == "__main__":
    unittest.main()
