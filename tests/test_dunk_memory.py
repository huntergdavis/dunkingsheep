"""Memory service v1: store, ingest, recall, redaction and stall regression."""

import json
import os
import sqlite3
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

from dunk_memory import (MemoryIngestor, MemoryStore, chunk_text,  # noqa: E402
                         fts_query, redact)


def write_jsonl(path, prefix="t", sessions=3, messages=8):
    with open(path, "w", encoding="utf-8") as handle:
        for session in range(sessions):
            for index in range(messages):
                handle.write(json.dumps({
                    "role": "user" if index % 2 == 0 else "assistant",
                    "text": f"{prefix} note {index} about flux capacitor "
                            f"{prefix}-{session}-{index}",
                    "session_id": f"{prefix}-session-{session}",
                    "cwd": "/home/hunter/workspace/demo",
                    "timestamp": 1780000000 + session * 100 + index,
                }) + "\n")
    return path


class MemoryStoreTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = os.path.join(self.tmp.name, "memory.db")
        self.store = MemoryStore(path=self.path)

    def tearDown(self):
        self.store.close()
        self.tmp.cleanup()

    def test_ingest_recall_context_and_stats(self):
        sample = write_jsonl(os.path.join(self.tmp.name, "s.jsonl"))
        result = self.store.ingest_path(sample, harness="generic", project="demo")
        self.assertEqual(3, result["sessions"])
        self.assertEqual(24, result["chunks"])
        rows = self.store.recall("flux capacitor", limit=5)
        self.assertTrue(rows)
        self.assertEqual("generic", rows[0]["harness"])
        self.assertEqual("demo", rows[0]["project"])
        digest = self.store.context("flux capacitor", sessions=2)
        self.assertLessEqual(digest["counts"]["sessions"], 2)
        self.assertTrue(digest["sessions"][0]["snippets"])
        stats = self.store.stats()
        self.assertEqual(1, stats["sources"])
        self.assertEqual(3, stats["sessions"])
        self.assertEqual(24, stats["chunks"])
        self.assertGreater(stats["db_bytes"]["db"], 0)
        self.assertGreaterEqual(stats["queries"]["count"], 2)  # recall + context

    def test_chunking_overlap(self):
        text = ("word " * 4000).strip()
        chunks = chunk_text(text, max_chars=1000, overlap_chars=100)
        self.assertGreater(len(chunks), 3)
        self.assertTrue(all(len(chunk) <= 1000 for chunk in chunks))
        self.assertIn(chunks[0][-40:].strip(), chunks[1])

    def test_redaction(self):
        cleaned = redact("key sk-live-abcdef0123456789abcdef and password=hunter2")
        self.assertNotIn("sk-live-abcdef0123456789abcdef", cleaned)
        self.assertNotIn("hunter2", cleaned)
        self.assertIn("[REDACTED", cleaned)
        path = os.path.join(self.tmp.name, "secret.jsonl")
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(json.dumps({
                "role": "user", "session_id": "secret",
                "text": "secretpad ghp_abcdefghijklmnopqrstuvwxyz0123456789",
                "cwd": "/home/hunter/workspace/demo",
            }) + "\n")
        self.store.ingest_path(path, harness="generic", project="demo")
        rows = self.store.recall("secretpad")
        self.assertTrue(rows)
        self.assertNotIn("ghp_abcdefghijklmnopqrstuvwxyz0123456789",
                         rows[0]["text"])
        self.assertIn("[REDACTED", rows[0]["text"])

    def test_source_adapters(self):
        cases = {
            "pi": [
                {"type": "session", "id": "pi-1", "cwd": "/home/hunter/workspace/demo",
                 "timestamp": "2026-10-18T10:00:00Z"},
                {"type": "message", "message": {"role": "user", "content": [
                    {"type": "text", "text": "pi unique alpha"}]}},
                {"type": "message", "message": {"role": "assistant", "content": [
                    {"type": "text", "text": "pi unique beta"}]}},
            ],
            "claude": [
                {"type": "user", "sessionId": "cl-1", "cwd": "/home/hunter/workspace/demo",
                 "timestamp": "2026-10-18T10:00:00Z",
                 "message": {"role": "user", "content": "claude unique alpha"}},
                {"type": "assistant", "sessionId": "cl-1",
                 "message": {"role": "assistant", "content": [
                     {"type": "text", "text": "claude unique beta"},
                     {"type": "thinking", "thinking": "ignore me"}]}},
            ],
            "codex": [
                {"type": "session_meta", "payload": {
                    "session_id": "cx-1", "cwd": "/home/hunter/workspace/demo",
                    "timestamp": "2026-10-18T10:00:00Z"}},
                {"type": "response_item", "payload": {
                    "type": "message", "role": "user",
                    "content": [{"type": "input_text", "text": "codex unique alpha"}]}},
                {"type": "response_item", "payload": {
                    "type": "message", "role": "developer",
                    "content": [{"type": "input_text", "text": "devonlytoken"}]}},
            ],
        }
        for harness, events in cases.items():
            path = os.path.join(self.tmp.name, f"{harness}.jsonl")
            with open(path, "w", encoding="utf-8") as handle:
                for event in events:
                    handle.write(json.dumps(event) + "\n")
            self.store.ingest_path(path, harness=harness, project="demo")
            rows = self.store.recall(f"{harness} unique alpha")
            self.assertTrue(rows, harness)
            self.assertEqual(harness, rows[0]["harness"])
        self.assertEqual([], self.store.recall("devonlytoken"))

    def test_ingestor_queue_and_status(self):
        sample = write_jsonl(os.path.join(self.tmp.name, "q.jsonl"), prefix="q")
        ingestor = MemoryIngestor(self.store, poll_s=0.1)
        enqueued = ingestor.enqueue(sample, harness="generic", project="demo")
        self.assertEqual(1, self.store.stats()["queue_depth"])
        ingestor.start()
        deadline = time.time() + 15
        while time.time() < deadline:
            if self.store.stats()["queue_depth"] == 0:
                break
            time.sleep(0.1)
        ingestor.stop()
        stats = self.store.stats()
        self.assertEqual(0, stats["queue_depth"])
        self.assertEqual(1, stats["queue"]["done"])
        self.assertTrue(self.store.recall("flux capacitor"))

    def test_stopped_ingest_is_skipped_not_failed(self):
        sample = write_jsonl(os.path.join(self.tmp.name, "stop.jsonl"))
        self.store.enqueue(sample, harness="generic", project="demo")
        lease = self.store.claim_next()
        self.store.finish(lease, False, error="stopped by operator", skipped=True)
        stats = self.store.stats()
        self.assertEqual(1, stats["queue"]["skipped"])
        self.assertEqual(0, stats["queue"]["failed"])
        self.assertEqual(1, stats["partial_sources"])
        # A later enqueue can resume the source.
        resumed = self.store.enqueue(sample, harness="generic", project="demo")
        self.assertEqual(1, self.store.stats()["queue"]["queued"])

    def test_recover_leases_requeues_an_expired_ingest(self):
        sample = write_jsonl(os.path.join(self.tmp.name, "lease.jsonl"))
        self.store.enqueue(sample, harness="generic", project="demo")
        self.store.claim_next(lease_s=0.01)
        time.sleep(0.05)
        self.assertEqual(1, self.store.recover_leases()["requeued"])
        self.assertEqual(1, self.store.stats()["queue"]["queued"])

    def test_recall_never_ingests_or_enqueues(self):
        sample = write_jsonl(os.path.join(self.tmp.name, "r.jsonl"))
        self.store.ingest_path(sample, harness="generic", project="demo")
        with mock.patch.object(self.store, "ingest_path",
                               side_effect=AssertionError("recall ingested")), \
                mock.patch.object(self.store, "enqueue",
                                  side_effect=AssertionError("recall enqueued")):
            rows = self.store.recall("flux capacitor")
            self.assertTrue(rows)
            self.store.context("flux capacitor")

    def test_read_during_write_does_not_block(self):
        sample = write_jsonl(os.path.join(self.tmp.name, "w.jsonl"))
        self.store.ingest_path(sample, harness="generic", project="demo")
        writer = sqlite3.connect(self.path, timeout=10)
        writer.execute("PRAGMA busy_timeout = 10000")
        writer.execute("BEGIN IMMEDIATE")
        result = {}

        def query():
            started = time.perf_counter()
            self.store.recall("flux capacitor", limit=5)
            result["ms"] = (time.perf_counter() - started) * 1000.0

        thread = threading.Thread(target=query)
        thread.start()
        time.sleep(1.0)  # hold the write lock while the reader runs
        writer.rollback()
        writer.close()
        thread.join(timeout=10)
        self.assertIn("ms", result)
        self.assertLess(result["ms"], 500.0,
                        "a read queued behind a writer; the stall is back")

    def test_parallel_recall_small(self):
        sample = write_jsonl(os.path.join(self.tmp.name, "p.jsonl"),
                             sessions=8, messages=20)
        self.store.ingest_path(sample, harness="generic", project="demo")
        latencies = []
        errors = []
        lock = threading.Lock()
        deadline = time.time() + 3

        def worker():
            local = []
            while time.time() < deadline:
                started = time.perf_counter()
                try:
                    self.store.recall("flux capacitor", limit=5)
                except Exception as error:  # noqa: BLE001
                    with lock:
                        errors.append(error)
                local.append((time.perf_counter() - started) * 1000.0)
            with lock:
                latencies.extend(local)

        threads = [threading.Thread(target=worker) for _ in range(8)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(timeout=15)
        self.assertEqual([], errors)
        ordered = sorted(latencies)
        p95 = ordered[min(len(ordered) - 1, int(0.95 * (len(ordered) - 1)))]
        self.assertLess(p95, 1000.0)

    def test_fts_query_is_safe(self):
        self.assertIsNone(fts_query("***"))
        self.assertIn('"flux"', fts_query("flux capacitor!"))


if __name__ == "__main__":
    unittest.main()
