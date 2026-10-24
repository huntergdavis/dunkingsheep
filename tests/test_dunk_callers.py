import os
import unittest

import _bootstrap  # noqa: F401  (isolate live state)
from dunk_callers import (
    agent_name,
    caller_alive,
    caller_from_peer,
    process_alive,
    process_start,
)


class SystemSenderHasNoPaneTests(unittest.TestCase):
    def test_a_system_send_does_not_carry_a_pane_identity(self):
        from dunk_client import FlockClient
        client = FlockClient(socket_path="/tmp/does-not-exist.sock", self_pane_id="w9:pH")
        captured = {}

        class FakeSock:
            def settimeout(self, *_a): pass
            def connect(self, _path):
                captured["sent"] = True
            def sendall(self, payload):
                captured["payload"] = payload.decode()
                raise OSError("stop here")
            def close(self): pass

        import socket as socket_mod
        original = socket_mod.socket
        socket_mod.socket = lambda *a, **k: FakeSock()
        try:
            try:
                client.call("inbox_send", to="x", body="y", system_sender="watchdog")
            except Exception:
                pass
        finally:
            socket_mod.socket = original
        import json as json_mod
        request = json_mod.loads(captured["payload"].split("\n")[0])
        self.assertIsNone(request["self_pane_id"],
                          "a system sender has no pane; attaching one got the claim refused")


class ProcessIdentityTests(unittest.TestCase):
    def test_self_is_alive_with_its_own_start_time(self):
        start = process_start(os.getpid())
        self.assertIsNotNone(start)
        self.assertTrue(process_alive(os.getpid(), start))

    def test_start_time_mismatch_is_dead(self):
        start = process_start(os.getpid())
        self.assertFalse(process_alive(os.getpid(), str(start) + "0"))

    def test_missing_pid_is_dead(self):
        self.assertFalse(process_alive(99_999_999))
        self.assertFalse(caller_alive({"pid": None, "start": None}))
        self.assertFalse(caller_alive(None))

    def test_agent_name_of_this_test_process_is_none(self):
        self.assertIsNone(agent_name(os.getpid()))

    def test_caller_walks_to_a_live_ancestor_and_keeps_the_pane(self):
        caller = caller_from_peer(os.getpid(), "w8:p8")
        self.assertEqual("w8:p8", caller["pane_id"])
        self.assertIsNotNone(caller["pid"])
        self.assertIsNotNone(caller["start"])
        self.assertIn(caller["source"], ("agent", "session", "peer"))
        self.assertTrue(caller_alive(caller))

    def test_the_agent_ancestor_is_preferred_over_the_short_lived_peer(self):
        caller = caller_from_peer(os.getpid(), "w8:p8")
        if caller["source"] != "agent":
            self.skipTest("this test process is not running under an agent")
        self.assertNotEqual(os.getpid(), caller["pid"])
        self.assertIn(caller["name"], {"pi", "claude", "codex", "opencode"})

    def test_invalid_peer_is_a_named_none_caller(self):
        caller = caller_from_peer(None, "w8:p8")
        self.assertEqual("none", caller["source"])
        self.assertIsNone(caller["pid"])
        self.assertFalse(caller_alive(caller))


if __name__ == "__main__":
    unittest.main()
