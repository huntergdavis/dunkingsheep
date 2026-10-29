"""t398 follow-up: the respawn CLI must parse its argv in dispatch.

The core respawn tests passed while the CLI crashed before its first RPC:
`run_client_command` had a local `import dunk_load, json, os`, making `json`
local to the whole function, so the respawn branch's `json.loads` raised
UnboundLocalError. These tests exercise the CLI dispatch (not only the core) and
a real CLI process, without a live daemon, pane, or mutation.
"""
import argparse
import json
import os
import socket
import subprocess
import sys
import tempfile
import threading
import unittest
from importlib.machinery import SourceFileLoader
from pathlib import Path
from unittest.mock import Mock

ROOT = Path(__file__).resolve().parent.parent
CLI = str(ROOT / "dunkingsheep")


def load_cli(name):
    return SourceFileLoader(name, CLI).load_module()


def respawn_args(**overrides):
    values = dict(
        command="respawn-pane", pane="%99", command_json=json.dumps(
            ["codex", "--no-daemon", "resume", "session"]),
        rollback_json=json.dumps(["codex", "resume", "session"]),
        expected_pid=123, expected_start="456", confirm_idle=True,
        reason="t398 dispatch regression",
    )
    values.update(overrides)
    return argparse.Namespace(**values)


class RespawnDispatchTests(unittest.TestCase):
    def setUp(self):
        self.mod = load_cli("dunkcli_respawn_dispatch")
        self.client = Mock()
        self.client.call.return_value = {
            "pane_id": "%99", "old_pid": 123, "pane_pid": 124,
            "reason": "t398 dispatch regression"}

    def test_valid_json_reaches_the_client_call(self):
        """The exact call that raised UnboundLocalError at line 2682."""
        result, _printer = self.mod.run_client_command(
            self.client, respawn_args())
        self.assertEqual("%99", result["pane_id"])
        self.client.call.assert_called_once_with(
            "respawn_pane", pane="%99",
            argv=["codex", "--no-daemon", "resume", "session"],
            rollback_argv=["codex", "resume", "session"],
            expected_pid=123, expected_start="456", confirm_idle=True,
            reason="t398 dispatch regression")

    def test_invalid_json_is_a_named_refusal_before_any_call(self):
        with self.assertRaises(SystemExit) as caught:
            self.mod.run_client_command(
                self.client, respawn_args(command_json="not-json"))
        self.assertEqual(2, caught.exception.code)
        self.assertIn("must be JSON arrays", caught.exception.message)
        self.client.call.assert_not_called()

    def test_rollback_must_also_be_a_string_array(self):
        with self.assertRaises(SystemExit) as caught:
            self.mod.run_client_command(
                self.client, respawn_args(rollback_json='{"not": "an array"}'))
        self.assertEqual(2, caught.exception.code)
        self.assertIn("must be a non-empty JSON array of strings",
                      caught.exception.message)
        self.client.call.assert_not_called()


class FakeRespawnDaemon:
    """A two-request daemon: status (so connect_or_start succeeds) then respawn.

    The respawn reply is a controlled refusal. It records the request, so the
    test can prove the CLI reached the respawn branch instead of short-circuiting
    at argument parsing or connection setup.
    """

    def __init__(self, path):
        self.path = path
        self.requests = []
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.bind(path)
        self.sock.listen(2)
        self.sock.settimeout(5)
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def run(self):
        for _ in range(2):
            try:
                conn, _ = self.sock.accept()
            except (OSError, socket.timeout):
                return
            try:
                raw = conn.makefile("rb").readline()
                request = json.loads(raw.decode())
                self.requests.append(request)
                if request.get("cmd") == "status":
                    reply = {"ok": True, "id": request.get("id"),
                             "result": {"dunkers": 0}}
                else:
                    reply = {"ok": False, "id": request.get("id"),
                             "error": "t398 test daemon refused without mutation"}
                conn.sendall((json.dumps(reply) + "\n").encode())
            except (OSError, ValueError):
                return
            finally:
                conn.close()

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass
        self.thread.join(1)


