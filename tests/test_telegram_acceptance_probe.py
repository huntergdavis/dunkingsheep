"""t769: the acceptance harness must not confuse "absent" with "not my role".

Review of #151 reproduced this against the live build: from a reviewer pane
`dunkingsheep --json owner outbox telegram --limit 1` exits 2 with
"owner_outbox is not permitted for role reviewer", and the first version of the
precondition read every non-zero exit as "this daemon does not expose owner
ingest/outbox/delivered yet" - so the harness lied about the BUILD, keyed on the
CALLER's role instead of on the help text it had just stopped grepping.

These tests drive the shell script with a fake `DS` on PATH and the real
`--probe-only` mode, so every branch is exercised without a daemon and without
writing a question into the owner mailbox.
"""

import os
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
SCRIPT = os.path.join(ROOT, "tools", "telegram_acceptance.sh")


class ProbeClassificationTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.fake = os.path.join(self.tmp.name, "dunkingsheep")

    def tearDown(self):
        self.tmp.cleanup()

    def fake_cli(self, body="", code=0):
        """A stand-in for `dunkingsheep` that prints `body` and exits `code`."""
        with open(self.fake, "w", encoding="utf-8") as handle:
            handle.write("#!/usr/bin/env bash\n")
            handle.write(f"cat <<'OUT'\n{body}\nOUT\n")
            handle.write(f"exit {code}\n")
        os.chmod(self.fake, 0o755)

    def probe(self, extra=()):
        env = dict(os.environ)
        env["DS"] = self.fake
        return subprocess.run(["bash", SCRIPT, "--probe-only", *extra],
                              capture_output=True, text=True, env=env, timeout=60)

    def test_a_deployed_verb_proceeds(self):
        self.fake_cli('{"ok": true}', 0)
        result = self.probe()
        self.assertEqual(0, result.returncode, result.stdout + result.stderr)
        self.assertIn("PROBE deployed", result.stdout)

    def test_a_role_refusal_is_not_reported_as_a_missing_verb(self):
        # The defect the review reproduced: same exit code as a bad flag, opposite
        # meaning. It must say DEPLOYED and send the operator to the right caller.
        self.fake_cli("error: owner_outbox is not permitted for role reviewer "
                      "(pane %102); see docs/FLAT_FLEET.md section 4.3", 2)
        result = self.probe()
        self.assertEqual(3, result.returncode, result.stdout + result.stderr)
        self.assertIn("PROBE not-permitted", result.stdout)
        self.assertNotIn("does not expose", result.stdout)

    def test_an_unknown_verb_is_reported_as_not_deployed(self):
        self.fake_cli("dunkingsheep: error: argument <command>: invalid choice: "
                      "'owner' (choose from allocation, status, ...)", 2)
        result = self.probe()
        self.assertEqual(2, result.returncode)
        self.assertIn("PROBE not-deployed", result.stdout)

    def test_an_argparse_usage_error_is_not_deployed(self):
        self.fake_cli("usage: dunkingsheep owner [-h] ...\nerror: unrecognized arguments: "
                      "--limit", 2)
        result = self.probe()
        self.assertEqual(2, result.returncode)
        self.assertIn("PROBE not-deployed", result.stdout)

    def test_anything_else_is_reported_verbatim_and_never_guessed(self):
        self.fake_cli("daemon unavailable ([Errno 11] Resource temporarily unavailable)", 2)
        result = self.probe()
        self.assertEqual(5, result.returncode)
        self.assertIn("[Errno 11] Resource temporarily unavailable", result.stdout)

    def test_the_full_run_refuses_before_writing_when_the_verb_is_absent(self):
        # No --probe-only: the harness must stop BEFORE the ingest write, and say
        # which of the two reasons it stopped for.
        self.fake_cli("error: invalid choice: 'owner'", 2)
        env = dict(os.environ)
        env["DS"] = self.fake
        result = subprocess.run(["bash", SCRIPT], capture_output=True, text=True,
                                env=env, timeout=60)
        self.assertEqual(2, result.returncode)
        self.assertIn("does not expose owner ingest/outbox/delivered", result.stdout)
        self.assertNotIn("PASS", result.stdout)

    def test_the_full_run_refuses_on_a_role_refusal_with_its_own_code(self):
        self.fake_cli("error: owner_outbox is not permitted for role worker (pane %108)", 2)
        env = dict(os.environ)
        env["DS"] = self.fake
        result = subprocess.run(["bash", SCRIPT], capture_output=True, text=True,
                                env=env, timeout=60)
        self.assertEqual(3, result.returncode)
        self.assertIn("HAS owner ingest/outbox/delivered", result.stdout)
        self.assertIn("nothing was written", result.stdout)
        self.assertNotIn("PASS", result.stdout)

    def test_the_script_is_executable_and_parses(self):
        self.assertTrue(os.access(SCRIPT, os.X_OK), "the harness must be runnable")
        result = subprocess.run(["bash", "-n", SCRIPT], capture_output=True, text=True)
        self.assertEqual(0, result.returncode, result.stderr)


if __name__ == "__main__":
    unittest.main()
