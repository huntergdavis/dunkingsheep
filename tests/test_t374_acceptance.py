"""t374 acceptance validator: the checks must be able to fail.

Fixtures mirror PR72's actual records: one next_job_handoff message id with
several state rows, an inbox `nj-` receipt with notify_at/transport_id/attempt,
a producer-bound turn id, and no_assigned_work on B.  The live run uses
tools/t374_acceptance.py collect.
"""
import importlib.util
import json
import sqlite3
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
TOOL = ROOT / "tools" / "t374_acceptance.py"
MID = "nj-" + "a" * 32


def load_tool():
    spec = importlib.util.spec_from_file_location("t374_acceptance", TOOL)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def lane_fixture(lane="pi", pane="%90", a="tA", b="tB"):
    handoff = {
        "message_id": MID, "from_task": a, "completion_at": 2.0,
        "caller": {"name": lane, "pid": 10, "start": "s"},
        "attempt": 1, "state": "started", "trigger": "completion",
        "attempt_at": 2.079, "transport_id": MID + ":1", "delivered_at": 2.092,
        "turn_id": "turn-1", "turn_started_at": 2.086, "turn_source": lane,
        "turn_caller": {"name": lane, "pid": 10, "start": "s"}, "at": 2.2,
    }
    return {
        "pane": pane, "A": a, "B": b,
        "A_task": {"completed_by": pane}, "B_task": {"completed_by": pane},
        "A_claims": [{"event": "claimed", "at": 1.0, "actor": pane,
                      "meta": json.dumps({"caller_name": lane, "caller_pid": 10})}],
        "B_claims": [{"event": "claimed", "at": 2.0, "actor": pane,
                      "meta": json.dumps({"caller_name": lane, "caller_pid": 10})}],
        "A_completed": {"event": "completed", "at": 2.0, "actor": pane},
        "B_completed": {"event": "completed", "at": 5.0, "actor": pane},
        "handoffs": [{"message_id": MID, "first_at": 2.0, "latest": handoff}],
        "handoff": handoff, "handoff_first_at": 2.0,
        "B_receipt": {
            "id": MID, "pane_id": pane, "sender": "next-job",
            "notify_state": "sent", "notify_at": 2.0926,
            "notify_transport_id": MID + ":1", "notify_attempt": 1,
            "read_at": 3.5, "acked_at": None,
            "body": f"TASK {b} [urgent] test\nhandoff: {MID} from {a}",
        },
        "next_job_decisions": [{"at": 5.0, "reason": "no_assigned_work",
                                "actor": pane}],
        "turn_starts": [{"kind": "turn.start", "at": 2.086, "pane_id": pane}],
        "human_sends": [],
        "A_artifact": {"exists": True, "prefix_ok": True},
        "B_artifact": {"exists": True, "prefix_ok": True},
    }


def receipt():
    return {"run_id": "fixture", "lanes": {"pi": lane_fixture("pi", "%90", "tA1", "tB1"),
                                           "codex": lane_fixture("codex", "%91", "tA2", "tB2")}}


