"""Safety and offline behavior of the pinned test-only Python oracle restore."""
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest import mock

SCRIPT = Path(__file__).resolve().parents[1] / "tools/restore_python_oracle.py"
spec = importlib.util.spec_from_file_location("restore_python_oracle", SCRIPT)
restore = importlib.util.module_from_spec(spec)
spec.loader.exec_module(restore)


class RestoreOracleTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.contents = {f"dunk_test{i:02}.py": f"source {i}\n".encode() for i in range(50)}
        self.contents["dunkingsheep"] = b"#!/bin/sh\n"
        self.files = [dict(path=name, bytes=len(data), sha256=hashlib.sha256(data).hexdigest(), mode="0o755" if name == "dunkingsheep" else "0o644") for name, data in self.contents.items()]
        self.manifest = self.root / "manifest.json"
        self.manifest.write_text(json.dumps(dict(commit="1b14a9fb15b7ccf8763b2e15a2ac428824cdc3a8", tag="python-pre-retirement-1b14a9fb", files=self.files)))
        self.root_patch = mock.patch.object(restore, "ROOT", self.root)
        self.manifest_patch = mock.patch.object(restore, "MANIFEST", self.manifest)
        self.root_patch.start(); self.manifest_patch.start()
        self.addCleanup(self.root_patch.stop); self.addCleanup(self.manifest_patch.stop)

    def git(self, *args):
        if args[0] == "show":
            return self.contents[args[1].split(":", 1)[1]]
        self.fail(f"unexpected network/git call: {args}")

    def test_cached_restore_is_offline_and_preserves_executable_mode(self):
        with mock.patch.object(restore.subprocess, "run", return_value=mock.Mock(returncode=0)), mock.patch.object(restore, "git", side_effect=self.git):
            restore.main()
        self.assertEqual((self.root / "dunkingsheep").stat().st_mode & 0o777, 0o755)
        self.assertEqual((self.root / "dunk_test00.py").read_bytes(), self.contents["dunk_test00.py"])

    def test_existing_modified_file_refuses_before_writing(self):
        existing = self.root / "dunk_test00.py"
        existing.write_bytes(b"keep me")
        with self.assertRaises(FileExistsError):
            restore.main()
        self.assertEqual(existing.read_bytes(), b"keep me")
        self.assertFalse((self.root / "dunk_test01.py").exists())

    def test_symlink_conflict_is_not_overwritten(self):
        outside = self.root / "outside"
        outside.write_bytes(b"preserve")
        (self.root / "dunk_test00.py").symlink_to(outside)
        with self.assertRaises(FileExistsError):
            restore.main()
        self.assertEqual(outside.read_bytes(), b"preserve")

    def test_manifest_path_traversal_is_rejected(self):
        data = json.loads(self.manifest.read_text())
        data["files"][0]["path"] = "../dunk_escape.py"
        self.manifest.write_text(json.dumps(data))
        with self.assertRaises(ValueError):
            restore.main()
        self.assertFalse((self.root.parent / "dunk_escape.py").exists())

    def test_corrupt_blob_publishes_no_files(self):
        def corrupt(*args):
            return b"corrupt" if args[1].endswith("dunk_test49.py") else self.git(*args)
        with mock.patch.object(restore.subprocess, "run", return_value=mock.Mock(returncode=0)), mock.patch.object(restore, "git", side_effect=corrupt), self.assertRaises(ValueError):
            restore.main()
        self.assertFalse((self.root / "dunk_test00.py").exists())

    def test_missing_object_fails_without_publishing_files(self):
        with mock.patch.object(restore.subprocess, "run", return_value=mock.Mock(returncode=1)), mock.patch.object(restore, "git", side_effect=subprocess.CalledProcessError(1, "fetch")), self.assertRaises(subprocess.CalledProcessError):
            restore.main()
        self.assertFalse((self.root / "dunk_test00.py").exists())

    def test_write_failure_reports_partial_files_without_deleting_them(self):
        original = Path.open
        def fail_second(path, mode="r", *args, **kwargs):
            if path == self.root / "dunk_test01.py" and mode == "xb":
                raise OSError("synthetic failure")
            return original(path, mode, *args, **kwargs)
        with mock.patch.object(restore.subprocess, "run", return_value=mock.Mock(returncode=0)), mock.patch.object(restore, "git", side_effect=self.git), mock.patch.object(Path, "open", fail_second), self.assertRaisesRegex(OSError, "partial restore created.*dunk_test00"):
            restore.main()
        self.assertEqual((self.root / "dunk_test00.py").read_bytes(), self.contents["dunk_test00.py"])
        self.assertFalse((self.root / "dunk_test01.py").exists())


if __name__ == "__main__":
    unittest.main()
