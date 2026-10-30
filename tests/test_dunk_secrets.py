"""The desk's own secrets must never be published (t756).

On 2026-10-29 two commits on two public PRs added `data/`: `data/auth.json`,
`data/owner-credentials` (plaintext desk password and API token) and
`data/messages.sqlite3` (the owner's conversation). One was a lane's
`git add -A` in a worktree where a test had just created `data/`; the other was
the same thing an hour later. These tests pin the three defences: the names are
ignored, the push refuses by name, and the PR wrapper refuses by name.
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
import dunk_secrets  # noqa: E402

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


class PatternTests(unittest.TestCase):
    def test_the_leaked_names_are_refused(self):
        for path in ("data/auth.json", "data/owner-credentials", "data/messages.sqlite3",
                     "web.sqlite3", "owner-credentials", "app.pem", ".env",
                     "keys/token_store.py", "sub/dir/state.sqlite3"):
            self.assertIsNotNone(dunk_secrets.matches(path), path)

    def test_ordinary_source_files_are_not(self):
        for path in ("tests/test_telegram_page_watermark.py", "docs/re/README.md",
                     "dunk_secrets.py", "src/recovered/overlay_u20_11ac_cpu.c",
                     "packaging/systemd/dunkingsheep-report.service"):
            self.assertIsNone(dunk_secrets.matches(path), path)

    def test_a_dotfile_keeps_its_dot(self):
        """The first version lstrip'd './' and turned '.env' into 'env'."""
        self.assertEqual(".env", dunk_secrets.matches(".env"))
        self.assertEqual(".env", dunk_secrets.matches("./.env"))

    def test_the_refusal_names_the_file_and_the_rule(self):
        found = dunk_secrets.leaks(["data/auth.json", "docs/x.md"])
        self.assertEqual([("data/auth.json", "data/")], found)


