"""Deterministic health self-check and self-repair (t12). No model involved."""

import json
import os
import sys
import tempfile
import time
import unittest
from types import SimpleNamespace
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

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