class ValidatorTests(unittest.TestCase):
    def setUp(self):
        self.tool = load_tool()

    def failures(self, value):
        return self.tool.validate(value)

    def assert_fails(self, value, needle):
        failures = self.failures(value)
        self.assertTrue(any(needle in f for f in failures), failures)

    def test_good_chain_passes(self):
        self.assertEqual([], self.failures(receipt()))

    def test_early_causal_turn_before_delivery_but_after_attempt_passes(self):
        value = receipt()
        h = value["lanes"]["pi"]["handoff"]
        self.assertLess(h["attempt_at"], h["turn_started_at"])
        self.assertLess(h["turn_started_at"], h["delivered_at"])
        self.assertLess(h["delivered_at"], value["lanes"]["pi"]["B_receipt"]["notify_at"])
        self.assertEqual([], self.failures(value))

    def test_unrelated_receipt_id_fails(self):
        value = receipt()
        value["lanes"]["pi"]["B_receipt"]["id"] = "nj-" + "b" * 32
        self.assert_fails(value, "receipt id is not the handoff")

    def test_turn_only_after_b_completion_fails(self):
        value = receipt()
        value["lanes"]["pi"]["handoff"]["turn_started_at"] = 1000.0
        self.assert_fails(value, "only turn start is AFTER B completion")

    def test_turn_before_transport_attempt_fails(self):
        value = receipt()
        value["lanes"]["pi"]["handoff"]["turn_started_at"] = 2.0
        self.assert_fails(value, "before the transport attempt")

    def test_missing_attempt_at_fails(self):
        value = receipt()
        del value["lanes"]["pi"]["handoff"]["attempt_at"]
        self.assert_fails(value, "no transport attempt_at")

    def test_early_turn_without_sent_receipt_fails(self):
        value = receipt()
        value["lanes"]["pi"]["B_receipt"]["notify_state"] = "queued"
        self.assert_fails(value, "notify_state")

    def test_foreign_a_claim_actor_fails(self):
        value = receipt()
        value["lanes"]["pi"]["A_claims"][0] = {
            "event": "claimed", "at": 1.0, "actor": "%OTHER",
            "meta": json.dumps({"caller_name": "intruder"})}
        self.assert_fails(value, "A claim caller")

    def test_foreign_b_claim_actor_fails(self):
        value = receipt()
        value["lanes"]["pi"]["B_claims"][0]["meta"] = json.dumps(
            {"caller_name": "intruder"})
        self.assert_fails(value, "B claim caller")

    def test_duplicate_handoff_message_fails(self):
        value = receipt()
        other = dict(value["lanes"]["pi"]["handoff"])
        other["message_id"] = "nj-" + "c" * 32
        value["lanes"]["pi"]["handoffs"].append(
            {"message_id": other["message_id"], "first_at": 2.1, "latest": other})
        self.assert_fails(value, "exactly 1 handoff message id")

    def test_handoff_not_started_fails(self):
        value = receipt()
        value["lanes"]["pi"]["handoff"]["state"] = "sent"
        self.assert_fails(value, "not 'started'")

    def test_unread_receipt_fails(self):
        value = receipt()
        value["lanes"]["pi"]["B_receipt"]["read_at"] = None
        value["lanes"]["pi"]["B_receipt"]["acked_at"] = None
        self.assert_fails(value, "never read/acked")

    def test_unsent_receipt_fails(self):
        value = receipt()
        value["lanes"]["pi"]["B_receipt"]["notify_state"] = "queued"
        self.assert_fails(value, "notify_state")

    def test_transport_mismatch_fails(self):
        value = receipt()
        value["lanes"]["pi"]["B_receipt"]["notify_transport_id"] = MID + ":2"
        self.assert_fails(value, "transport_id")

    def test_missing_no_work_fails(self):
        value = receipt()
        value["lanes"]["pi"]["next_job_decisions"] = []
        self.assert_fails(value, "no_assigned_work")

    def test_b_completed_before_turn_start_fails(self):
        value = receipt()
        value["lanes"]["pi"]["B_completed"]["at"] = 2.0
        self.assert_fails(value, "B completed before its own turn started")

    def test_b_claimed_before_a_completed_fails(self):
        value = receipt()
        value["lanes"]["pi"]["B_claims"][0]["at"] = 1.0
        self.assert_fails(value, "before A completed")

    def test_b_claim_not_in_completion_transaction_fails(self):
        value = receipt()
        value["lanes"]["pi"]["B_claims"][0]["at"] = 10.0
        self.assert_fails(value, "not in A's completion transaction")

    def test_missing_turn_start_metric_fails(self):
        value = receipt()
        value["lanes"]["pi"]["turn_starts"] = []
        self.assert_fails(value, "no matching turn.start")

    def test_human_send_fails(self):
        value = receipt()
        value["lanes"]["pi"]["human_sends"] = [{"message_id": "m9"}]
        self.assert_fails(value, "no human nudge")

    def test_bad_artifact_fails(self):
        value = receipt()
        value["lanes"]["pi"]["B_artifact"]["prefix_ok"] = False
        self.assert_fails(value, "B artifact")

    def test_completion_actor_mismatch_fails(self):
        value = receipt()
        value["lanes"]["pi"]["B_completed"]["actor"] = "%OTHER"
        self.assert_fails(value, "B completion actor")

    def test_unavailable_human_send_evidence_fails(self):
        value = receipt()
        value["lanes"]["pi"]["human_sends"] = None
        self.assert_fails(value, "human-send evidence unavailable")

    def test_duplicate_b_completion_event_fails(self):
        value = receipt()
        value["lanes"]["pi"]["B_completeds"] = [
            value["lanes"]["pi"]["B_completed"],
            dict(value["lanes"]["pi"]["B_completed"])]
        self.assert_fails(value, "exactly 1 B completion event")


