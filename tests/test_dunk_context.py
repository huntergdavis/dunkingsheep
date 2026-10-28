"""Context store invariants: truths, refusals, supersession, disposable index."""

import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

from tests import _bootstrap  # noqa: F401

from dunk_context import ContextError, Store


class ContextTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.store = Store(Path(self.tmp.name) / ".dunkingsheep")

    def test_chain_head_is_the_only_live_article_across_views(self):
        from dunk_context import digest_both
        from dunk_views import project_summary
        first = self.store.add("obsolete-one", cls="fact", subject="chain",
                               type_="reference", authority="measured")
        self.store.supersede(first["slug"], "obsolete-two")
        head = self.store.supersede(first["slug"], "current-three")
        retired = self.store.add("retired-claim", cls="fact", subject="retired",
                                 type_="reference", authority="measured")
        self.store.retire(retired["slug"], "withdrawn")
        for key in (first["slug"], "chain", head["slug"]):
            self.assertEqual("current-three", self.store.resolve(key)["body"])
        self.assertIsNone(self.store.resolve(retired["slug"]))
        self.assertTrue(self.store.find(retired["slug"])["retired_at"])
        self.assertEqual(4, len(self.store.entries()), "history must survive")
        for text in (self.store.digest(), digest_both(self.store.root, which="project")):
            self.assertIn("current-three", text)
            for excluded in ("obsolete-one", "obsolete-two", "retired-claim"):
                self.assertNotIn(excluded, text)
        self.assertEqual([head["slug"]], self.store.stats()["never_read"])
        self.assertEqual(1, self.store.stats()["counts"]["fact"])
        self.assertEqual([head["slug"]], [e["slug"] for e in self.store.entry_stats()])
        flock = Mock()
        flock.resolve_lanes.return_value = []
        flock.task_list.return_value = {"tasks": []}
        with patch("dunk_context.GLOBAL_DIR", Path(self.tmp.name) / "empty-global"):
            summary = project_summary(flock, project="synthetic", path=self.tmp.name)
        self.assertEqual(1, summary["knowledge"]["articles_total"])
        self.assertEqual(head["slug"], summary["knowledge"]["articles"][0]["slug"])
        self.store.retire(first["slug"], "retire the current head")
        self.assertIsNone(self.store.resolve("chain"))
        self.assertEqual([], self.store.live_entries())

    def test_legacy_retirement_and_corrupt_chains_fail_closed(self):
        from dunk_context import _render
        old = self.store.add("old\n\n**Retired 2026-10-01**: withdrawn", cls="fact",
                             subject="legacy", type_="reference")
        self.assertIsNone(self.store.resolve(old["slug"]))
        self.assertEqual([], self.store.live_entries())
        for successor in ("missing", old["slug"]):
            entry = self.store.find(old["slug"])
            entry["superseded_by"] = successor
            Path(entry["path"]).write_text(_render(entry, entry["body"]))
            with self.assertRaises(ContextError):
                self.store.resolve(old["slug"])

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

    def test_a_supersede_within_the_same_day_gets_its_own_slug(self):
        """Same day + same subject derived the same slug, so a supersede collided
        with the entry it was replacing - the mechanism could never work in a day."""
        first = self.store.add("the old claim", cls="fact", subject="claim",
                              type_="lesson", authority="agent", stated="2026-10-22")
        second = self.store.supersede(first["slug"], "the corrected claim")
        self.assertNotEqual(first["slug"], second["slug"])
        old = self.store.find(first["slug"])
        new = self.store.find(second["slug"])
        self.assertEqual(second["slug"], old["superseded_by"])
        self.assertEqual(first["slug"], new["supersedes"])
        self.assertIn("the corrected claim", new["body"])

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


class ContextCliTests(unittest.TestCase):
    """The CLI surface an agent actually types: a wrong shape must teach, not traceback."""

    def setUp(self):
        import os, subprocess, sys
        self._mods = (os, subprocess, sys)
        self.tmp = __import__("tempfile").TemporaryDirectory()
        self.env = dict(os.environ, DUNKINGSHEEP_DIR=self.tmp.name)
        self.cli = str(Path(__file__).resolve().parent.parent / "dunkingsheep")

    def tearDown(self):
        self.tmp.cleanup()

    def run_cli(self, *args):
        os, subprocess, sys = self._mods
        return subprocess.run([sys.executable, self.cli, *args], capture_output=True,
                              text=True, timeout=120, env=self.env, cwd=self.tmp.name)

    def test_text_adds_inline_and_a_bare_sentence_is_a_named_refusal(self):
        added = self.run_cli("context", "add", "--text", "a claim", "--class", "fact",
                             "--subject", "inline", "--authority", "agent")
        self.assertEqual(0, added.returncode, added.stderr)
        self.assertIn("added fact/", added.stdout)
        bare = self.run_cli("context", "add", "a bare sentence", "--class", "fact",
                            "--subject", "x")
        self.assertEqual(2, bare.returncode)
        self.assertIn("no such file", bare.stderr)
        self.assertIn("--text", bare.stderr, "the refusal must name the form that works")
        self.assertNotIn("Traceback", bare.stderr)

    def test_a_bad_enum_is_a_named_refusal_not_a_traceback(self):
        bad = self.run_cli("context", "add", "--text", "x", "--class", "fact",
                           "--subject", "y", "--authority", "stated")
        self.assertEqual(2, bad.returncode)
        self.assertIn("authority must be one of", bad.stderr)
        self.assertNotIn("Traceback", bad.stderr)

    def test_get_original_slug_and_subject_resolve_three_revision_head(self):
        store = Store(Path(self.tmp.name) / ".dunkingsheep")
        first = store.add("old-body", cls="fact", subject="chain", type_="reference")
        store.supersede(first["slug"], "middle-body")
        head = store.supersede(first["slug"], "newest-body")
        for key in (first["slug"], "chain"):
            result = self.run_cli("context", "get", key)
            self.assertEqual(0, result.returncode, result.stderr)
            self.assertIn("newest-body", result.stdout)
            self.assertNotIn("old-body", result.stdout)
        store.retire(head["slug"], "withdrawn")
        result = self.run_cli("context", "get", first["slug"])
        self.assertNotIn("newest-body", result.stdout)
