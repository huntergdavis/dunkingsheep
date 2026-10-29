"""t89: real CLI/cache path, isolated mailbox client and private cache files."""
import contextlib
import importlib.machinery
import importlib.util
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from tests import _bootstrap  # noqa: F401


class ReadAfterWriteTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        loader = importlib.machinery.SourceFileLoader(
            "cli_t89", str(Path(__file__).resolve().parents[1] / "dunkingsheep")
        )
        self.cli = importlib.util.module_from_spec(importlib.util.spec_from_loader(loader.name, loader))
        loader.exec_module(self.cli)
        env = patch.dict(os.environ, {
            "DUNKINGSHEEP_SOCKET": self.tmp.name + "/never-connect.sock",
            "DUNKINGSHEEP_MIN_POLL_S": "60", "DUNKINGSHEEP_FRESH": "0",
            "DUNKINGSHEEP_NICE": "0",
        })
        env.start()
        self.addCleanup(env.stop)
        generation = patch.object(self.cli, "_generation_path",
                                  return_value=self.tmp.name + "/generation")
        generation.start()
        self.addCleanup(generation.stop)
        path, _ = self.cli._coalesce(["--json", "inbox", "count"])
        for suffix in ("", ".out", ".gen"):
            self.addCleanup(Path(path + suffix).unlink, missing_ok=True)
        self.total = 97
        self.read_during_write = False
        self.calls = []
        client = patch.object(self.cli.FlockClient, "connect_or_start")
        self.client = client.start()
        self.addCleanup(client.stop)
        self.client.return_value.call.side_effect = self.call
        # Any accidental process/socket fallback fails instead of reaching a pane.
        for target in ("subprocess.Popen", "socket.socket"):
            guard = patch(target, side_effect=AssertionError("external operation"))
            guard.start()
            self.addCleanup(guard.stop)

    def call(self, method, **kwargs):
        self.calls.append(method)
        if method == "inbox_count":
            return {"total": self.total}
        if method == "inbox_read":
            if self.read_during_write:
                during, _ = self.run_cli("--json", "inbox", "count")
                self.assertEqual(97, during["total"])
            self.total = 0
            return {"messages": []}
        raise AssertionError(method)

    def run_cli(self, *args):
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = self.cli._run(list(args))
        self.assertEqual(0, code, err.getvalue())
        return json.loads(out.getvalue()), err.getvalue()

    def drain_and_check(self, *prefix):
        self.assertEqual(97, self.run_cli("--json", "inbox", "count")[0]["total"])
        cached, marker = self.run_cli("--json", "inbox", "count")
        self.assertEqual(97, cached["total"])
        self.assertIn('"cached": true', marker)
        self.run_cli("--json", *prefix, "inbox", "read", "--limit", "200")
        after, marker = self.run_cli("--json", "inbox", "count")
        self.assertEqual(0, after["total"], "cached pre-drain count survived write")
        self.assertNotIn('"cached": true', marker)
        expected = ["inbox_count", "inbox_read"]
        if self.read_during_write:
            expected.append("inbox_count")
        self.assertEqual(expected + ["inbox_count"], self.calls)
        self.assertTrue(Path(self.tmp.name + "/generation").read_text().endswith("-post"))

    def test_plain_write_invalidates_actual_count(self):
        self.drain_and_check()

    def test_self_pane_write_invalidates_actual_count(self):
        self.drain_and_check("--self-pane", "%35")

    def test_equals_self_pane_write_invalidates_actual_count(self):
        self.drain_and_check("--self-pane=%35")

    def test_combined_globals_write_invalidates_actual_count(self):
        self.drain_and_check("--no-start-daemon", "--self-pane", "%35",
                             "--socket", self.tmp.name + "/never-connect.sock")

    def test_prefixed_read_only_commands_do_not_invalidate(self):
        for prefix in (["--self-pane", "%35"], ["--self-pane=%35"],
                       ["--socket", self.tmp.name + "/never-connect.sock"]):
            for command in (["inbox", "count"], ["task", "read", "t89"]):
                with self.subTest(prefix=prefix, command=command):
                    self.assertFalse(self.cli._is_mutating(prefix + command))

    def test_count_during_write_is_invalidated_when_write_finishes(self):
        self.read_during_write = True
        self.drain_and_check("--self-pane", "%35")

    def test_read_only_and_fresh_bypass_controls(self):
        self.run_cli("--json", "inbox", "count")
        self.assertFalse(Path(self.tmp.name + "/generation").exists())
        self.total = 12  # external changes still need FRESH or cache expiry
        self.assertEqual(97, self.run_cli("--json", "inbox", "count")[0]["total"])
        with patch.dict(os.environ, {"DUNKINGSHEEP_FRESH": "1"}):
            answer, marker = self.run_cli("--json", "inbox", "count")
        self.assertEqual(12, answer["total"])
        self.assertNotIn('"cached": true', marker)