class CollectorTests(unittest.TestCase):
    """The collector reads PR72's SQL shapes, not a hand-written receipt."""

    def setUp(self):
        self.tool = load_tool()
        self.tmp = Path(tempfile.mkdtemp(prefix="t374-collect-"))
        self.addCleanup(self._rm)
        self.run = self.tmp / "run"
        self.tool.plan(self.run, run_id="fixture", wait_seconds=1)
        manifest = json.loads((self.run / "manifest.json").read_text())
        manifest["prepared_at"] = 1
        (self.run / "manifest.json").write_text(json.dumps(manifest))
        self.manifest = manifest
        self.tasks = {"pi": {"pane": "%90", "A": "tA1", "B": "tB1"},
                      "codex": {"pane": "%91", "A": "tA2", "B": "tB2"},
                      }
        (self.run / "tasks.json").write_text(json.dumps(self.tasks))
        self._seed()
        self.tool.TASKS_DB = self.tmp / "tasks.db"
        self.tool.METRICS_DB = self.tmp / "metrics.db"
        self.tool.CONFIG_DIR = self.tmp
        self.tool._messages = lambda: []

    def _rm(self):
        import shutil
        shutil.rmtree(self.tmp, ignore_errors=True)

    def _seed(self):
        tasks = sqlite3.connect(self.tmp / "tasks.db")
        tasks.executescript("""
        CREATE TABLE tasks (id TEXT PRIMARY KEY, status TEXT, assignees TEXT,
            created_at REAL, updated_at REAL, cancelled_at REAL, completed_by TEXT);
        CREATE TABLE task_events (id INTEGER PRIMARY KEY, task_id TEXT, event TEXT,
            at REAL, actor TEXT, status_from TEXT, status_to TEXT, meta TEXT);
        CREATE TABLE task_leases (task_id TEXT, pane_id TEXT, claimed_at REAL,
            heartbeat_at REAL, expires_at REAL, caller_pid INTEGER, caller_name TEXT);
        """)
        inbox = sqlite3.connect(self.tmp / "inbox.db")
        inbox.executescript("""
        CREATE TABLE messages (id TEXT PRIMARY KEY, mailbox TEXT, mailbox_label TEXT,
            pane_id TEXT, sender TEXT, subject TEXT, body TEXT, state TEXT,
            created_at REAL, read_at REAL, acked_at REAL, notify_state TEXT,
            notify_reason TEXT, notify_at REAL, notify_transport_id TEXT,
            notify_attempt INTEGER);
        """)
        metrics = sqlite3.connect(self.tmp / "metrics.db")
        metrics.executescript("""
        CREATE TABLE events (id INTEGER PRIMARY KEY, at REAL, kind TEXT, pane_id TEXT,
            source TEXT, outcome TEXT, reason TEXT, duration_ms INTEGER);
        """)
        for index, (lane, row) in enumerate(self.tasks.items()):
            base = 10 * (index + 1)
            pane, a, b = row["pane"], row["A"], row["B"]
            mid = f"nj-{index:032x}"
            for task_id in (a, b):
                tasks.execute("INSERT INTO tasks VALUES (?,?,?,?,?,NULL,?)",
                              (task_id, "done", json.dumps([pane]), 1, 1, pane))
            tasks.execute("INSERT INTO task_events (task_id,event,at,actor,meta) "
                          "VALUES (?, 'claimed', ?, ?, ?)",
                          (a, base, pane,
                           json.dumps({"caller_name": lane, "caller_pid": 1})))
            tasks.execute("INSERT INTO task_events (task_id,event,at,actor,meta) "
                          "VALUES (?, 'completed', ?, ?, NULL)", (a, base + 1, pane))
            tasks.execute("INSERT INTO task_events (task_id,event,at,actor,meta) "
                          "VALUES (?, 'claimed', ?, ?, ?)",
                          (b, base + 1, pane,
                           json.dumps({"caller_name": lane, "caller_pid": 1})))
            base_meta = {"message_id": mid, "from_task": a, "completion_at": base + 1,
                         "caller": {"name": lane, "pid": 1},
                         "attempt": 1, "state": "pending", "trigger": "completion"}
            for at, state, extra in (
                    (base + 1, "pending", {}),
                    (base + 1.10, "sent", {"attempt_at": base + 1.05,
                                            "transport_id": mid + ":1",
                                            "delivered_at": base + 1.10}),
                    (base + 1.20, "started", {"attempt_at": base + 1.05,
                                               "transport_id": mid + ":1",
                                               "delivered_at": base + 1.10,
                                               "turn_id": f"turn-{lane}",
                                               "turn_started_at": base + 1.08,
                                               "turn_source": lane,
                                               "turn_caller": {"name": lane, "pid": 1}})):
                meta = dict(base_meta, state=state, **extra)
                tasks.execute("INSERT INTO task_events (task_id,event,at,actor,meta) "
                              "VALUES (?, 'next_job_handoff', ?, ?, ?)",
                              (b, at, pane, json.dumps(meta)))
            tasks.execute("INSERT INTO task_events (task_id,event,at,actor,meta) "
                          "VALUES (?, 'completed', ?, ?, NULL)", (b, base + 4, pane))
            tasks.execute("INSERT INTO task_events (task_id,event,at,actor,meta) "
                          "VALUES (?, 'next_job_decision', ?, ?, ?)",
                          (b, base + 4, pane,
                           json.dumps({"reason": "no_assigned_work"})))
            inbox.execute("INSERT INTO messages VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                          (mid, "lane", "lane", pane, "next-job", f"TASK {b}",
                           f"TASK {b} test\nhandoff: {mid}", "read", base + 1,
                           base + 3.5, None, "sent", None, base + 2, mid + ":1", 1))
            metrics.execute("INSERT INTO events (at,kind,pane_id,source,outcome) "
                            "VALUES (?, 'turn.start', ?, ?, 'ok')",
                            (base + 3, pane, lane))
        tasks.commit()
        inbox.commit()
        metrics.commit()
        tasks.close()
        inbox.close()
        metrics.close()
        for lane, row in self.tasks.items():
            for key in ("A", "B"):
                spec = self.manifest["lanes"][lane]["tasks"][key]
                path = Path(spec["artifact"])
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(spec["expect_prefix"] + " " + lane + "\n")

    def test_collect_then_validate_passes_on_pr72_shapes(self):
        receipt = self.tool.collect(self.run)
        self.assertEqual([], self.tool.validate(receipt))

    def test_collect_rejects_a_guarded_enter_record(self):
        # The real send_keys_now path records source=send_keys, text=Enter.
        # This is a collector-level negative, not a hand-written validator field.
        self.tool._messages = lambda: [{
            "message_id": "m-manual-enter", "pane_id": "%90",
            "source": "send_keys", "status": "sent", "created_at": 2.5,
            "text": "Enter"}]
        receipt = self.tool.collect(self.run)
        failures = self.tool.validate(receipt)
        self.assertTrue(any("no human nudge" in f for f in failures), failures)
        self.assertEqual(1, len(receipt["lanes"]["pi"]["human_sends"]))


