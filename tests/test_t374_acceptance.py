"""t374 acceptance validator: the checks must be able to fail.

These are fixture receipts, not acceptance evidence.  The live run uses
tools/t374_acceptance.py collect, which reads tasks.db / metrics.db / messages.
"""
import importlib.util
import json
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
TOOL = ROOT / "tools" / "t374_acceptance.py"


def load_tool():
    spec = importlib.util.spec_from_file_location("t374_acceptance", TOOL)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def handoff(meta=None, at=3):
    return {"id": 10, "event": "next_job_handoff", "at": at,
            "meta": json.dumps(meta or {"from_task": "tA", "message_id": "m1",
                                        "attempt": 0})}


def lane_fixture():
    return {
        "pane": "%90", "A": "tA", "B": "tB",
        "A_state": {"events": [
            {"event": "claimed", "at": 1}, {"event": "completed", "at": 2}]},
        "B_state": {"events": [
            handoff(), {"event": "claimed", "at": 4},
            {"event": "completed", "at": 6}]},
        "A_completed_at": 2, "B_completed_at": 6,
        "A_artifact": {"exists": True, "prefix_ok": True},
        "B_artifact": {"exists": True, "prefix_ok": True},
        "handoffs": [handoff()],
        "B_claims": [{"event": "claimed", "at": 4}],
        "B_sent": [{"message_id": "m1", "source": "turn_end", "status": "sent"}],
        "B_messages": [{"message_id": "m1", "status": "sent"}],
        "human_sends": [],
        "B_turn_starts_after_claim": [{"kind": "turn.start", "at": 5}],
        "no_work": [{"kind": "turn.end.dispatch", "reason": "no_assigned_work", "at": 7}],
    }


def receipt():
    return {"run_id": "fixture", "lanes": {"pi": lane_fixture(),
                                           "codex": lane_fixture()}}


class ValidatorTests(unittest.TestCase):
    def setUp(self):
        self.tool = load_tool()

    def failures(self, value):
        return self.tool.validate(value)

    def test_good_chain_passes(self):
        self.assertEqual([], self.failures(receipt()))

    def test_missing_handoff_fails(self):
        value = receipt()
        value["lanes"]["pi"]["handoffs"] = []
        value["lanes"]["pi"]["B_state"]["events"] = [
            {"event": "claimed", "at": 4}, {"event": "completed", "at": 6}]
        self.assertTrue(any("no next_job_handoff" in f for f in self.failures(value)))

    def test_wrong_from_task_fails(self):
        value = receipt()
        value["lanes"]["pi"]["handoffs"] = [
            handoff({"from_task": "tOTHER", "message_id": "m1"})]
        value["lanes"]["pi"]["B_state"]["events"][0] = value["lanes"]["pi"]["handoffs"][0]
        self.assertTrue(any("from_task != A" in f for f in self.failures(value)))

    def test_duplicate_b_claim_fails(self):
        value = receipt()
        value["lanes"]["pi"]["B_claims"].append({"event": "claimed", "at": 4.5})
        self.assertTrue(any("exactly 1 B claim" in f for f in self.failures(value)))

    def test_duplicate_b_delivery_fails(self):
        value = receipt()
        value["lanes"]["pi"]["B_sent"].append(
            {"message_id": "m2", "source": "turn_end", "status": "sent"})
        self.assertTrue(any("exactly 1 sent B message" in f for f in self.failures(value)))

    def test_b_claimed_before_a_completed_fails(self):
        value = receipt()
        value["lanes"]["pi"]["B_claims"][0]["at"] = 1.5
        self.assertTrue(any("before A completed" in f for f in self.failures(value)))

    def test_human_send_fails(self):
        value = receipt()
        value["lanes"]["pi"]["human_sends"] = [{"message_id": "m9"}]
        self.assertTrue(any("no human nudge" in f for f in self.failures(value)))

    def test_missing_no_work_fails(self):
        value = receipt()
        value["lanes"]["pi"]["no_work"] = []
        self.assertTrue(any("no_assigned_work" in f for f in self.failures(value)))

    def test_bad_artifact_fails(self):
        value = receipt()
        value["lanes"]["pi"]["B_artifact"]["prefix_ok"] = False
        self.assertTrue(any("B artifact" in f for f in self.failures(value)))

    def test_missing_b_turn_start_fails(self):
        value = receipt()
        value["lanes"]["pi"]["B_turn_starts_after_claim"] = []
        self.assertTrue(any("no B turn start" in f for f in self.failures(value)))


if __name__ == "__main__":
    unittest.main()


