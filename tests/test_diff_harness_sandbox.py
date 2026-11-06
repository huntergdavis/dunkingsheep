"""The sandbox rule for the harness (t1605, decision A): an explicit daemon environment, a named sandbox
tmux server, and the live daemon's socket and state dir refused by name. No daemon, tmux or live
socket is touched: the daemon process is faked and the socket is a placeholder file."""

import contextlib
import io
import json
import os
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(__file__)), "tools"))

import _bootstrap  # noqa: F401,E402
import diff_harness as harness  # noqa: E402
import production_replay as gate_module  # noqa: E402

# What an agent pane looks like: the guard shims first on PATH, and the live tmux and herdr identity.
AGENT_PATH = "/agent-pane/tmux-guard:/usr/bin:/bin"
AGENT_ENV = {
    "TMUX": "/tmp/tmux-1000/default,2342470,32",
    "TMUX_PANE": "%217",
    "HERDR_PANE_ID": "w1:p1",
    "HERDR_SOCKET_PATH": "/tmp/herdr.sock",
    "DUNKINGSHEEP_VP_PANE": "%7",
    "DUNKINGSHEEP_OWNER_PANE": "%9",
    "PATH": AGENT_PATH,
}


class FakeProcess:
    """Stands in for the daemon process. It records its argv and environment and never exits."""

    started = []

    def __init__(self, argv, env=None, stdout=None, stderr=None):
        self.argv = list(argv)
        self.env = dict(env or {})
        FakeProcess.started.append(self)

    def poll(self):
        return None

    def terminate(self):
        pass

    def kill(self):
        pass

    def wait(self, timeout=None):
        return 0


class StartDaemonEnvironmentTests(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.dir.cleanup)
        self.state = os.path.join(self.dir.name, "state")
        os.makedirs(self.state)
        with open(os.path.join(self.state, "superusers.json"), "w", encoding="utf-8") as handle:
            json.dump({"panes": [], "names": ["owner"]}, handle)
        self.socket_path = os.path.join(self.dir.name, "ds.sock")
        # start_daemon waits for the socket path to exist, then asks it for status.
        open(self.socket_path, "w", encoding="utf-8").close()
        FakeProcess.started = []
        for patcher in (mock.patch.object(harness.subprocess, "Popen", FakeProcess),
                        mock.patch.object(harness, "call", return_value=({"ok": True}, 0.0)),
                        mock.patch.dict(os.environ, AGENT_ENV)):
            patcher.start()
            self.addCleanup(patcher.stop)

    def start(self, tmux_socket="ds-test-sandbox"):
        proc, workdir, log = harness.start_daemon(self.state, self.socket_path, tmux_socket)
        self.addCleanup(harness.stop_daemon, proc, log)
        self.assertEqual(1, len(FakeProcess.started))
        return FakeProcess.started[-1].env

    def test_the_daemon_gets_no_terminal_identity_or_guard_path_from_the_caller(self):
        env = self.start()
        for key in env:
            self.assertFalse(key.startswith("HERDR_"), key)
        for key in ("TMUX", "TMUX_PANE", "DUNKINGSHEEP_VP_PANE", "DUNKINGSHEEP_OWNER_PANE"):
            self.assertNotIn(key, env)
        self.assertEqual(harness.SANDBOX_PATH, env["PATH"])
        self.assertNotIn("tmux-guard", env["PATH"])

    def test_tmux_is_reached_only_on_the_named_sandbox_server(self):
        env = self.start(tmux_socket="ds-test-sandbox")
        self.assertEqual("ds-test-sandbox", env["DUNKINGSHEEP_TMUX_SOCKET"])
        self.assertEqual(harness.TMUX_BIN, env["DUNKINGSHEEP_TMUX_BIN"])
        self.assertEqual("tmux", env["DUNKINGSHEEP_TERMINAL_BACKEND"])

    def test_state_socket_and_home_point_into_the_copy_not_the_caller(self):
        env = self.start()
        self.assertEqual(self.socket_path, env["DUNKINGSHEEP_SOCKET"])
        self.assertNotEqual(os.path.realpath(self.state), os.path.realpath(env["DUNKINGSHEEP_DIR"]))
        self.assertTrue(env["DUNKINGSHEEP_DIR"].endswith(os.sep + "state"))
        self.assertTrue(env["HOME"].endswith(os.sep + "home"))
        self.assertNotEqual(os.path.realpath(os.path.expanduser("~")), os.path.realpath(env["HOME"]))
        self.assertEqual("1", env["DUNKINGSHEEP_NO_START_DAEMON"])

    def test_an_empty_tmux_socket_is_refused_before_any_process_starts(self):
        with self.assertRaises(SystemExit):
            harness.start_daemon(self.state, self.socket_path, "")
        self.assertEqual([], FakeProcess.started)

    def test_the_live_state_dir_is_refused_before_any_copy_or_process(self):
        with self.assertRaises(SystemExit):
            harness.start_daemon(harness.LIVE_CONFIG_DIR, self.socket_path, "ds-test-sandbox")
        self.assertEqual([], FakeProcess.started)

    def test_the_live_socket_is_refused_before_any_process(self):
        with self.assertRaises(SystemExit):
            harness.start_daemon(self.state, harness.LIVE_SOCKET, "ds-test-sandbox")
        self.assertEqual([], FakeProcess.started)


class LiveSocketRefusalTests(unittest.TestCase):
    def test_the_live_socket_is_refused_and_a_sandbox_socket_is_not(self):
        with self.assertRaises(SystemExit):
            harness.refuse_live_socket(harness.LIVE_SOCKET)
        harness.refuse_live_socket("/tmp/sandbox-example/ds.sock")

    def test_the_replay_gate_refuses_the_live_socket_before_it_connects(self):
        with tempfile.TemporaryDirectory() as tmp:
            traffic = os.path.join(tmp, "traffic.jsonl")
            with open(traffic, "w", encoding="utf-8") as handle:
                handle.write(json.dumps({"cmd": "status", "args": {}, "response": {}}) + "\n")
            with mock.patch.object(harness, "replay",
                                   side_effect=AssertionError("must not connect")):
                with self.assertRaises(SystemExit):
                    gate_module.main(["--traffic", traffic, "--against", harness.LIVE_SOCKET,
                                      "--commands", "status"])

    def test_record_refuses_the_live_socket_as_target_or_as_listen_path(self):
        with mock.patch.object(harness, "record", side_effect=AssertionError("must not record")):
            with self.assertRaises(SystemExit):
                harness.main(["record", "--target", harness.LIVE_SOCKET,
                              "--listen", "/tmp/sandbox-proxy.sock", "--out", "/tmp/x.jsonl"])
            with self.assertRaises(SystemExit):
                harness.main(["record", "--target", "/tmp/sandbox-daemon.sock",
                              "--listen", harness.LIVE_SOCKET, "--out", "/tmp/x.jsonl"])

    def test_golden_without_a_tmux_socket_is_a_named_refusal(self):
        with tempfile.TemporaryDirectory() as tmp:
            state = os.path.join(tmp, "state")
            os.makedirs(state)
            traffic = os.path.join(tmp, "traffic.jsonl")
            open(traffic, "w", encoding="utf-8").close()
            with mock.patch.object(harness, "start_daemon",
                                   side_effect=AssertionError("must not start")):
                with contextlib.redirect_stderr(io.StringIO()) as err:
                    code = harness.main(["golden", "--traffic", traffic, "--state", state])
        self.assertEqual(2, code)
        self.assertIn("--tmux-socket", err.getvalue())


if __name__ == "__main__":
    unittest.main()
