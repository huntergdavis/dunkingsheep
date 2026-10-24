"""Deterministic health self-check and self-repair (t12). No model involved."""

import json
import os
from pathlib import Path
import sys
import tempfile
import time
import unittest
from types import SimpleNamespace
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401  (isolate live state)
from dunk_health import run_health  # noqa: E402
from dunk_memory import MemoryStore  # noqa: E402


def write_jsonl(path, prefix="t", sessions=1, messages=4):
    with open(path, "w", encoding="utf-8") as handle:
        for session in range(sessions):
            for index in range(messages):
                handle.write(json.dumps({
                    "role": "user",
                    "text": f"{prefix} note {index} about flux capacitor",
                    "session_id": f"{prefix}-{session}",
                    "cwd": "/tmp/demo",
                    "timestamp": 1780000000 + index,
                }) + "\n")
    return path


class FakeFlock:
    """The slice of Flock that run_health touches, backed by a real store."""

    def __init__(self, store, totals=None):
        self.memory = store
        self.tasks = SimpleNamespace(stats=lambda: {"totals": totals or {}})

    def memory_stats(self):
        return self.memory.stats()

    def memory_status(self):
        return self.memory.status()


class HealthTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = os.path.join(self.tmp.name, "memory.db")
        self.store = MemoryStore(path=self.path)
        self.flock = FakeFlock(self.store)

    def tearDown(self):
        self.store.close()
        self.tmp.cleanup()

    def test_fresh_store_is_ok(self):
        result = run_health(self.flock)
        self.assertTrue(result["ok"], result["problems"])
        self.assertEqual("ok (shallow)", result["checks"]["integrity"])
        self.assertEqual([], result["repairs"])

    def test_deep_check_runs_quick_check(self):
        result = run_health(self.flock, deep=True)
        self.assertTrue(result["ok"], result["problems"])
        self.assertEqual("ok", result["checks"]["integrity"])

    def test_repairs_a_stale_ingest_lease(self):
        sample = write_jsonl(os.path.join(self.tmp.name, "s.jsonl"))
        self.store.enqueue(sample, harness="generic", project="demo")
        self.store.claim_next(lease_s=0.01)
        time.sleep(0.05)

        dry = run_health(self.flock)
        self.assertFalse(dry["ok"])
        self.assertTrue(any("stale" in problem for problem in dry["problems"]))

        fixed = run_health(self.flock, repair=True)
        self.assertTrue(fixed["ok"], fixed["problems"])
        self.assertTrue(any("requeued" in repair for repair in fixed["repairs"]))
        self.assertEqual(1, self.store.stats()["queue"]["queued"])

    def test_flags_and_checkpoints_a_large_wal(self):
        sample = write_jsonl(os.path.join(self.tmp.name, "w.jsonl"))
        self.store.enqueue(sample, harness="generic", project="demo")
        with mock.patch("dunk_health.WAL_WARN_BYTES", 1):
            dry = run_health(self.flock)
            self.assertTrue(any("WAL" in problem for problem in dry["problems"]))
            fixed = run_health(self.flock, repair=True)
            self.assertTrue(any("WAL" in repair for repair in fixed["repairs"]))
            self.assertTrue(fixed["ok"], fixed["problems"])


if __name__ == "__main__":
    unittest.main()


class DispatchFreshnessTests(unittest.TestCase):
    """'The feeder is dead' must be a number, not an absence.

    The dispatcher was failing for hours while a human fed lanes by hand, so nothing
    looked wrong. A tick that has never run and a tick that stopped are different
    facts, and both are reported with an age.
    """

    def setUp(self):
        import dunk_health
        self.health = dunk_health
        self.tmp = tempfile.TemporaryDirectory()

    def tearDown(self):
        self.tmp.cleanup()

    def write(self, payload):
        path = os.path.join(self.tmp.name, "dispatch-state.json")
        with open(path, "w") as handle:
            if isinstance(payload, str):
                handle.write(payload)
            else:
                json.dump(payload, handle)

    def test_no_state_file_says_never_not_zero(self):
        result = self.health._dispatch_freshness(self.tmp.name, time.time())
        self.assertEqual("absent", result["status"])
        self.assertIsNone(result["age_s"])
        self.assertIn("ever run", result["reason"])

    def test_a_recent_tick_is_ok_with_its_age(self):
        self.write({"tick_at": 1000.0})
        result = self.health._dispatch_freshness(self.tmp.name, 1120.0)
        self.assertEqual("ok", result["status"])
        self.assertEqual(120, result["age_s"])

    def test_a_stopped_tick_is_stale_and_names_the_threshold(self):
        self.write({"tick_at": 1000.0})
        result = self.health._dispatch_freshness(self.tmp.name, 1000.0 + self.health.DISPATCH_STALE_S + 1)
        self.assertEqual("stale", result["status"])
        self.assertEqual(self.health.DISPATCH_STALE_S + 1, result["age_s"])

    def test_the_human_line_reports_minutes(self):
        import contextlib, io
        from importlib.machinery import SourceFileLoader
        cli = SourceFileLoader("dunkcli_health",
                               str(Path(__file__).resolve().parent.parent / "dunkingsheep")).load_module()
        result = {"ok": True, "checks": {"dispatch": {"status": "stale", "age_s": 1030}}}
        with contextlib.redirect_stdout(io.StringIO()) as buffer:
            cli.print_health(result)
        line = [row for row in buffer.getvalue().splitlines() if "dispatcher" in row][0]
        self.assertIn("last_tick=17.2m ago (1030s)", line)

    def test_an_unreadable_state_is_absent_not_ok(self):
        self.write("{not json")
        self.assertEqual("absent", self.health._dispatch_freshness(self.tmp.name, 1000.0)["status"])


class DispatchHealthWiringTests(unittest.TestCase):
    def test_health_reports_the_dispatcher_check(self):
        import dunk_health
        import dunk_core
        from fakes import FakeHerdr
        flock = dunk_core.Flock(herdr=FakeHerdr(), persist=False, restore=False)
        result = dunk_health.run_health(flock)
        self.assertIn("dispatch", result["checks"])
        self.assertIn(result["checks"]["dispatch"]["status"], ("ok", "stale", "absent"))
