"""Fault-path coverage for closing the FlockClient reply stream."""

import json
import os
import socket
import tempfile
import threading
import unittest
from unittest import mock

import dunk_client
from dunk_client import DaemonUnavailable, FlockClient
from dunk_core import DunkError


class _ReplyPeer:
    def __init__(self, path, response):
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(path)
        self.listener.listen(1)
        self.response = response
        self.request_seen = threading.Event()
        self.client_eof = threading.Event()
        self.failure = None
        self.thread = threading.Thread(target=self._run, daemon=True)
        self.thread.start()

    def _run(self):
        try:
            conn, _ = self.listener.accept()
            with conn:
                conn.settimeout(2)
                request = conn.makefile("rb").readline()
                if not request:
                    raise AssertionError("client closed before sending its request")
                self.request_seen.set()
                if self.response == "timeout":
                    pass
                elif self.response == "eof":
                    conn.shutdown(socket.SHUT_WR)
                else:
                    conn.sendall(self.response)
                self.client_eof.set() if conn.recv(1) == b"" else None
        except BaseException as error:  # surface thread failures in the test
            self.failure = error
        finally:
            self.listener.close()

    def close(self):
        self.thread.join(3)
        if self.thread.is_alive():
            self.listener.close()
            self.fail_thread = True
        else:
            self.fail_thread = False


class ClientReplyStreamCleanupTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = os.path.join(self.tmp.name, "client.sock")
        self.addCleanup(self.tmp.cleanup)

    def _run_peer_case(self, response, expected_error=None):
        peer = _ReplyPeer(self.path, response)
        self.addCleanup(peer.close)
        client = FlockClient(socket_path=self.path, self_pane_id="%1", timeout=0.05)
        retained = []
        if expected_error is None:
            self.assertEqual(client._call_once({"cmd": "status"}), {"alive": True})
        else:
            try:
                client._call_once({"cmd": "status"})
            except expected_error as error:
                # Keep the exception and its traceback alive while checking peer EOF.
                retained.append(error)
            else:
                self.fail(f"expected {expected_error.__name__}")
        self.assertTrue(peer.request_seen.wait(1), "peer did not receive the request")
        self.assertTrue(peer.client_eof.wait(1), "peer stayed open after the client call")
        peer.close()
        self.assertFalse(peer.fail_thread, "peer did not finish")
        self.assertIsNone(peer.failure)
        self.assertTrue(retained or expected_error is None)

    def test_timeout_closes_reply_file_while_exception_is_retained(self):
        self._run_peer_case("timeout", TimeoutError)

    def test_eof_closes_reply_file_while_daemon_unavailable_is_retained(self):
        self._run_peer_case("eof", DaemonUnavailable)

    def test_malformed_reply_closes_file_while_json_error_is_retained(self):
        self._run_peer_case(b"not-json\n", json.JSONDecodeError)

    def test_daemon_error_closes_file_while_dunk_error_is_retained(self):
        self._run_peer_case(b'{"ok":false,"error":"refused"}\n', DunkError)

    def test_success_closes_reply_file(self):
        self._run_peer_case(b'{"ok":true,"result":{"alive":true}}\n')

    def test_send_failure_still_closes_socket(self):
        class BrokenSocket:
            closed = False

            def settimeout(self, _timeout):
                pass

            def connect(self, _path):
                pass

            def sendall(self, _payload):
                raise BrokenPipeError("injected send failure")

            def close(self):
                self.closed = True

        broken = BrokenSocket()
        with mock.patch.object(dunk_client.socket, "socket", return_value=broken):
            with self.assertRaises(BrokenPipeError):
                FlockClient(socket_path=self.path, self_pane_id="%1")._call_once(
                    {"cmd": "status"})
        self.assertTrue(broken.closed)

    def test_failed_connect_still_closes_socket(self):
        class BrokenSocket:
            closed = False

            def settimeout(self, _timeout):
                pass

            def connect(self, _path):
                raise FileNotFoundError("injected connect failure")

            def close(self):
                self.closed = True

        broken = BrokenSocket()
        with mock.patch.object(dunk_client.socket, "socket", return_value=broken):
            with self.assertRaises(FileNotFoundError):
                FlockClient(socket_path=self.path, self_pane_id="%1")._call_once(
                    {"cmd": "status"})
        self.assertTrue(broken.closed)


if __name__ == "__main__":
    unittest.main()
