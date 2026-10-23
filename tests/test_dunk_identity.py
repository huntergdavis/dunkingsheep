"""The version must not be able to lie in the reassuring direction.

For eight commits this file's VERSION said 2.37.4 while the commit messages claimed
2.38 through 2.51, and `status` printed the constant - so "is my fix live?" answered
wrong, and it answered wrong in the direction that tells you everything is fine.

Two guards:

- VERSION must match the version in the latest commit subject, so a commit cannot
  ship a version claim the code does not make;
- `status` carries the code's own hash and says whether the file on disk is the code
  the process is running.
"""
import os
import re
import subprocess
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(ROOT / "tests"))
import _bootstrap  # noqa: E402,F401

import dunk_core  # noqa: E402


class VersionTests(unittest.TestCase):
    def test_the_constant_matches_the_latest_commit_that_declares_a_version(self):
        try:
            log = subprocess.run(["git", "log", "--format=%s", "-20"], cwd=ROOT,
                                 capture_output=True, text=True, timeout=20).stdout
        except (OSError, subprocess.SubprocessError):
            self.skipTest("git unavailable")
        if not log.strip():
            self.skipTest("not a git checkout")
        claimed = None
        for subject in log.splitlines():
            match = re.match(r"^(\d+\.\d+\.\d+):", subject)
            if match:
                claimed = match.group(1)
                break
        self.assertIsNotNone(claimed, "no commit subject declares a version")
        self.assertEqual(claimed, dunk_core.VERSION,
                         "the latest commit claims a different version than the code")

    def test_the_identity_is_derived_from_the_file(self):
        identity = dunk_core.code_identity()
        self.assertEqual(12, len(identity["code_hash"]))
        self.assertIsNotNone(identity["code_mtime"])

    def test_staleness_compares_this_process_to_the_file(self):
        self.assertEqual(True, dunk_core.code_is_stale({"code_hash": "aaaaaaaaaaaa"},
                                                       {"code_hash": "bbbbbbbbbbbb"}))
        self.assertEqual(False, dunk_core.code_is_stale({"code_hash": "cccccccccccc"},
                                                        {"code_hash": "cccccccccccc"}))

    def test_status_carries_the_identity_and_the_staleness_answer(self):
        import tempfile
        from fakes import FakeHerdr
        flock = dunk_core.Flock(herdr=FakeHerdr(), persist=False, restore=False)
        status = flock.status(check_herdr=False)
        for key in ("version", "code_hash", "code_mtime", "started_at", "code_stale"):
            self.assertIn(key, status)
        self.assertEqual(dunk_core.CODE_IDENTITY["code_hash"], status["code_hash"])
        self.assertFalse(status["code_stale"], "a fresh process runs the file on disk")


if __name__ == "__main__":
    unittest.main()
