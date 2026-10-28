"""Package identity acceptance without a daemon, terminal, or live state."""
import contextlib
import io
import json
import os
from pathlib import Path
import shutil
import runpy
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tests"))
sys.path.insert(0, str(ROOT))
import _bootstrap  # noqa: E402,F401
import dunk_core  # noqa: E402

PROBE = r'''
import contextlib, io, json, os, runpy, subprocess, sys
from pathlib import Path
from unittest.mock import patch

def forbidden(*args, **kwargs):
    raise AssertionError("probe attempted an external process or signal")

with contextlib.ExitStack() as stack:
    for target in ("subprocess.Popen", "os.system", "os.execv", "os.execve", "os.kill"):
        stack.enter_context(patch(target, forbidden))
    import dunk_core, dunk_sqlite
    from fakes import FakeHerdr
    flock = dunk_core.Flock(herdr=FakeHerdr(), persist=False, restore=False)
    def snapshot():
        status = flock.status(check_herdr=False)
        poll = flock.poll_status()
        cli = runpy.run_path("dunkingsheep", run_name="identity_fixture")
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            cli["print_status"](status)
        return {"status": status, "poll": poll, "text": output.getvalue(),
                "loaded_marker": getattr(dunk_sqlite, "IDENTITY_TEST_MARKER", None)}
    result = {"before": snapshot()}
    if sys.argv[1] == "edit":
        path = Path("dunk_sqlite.py")
        stat = path.stat()
        path.write_text(path.read_text() + '\nIDENTITY_TEST_MARKER = "new-source"\n')
        os.utime(path, ns=(stat.st_atime_ns, stat.st_mtime_ns))
        result["after"] = snapshot()
    print(json.dumps(result))
'''


class PackageLifecycleTests(unittest.TestCase):
    def test_non_core_edit_is_named_until_new_process_imports_it(self):
        with tempfile.TemporaryDirectory(prefix="dunk-identity-") as directory:
            root = Path(directory)
            for source in ROOT.glob("*.py"):
                shutil.copy2(source, root / source.name)
            shutil.copy2(ROOT / "dunkingsheep", root / "dunkingsheep")
            # A fixture dependency outside the application module tree.
            fixtures = root / "fixtures"
            fixtures.mkdir()
            shutil.copy2(ROOT / "tests/fakes.py", fixtures / "fakes.py")
            env = dict(os.environ, DUNKINGSHEEP_DIR=str(root / "state"),
                       DUNKINGSHEEP_SOCKET=str(root / "unused.sock"),
                       PYTHONPATH=str(fixtures), PYTHONDONTWRITEBYTECODE="1")

            def probe(action):
                run = subprocess.run([sys.executable, "-B", "-c", PROBE, action],
                                     cwd=root, env=env, capture_output=True,
                                     text=True, timeout=30)
                self.assertEqual(0, run.returncode, run.stderr)
                return json.loads(run.stdout)

            first = probe("edit")
            restarted = probe("fresh")["before"]
            for endpoint in ("status", "poll"):
                with self.subTest(endpoint=endpoint):
                    self.assertIs(False, first["before"][endpoint]["code_stale"])
                    self.assertIs(True, first["after"][endpoint]["code_stale"])
                    self.assertEqual(["dunk_sqlite.py"],
                                     first["after"][endpoint]["code_changed_modules"])
                    self.assertEqual(first["before"][endpoint]["code_hash"],
                                     first["after"][endpoint]["code_hash"])
                    self.assertIs(False, restarted[endpoint]["code_stale"])
                    self.assertEqual([], restarted[endpoint]["code_changed_modules"])
                    self.assertNotEqual(first["before"][endpoint]["code_hash"],
                                        restarted[endpoint]["code_hash"])
            self.assertIsNone(first["after"]["loaded_marker"])
            self.assertEqual("new-source", restarted["loaded_marker"])
            self.assertIn("dunk_sqlite.py", first["after"]["text"])
            self.assertIn("YES", first["after"]["text"])


class PackageManifestTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="dunk-manifest-")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        (self.root / "dunk_core.py").write_text("CORE = 1\n")
        (self.root / "dunk_sqlite.py").write_text("VALUE = 1\n")
        (self.root / "dunkingsheep").write_text("# entrypoint\n")
        self.baseline = dunk_core.code_identity(self.root)
        self.file_patch = patch.object(dunk_core, "__file__", str(self.root / "dunk_core.py"))
        self.file_patch.start()
        self.addCleanup(self.file_patch.stop)
        self.identity_patch = patch.object(dunk_core, "CODE_IDENTITY", self.baseline)
        self.identity_patch.start()
        self.addCleanup(self.identity_patch.stop)

    def test_same_size_edit_with_preserved_mtime_is_stale(self):
        path = self.root / "dunk_sqlite.py"
        stat = path.stat()
        path.write_text("VALUE = 2\n")
        os.utime(path, ns=(stat.st_atime_ns, stat.st_mtime_ns))
        result = dunk_core.code_status()
        self.assertIs(True, result["code_stale"])
        self.assertEqual(["dunk_sqlite.py"], result["code_changed_modules"])

    def test_mtime_only_change_is_not_stale(self):
        path = self.root / "dunk_sqlite.py"
        os.utime(path, (1, 1))
        self.assertIs(False, dunk_core.code_status()["code_stale"])

    def test_added_and_removed_modules_are_named(self):
        (self.root / "dunk_sqlite.py").unlink()
        (self.root / "dunk_new.py").write_text("NEW = 1\n")
        result = dunk_core.code_status()
        self.assertIs(True, result["code_stale"])
        self.assertEqual(["dunk_new.py", "dunk_sqlite.py"], result["code_changed_modules"])

    def test_cli_entrypoint_edit_is_named(self):
        (self.root / "dunkingsheep").write_text("# changed\n")
        self.assertEqual(["dunkingsheep"], dunk_core.code_status()["code_changed_modules"])

    def test_unreadable_module_is_unknown_and_named(self):
        original = Path.read_bytes
        def read(path):
            if path.name == "dunk_sqlite.py":
                raise PermissionError("synthetic read failure")
            return original(path)
        with patch.object(Path, "read_bytes", read):
            result = dunk_core.code_status()
        self.assertIsNone(result["code_stale"])
        self.assertEqual(["dunk_sqlite.py"], result["code_identity_errors"])
        self.assertEqual([], result["code_changed_modules"])
        with patch("subprocess.Popen", side_effect=AssertionError("external process")):
            cli = runpy.run_path(str(ROOT / "dunkingsheep"), run_name="identity_fixture")
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                cli["print_status"](result)
        self.assertIn("UNKNOWN", output.getvalue())
        self.assertIn("dunk_sqlite.py", output.getvalue())

    def test_single_file_identity_and_missing_file(self):
        current = dunk_core.code_identity(self.root / "dunk_core.py")
        self.assertEqual(["dunk_core.py"], list(current["code_modules"]))
        missing = dunk_core.code_identity(self.root / "missing.py")
        self.assertIsNone(dunk_core.code_is_stale(current, missing))
        self.assertEqual(["missing.py"], missing["code_errors"])

    def test_real_application_manifest_covers_all_reported_modules(self):
        identity = dunk_core.code_identity(ROOT)
        for name in ("dispatch", "metrics", "server", "state", "inbox", "memory",
                     "context", "health", "owner", "sqlite"):
            with self.subTest(module=name):
                self.assertIn(f"dunk_{name}.py", identity["code_modules"])
        self.assertNotIn("test_package_identity.py", identity["code_modules"])


if __name__ == "__main__":
    unittest.main()