if __name__ == "__main__":
    unittest.main()


class OperatorBoundaryTests(unittest.TestCase):
    """Task writes must use the authentic operator, never the target lane."""

    def setUp(self):
        self.tool = load_tool()
        self.calls = []
        self.result = None

    def cli(self, *args, **kwargs):
        self.calls.append(list(args))
        return self.result

    def test_create_task_uses_operator_and_global_exclusive_assign(self):
        from types import SimpleNamespace
        self.result = SimpleNamespace(returncode=0, stdout=json.dumps({"id": "tA"}),
                                      stderr="")
        self.tool._cli = self.cli
        spec = {"title": "A", "body": "b"}
        tid = self.tool._create_task("%2", "%90", spec)
        self.assertEqual("tA", tid)
        args = self.calls[0]
        self.assertEqual("%2", args[args.index("--self-pane") + 1])
        self.assertEqual("global", args[args.index("--scope") + 1])
        self.assertEqual("%90", args[args.index("--assign") + 1])
        self.assertNotEqual("%90", args[args.index("--self-pane") + 1])

    def test_create_task_refuses_same_operator_and_target(self):
        with self.assertRaises(SystemExit):
            self.tool._create_task("%90", "%90", {"title": "A", "body": "b"})

    def test_task_add_failure_names_the_target(self):
        from types import SimpleNamespace
        self.result = SimpleNamespace(returncode=2, stdout="", stderr="refused")
        self.tool._cli = self.cli
        with self.assertRaises(SystemExit) as caught:
            self.tool._create_task("%2", "%90", {"title": "A", "body": "b"})
        self.assertIn("%90", str(caught.exception))

    def test_cleanup_uses_operator_never_target(self):
        tmp = Path(tempfile.mkdtemp(prefix="t374-cleanup-"))
        self.addCleanup(lambda: __import__("shutil").rmtree(tmp, ignore_errors=True))
        (tmp / "tasks.json").write_text(json.dumps({
            "pi": {"pane": "%90", "A": "tA1", "B": "tB1"},
            "codex": {"pane": "%91", "A": "tA2", "B": "tB2"}}))
        commands = self.tool.cleanup(tmp, execute=False, operator_pane="%2")
        for command in commands:
            self.assertEqual("%2", command[command.index("--self-pane") + 1])
            self.assertNotIn("%90", command)
            self.assertNotIn("%91", command)

    def test_cleanup_requires_operator_before_any_mutation(self):
        tmp = Path(tempfile.mkdtemp(prefix="t374-cleanup-"))
        self.addCleanup(lambda: __import__("shutil").rmtree(tmp, ignore_errors=True))
        with self.assertRaises(SystemExit):
            self.tool.cleanup(tmp, execute=True, operator_pane=None)
