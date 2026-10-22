"""The send-path guard: a body the sending shell was free to rewrite is refused.

The defect this covers, hit eight times in one night: a backtick in an unquoted
shell argument is executed BEFORE the CLI runs, so the word between the
backticks disappears and the message arrives with a hole that only the reader
sees. The guard refuses survivors of that rewrite rather than guessing; the test
below also nails down what it cannot see, so nobody later believes it can.
"""
import os
import subprocess
import sys
import tempfile
import unittest
from importlib.machinery import SourceFileLoader
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
CLI = str(ROOT / "dunkingsheep")


def load_cli():
    return SourceFileLoader("dunkcli_guard", CLI).load_module()


class SubstitutionRefusalTests(unittest.TestCase):
    def setUp(self):
        self.mod = load_cli()

    def refuse(self, body, **kwargs):
        with self.assertRaises(SystemExit) as caught:
            self.mod._refuse_shell_substitution(body, **kwargs)
        return str(caught.exception)

    def test_each_fragment_is_named_with_a_fix(self):
        for fragment in ("`", "$(", "${"):
            message = self.refuse(f"before {fragment} after")
            self.assertIn(repr(fragment), message, fragment)
            self.assertIn("--literal", message)
            self.assertIn("--body-file", message)

    def test_literal_bypasses_and_plain_text_is_untouched(self):
        self.mod._refuse_shell_substitution("hello `world`", literal=True)   # no raise
        self.mod._refuse_shell_substitution("an ordinary sentence")          # no raise
        self.mod._refuse_shell_substitution("costs $5 and $6")               # a bare $ is fine
        self.mod._refuse_shell_substitution("")                              # empty is fine

    def test_the_subject_is_checked_too(self):
        message = self.refuse("body", subject="run `make`")
        self.assertIn("the subject", message)

    def test_it_cannot_see_what_the_shell_already_removed(self):
        # A body that reached us through an unquoted shell arrives with the hole
        # already in it: two words and a gap. Nothing here can detect that. This
        # test exists so the limit is recorded, not discovered again.
        self.mod._refuse_shell_substitution("LIVE IS NOT IN THE MODEL:  was 27 in all 461 samples")
        self.mod._refuse_shell_substitution("a body with  two spaces in it")


class SubstitutionRefusalCliTests(unittest.TestCase):
    def run_cli(self, *args, stdin=None):
        env = dict(os.environ, DUNKINGSHEEP_DIR=tempfile.mkdtemp(prefix="dunk-guard-"))
        return subprocess.run([sys.executable, CLI, *args], capture_output=True, text=True,
                              timeout=120, env=env, input=stdin)

    def test_the_cli_exits_2_and_says_why(self):
        proc = self.run_cli("inbox", "send", "self", "hello `world`")
        self.assertEqual(2, proc.returncode)
        self.assertIn("shell-substitution", proc.stderr)
        self.assertIn("--literal", proc.stderr)
        self.assertNotIn("Traceback", proc.stderr)


if __name__ == "__main__":
    unittest.main()
