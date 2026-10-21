"""Post-upgrade recovery (recover/doctor) and the launch-memory file."""

import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401  (isolate live state)
from dunk_core import Dunker, Flock  # noqa: E402
from dunk_inbox import Inbox  # noqa: E402
from dunk_superusers import Superusers  # noqa: E402
from dunk_tasks import Tasks  # noqa: E402
from fakes import FakeHerdr  # noqa: E402


class FakeMemory:
    def __init__(self, rows=None):
        self.rows = rows or {}

    def latest_session(self, harness, cwd=None):
        return self.rows.get((harness, cwd))


def pane(pane_id, cwd, agent=None, session=None, title=None, status="unknown"):
    item = {
        "pane_id": pane_id, "cwd": cwd, "agent_status": status,
        "terminal_title_stripped": title or f"hunter@cell: {cwd}",
        "tab_id": "w1:t1", "workspace_id": "w1",
        "terminal_id": f"term_{pane_id}",
    }
    if agent:
        item["agent"] = agent
    if session:
        item["agent_session"] = {"agent": agent, "kind": "id", "value": session}
    return item


class RecoverTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.panes = [
            pane("w1:p1", "/w/pi", agent="pi", session="live-session",
                 title="\u03c0 - pi", status="working"),
            pane("w1:p2", "/w/codex"),
            pane("w1:p3", "/w/x"),
            pane("w1:p4", "/w/pi2"),
        ]
        self.herdr = FakeHerdr(panes=self.panes)
        self.flock = Flock(
            herdr=self.herdr, persist=False, restore=False,
            state_file=os.path.join(self.tmp.name, "dunks.json"),
            launches_file=os.path.join(self.tmp.name, "launches.json"),
            inbox=Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                        blob_dir=os.path.join(self.tmp.name, "blobs")),
            tasks=Tasks(path=os.path.join(self.tmp.name, "tasks.db")),
            superusers=Superusers(path=os.path.join(self.tmp.name, "superusers.json")),
            memory=FakeMemory({("pi", "/w/pi2"): {"id": "p2"}}))
        self.flock.dunkers = [
            Dunker("d1", target_pane_id="w1:p1", target_agent="pi",
                   target_cwd="/w/pi", running=True),
            Dunker("d2", target_pane_id="w1:p2", target_agent="codex",
                   target_cwd="/w/codex", running=True),
            Dunker("d3", target_pane_id="w1:p3", target_agent="opencode",
                   target_cwd="/w/x", running=True),
            Dunker("d4", target_pane_id="w1:p4", target_agent="pi",
                   target_cwd="/w/pi2", running=True),
        ]
        self.flock.launches = {
            "w1:p2": {"agent": "codex", "session": "abc", "cwd": "/w/codex"},
        }
        self.addCleanup(self.flock.close)

    def test_plan_distinguishes_live_bare_and_unknown(self):
        result = self.flock.recover()
        plan = {item["pane_id"]: item for item in result["plan"]}
        self.assertEqual("already-running", plan["w1:p1"]["action"])
        self.assertEqual("launch", plan["w1:p2"]["action"])
        self.assertEqual("cd /w/codex && codex resume abc", plan["w1:p2"]["command"])
        self.assertEqual("recorded-session", plan["w1:p2"]["source"])
        self.assertEqual("no-session", plan["w1:p3"]["action"])
        self.assertEqual("launch", plan["w1:p4"]["action"])
        self.assertEqual("cd /w/pi2 && pi --session p2", plan["w1:p4"]["command"])
        self.assertEqual("memory-latest", plan["w1:p4"]["source"])
        self.assertEqual("herdr 0.9.5", result["herdr"])
        self.assertFalse(result["ok"])

    def test_apply_types_only_into_empty_shells(self):
        result = self.flock.recover(apply=True)
        typed = [text for pane_id, text in self.herdr.text_only]
        self.assertIn("cd /w/codex && codex resume abc", typed)
        self.assertIn("cd /w/pi2 && pi --session p2", typed)
        touched = {pane_id for pane_id, _text in self.herdr.text_only}
        self.assertNotIn("w1:p1", touched)
        self.assertNotIn("w1:p3", touched)
        self.assertEqual(["w1:p2", "w1:p4"], sorted(result["applied"]))
        self.assertEqual("recover", self.flock.launches["w1:p2"]["source"])

    def test_doctor_flags_stale_target_metadata(self):
        self.panes[0]["cwd"] = "/w/real"
        self.flock.dunkers.append(
            Dunker("d52", target_pane_id="w1:p1", target_agent="opencode",
                   target_cwd="/w/old", running=True))
        self.flock._guard_status = lambda: {"installed": True,
                                            "shadowing_herdr": True}
        result = self.flock.doctor()
        kinds = {warning["kind"] for warning in result["warnings"]}
        self.assertIn("agent-mismatch", kinds)
        self.assertIn("cwd-mismatch", kinds)
        self.assertFalse(result["ok"])
        self.assertTrue(result["surfaces"]["read_visible"])
        self.assertEqual("herdr 0.9.5", result["herdr_version"])

    def test_doctor_ok_when_clean(self):
        self.flock.dunkers = [
            Dunker("d1", target_pane_id="w1:p1", target_agent="pi",
                   target_cwd="/w/pi", running=True)]
        self.flock._guard_status = lambda: {"installed": True,
                                            "shadowing_herdr": True}
        result = self.flock.doctor()
        self.assertTrue(result["ok"], result["warnings"])

    def test_start_agent_records_the_exact_argv(self):
        self.flock.dunkers = []
        result = self.flock.start_agent("builder", "codex --model gpt-5",
                                        workspace="w1")
        entry = self.flock.launches[result["pane_id"]]
        self.assertEqual(["codex", "--model", "gpt-5"], entry["argv"])
        self.assertEqual("start_agent", entry["source"])


if __name__ == "__main__":
    unittest.main()
