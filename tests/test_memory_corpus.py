"""T51: the golden corpus must exist, be reproducible, and pin what recall MEANS.

T51's acceptance was "recall results identical on the golden corpus" with no corpus on
main, so the acceptance could not be measured at all. These tests pin the fixture
instead of the port: it must rebuild byte-identically, it must carry redaction, and it
must actually exercise the paths a port can drop without noticing (bm25 ORDER, the
AND-then-OR fallback, and the three SQL filters).
"""

import importlib.util
import json
import os
import sys
import unittest
from importlib.machinery import SourceFileLoader

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
TOOL = os.path.join(ROOT, "tools", "snapshot_memory_corpus.py")
FIXTURE = os.path.join(ROOT, "rust", "fixtures", "memory", "corpus.json")


def load_tool():
    loader = SourceFileLoader("snapshot_memory_corpus_under_test", TOOL)
    spec = importlib.util.spec_from_loader("snapshot_memory_corpus_under_test", loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


class CorpusFixtureTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tool = load_tool()
        cls.fixture = json.loads(open(FIXTURE, encoding="utf-8").read())

    def test_the_committed_fixture_matches_a_fresh_build(self):
        generated = json.dumps(self.tool.normalise(self.tool.build()), indent=2,
                               sort_keys=True) + "\n"
        self.assertEqual(open(FIXTURE, encoding="utf-8").read(), generated,
                         "the fixture is stale: run tools/snapshot_memory_corpus.py --write")

    def test_two_builds_are_byte_identical(self):
        first = json.dumps(self.tool.normalise(self.tool.build()), sort_keys=True)
        second = json.dumps(self.tool.normalise(self.tool.build()), sort_keys=True)
        self.assertEqual(first, second, "the fixture build is not deterministic")

    def test_environment_is_not_part_of_the_contract(self):
        # Wall clock, temp paths, latencies and byte sizes made the first version
        # irreproducible (23 diff lines) - they are measurements of one implementation,
        # not behaviour a port must match.
        expected = self.fixture["expected"]
        self.assertNotIn("generated_at", expected["context"])
        for key in ("path", "db_bytes", "queries", "last_ingest"):
            self.assertNotIn(key, expected["stats"], key)

    def test_the_chunk_texts_are_already_redacted(self):
        # A port that forgets a SECRET_PATTERNS entry must produce a diff here.
        blob = json.dumps(self.fixture["chunks"])
        for shape in ("sk-live_", "ghp_", "hunter2CorrectHorse"):
            self.assertNotIn(shape, blob, f"{shape} survived into the fixture")
        self.assertIn("[REDACTED", blob)

    def test_the_fixture_exercises_the_and_then_or_fallback(self):
        # `recall()` tries AND first and falls back to OR when that finds nothing. The
        # proof is not "hits span sessions" (they do not, and that was my first, wrong
        # assertion): it is a query whose words appear in NO single chunk while its
        # expected hits are non-empty - which is only reachable through the fallback.
        texts = [chunk["text"].lower() for chunk in self.fixture["chunks"]]
        fallback = []
        for entry in self.fixture["expected"]["recall"]:
            words = [word for word in entry["query"].lower().split() if len(word) > 2]
            if not words or not entry["recall"]:
                continue
            if not any(all(word in text for word in words) for text in texts):
                fallback.append(entry["query"])
        self.assertTrue(fallback,
                        "no query needs the OR fallback, so a port that drops it passes")

    def test_every_filter_is_exercised(self):
        kwargs = [entry["kwargs"] for entry in self.fixture["expected"]["recall"]]
        keys = {key for spec in kwargs for key in spec}
        for needed in ("harness", "project", "since", "limit"):
            self.assertIn(needed, keys, f"no query exercises {needed}")

    def test_the_expected_block_carries_order_and_scores(self):
        entry = next(e for e in self.fixture["expected"]["recall"] if e["recall"])
        hit = entry["recall"][0]
        for key in ("chunk_id", "session_id", "score", "text"):
            self.assertIn(key, hit)
        self.assertIsInstance(hit["score"], (int, float))
        if len(entry["recall"]) > 1:
            scores = [h["score"] for h in entry["recall"]]
            self.assertEqual(sorted(scores), scores,
                             "results must be ordered by bm25, which is what a port gets wrong")

    def test_check_mode_is_a_real_gate(self):
        self.assertEqual(0, self.tool.main(["--check"]))
        real = open(FIXTURE, encoding="utf-8").read()
        try:
            with open(FIXTURE, "w", encoding="utf-8") as handle:
                handle.write(real.replace('"limit": 5', '"limit": 6', 1))
            self.assertEqual(1, self.tool.main(["--check"]),
                             "a moved target must fail the gate")
        finally:
            with open(FIXTURE, "w", encoding="utf-8") as handle:
                handle.write(real)


if __name__ == "__main__":
    unittest.main()
