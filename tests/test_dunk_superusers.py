"""Superuser allowlist: store, destructive gating, self-tab force, sandbox.

The 2026-10-18 incident: free verification agents were spawned into the VP's
tab, then one ran `herdr tab close <tab>` and took the VP plus eight agents
with it. These tests pin the result: a non-superuser cannot close anything,
and even a superuser closing the tab it is running in must pass force.
"""

import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401  (isolate live state)
import dunk_api  # noqa: E402
from dunk_core import DunkError, Flock  # noqa: E402
from dunk_inbox import Inbox  # noqa: E402
from dunk_superusers import Superusers  # noqa: E402
from dunk_tasks import Tasks  # noqa: E402
from fakes import FakeHerdr  # noqa: E402

VP_PANE = "w9:p8"
VP_TAB = "w9:t8"
SUPERVISOR = "w1:p2"
OUTSIDER = "w2:p1"

# The VP plus eight free-model agents in one tab, exactly the shape that was
# lost, plus a separate supervisor pane and an outsider pane.
INCIDENT_PANES = [
    {"pane_id": "w1:p1", "tab_id": "w1:t1", "workspace_id": "w1", "cwd": "/tmp/a",
     "agent_status": "unknown", "terminal_id": "t1"},
    {"pane_id": SUPERVISOR, "tab_id": "w1:t2", "workspace_id": "w1", "cwd": "/tmp/b",
     "agent": "claude", "agent_status": "idle", "terminal_id": "t2"},
    {"pane_id": OUTSIDER, "tab_id": "w2:t1", "workspace_id": "w2", "cwd": "/tmp/c",
     "agent": "codex", "agent_status": "idle", "terminal_id": "t3"},
    {"pane_id": VP_PANE, "tab_id": VP_TAB, "workspace_id": "w9", "cwd": "/tmp/vp",
     "agent": "opencode", "agent_status": "idle", "terminal_id": "t8",
     "tab_label": "VP of agentry"},
]
for index in range(1, 9):
    INCIDENT_PANES.append({
        "pane_id": f"w9:p{index}", "tab_id": VP_TAB, "workspace_id": "w9",
        "cwd": "/tmp/vp", "agent": "codex", "agent_status": "idle",
        "terminal_id": f"t9{index}",
    })
INCIDENT_TABS = [
    {"tab_id": "w1:t1", "workspace_id": "w1", "label": "shell", "number": 1},
    {"tab_id": "w1:t2", "workspace_id": "w1", "label": "Supervisor", "number": 2},
    {"tab_id": "w2:t1", "workspace_id": "w2", "label": "Outsider", "number": 1},
    {"tab_id": VP_TAB, "workspace_id": "w9", "label": "VP of agentry", "number": 1},
]
INCIDENT_WORKSPACES = [
    {"workspace_id": "w1", "label": "alpha", "number": 1},
    {"workspace_id": "w2", "label": "beta", "number": 2},
    {"workspace_id": "w9", "label": "admin", "number": 3},
]


def incident_herdr():
    herdr = FakeHerdr(panes=INCIDENT_PANES)
    herdr.tabs = [dict(tab) for tab in INCIDENT_TABS]
    herdr.workspaces = [dict(ws) for ws in INCIDENT_WORKSPACES]
    return herdr


class SuperuserStoreTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = os.path.join(self.tmp.name, "superusers.json")

    def tearDown(self):
        self.tmp.cleanup()

    def test_defaults_are_the_vp_and_owner_and_file_is_private(self):
        store = Superusers(path=self.path)
        self.assertIn("VP of agentry", store.names)
        self.assertIn("owner", store.names)
        self.assertFalse(store.empty())
        self.assertEqual(0o600, os.stat(self.path).st_mode & 0o777)

    def test_add_remove_and_name_matching(self):
        store = Superusers(path=self.path)
        store.add(VP_PANE)
        self.assertTrue(store.is_superuser(VP_PANE))
        self.assertTrue(store.is_superuser("w1:p9", {"tab_label": "VP of agentry"}))
        self.assertFalse(store.is_superuser("w1:p9", {"tab_label": "writer"}))
        self.assertTrue(store.remove(VP_PANE))
        self.assertFalse(store.is_superuser(VP_PANE))

    def test_flat_list_and_explicit_empty(self):
        with open(self.path, "w", encoding="utf-8") as handle:
            json.dump({"superusers": ["w1:p1", "VP of agentry"]}, handle)
        store = Superusers(path=self.path)
        self.assertTrue(store.is_superuser("w1:p1"))
        self.assertTrue(store.is_superuser("w1:p2", {"label": "VP of agentry"}))
        empty = Superusers(path=os.path.join(self.tmp.name, "empty.json"), create=False)
        empty.save()
        self.assertTrue(empty.empty())


class SuperuserGatingTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.herdr = incident_herdr()
        self.superusers = Superusers(
            path=os.path.join(self.tmp.name, "superusers.json"))
        self.superusers.add(SUPERVISOR)
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

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def call(self, name, args=None, pane=None):
        return dunk_api.dispatch(self.flock, name, args or {}, self_pane_id=pane)

    # -- the incident regression -------------------------------------------

    def test_non_superuser_cannot_close_the_vp_tab(self):
        with self.assertRaisesRegex(DunkError, "requires a superuser"):
            self.call("close", {"kind": "tab", "target": VP_TAB}, OUTSIDER)
        self.assertIsNotNone(self.herdr.get_pane(VP_PANE))

    def test_superuser_closing_its_own_tab_requires_force(self):
        with self.assertRaisesRegex(DunkError, "pass --force"):
            self.call("close", {"kind": "tab", "target": VP_TAB}, VP_PANE)
        # Nothing was touched.
        self.assertIsNotNone(self.herdr.get_pane(VP_PANE))
        closed = self.call("close",
                           {"kind": "tab", "target": VP_TAB, "force": True},
                           VP_PANE)
        self.assertEqual(VP_TAB, closed["id"])
        self.assertEqual(9, len(closed["closed_panes"]))
        self.assertIsNone(self.herdr.get_pane(VP_PANE))

    def test_another_superuser_may_close_the_vp_tab(self):
        closed = self.call("close", {"kind": "tab", "target": VP_TAB}, SUPERVISOR)
        self.assertEqual(9, len(closed["closed_panes"]))
        self.assertIsNone(self.herdr.get_pane(VP_PANE))

    def test_superuser_by_name_survives_a_pane_id_change(self):
        # The VP pane is on the allowlist by its tab label, not its id.
        self.assertTrue(self.flock.is_superuser(VP_PANE))
        self.flock.require_superuser(VP_PANE, "close")  # does not raise
        status = self.call("superuser_status", {}, VP_PANE)
        self.assertTrue(status["superuser"])
        self.assertIn("VP of agentry", status["names"])

    # -- the other destructive surfaces ------------------------------------

    def test_non_superuser_is_refused_everywhere_destructive(self):
        for name, args in (
            ("shutdown", {}),
            ("add_dunk", {"target": "self", "text": "hi", "interval_minutes": 5}),
            ("start_agent", {"name": "helper", "command": "codex"}),
            ("herdr", {"command": "tab list"}),
        ):
            with self.assertRaisesRegex(DunkError, "requires a superuser"):
                self.call(name, args, OUTSIDER)

    def test_non_superuser_may_only_send_to_itself(self):
        with self.assertRaisesRegex(DunkError, "requires a superuser"):
            self.call("send_text", {"target": VP_PANE, "text": "hi"}, OUTSIDER)
        result = self.call("send_text", {"target": "self", "text": "hi"}, OUTSIDER)
        self.assertEqual(OUTSIDER, result["pane_id"])

    def test_superuser_add_and_remove_are_gated(self):
        with self.assertRaisesRegex(DunkError, "requires a superuser"):
            self.call("superuser_add", {"entry": "w3:p3"}, OUTSIDER)
        added = self.call("superuser_add", {"entry": "w3:p3"}, SUPERVISOR)
        self.assertIn("w3:p3", added["panes"])
        removed = self.call("superuser_remove", {"entry": "w3:p3"}, SUPERVISOR)
        self.assertTrue(removed["removed"])

    def test_recover_bootstraps_an_empty_allowlist_then_requires_superuser(self):
        empty = Superusers(path=os.path.join(self.tmp.name, "empty.json"),
                           create=False)
        empty.save()
        flock = Flock(
            herdr=incident_herdr(),
            state_file=os.path.join(self.tmp.name, "empty-dunks.json"),
            persist=False,
            restore=False,
            inbox=Inbox(path=os.path.join(self.tmp.name, "empty-inbox.db"),
                        blob_dir=os.path.join(self.tmp.name, "empty-blobs")),
            tasks=Tasks(path=os.path.join(self.tmp.name, "empty-tasks.db")),
            superusers=empty,
        )
        try:
            with self.assertRaisesRegex(DunkError, "allowlist is empty"):
                dunk_api.dispatch(flock, "close", {"kind": "pane", "target": VP_PANE},
                                  self_pane_id=OUTSIDER)
            result = dunk_api.dispatch(flock, "superuser_recover",
                                       {"name": "VP of agentry"}, self_pane_id=VP_PANE)
            self.assertTrue(result["recovered"])
            self.assertIn(VP_PANE, result["panes"])
            with self.assertRaisesRegex(DunkError, "requires an existing superuser"):
                dunk_api.dispatch(flock, "superuser_recover", {}, self_pane_id=OUTSIDER)
        finally:
            flock.close()
            flock.inbox.close()
            flock.tasks.close()

    def test_start_agent_defaults_to_the_sandbox_workspace(self):
        started = self.call("start_agent", {"name": "helper", "command": "codex"},
                            SUPERVISOR)
        sandbox = [ws for ws in self.herdr.workspaces
                   if ws.get("label") == "sandbox"]
        self.assertEqual(1, len(sandbox), "a dedicated sandbox workspace is required")
        self.assertEqual(sandbox[0]["workspace_id"], started["agent"]["workspace_id"])
        self.assertIn(("create_workspace", "sandbox", None, False),
                      self.herdr.layout_calls)


if __name__ == "__main__":
    unittest.main()
