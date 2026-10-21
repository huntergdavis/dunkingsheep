import importlib.util
import json
import os
import re
import socket
import sys
import tempfile
import threading
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
POLL_PATH = ROOT / "tools" / "dunk_poll.py"


def load_poll():
    spec = importlib.util.spec_from_file_location("dunk_poll_client", POLL_PATH)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class FakeDaemon:
    """One connection, one newline-JSON request, one canned response."""

    def __init__(self, result=None, error=None):
        self.dir = tempfile.TemporaryDirectory()
        self.path = os.path.join(self.dir.name, "dunk.sock")
        self.result = result if result is not None else {
            "status": {"self_pane_id": "wS:p2", "version": "9.9.9",
                       "running": 1, "dunkers": 2, "queued_messages": 0},
            "inbox_count": {"mailbox": "wS:p2", "unread": 1, "total": 3},
            "dunks": {"dunks": [], "messages": [
                {"message_id": "i1", "sender": "w9:pH", "subject": "hi"}]},
        }
        self.error = error
        self.request = None
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.bind(self.path)
        self.sock.listen(1)
        self.thread = threading.Thread(target=self._serve, daemon=True)
        self.thread.start()

    def _serve(self):
        try:
            conn, _ = self.sock.accept()
        except OSError:
            return
        with conn:
            data = b""
            while not data.endswith(b"\n"):
                chunk = conn.recv(65536)
                if not chunk:
                    break
                data += chunk
            self.request = json.loads(data.decode("utf-8"))
            response = ({"id": self.request.get("id", 1), "ok": True,
                         "result": self.result}
                        if self.error is None else
                        {"id": self.request.get("id", 1), "ok": False,
                         "error": self.error})
            conn.sendall((json.dumps(response) + "\n").encode("utf-8"))

    def close(self):
        try:
            self.sock.close()
        finally:
            self.dir.cleanup()


class PollClientTests(unittest.TestCase):
    def setUp(self):
        self.module = load_poll()

    def test_one_batch_carries_the_pane_identity_and_options(self):
        daemon = FakeDaemon()
        self.addCleanup(daemon.close)
        result = self.module.poll(daemon.path, self_pane="wS:p2", limit=7,
                                  panes=True, tasks=True, check_herdr=False)
        self.assertEqual("poll", daemon.request["cmd"])
        self.assertEqual("wS:p2", daemon.request["self_pane_id"])
        self.assertEqual(7, daemon.request["args"]["limit"])
        self.assertTrue(daemon.request["args"]["panes"])
        self.assertTrue(daemon.request["args"]["tasks"])
        self.assertFalse(daemon.request["args"]["check_herdr"])
        self.assertEqual(1, result["inbox_count"]["unread"])

    def test_cli_prints_json_and_text(self):
        daemon = FakeDaemon()
        self.addCleanup(daemon.close)
        import subprocess
        env = dict(os.environ, DUNKINGSHEEP_SOCKET=daemon.path)
        proc = subprocess.run(
            [sys.executable, str(POLL_PATH), "--self-pane", "wS:p2", "--text"],
            capture_output=True, text=True, timeout=20, env=env)
        self.assertEqual(0, proc.returncode, proc.stderr)
        self.assertIn("inbox 1 unread / 3", proc.stdout)
        self.assertIn("#i1 w9:pH: hi", proc.stdout)

    def test_a_missing_socket_is_a_clean_error(self):
        import subprocess
        missing = os.path.join(tempfile.gettempdir(), "no-such-dsh.sock")
        proc = subprocess.run(
            [sys.executable, str(POLL_PATH), "--socket", missing],
            capture_output=True, text=True, timeout=20)
        self.assertEqual(2, proc.returncode)
        self.assertIn("daemon socket not found", proc.stderr)

    def test_an_error_response_is_not_silent(self):
        daemon = FakeDaemon(error="no live pane")
        self.addCleanup(daemon.close)
        with self.assertRaisesRegex(RuntimeError, "no live pane"):
            self.module.poll(daemon.path, self_pane="wS:p2")

    def test_the_client_imports_no_dunk_modules(self):
        source = POLL_PATH.read_text(encoding="utf-8")
        for line in source.splitlines():
            if re.match(r"\s*(import|from)\s+", line):
                self.assertFalse(line.lstrip().startswith(("import dunk_", "from dunk_")),
                                 line)
                self.assertNotIn("import dunkingsheep", line)
        self.assertIn("import socket", source)
        self.assertNotIn("from dunk_client", source)


class LauncherTests(unittest.TestCase):
    def test_install_launcher_writes_the_stdlib_poll_client(self):
        import dunk_guard
        with tempfile.TemporaryDirectory() as tmp:
            result = dunk_guard.install_launcher(bin_dir=tmp)
            target = os.path.join(tmp, "dunk-poll")
            self.assertEqual(target, result["poll_launcher"])
            self.assertTrue(os.path.isfile(target))
            self.assertTrue(os.access(target, os.X_OK))
            text = Path(target).read_text(encoding="utf-8")
            self.assertIn("dunk_poll.py", text)
            self.assertIn("exec", text)


if __name__ == "__main__":
    unittest.main()
