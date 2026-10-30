#!/usr/bin/env python3
"""t535: the client must not call a BUSY daemon a DEAD one.

Measured defect (2026-10-29): at load 40 on 8 CPUs the daemon's listen accept
queue filled (Recv-Q 129 against backlog 128) while the daemon was alive and
serving. `connect_or_start` classified the timeout as "not running", asked
systemd to start a second daemon, then reported "daemon did not come up" - a
false diagnosis that invites a restart of a healthy daemon, and the retry storm
it causes is what fills the queue in the first place.

These tests use real unix sockets and no daemon: a socket that accepts and
never replies IS the measured busy case.
"""

import os
import socket
import tempfile
import threading
import time
import unittest
from unittest import mock

import dunk_client
from dunk_client import (DaemonBusy, DaemonUnavailable, FlockClient,
                         PROBE_ABSENT, PROBE_BUSY, PROBE_RUNNING)


class _Server:
    """A unix-socket server that answers (or deliberately does not)."""

    def __init__(self, path, reply=None):
        self.path = path
        self.reply = reply
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.bind(path)
        self.sock.listen(8)
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self._run, daemon=True)
        self.thread.start()

    def _run(self):
        while not self.stop.is_set():
            try:
                conn, _ = self.sock.accept()
            except OSError:
                return
            try:
                conn.recv(65536)
                if self.reply is not None:
                    conn.sendall((self.reply + "\n").encode())
            except OSError:
                pass
            finally:
                if self.reply is None:
                    # Hold the connection open and silent: the measured
                    # accept-queue-full shape, from the client's side.
                    time.sleep(1.0)
                conn.close()

    def close(self):
        self.stop.set()
        try:
            self.sock.close()
        except OSError:
            pass
        try:
            os.unlink(self.path)
        except OSError:
            pass


class BusyDaemonIsNotDeadTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = os.path.join(self.tmp.name, "dunk.sock")
        self.addCleanup(self.tmp.cleanup)
        # Keep the test fast: the backoff is the behaviour, not the wall clock.
        patcher = mock.patch.object(dunk_client, "BUSY_RETRY_DELAYS", (0.01, 0.01))
        patcher.start()
        self.addCleanup(patcher.stop)

    def test_silent_socket_probes_busy(self):
        server = _Server(self.path, reply=None)
        self.addCleanup(server.close)
        client = FlockClient(socket_path=self.path, timeout=0.3)
        self.assertEqual(client.probe(), PROBE_BUSY)
        self.assertFalse(client.is_running())

    def test_busy_never_starts_a_daemon_and_says_do_not_restart(self):
        server = _Server(self.path, reply=None)
        self.addCleanup(server.close)
        with mock.patch.object(FlockClient, "start_daemon",
                               side_effect=AssertionError("start_daemon on a BUSY daemon")) as started:
            with self.assertRaises(DaemonBusy) as caught:
                FlockClient.connect_or_start(socket_path=self.path, timeout=0.3)
        self.assertFalse(started.called, "a busy daemon must not be started")
        message = str(caught.exception)
        self.assertIn("running but did not answer", message)
        self.assertIn("Do NOT restart", message)
        self.assertNotIn("did not come up", message)

    def test_busy_error_stays_catchable_as_daemon_unavailable(self):
        self.assertTrue(issubclass(DaemonBusy, DaemonUnavailable))

    def test_absent_socket_is_absent_and_startable(self):
        client = FlockClient(socket_path=self.path, timeout=0.3)
        self.assertEqual(client.probe(), PROBE_ABSENT)
        with self.assertRaises(DaemonUnavailable) as caught:
            FlockClient.connect_or_start(socket_path=self.path, start=False, timeout=0.3)
        self.assertIn("no dunkingsheep daemon", str(caught.exception))

    def test_start_path_succeeds_when_the_daemon_answers(self):
        path = self.path
        holder = {}

        def fake_start(client):
            holder["server"] = _Server(path, reply='{"ok": true, "result": {}}')

        with mock.patch.object(FlockClient, "start_daemon", fake_start):
            client = FlockClient.connect_or_start(socket_path=path, timeout=2.0)
        self.addCleanup(holder["server"].close)
        self.assertEqual(client.probe(), PROBE_RUNNING)

    def test_answering_daemon_is_running(self):
        server = _Server(self.path, reply='{"ok": true, "result": {}}')
        self.addCleanup(server.close)
        client = FlockClient(socket_path=self.path, timeout=1.0)
        self.assertEqual(client.probe(), PROBE_RUNNING)
        self.assertTrue(client.is_running())


if __name__ == "__main__":
    unittest.main()
