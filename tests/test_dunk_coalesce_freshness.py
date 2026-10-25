"""t89: a cached read must not survive a write.

Measured 2026-10-24: `inbox read --limit 200` drained 200 messages, and the very next
`inbox count` returned the PRE-drain count (97) for up to DUNKINGSHEEP_MIN_POLL_S. I
nearly filed "reads are not marking" against code that works.

The fix is a generation file: a mutating command bumps it, a cached answer records
the generation it was computed under, and a mismatch means recompute.
"""

import json
import os
import subprocess
import sys
import tempfile
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CLI = os.path.join(ROOT, "dunkingsheep")


class CoalesceFreshnessTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.env = dict(os.environ, DUNKINGSHEEP_DIR=os.path.join(self.tmp.name, "cfg"),
                        DUNKINGSHEEP_SOCKET=os.path.join(self.tmp.name, "ds.sock"),
                        DUNKINGSHEEP_MIN_POLL_S="60", TMUX_PANE="",
                        DUNKINGSHEEP_TTY_IDENTITY="0", DUNKINGSHEEP_CLAUDE_STATUS="0")

    def tearDown(self):
        subprocess.run([sys.executable, CLI, "shutdown"], env=self.env,
                       capture_output=True, text=True, timeout=30)
        self.tmp.cleanup()

    def run_cli(self, *args):
        return subprocess.run([sys.executable, CLI, *args], env=self.env,
                              capture_output=True, text=True, timeout=60)

    def test_a_write_invalidates_the_cached_read(self):
        # `status` is a coalescable read; `task add` is a write.
        self.run_cli("task", "add", "first task")
        first = self.run_cli("status")
        self.assertEqual(0, first.returncode, first.stderr)
        second = self.run_cli("status")
        self.assertEqual(0, second.returncode, second.stderr)
        self.assertIn("cached", second.stderr, "the second read should have been cached")

        # A write, then the same read: it must NOT replay the pre-write answer.
        self.run_cli("task", "add", "second task")
        after = self.run_cli("status")
        self.assertEqual(0, after.returncode, after.stderr)
        self.assertNotIn("cached", after.stderr,
                         "a read after a write replayed the pre-write answer")

    def test_a_mutation_bumps_the_generation_before_and_after(self):
        """The interleaving case: a read that lands between the pre-bump and the write
        completing would cache a pre-write answer under the NEW generation, and that
        answer would look valid until the next mutation. So the bump happens twice, and
        the value says which phase wrote it."""
        self.run_cli("task", "add", "a task")
        import glob
        pattern = os.path.join("/tmp", "dunkingsheep-gen-" + str(os.getuid()) + "-*")
        recent = [path for path in glob.glob(pattern)
                  if os.path.getmtime(path) > __import__("time").time() - 120]
        self.assertTrue(recent, "no generation file was written")
        values = []
        for path in recent:
            with open(path, encoding="utf-8") as handle:
                values.append(handle.read().strip())
        self.assertTrue(any(value.endswith("-post") for value in values),
                        f"the generation was not bumped after the run: {values}")

    def test_reads_with_no_write_between_them_stay_cached(self):
        """The coalescer still does its job: this is a fix for writes, not a
        blanket disabling of the cache."""
        self.run_cli("status")
        again = self.run_cli("status")
        self.assertIn("cached", again.stderr)


if __name__ == "__main__":
    unittest.main()
