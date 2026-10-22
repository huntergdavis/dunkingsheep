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

    def test_get_and_search_find_a_global_truth_from_a_project(self):
        from dunk_context import select_stores
        global_store = Store(global_store=True)
        global_store.ensure()
        if global_store.find("rule-global") is None:
            global_store.add("a global rule", cls="ground-truth", subject="rule-global",
                             type_="principle", authority="owner", source="owner")
        self.store.add("a project fact", cls="fact", subject="project-fact", type_="measurement",
                       authority="measured", recheck_after="30d")
        stores = select_stores(self.store.root)
        found = {name: t.find("rule-global") for name, t in stores.items()}
        self.assertIsNotNone(found["global"])
        self.assertIsNone(found["project"])

    def test_delivery_is_recorded_separately_from_reading(self):
        # "nobody reads it" and "nobody was ever told it" are different failures.
        from dunk_context import digest_both
        self.store.add("rule a", cls="ground-truth", subject="rule-a", type_="principle",
                       authority="owner", source="owner")
        before = self.store.stats()
        self.assertEqual(0, before["injected_7d"])
        self.assertEqual(1, before["never_delivered_count"])
        digest_both(self.store.root, which="project")
        after = self.store.stats()
        self.assertEqual(1, after["injected_7d"])
        self.assertEqual(0, after["never_delivered_count"])
        self.assertEqual(1, after["never_read_count"])   # delivered, not opened
        self.store.log_access(self.store.find("rule-a")["slug"])   # what `context get` does
        self.assertEqual(0, self.store.stats()["never_read_count"])

    def test_digest_from_a_project_includes_the_global_ground_truths(self):
        from dunk_context import digest_both
        self.store.add("project fact", cls="fact", subject="p", type_="measurement",
                       authority="measured", recheck_after="30d")
        global_store = Store(global_store=True)
        global_store.ensure()
        if global_store.find("rule-one") is None:
            global_store.add("global rule one", cls="ground-truth", subject="rule-one",
                             type_="principle", authority="owner", source="owner")
        digest = digest_both(self.store.root)
        self.assertIn("Ground truths (", digest)
        self.assertIn("global rule one", digest)
        self.assertIn("project fact", digest)

    def test_report_shows_both_stores_separately(self):
        from dunk_context import report
        self.store.add("project entry", cls="fact", subject="p", type_="measurement",
                       authority="measured", recheck_after="30d")
        global_store = Store(global_store=True)
        global_store.ensure()
        if global_store.find("g1") is None:
            global_store.add("global entry", cls="fact", subject="g1", type_="measurement",
                             authority="measured", recheck_after="30d")
        out = report(self.store.root, which=None)
        self.assertEqual({"global", "project"}, set(out))
        self.assertEqual(1, out["project"]["stats"]["counts"]["fact"])
        self.assertGreaterEqual(out["global"]["stats"]["counts"]["fact"], 1)
        only_global = report(self.store.root, which="global")
        self.assertEqual({"global"}, set(only_global))

    def test_each_store_has_its_own_disposable_index(self):
        self.store.add("project alpha", cls="fact", subject="p", type_="measurement",
                       authority="measured", recheck_after="30d")
        global_store = Store(global_store=True)
        global_store.ensure()
        self.store.index(); global_store.index()
        self.assertNotEqual(self.store.index_path(), global_store.index_path())
        p1 = self.store.search("alpha"); g1 = global_store.search("alpha")
        self.store.index_path().unlink(); global_store.index_path().unlink()
        self.assertEqual(p1, self.store.search("alpha"))
        self.assertEqual(g1, global_store.search("alpha"))

    def test_access_logging_and_never_read(self):
        read = self.store.add("read me", cls="fact", subject="r", type_="measurement",
                              authority="measured", recheck_after="30d")
        self.store.add("never me", cls="fact", subject="n", type_="measurement",
                       authority="measured", recheck_after="30d")
        self.store.log_access(read["slug"])
        stats = self.store.stats()
        self.assertEqual(1, stats["reads_total"])
        self.assertEqual(1, stats["never_read_count"])
