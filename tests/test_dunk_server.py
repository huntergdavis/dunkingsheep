import json
import os
import socket
import sys
import tempfile
import threading
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

from dunk_client import FlockClient  # noqa: E402
from dunk_core import DunkError, Flock  # noqa: E402
from dunk_server import AlreadyRunning, FlockServer  # noqa: E402
from fakes import FakeHerdr, claude_box_typing_ansi  # noqa: E402


class ServerRoundTripTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.sock_path = os.path.join(self.tmp.name, "s.sock")
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)
        self.server = FlockServer(self.flock, socket_path=self.sock_path)
        self.server.bind()
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.client = FlockClient(socket_path=self.sock_path, self_pane_id="w1:p2")

    def tearDown(self):
        self.server.shutdown()
        self.thread.join(3)
        self.tmp.cleanup()

    def test_status_and_crud_over_the_socket(self):
        status = self.client.call("status")
        self.assertEqual(0, status["dunkers"])
        self.assertEqual("w1:p2", status["self_pane_id"])

        dunk = self.client.add_dunk(target="self", text="hi {id}", interval_minutes=15)
        self.assertEqual("w1:p2", dunk["target_pane_id"])
        self.assertTrue(dunk["running"])

        listed = self.client.list_dunks()["dunks"]
        self.assertEqual([dunk["id"]], [d["id"] for d in listed])

        fired = self.client.fire_dunk(id=dunk["id"])
        self.assertTrue(fired["ok"])
        self.assertEqual([("w1:p2", "hi d1")], self.herdr.sends)

        self.client.stop_dunk(id=dunk["id"])
        self.assertFalse(self.client.get_dunk(id=dunk["id"])["running"])
        self.client.remove_dunk(id=dunk["id"])
        self.assertEqual([], self.client.list_dunks()["dunks"])

    def test_errors_are_reported_not_raised_by_the_daemon(self):
        with self.assertRaisesRegex(DunkError, "no dunk with id"):
            self.client.get_dunk(id="d99")
        with self.assertRaisesRegex(DunkError, "unknown command"):
            self.client.call("explode")
        with self.assertRaisesRegex(DunkError, "missing required"):
            self.client.call("get_dunk")
        with self.assertRaisesRegex(DunkError, "unknown argument"):
            self.client.call("status", bogus=1)
        # The daemon is still alive afterwards.
        self.assertTrue(self.client.is_running())

    def test_list_panes_marks_self_and_read_pane_works(self):
        panes = self.client.list_panes()
        self_panes = [p for p in panes["panes"] if p["is_self"]]
        self.assertEqual(["w1:p2"], [p["pane_id"] for p in self_panes])
        sent = self.client.send_text(target="codex", text="ping", wait_s=5)
        self.assertTrue(sent["ok"])
        self.assertEqual("ping\n", self.client.read_pane(target="w2:p1")["text"])

    def test_messages_queue_over_the_socket(self):
        self.herdr.set_screen("w2:p1", claude_box_typing_ansi("mid sentence"))
        queued = self.client.send_text(target="codex", text="hold this", wait_s=0.2)
        self.assertTrue(queued["queued"])
        self.assertEqual([queued["message_id"]],
                         [m["message_id"] for m in self.client.list_messages()["queued"]])
        self.assertEqual("queued", self.client.get_message(
            message_id=queued["message_id"])["status"])
        cancelled = self.client.cancel_message(message_id=queued["message_id"])
        self.assertEqual("cancelled", cancelled["status"])
        self.assertEqual([], self.client.list_messages()["queued"])

    def test_layout_control_over_the_socket(self):
        created = self.client.create_workspace(label="Site", command="claude")
        self.assertEqual("Site", created["workspace"]["label"])
        tab = self.client.create_tab(label="writer")  # defaults to the caller's workspace (w1)
        self.assertEqual("w1", tab["tab"]["workspace_id"])
        agent = self.client.start_agent(name="helper", command="codex", workspace="Site")
        self.assertEqual(created["workspace"]["workspace_id"], agent["agent"]["workspace_id"])
        listed = self.client.list_workspaces()
        self.assertEqual({"workspace_id": "w1", "tab_id": "w1:t2"}, listed["self"])
        self.assertIn("Site", [w["label"] for w in listed["workspaces"]])
        self.assertEqual("writer", self.client.rename(kind="tab", target="writer",
                                                      label="writer")["label"])
        with self.assertRaisesRegex(DunkError, "refusing to close"):
            self.client.close(kind="workspace", target="self")
        closed = self.client.close(kind="workspace", target="Site")
        self.assertEqual(created["workspace"]["workspace_id"], closed["id"])
        self.assertIn(agent["pane_id"], closed["closed_panes"])
        passthrough = self.client.herdr(command="pane zoom self --on")
        self.assertEqual(["pane", "zoom", "w1:p2", "--on"], passthrough["argv"])
        self.assertIn("Usage: herdr", self.client.herdr_help()["text"])
        self.assertIn("Usage: herdr", self.client.help()["herdr_help"])

    def test_raw_protocol_handles_multiple_requests_and_bad_json(self):
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.connect(self.sock_path)
        reader = sock.makefile("rb")
        sock.sendall(b'not json\n{"id": 7, "cmd": "status"}\n[1,2]\n')
        first = json.loads(reader.readline())
        second = json.loads(reader.readline())
        third = json.loads(reader.readline())
        sock.close()
        self.assertFalse(first["ok"])
        self.assertEqual("invalid JSON", first["error"])
        self.assertTrue(second["ok"])
        self.assertEqual(7, second["id"])
        self.assertFalse(third["ok"])

    def test_shutdown_command_stops_the_server(self):
        self.client.add_dunk(target="w1:p1", interval_minutes=5)
        result = self.client.call("shutdown", stop_all=True)
        self.assertTrue(result["shutting_down"])
        self.thread.join(3)
        self.assertFalse(self.thread.is_alive())
        self.assertFalse(os.path.exists(self.sock_path))
        self.assertFalse(self.client.is_running())

    def test_second_server_on_same_socket_refuses(self):
        other = FlockServer(Flock(herdr=self.herdr, persist=False, restore=False),
                            socket_path=self.sock_path)
        with self.assertRaises(AlreadyRunning):
            other.bind()


class StaleSocketTests(unittest.TestCase):
    def test_stale_socket_file_is_reclaimed(self):
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "stale.sock")
            dead = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            dead.bind(path)
            dead.close()  # leaves the file behind with nobody listening
            self.assertTrue(os.path.exists(path))
            server = FlockServer(Flock(herdr=FakeHerdr(), persist=False, restore=False),
                                 socket_path=path)
            server.bind()
            try:
                self.assertIsNotNone(server.sock)
            finally:
                server.close()
            self.assertFalse(os.path.exists(path))


if __name__ == "__main__":
    unittest.main()
