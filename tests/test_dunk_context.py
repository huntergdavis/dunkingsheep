"""Context store invariants: truths, refusals, supersession, disposable index."""

import tempfile
import time
import unittest
from pathlib import Path

from tests import _bootstrap  # noqa: F401

from dunk_context import ContextError, Store


class ContextTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.store = Store(Path(self.tmp.name) / ".dunkingsheep")

    def test_an_agent_ground_truth_is_refused_by_check_and_index(self):
        self.store.add("agents may propose but never promote", cls="ground-truth",
                       subject="authority", type_="principle", authority="agent")
        report = self.store.check()
        self.assertTrue(any("authority != owner" in p["problem"] for p in report["refusals"]))
        self.store.index()
        self.assertEqual([], self.store.search("promote"))

    def test_an_owner_ground_truth_survives_and_is_never_stale(self):
        self.store.add("the owner decides", cls="ground-truth", subject="authority",
                       type_="principle", authority="owner", source="owner #1")
        self.assertEqual([], self.store.check()["refusals"])
        self.assertIn("Ground truths (1)", self.store.digest())

    def test_supersession_keeps_both_and_links_them(self):
        old = self.store.add("old truth", cls="ground-truth", subject="thing",
                             type_="principle", authority="owner", stated="2026-10-01")
        new = self.store.supersede(old["slug"], "new truth")
        self.assertIsNotNone(self.store.find(old["slug"]))
        self.assertEqual(old["slug"], self.store.find(new["slug"])["supersedes"])
        self.assertEqual(new["slug"], self.store.find(old["slug"])["superseded_by"])

    def test_a_supersede_target_must_exist(self):
        with self.assertRaises(ContextError):
            self.store.add("x", cls="fact", subject="s", type_="measurement",
                           authority="measured", supersedes="nope")

    def test_the_index_is_disposable_and_search_is_identical(self):
        self.store.add("alpha beta gamma", cls="fact", subject="one", type_="measurement",
                       authority="measured", recheck_after="30d")
        self.store.index()
        first = self.store.search("alpha")
        self.store.index_path().unlink()
        second = self.store.search("alpha")
        self.assertEqual(first, second)

    def test_the_digest_is_bounded_and_names_what_it_omitted(self):
        for index in range(40):
            self.store.add(f"entry number {index} with some text", cls="fact",
                           subject=f"s{index}", type_="measurement", authority="measured",
                           recheck_after="30d")
        self.store.add("the owner decides", cls="ground-truth", subject="authority",
                       type_="principle", authority="owner")
        digest = self.store.digest(max_bytes=400)
        self.assertLessEqual(len(digest.encode("utf-8")), 500)
        self.assertIn("omitted:", digest)
        self.assertIn("Ground truths (1)", digest)

    def test_a_stale_fact_is_a_warning_not_a_deletion(self):
        entry = self.store.add("measured once", cls="fact", subject="m", type_="measurement",
                               authority="measured", stated="2020-01-01", recheck_after="7d")
        report = self.store.check()
        self.assertTrue(any("stale" in p["problem"] for p in report["stale"]))
        self.assertEqual("measured once", self.store.find(entry["slug"])["body"])
        self.assertIn("STALE", self.store.digest())

    def test_access_logging_and_never_read(self):
        read = self.store.add("read me", cls="fact", subject="r", type_="measurement",
                              authority="measured", recheck_after="30d")
        self.store.add("never me", cls="fact", subject="n", type_="measurement",
                       authority="measured", recheck_after="30d")
        self.store.log_access(read["slug"])
        stats = self.store.stats()
        self.assertEqual(1, stats["reads_total"])
        self.assertEqual(1, stats["never_read_count"])
