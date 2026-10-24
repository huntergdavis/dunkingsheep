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


class PollCacheScopeTests(unittest.TestCase):
    """Two stores must never share one cached answer.

    Found from a flaky test: its temp daemon reported total 0, and a second call
    was handed total 163 from the LIVE mailbox, because the cache key was the
    command name alone. The same collision could hand a sandboxed agent the live
    inbox.
    """

    def setUp(self):
        self.mod = load_cli()

    def path_for(self, socket, command=("inbox", "count")):
        import os
        saved = {k: os.environ.get(k) for k in ("DUNKINGSHEEP_SOCKET", "DUNKINGSHEEP_MIN_POLL_S",
                                                "DUNKINGSHEEP_FRESH")}
        os.environ["DUNKINGSHEEP_SOCKET"] = socket
        os.environ["DUNKINGSHEEP_MIN_POLL_S"] = "60"
        os.environ.pop("DUNKINGSHEEP_FRESH", None)
        try:
            path, _hit = self.mod._coalesce(list(command))
            return path
        finally:
            for key, value in saved.items():
                if value is None:
                    os.environ.pop(key, None)
                else:
                    os.environ[key] = value

    def test_each_store_gets_its_own_cache_entry(self):
        temp = self.path_for("/tmp/dunkingsheep-tests-abc/test.sock")
        live = self.path_for("/home/hunter/.config/dunkingsheep/dunkingsheep.sock")
        self.assertNotEqual(temp, live, "two stores shared one poll cache entry")

    def test_two_mailboxes_do_not_share_one_cached_answer(self):
        """Keying by command alone handed lane A's numbers to a caller asking about B."""
        first = self.path_for("/tmp/x/test.sock", ("inbox", "count", "wR:p5"))
        second = self.path_for("/tmp/x/test.sock", ("inbox", "count", "wR:p4"))
        self.assertNotEqual(first, second, "different mailboxes shared one cache entry")
        again = self.path_for("/tmp/x/test.sock", ("inbox", "count", "wR:p5"))
        self.assertEqual(first, again)

    def test_the_same_store_reuses_its_entry(self):
        first = self.path_for("/tmp/x/test.sock")
        second = self.path_for("/tmp/x/test.sock")
        self.assertEqual(first, second)


class StillWorkingTests(unittest.TestCase):
    """A pace problem must not read as a failure.

    'error: timed out' sent the VP hunting a hang that did not exist: the tick was a
    bounded job that outgrew a 15 s socket deadline. The message must name the phase
    and the elapsed time, and the exit code must say 'temporary'.
    """

    def setUp(self):
        self.mod = load_cli()

    def test_the_message_names_the_phase_the_elapsed_time_and_the_next_place_to_look(self):
        message = self.mod._still_working_message("dispatch", 180)
        self.assertIn("still working after 180s", message)
        self.assertIn("dispatch", message)
        self.assertIn("pace problem, not a failure", message)
        self.assertIn("dispatch-state.json", message)
        self.assertNotIn("error", message.lower())

    def test_the_exit_code_is_distinct_from_a_real_failure(self):
        self.assertEqual(75, self.mod.EXIT_STILL_WORKING)
        self.assertNotIn(self.mod.EXIT_STILL_WORKING, (0, 2, 3),
                         "a slow batch must not share an exit code with a refusal")
        self.assertIn("dispatch", self.mod.BATCH_COMMANDS)