class CollectorTests(unittest.TestCase):
    """The collector reads the real SQL shapes, not a hand-written receipt."""

    def setUp(self):
        self.tool = load_tool()
        self.tmp = Path(self._tmp())
        self.addCleanup(self._rm)
        self.run = self.tmp / "run"
        self.tool.plan(self.run, run_id="fixture", wait_seconds=1)
        manifest = json.loads((self.run / "manifest.json").read_text())
        manifest["prepared_at"] = 1
        (self.run / "manifest.json").write_text(json.dumps(manifest))
        self.manifest = manifest
        self.tasks = {"pi": {"pane": "%90", "A": "tA1", "B": "tB1"},
                      "codex": {"pane": "%91", "A": "tA2", "B": "tB2"}}
        (self.run / "tasks.json").write_text(json.dumps(self.tasks))
        self._seed_db()
        self._seed_artifacts()
        self.tool.TASKS_DB = self.tmp / "tasks.db"
        self.tool.METRICS_DB = self.tmp / "metrics.db"
        self.tool._messages = lambda: [
            {"message_id": f"m-{lane}", "pane_id": row["pane"],
             "source": "turn_end", "status": "sent", "created_at": 3.5,
             "text": f"[turn-end {row['B']}] next job"}
            for lane, row in self.tasks.items()]

    def _tmp(self):
        import tempfile
        return tempfile.mkdtemp(prefix="t374-collect-")

    def _rm(self):
        import shutil
        shutil.rmtree(self.tmp, ignore_errors=True)

    def _seed_db(self):
        import sqlite3
        tasks = sqlite3.connect(self.tmp / "tasks.db")
        tasks.executescript("""
        CREATE TABLE tasks (id TEXT PRIMARY KEY, status TEXT, assignees TEXT,
            created_at REAL, updated_at REAL, cancelled_at REAL);
        CREATE TABLE task_events (id INTEGER PRIMARY KEY, task_id TEXT, event TEXT,
            at REAL, actor TEXT, status_from TEXT, status_to TEXT, meta TEXT);
        CREATE TABLE task_leases (task_id TEXT, pane_id TEXT, claimed_at REAL,
            heartbeat_at REAL, expires_at REAL, caller_pid INTEGER, caller_name TEXT);
        """)
        metrics = sqlite3.connect(self.tmp / "metrics.db")
        metrics.executescript("""
        CREATE TABLE events (id INTEGER PRIMARY KEY, at REAL, kind TEXT, pane_id TEXT,
            source TEXT, outcome TEXT, reason TEXT, duration_ms INTEGER);
        """)
        for index, (lane, row) in enumerate(self.tasks.items()):
            base = 10 * (index + 1)
            pane = row["pane"]
            for task_id, name in ((row["A"], "A"), (row["B"], "B")):
                tasks.execute("INSERT INTO tasks VALUES (?,?,?,?,?,NULL)",
                              (task_id, "in_progress", json.dumps([pane]), 1, 1))
            tasks.execute("INSERT INTO task_events (task_id,event,at,actor,meta) "
                          "VALUES (?, 'claimed', ?, ?, NULL)", (row["A"], base, pane))
            tasks.execute("INSERT INTO task_events (task_id,event,at,actor,meta) "
                          "VALUES (?, 'completed', ?, ?, NULL)",
                          (row["A"], base + 1, pane))
            tasks.execute("INSERT INTO task_events (task_id,event,at,actor,meta) "
                          "VALUES (?, 'next_job_handoff', ?, ?, ?)",
                          (row["B"], base + 2, pane,
                           json.dumps({"from_task": row["A"],
                                        "message_id": f"m-{lane}", "attempt": 0})))
            tasks.execute("INSERT INTO task_events (task_id,event,at,actor,meta) "
                          "VALUES (?, 'claimed', ?, ?, NULL)",
                          (row["B"], base + 3, pane))
            tasks.execute("INSERT INTO task_events (task_id,event,at,actor,meta) "
                          "VALUES (?, 'completed', ?, ?, NULL)",
                          (row["B"], base + 5, pane))
            tasks.execute("INSERT INTO task_leases VALUES (?,?,?,?,?,?,?)",
                          (row["B"], pane, base + 3, base + 3, 9999, 1, lane))
            for at, kind, reason in ((base, "turn.start", None),
                                     (base + 1, "turn.end", "stop"),
                                     (base + 2, "turn.end.dispatch", "queued"),
                                     (base + 2.5, "message.sent", None),
                                     (base + 4, "turn.start", None),
                                     (base + 5, "turn.end", "stop"),
                                     (base + 6, "turn.end.dispatch", "no_assigned_work")):
                metrics.execute("INSERT INTO events (at,kind,pane_id,source,outcome,reason) "
                                "VALUES (?,?,?,?,?,?)",
                                (at, kind, pane, "turn_end", "ok", reason))
        tasks.commit()
        metrics.commit()
        tasks.close()
        metrics.close()

    def _seed_artifacts(self):
        for lane, row in self.tasks.items():
            for key in ("A", "B"):
                spec = self.manifest["lanes"][lane]["tasks"][key]
                path = Path(spec["artifact"])
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(spec["expect_prefix"] + " " + lane + "\n")

    def test_collect_then_validate_passes_on_real_sql_shapes(self):
        receipt = self.tool.collect(self.run)
        self.assertEqual([], self.tool.validate(receipt))
