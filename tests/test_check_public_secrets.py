"""t746: a public repo must refuse to publish secret-shaped files.

Measured 2026-10-29: the PUBLIC repo huntergdavis/dunkingsheep served
data/auth.json and data/owner-credentials at commit 52847e5 through
raw.githubusercontent (HTTP 200), with a cleartext Password: line and an API token:
line. The values were dead test credentials - nothing live matched them - but the
files should never have been tracked, and a force-push does not unpublish a SHA.

These tests pin both halves of the fix: the scanner refuses the shape, and
.gitignore on main covers the path that let it happen.
"""

import os
import shutil
import subprocess
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.join(ROOT, "tools"))

import check_public_secrets as scanner  # noqa: E402


class SecretScanTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.repo = self.tmp.name
        subprocess.run(["git", "init", "-q", self.repo], check=True)
        subprocess.run(["git", "-C", self.repo, "config", "user.email", "t@example.com"],
                       check=True)
        subprocess.run(["git", "-C", self.repo, "config", "user.name", "t"], check=True)

    def tearDown(self):
        self.tmp.cleanup()

    def track(self, relpath, text):
        path = os.path.join(self.repo, relpath)
        os.makedirs(os.path.dirname(path), exist_ok=True) if os.path.dirname(path) else None
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(text)
        subprocess.run(["git", "-C", self.repo, "add", "-f", relpath], check=True)

    def test_a_credentials_file_is_refused(self):
        self.track("data/owner-credentials", "Password: hunter2\nAPI token: abc\n")
        findings = scanner.scan(self.repo)
        self.assertEqual(1, len(findings))
        self.assertIn("data/", findings[0])
        self.assertIn("must not be tracked", findings[0])

    def test_a_sqlite_database_is_refused(self):
        """Outside data/ too: a database is a database wherever it sits."""
        self.track("var/messages.sqlite3", "SQLite format 3\0")
        self.assertIn("sqlite database", " ".join(scanner.scan(self.repo)))

    def test_a_cleartext_password_line_is_refused(self):
        self.track("notes.txt", "Password: correct-horse-battery-staple\n")
        findings = scanner.scan(self.repo)
        self.assertEqual(1, len(findings))
        self.assertIn("Password", findings[0])

    def test_a_long_api_token_assignment_is_refused(self):
        self.track("config.ini", "api_token = sk-live-0123456789abcdef0123456789\n")
        self.assertIn("api_token", " ".join(scanner.scan(self.repo)))

    def test_prose_about_a_password_is_not_a_finding(self):
        """A label with no value is documentation, and a scanner that cries wolf is a
        scanner nobody runs (this is why the first version was narrowed)."""
        self.track("README.md", "Use the existing Password: ... harness format.\n")
        self.assertEqual([], scanner.scan(self.repo))

    def test_an_ignored_file_is_not_scanned(self):
        """Only TRACKED files are published; an ignored one is the point of .gitignore."""
        with open(os.path.join(self.repo, ".gitignore"), "w", encoding="utf-8") as handle:
            handle.write("data/\n")
        subprocess.run(["git", "-C", self.repo, "add", ".gitignore"], check=True)
        path = os.path.join(self.repo, "data", "owner-credentials")
        os.makedirs(os.path.dirname(path))
        with open(path, "w", encoding="utf-8") as handle:
            handle.write("Password: hunter2\n")
        self.assertEqual([], scanner.scan(self.repo))

    def test_the_exit_code_and_message_are_clear(self):
        self.track("data/auth.json", '{"token": "x"}\n')
        proc = subprocess.run([sys.executable,
                               os.path.join(ROOT, "tools", "check_public_secrets.py"),
                               "--repo", self.repo],
                              capture_output=True, text=True)
        self.assertEqual(1, proc.returncode)
        self.assertIn("PUBLIC-REPO SECRET SCAN FAILED", proc.stderr)
        self.assertIn("force-push does NOT unpublish", proc.stderr)

    def test_this_repository_is_clean(self):
        """The tree that ships this scanner must pass it, allowlist included."""
        self.assertEqual([], scanner.scan(ROOT))

    def test_a_workflow_runs_the_scanner(self):
        """A gate that only exists in a test is not a gate: the workflow must call it."""
        path = os.path.join(ROOT, ".github", "workflows", "secret-scan.yml")
        with open(path, encoding="utf-8") as handle:
            text = handle.read()
        self.assertIn("tools/check_public_secrets.py", text)
        self.assertIn("on:", text)

    def test_gitignore_on_main_ignores_the_data_directory(self):
        """.gitignore in the repo (not only on a branch) must cover data/ and the two
        credential names, because the next `git add -A` is what published them."""
        with open(os.path.join(ROOT, ".gitignore"), encoding="utf-8") as handle:
            rules = [line.strip() for line in handle if line.strip()
                     and not line.startswith("#")]
        for wanted in ("data/", "auth.json", "owner-credentials", "*.sqlite3"):
            self.assertIn(wanted, rules, f"{wanted} must be ignored on main")


if __name__ == "__main__":
    unittest.main()