class RespawnCliProcessTests(unittest.TestCase):
    """A real process, isolated, with a deliberately nonexistent private socket."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.socket = os.path.join(self.tmp.name, "absent.sock")
        self.env = dict(
            os.environ,
            DUNKINGSHEEP_DIR=self.tmp.name,
            DUNKINGSHEEP_SOCKET=self.socket,
            DUNKINGSHEEP_TTY_IDENTITY="0",
            TMUX_PANE="",
            HERDR_PANE_ID="",
        )

    def tearDown(self):
        self.tmp.cleanup()

    def run_cli(self, command_json, socket_path=None):
        path = socket_path or self.socket
        return subprocess.run(
            [sys.executable, CLI, "--socket", path,
             "--no-start-daemon", "--self-pane", "%99",
             "respawn-pane", "%99",
             "--expected-pid", "123", "--expected-start", "456",
             "--confirm-idle", "--reason", "t398 CLI regression",
             "--command-json", command_json,
             "--rollback-json", json.dumps(["codex", "resume", "session"])],
            env=dict(self.env, DUNKINGSHEEP_SOCKET=path),
            capture_output=True, text=True, timeout=30)

    def test_valid_argv_reaches_normal_daemon_absent_refusal(self):
        proc = self.run_cli(json.dumps(["codex", "--no-daemon", "resume", "session"]))
        self.assertEqual(3, proc.returncode, msg=proc.stdout + proc.stderr)
        self.assertIn("no dunkingsheep daemon", proc.stderr)
        self.assertNotIn("Traceback", proc.stderr)
        self.assertNotIn("UnboundLocalError", proc.stderr)

    def test_real_cli_reaches_the_respawn_branch(self):
        """Path evidence: the fake daemon receives the respawn request.

        On 3f979 this process dies with UnboundLocalError before the request;
        after the fix it sends respawn_pane to the private test daemon and gets
        its controlled refusal.
        """
        daemon = FakeRespawnDaemon(os.path.join(self.tmp.name, "fake.sock"))
        try:
            proc = subprocess.run(
                [sys.executable, CLI, "--socket", daemon.path,
                 "--no-start-daemon", "--self-pane", "%99",
                 "respawn-pane", "%99",
                 "--expected-pid", "123", "--expected-start", "456",
                 "--confirm-idle", "--reason", "t398 branch evidence",
                 "--command-json", json.dumps(["codex", "--no-daemon", "resume", "session"]),
                 "--rollback-json", json.dumps(["codex", "resume", "session"])],
                env=dict(self.env, DUNKINGSHEEP_SOCKET=daemon.path),
                capture_output=True, text=True, timeout=30)
        finally:
            daemon.close()
        self.assertNotIn("Traceback", proc.stderr)
        self.assertNotIn("UnboundLocalError", proc.stderr)
        self.assertEqual(2, proc.returncode, msg=proc.stdout + proc.stderr)
        self.assertIn("t398 test daemon refused without mutation", proc.stderr)
        self.assertEqual(["status", "respawn_pane"],
                         [row.get("cmd") for row in daemon.requests])
        respawn = daemon.requests[-1]
        self.assertEqual("%99", respawn["args"]["pane"])
        self.assertEqual(["codex", "--no-daemon", "resume", "session"],
                         respawn["args"]["argv"])
        self.assertEqual(["codex", "resume", "session"],
                         respawn["args"]["rollback_argv"])
        self.assertEqual(123, respawn["args"]["expected_pid"])
        self.assertEqual("456", respawn["args"]["expected_start"])
        self.assertTrue(respawn["args"]["confirm_idle"])
        self.assertEqual("t398 branch evidence", respawn["args"]["reason"])
        self.assertEqual("%99", respawn["self_pane_id"])

    def test_invalid_json_is_refused_before_handshake_or_mutation(self):
        daemon = FakeRespawnDaemon(os.path.join(self.tmp.name, "invalid.sock"))
        try:
            proc = self.run_cli("not-json", socket_path=daemon.path)
        finally:
            daemon.close()
        self.assertNotIn("Traceback", proc.stderr)
        self.assertNotIn("UnboundLocalError", proc.stderr)
        self.assertEqual(2, proc.returncode, msg=proc.stdout + proc.stderr)
        self.assertIn("must be JSON arrays", proc.stderr)
        self.assertEqual([], daemon.requests)

    def test_invalid_json_is_refused_without_a_traceback(self):
        proc = self.run_cli("not-json")
        self.assertEqual(2, proc.returncode)
        self.assertIn("must be JSON arrays", proc.stderr)
        self.assertNotIn("Traceback", proc.stderr)
        self.assertNotIn("UnboundLocalError", proc.stderr)


if __name__ == "__main__":
    unittest.main()
