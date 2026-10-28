"""t121: the digest, the summary and the counts list only LIVE articles.

Repro that produced this: `context supersede --global <slug> file` three times on one subject, then
`context digest` listed every copy - seven entries for one article. A wiki page updated daily would add
one digest line per day.

Also: `retire` used to append a sentence to the body and nothing else, so code could not see that an
entry was retired; and `context get <subject>` returned whichever entry matched first, i.e. an older body
after a supersede.
"""

import os
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
import dunk_context  # noqa: E402

CLI = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "dunkingsheep")


class ChainHeadsTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix="ctx-")
        self.store = dunk_context.Store(os.path.join(self.tmp, ".dunkingsheep"))
        self.store.ensure()

    def _chain(self, depth=3):
        first = self.store.add("v1 body", cls="fact", subject="wiki", type_="reference")
        slug = first["slug"]
        for step in range(2, depth + 1):
            slug = self.store.supersede(slug, f"v{step} body")["slug"]
        return slug

    def test_only_the_head_is_listed(self):
        head = self._chain(3)
        heads = [e["slug"] for e in self.store.heads()]
        self.assertEqual([head], heads)
        self.assertEqual(3, len([e for e in self.store.entries() if e.get("subject") == "wiki"]))

    def test_the_digest_shows_one_line_for_a_three_deep_chain(self):
        head = self._chain(3)
        text = self.store.digest()
        self.assertEqual(1, text.count("[fact/wiki]"), text)
        self.assertIn("v3 body", text)
        self.assertNotIn("v1 body", text)
        self.assertIn("2 superseded/retired entries not shown", text)

    def test_a_retired_entry_is_not_live_and_is_machine_readable(self):
        entry = self.store.add("old rule", cls="fact", subject="rule", type_="reference")
        self.store.retire(entry["slug"], "superseded by policy")
        stored = self.store.find(entry["slug"])
        self.assertTrue(stored.get("retired"), "retire must set a field, not only append prose")
        self.assertEqual([], [e["slug"] for e in self.store.heads()])
        text = self.store.digest()
        self.assertNotIn("[fact/rule]", text)
        self.assertIn("1 superseded/retired entries not shown", text)

    def test_stats_counts_heads_and_keeps_the_hidden_rows_visible(self):
        self._chain(3)
        entry = self.store.add("retired one", cls="fact", subject="other", type_="reference")
        self.store.retire(entry["slug"], "no longer true")
        stats = self.store.stats()
        self.assertEqual(1, stats["counts"]["fact"], "only the head counts as a live fact")
        self.assertEqual(1, stats["heads_total"])
        self.assertEqual(2, stats["superseded_total"])
        self.assertEqual(1, stats["retired_total"])

    def test_get_resolves_to_the_chain_head(self):
        head = self._chain(3)
        self.assertEqual(head, self.store.resolve("wiki")["slug"])
        self.assertEqual("v3 body", self.store.resolve("wiki")["body"])
        # An OLD slug in the chain also resolves forward.
        oldest = [e for e in self.store.entries() if e.get("subject") == "wiki"][0]["slug"]
        self.assertEqual(head, self.store.resolve(oldest)["slug"])

    def test_search_does_not_return_superseded_copies(self):
        self._chain(3)
        hits = [row["slug"] for row in self.store.search("body")]
        self.assertEqual(1, len(hits), hits)

    def test_the_cli_get_prints_the_head(self):
        head = self._chain(3)
        result = subprocess.run(
            [sys.executable, CLI, "context", "get", "wiki"],
            capture_output=True, text=True, timeout=60, cwd=self.tmp)
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn("v3 body", result.stdout)
        self.assertNotIn("v1 body", result.stdout)
        self.assertIn(head, result.stdout)

    def test_the_cli_digest_shows_one_line(self):
        self._chain(3)
        result = subprocess.run(
            [sys.executable, CLI, "context", "digest"],
            capture_output=True, text=True, timeout=60, cwd=self.tmp)
        self.assertEqual(0, result.returncode, result.stderr)
        # the CLI labels the store ("[project/fact/wiki]"), so assert the CONTENT property:
        self.assertEqual(1, result.stdout.count("v3 body"), result.stdout)
        self.assertNotIn("v1 body", result.stdout)
        self.assertIn("2 superseded/retired entries not shown", result.stdout)


if __name__ == "__main__":
    unittest.main()
