"""t766: the manifest generator must describe the WHOLE tree, and be its own judge.

The first version walked `ast.Module.body` only, so a class emitted one line and never
its methods: the generated document was 2,070 lines against a committed 2,803 and every
short section was a class-heavy module (dunk_core.py 138 vs 411). A document that omits
every method of every class is the "green check measuring nothing" defect this task
exists to catch, so the tests here pin the inventory rules rather than the whole file.
"""

import importlib.util
import os
import re
import sys
import tempfile
import unittest
from importlib.machinery import SourceFileLoader

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
GENERATOR = os.path.join(ROOT, "tools", "gen_manifest.py")


def load_generator(root=None):
    loader = SourceFileLoader("gen_manifest_under_test", GENERATOR)
    spec = importlib.util.spec_from_loader("gen_manifest_under_test", loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    if root is not None:
        from pathlib import Path
        module.ROOT = Path(root)
        module.OUT = Path(root) / "docs" / "rust" / "MANIFEST.md"
    return module


class InventoryRuleTests(unittest.TestCase):
    """The rules the artefact already follows, pinned as rules."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = self.tmp.name

    def tearDown(self):
        self.tmp.cleanup()

    def write_module(self, name, source):
        path = os.path.join(self.dir, name)
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(source)
        return name

    def inventory(self):
        """Run the real inventory_section over a root that holds only our fixture."""
        gen = load_generator(self.dir)
        (open if False else None)
        return gen.inventory_section()

    def test_a_class_emits_every_method_nested_under_it(self):
        # THE defect: ClassDef bodies were never walked, so all 47 methods of
        # dunk_tasks.Tasks were missing from the document.
        self.write_module("mod.py", """
class Store:
    \"\"\"The store.\"\"\"

    def __init__(self, path):
        \"\"\"Open it.\"\"\"

    def claim_next(self, scope):
        \"\"\"Atomically claim work.\"\"\"

    async def flush(self):
        pass
""")
        text = self.inventory()
        self.assertIn("- class **Store** \u2014 The store.", text)
        self.assertIn("  - `__init__(path)` \u2014 Open it.", text)
        self.assertIn("  - `claim_next(scope)` \u2014 Atomically claim work.", text)
        self.assertIn("  - `flush()`", text)

    def test_self_is_dropped_from_class_methods_but_cls_is_kept(self):
        # The artefact shows methods the way an instance calls them, and keeps `cls`
        # (`from_state(cls, data)`); stripping both would have been a guess.
        self.write_module("mod.py", """
class Row:
    def load(cls, text):
        pass

    def plain(self, a, b):
        pass
""")
        text = self.inventory()
        self.assertIn("  - `load(cls, text)`", text)
        self.assertIn("  - `plain(a, b)`", text)

    def test_a_long_const_value_is_cut_to_ninety_characters(self):
        # 90 is the artefact's own rule: its longest const value is exactly 90 chars.
        value = "x" * 400
        self.write_module("mod.py", f"LONG = '{value}'\n")
        text = self.inventory()
        match = re.search(r"- const `LONG` = `(.*)`$", text, re.M)
        self.assertIsNotNone(match)
        self.assertEqual(90, len(match.group(1)))

    def test_module_level_functions_and_consts_still_appear(self):
        self.write_module("mod.py", """
LIMIT = 5

def helper(a, b):
    \"\"\"Do the thing.\"\"\"
""")
        text = self.inventory()
        self.assertIn("- const `LIMIT` = `5`", text)
        self.assertIn("- `helper(a, b)` \u2014 Do the thing.", text)

    def test_the_module_header_carries_its_line_count(self):
        self.write_module("mod.py", "a = 1\nb = 2\nc = 3\n")
        self.assertIn("## mod.py (3 lines)", self.inventory())


class ManifestIsItsOwnJudgeTests(unittest.TestCase):
    """`--check` must be a real check: same tree, byte for byte, or it fails."""

    def test_two_runs_on_one_tree_are_byte_identical(self):
        gen = load_generator()
        first = gen.build()
        second = gen.build()
        self.assertEqual(first, second, "generation is not deterministic")

    def test_the_header_names_the_revision_it_was_generated_from(self):
        # A frozen "038d9aa" header made --check pass on a stale document.
        gen = load_generator()
        header = gen.build().split("\n", 1)[0]
        self.assertTrue(header.startswith("# Port manifest (generated from "), header)
        self.assertNotIn("038d9aa", header, "the revision must be read, not remembered")

    def test_the_committed_manifest_matches_a_fresh_generation(self):
        # The gate itself: run the generator and compare with what is committed.
        gen = load_generator()
        self.assertEqual(gen.OUT.read_text(), gen.build(),
                         "docs/rust/MANIFEST.md is stale: run tools/gen_manifest.py")

    def test_check_refuses_a_stale_document(self):
        gen = load_generator()
        original = gen.OUT.read_text()
        try:
            gen.OUT.write_text(original.replace("\n", "\nSTALE ", 1))
            self.assertEqual(1, gen.main(["--check"]))
        finally:
            gen.OUT.write_text(original)
        self.assertEqual(0, gen.main(["--check"]))


if __name__ == "__main__":
    unittest.main()
