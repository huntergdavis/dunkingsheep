"""t14: the ground-truth intent set and side-by-side recall/overlap reporting."""

import os
import sqlite3
import sys
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, ROOT)
sys.path.insert(0, os.path.join(ROOT, "tools"))

import memory_parity as mp  # noqa: E402

LIVE_DB = os.path.expanduser("~/.config/dunkingsheep/memory.db")


class IntentSetTests(unittest.TestCase):
    def test_set_is_well_formed(self):
        items = mp.INTENT_GROUND_TRUTH
        self.assertGreaterEqual(len(items), 20)
        self.assertEqual(len(items), len({item["query"] for item in items}))
        self.assertEqual(len(items), len({item["expected"] for item in items}))
        for item in items:
            self.assertIn(item["harness"], ("claude", "codex", "pi", "opencode"))
            self.assertTrue(item["expected"])
            self.assertTrue(item.get("topic"))
            self.assertGreaterEqual(len(item["query"].split()), 5, item["query"])
        harnesses = {item["harness"] for item in items}
        self.assertEqual({"claude", "codex", "pi", "opencode"}, harnesses)

    def test_expected_sessions_exist_and_queries_are_not_verbatim(self):
        if not os.path.exists(LIVE_DB):
            self.skipTest("live memory DB not present")
        conn = sqlite3.connect(f"file:{LIVE_DB}?mode=ro", uri=True)
        try:
            for item in mp.INTENT_GROUND_TRUTH:
                row = conn.execute(
                    "SELECT harness FROM sessions WHERE id=?",
                    (item["expected"],)).fetchone()
                self.assertIsNotNone(row, f"missing session {item['expected']}")
                self.assertEqual(item["harness"], row[0])
                query = " ".join(item["query"].lower().split())
                for (text,) in conn.execute(
                        "SELECT text FROM chunks WHERE session_id=?",
                        (item["expected"],)):
                    body = " ".join((text or "").lower().split())
                    self.assertNotIn(query, body,
                                     f"query is a verbatim slice: {item['query']}")
        finally:
            conn.close()

    def test_render_publishes_recall_and_overlap_next_to_each_other(self):
        report = {
            "counts": {"sources": 1, "sessions": 1, "chunks": 1},
            "deja_answered": True,
            "rollback": {"passed": True},
            "intent": {
                "queries": 2, "scored": 2, "deja_errors": 0,
                "deja_skipped": False,
                "memory_recall5": 0.5, "deja_recall5": 0.25,
                "mean_top5_overlap": 0.3, "top1_match_rate": 0.2,
                "p50_ms": 1.0, "p95_ms": 2.0,
            },
            "intent_details": [{
                "query": "a paraphrased question", "expected": "abc",
                "memory_hit": True, "deja_hit": False, "overlap": 0.2,
            }],
        }
        markdown = mp.render_markdown(report)
        self.assertIn("recall@5", markdown)
        self.assertIn("mean top-5 overlap", markdown)
        self.assertIn("0.5", markdown)
        self.assertIn("0.3", markdown)
        # Both metrics live in the same table row block for the intent set.
        table = markdown.split("Ground-truth intent queries", 1)[1]
        self.assertIn("recall@5", table)
        self.assertIn("mean top-5 overlap", table)

    def test_memory_only_marks_deja_skipped(self):
        class FakeStore:
            def recall(self, query, limit=5, harness=None):
                return [{"session_id": "s1"}]

        metrics, details = mp.score_queries(
            FakeStore(), [{"query": "a paraphrased question", "expected": "s1"}],
            set(), skip_deja=True)
        self.assertTrue(metrics["deja_skipped"])
        self.assertEqual(1.0, metrics["memory_recall5"])
        self.assertIsNone(metrics["mean_top5_overlap"])
        self.assertIsNone(metrics["deja_recall5"])
        self.assertTrue(details[0]["memory_hit"])
        self.assertTrue(details[0]["deja_skipped"])


if __name__ == "__main__":
    unittest.main()
