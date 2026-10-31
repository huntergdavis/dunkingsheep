"""t1147: the Front Desk lane must be messageable even though its `agent` field is empty."""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from web.roster import build_roster, is_lane  # noqa: E402

# The live %133 record (2026-10-30): a working lane with agent == "".
DESK = {"pane_id": "%133", "workspace_label": "bootstrap", "tab_label": "sn-desk",
        "agent": "", "agent_status": "working", "cwd": "/home/hunter/workspace/desk"}
SHELL = {"pane_id": "%3", "workspace_label": "par-a", "tab_label": "bash",
         "agent": "", "agent_status": None, "cwd": "/home/hunter/workspace"}
CONFIG = [{"id": "cos", "name": "Front Desk", "match": "desk", "kind": "active"}]


class DeskLane(unittest.TestCase):
    def row(self, panes):
        return {r["id"]: r for r in build_roster(CONFIG, panes)}["cos"]

    def test_a_working_lane_with_no_agent_name_can_be_messaged(self):
        row = self.row([DESK])
        self.assertEqual("%133", row["pane_id"])
        self.assertTrue(row["online"])
        self.assertTrue(row["can_message"], "the composer must not read 'Agent unavailable'")
        self.assertEqual("working", row["status"])

    def test_the_state_follows_the_daemons_status_not_a_constant(self):
        for raw, shown in (("idle", "idle"), ("blocked", "waiting"), ("working", "working")):
            row = self.row([dict(DESK, agent_status=raw)])
            self.assertEqual((True, shown), (row["can_message"], row["status"]))

    def test_no_pane_is_offline_and_cannot_be_messaged(self):
        row = self.row([])
        self.assertEqual((False, False, "offline"),
                         (row["online"], row["can_message"], row["status"]))

    def test_a_bare_shell_is_not_a_lane(self):
        self.assertFalse(is_lane(SHELL))
        self.assertFalse(is_lane(None))
        self.assertTrue(is_lane(dict(SHELL, agent="claude")))
        names = [r["id"] for r in build_roster([], [SHELL, DESK])]
        self.assertEqual(1, len(names), "only the lane is discovered, not the shell")

    def test_a_pinned_dead_desk_stays_offline(self):
        row = self.row([dict(DESK, pane_id="%999")])
        self.assertTrue(row["online"], "cwd match still finds the live desk")
        pinned = build_roster([dict(CONFIG[0], pane="%129")], [DESK])[0]
        self.assertEqual((False, False), (pinned["online"], pinned["can_message"]))


if __name__ == "__main__":
    unittest.main()


import importlib.util  # noqa: E402
import tempfile  # noqa: E402


@unittest.skipUnless(importlib.util.find_spec("fastapi"), "web.app needs fastapi")
class SendThroughTheApi(unittest.TestCase):
    """POST the real route with a fake daemon: the desk lane is accepted and typed to."""

    def test_a_message_to_the_front_desk_reaches_the_daemon_with_its_own_outcome(self):
        with tempfile.TemporaryDirectory() as tmp:
            os.environ["VP_DATA_DIR"] = tmp
            from fastapi.testclient import TestClient
            from web import app as desk

            calls = []

            class Fake:
                def call(self, verb, **args):
                    calls.append((verb, args))
                    if verb == "list_panes":
                        return {"panes": [DESK]}
                    if verb == "delivery_capability":
                        return {"pane_id": "%133", "alive": True, "send": "guarded"}
                    if verb == "send_text":
                        return {"outcome": "queued", "message_id": "m1"}
                    return {}

            saved = desk._flock
            desk._flock = lambda: Fake()
            desk.app.dependency_overrides[desk.require_auth] = lambda: "owner"
            try:
                client = TestClient(desk.app)
                roster = client.get("/api/channels").json()["channels"]
                cos = next(c for c in roster if c["pane_id"] == "%133")
                self.assertTrue(cos["can_message"])
                reply = client.post("/api/pane-send", json={"pane": "%133", "text": "hello desk"})
                # what the composer actually posts: a channel id plus the pane it resolved
                threaded = client.post("/api/pane-send", json={
                    "pane": "%133", "channel": cos["id"], "text": "via the thread"})
            finally:
                desk._flock = saved
                desk.app.dependency_overrides.clear()
            self.assertEqual(200, reply.status_code, reply.text)
            self.assertEqual("queued", reply.json()["outcome"], "the daemon's word, not 'delivered'")
            self.assertEqual(200, threaded.status_code, threaded.text)
            sent = [args for verb, args in calls if verb == "send_text"]
            self.assertEqual(2, len(sent))
            self.assertIn("via the thread", sent[1]["text"])
            self.assertEqual("%133", sent[0]["target"])
            self.assertIn("hello desk", sent[0]["text"])