class RepositoryTests(unittest.TestCase):
    """The check runs against a real git repository, because that is where the leak happened."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.repo = os.path.join(self.tmp.name, "repo")
        os.makedirs(self.repo)
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.email", "t@example.com")
        self.git("config", "user.name", "t")
        with open(os.path.join(self.repo, "README.md"), "w") as handle:
            handle.write("base\n")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "base")

    def tearDown(self):
        self.tmp.cleanup()

    def git(self, *args):
        done = subprocess.run(["git", "-C", self.repo, *args],
                              capture_output=True, text=True)
        self.assertEqual(0, done.returncode, done.stderr)
        return done.stdout

    def write(self, path, text="x"):
        target = os.path.join(self.repo, path)
        os.makedirs(os.path.dirname(target) or self.repo, exist_ok=True)
        with open(target, "w") as handle:
            handle.write(text)

    def test_a_clean_branch_passes(self):
        self.write("feature.py")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "feature")
        message, error = dunk_secrets.refusal(cwd=self.repo, base="main")
        self.assertIsNone(message)
        self.assertIsNone(error)

    def test_a_committed_data_directory_is_refused(self):
        self.git("checkout", "-q", "-b", "lane/leak")   # lanes push branches, not main
        self.write("data/owner-credentials", "Password: hunter2\n")
        self.write("data/messages.sqlite3", "sqlite")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "oops")
        message, _error = dunk_secrets.refusal(cwd=self.repo, base="main")
        self.assertIsNotNone(message)
        self.assertIn("data/owner-credentials", message)
        self.assertIn("must never leave this host", message)

    def test_a_staged_but_uncommitted_data_file_is_refused(self):
        """The exact moment of the incident: `git add -A` with data/ present."""
        self.write("data/auth.json", '{"token": "x"}')
        self.git("add", "-A")
        message, _error = dunk_secrets.refusal(cwd=self.repo, base="main")
        self.assertIsNotNone(message)
        self.assertIn("data/auth.json", message)

    def test_the_cli_exit_code_is_what_the_hooks_branch_on(self):
        self.git("checkout", "-q", "-b", "lane/leak")
        self.write("data/owner-credentials", "Password: hunter2\n")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "oops")
        code = dunk_secrets.main(["--cwd", self.repo, "--base", "main", "--quiet"])
        self.assertEqual(1, code)
        os.remove(os.path.join(self.repo, "data", "owner-credentials"))
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "fix")
        self.assertEqual(0, dunk_secrets.main(["--cwd", self.repo, "--base", "main"]))


class AgreementWithTheCiScanner(unittest.TestCase):
    """Two rule lists exist on purpose - the pre-push refusal here and the CI scanner
    in tools/check_public_secrets.py (t746). They must not drift: a name one refuses
    and the other allows is a hole with a rule in front of it."""

    LEAKED = ("data/auth.json", "data/owner-credentials", "data/messages.sqlite3",
              "web.sqlite3", "owner-credentials")

    def setUp(self):
        import importlib.util
        path = os.path.join(ROOT, "tools", "check_public_secrets.py")
        if not os.path.exists(path):
            raise unittest.SkipTest("tools/check_public_secrets.py is not on this base")
        spec = importlib.util.spec_from_file_location("check_public_secrets", path)
        self.scanner = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.scanner)

    def test_every_name_the_scanner_refuses_is_refused_here_too(self):
        for path in self.LEAKED:
            self.assertIsNotNone(self.scanner.scan_path(path), f"the CI scanner allows {path}")
            self.assertIsNotNone(dunk_secrets.matches(path), f"the pre-push guard allows {path}")

    def test_neither_side_refuses_ordinary_source(self):
        for path in ("tests/test_dunk_secrets.py", "docs/re/README.md",
                     "src/recovered/overlay_u20_list_walk_11ac_cpu.c"):
            self.assertIsNone(self.scanner.scan_path(path), f"the CI scanner refuses {path}")
            self.assertIsNone(dunk_secrets.matches(path), f"the pre-push guard refuses {path}")


class WiringTests(unittest.TestCase):
    """The two hook points are wired, and .gitignore carries the same names."""

    def test_gitignore_carries_every_pattern(self):
        text = open(os.path.join(ROOT, ".gitignore"), encoding="utf-8").read()
        for pattern in ("data/", "*.sqlite3", "*-credentials", "*.pem", "*token*", ".env"):
            self.assertIn(pattern, text, pattern)

    def test_the_credential_helper_refuses_before_handing_out_a_token(self):
        text = open(os.path.join(ROOT, "dunkingsheep"), encoding="utf-8").read()
        helper = text.split("def _git_credential(", 1)[1].split("\ndef ", 1)[0]
        self.assertIn("dunk_secrets.refusal()", helper,
                      "the credential helper does not check for secrets")
        self.assertLess(helper.index("dunk_secrets.refusal()"),
                        helper.index("client.call(\"gh_token\")"),
                        "the token is fetched before the check")

    def test_the_gh_wrapper_refuses_pr_create(self):
        text = open(os.path.join(ROOT, "dunkingsheep"), encoding="utf-8").read()
        wrapper = text.split("GH_WRAPPER = ", 1)[1].split('"""', 2)[1]
        self.assertIn("pr create", wrapper)
        self.assertIn("secrets-check", wrapper)
        self.assertIn("exit 1", wrapper)

    def test_the_cli_returns_the_code_not_a_payload(self):
        """The first wiring printed the refusal and still exited 0, so a push
        would have gone through with the message on screen."""
        text = open(os.path.join(ROOT, "dunkingsheep"), encoding="utf-8").read()
        block = text.split('if command == "secrets-check":', 1)[1].split("if command ==", 1)[0]
        self.assertIn("return dunk_secrets.main(", block)
        self.assertNotIn("(lambda r: None)", block)

    def test_the_wrapper_script_is_valid_shell(self):
        text = open(os.path.join(ROOT, "dunkingsheep"), encoding="utf-8").read()
        wrapper = text.split("GH_WRAPPER = ", 1)[1].split('"""', 2)[1]
        path = os.path.join(tempfile.mkdtemp(), "gh")
        with open(path, "w") as handle:
            handle.write(wrapper.replace("__REAL_GH__", "/bin/true"))
        done = subprocess.run(["sh", "-n", path], capture_output=True, text=True)
        shutil.rmtree(os.path.dirname(path), ignore_errors=True)
        self.assertEqual(0, done.returncode, done.stderr)


if __name__ == "__main__":
    unittest.main()
