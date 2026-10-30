"""T01: the differential harness must judge, and must fail loudly.

Every port task is decided by this file (RUST_PORT.md section 7), so its own
failure modes matter: a fixture that stores a credential, a comparison that masks
real behaviour, or a run that reports "0 diffs" because it never measured. Each
of those is pinned here, with a fake daemon on a temp socket - no live daemon, no
spawned `serve` (the owner's order after the 2026-10-30 daemon-spawn incident).
"""

import json
import os
import socket
import sys
import tempfile
import threading
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(__file__)), "tools"))

import _bootstrap  # noqa: F401
import diff_harness as harness  # noqa: E402


class FakeDaemon:
    """A one-line-JSON socket server: replies with a scripted response per cmd."""

    def __init__(self, replies, path):
        self.replies = dict(replies)
        self.path = path
        self.seen = []

    def start(self):
        socket_path = self.path
        if os.path.exists(socket_path):
            os.unlink(socket_path)
        self.server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.server.bind(socket_path)
        self.server.listen(16)
        self.thread = threading.Thread(target=self._serve, daemon=True)
        self.thread.start()
        return self

    def _serve(self):
        while True:
            try:
                conn, _addr = self.server.accept()
            except OSError:
                return
            try:
                raw = conn.makefile("rb").readline()
                if not raw:
                    continue
                request = json.loads(raw.decode("utf-8", "replace"))
                self.seen.append(request)
                reply = self.replies.get(request.get("cmd"),
                                         {"ok": False, "error": "unknown command"})
                reply = dict(reply)
                reply.setdefault("id", request.get("id"))
                conn.sendall((json.dumps(reply) + "\n").encode("utf-8"))
            except (ValueError, OSError):
                pass
            finally:
                conn.close()

    def stop(self):
        try:
            self.server.close()
        except OSError:
            pass
        try:
            os.unlink(self.path)
        except OSError:
            pass


class HarnessTestCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = self.tmp.name

    def tearDown(self):
        self.tmp.cleanup()

    def path(self, name):
        return os.path.join(self.dir, name)


class ScrubTests(HarnessTestCase):
    def test_a_service_token_never_reaches_the_fixture(self):
        scrubbed = harness.scrub({"service_token": "tok-live", "args": {"id": "t1"}})
        self.assertEqual(harness.SCRUB, scrubbed["service_token"])
        self.assertEqual("t1", scrubbed["args"]["id"])

    def test_nested_token_secret_and_password_values_are_scrubbed(self):
        value = {"result": {"apps": {"author": {"token": "ghs_abc", "kind": "author"}},
                            "credentials": [{"password": "hunter2", "user": "x"}],
                            "tokens": {"author": {"token": "ghs_abc"}},
                            "note": "keep me"}}
        scrubbed = harness.scrub(value)
        self.assertEqual(harness.SCRUB, scrubbed["result"]["apps"]["author"]["token"])
        self.assertEqual("author", scrubbed["result"]["apps"]["author"]["kind"])
        self.assertEqual(harness.SCRUB, scrubbed["result"]["credentials"][0]["password"])
        self.assertEqual("x", scrubbed["result"]["credentials"][0]["user"])
        self.assertEqual(harness.SCRUB, scrubbed["result"]["tokens"],
                         "a whole token TABLE is a secret, not a structure to keep")
        self.assertEqual("keep me", scrubbed["result"]["note"])

    def test_an_empty_secret_keeps_its_shape(self):
        scrubbed = harness.scrub({"token": None, "tokens": {}})
        self.assertIsNone(scrubbed["token"])
        self.assertEqual({}, scrubbed["tokens"])


class MaskTests(HarnessTestCase):
    def rules(self, **overrides):
        base = {"keys": ["created_at", "ats"], "patterns": ["^t[0-9]+$"],
                "epoch_keys": ["created_at"]}
        base.update(overrides)
        return base

    def test_the_named_keys_are_masked_and_everything_else_is_not(self):
        masked = harness.mask({"created_at": 1793344628.9, "title": "keep this"},
                              self.rules())
        self.assertEqual(harness.MASK, masked["created_at"])
        self.assertEqual("keep this", masked["title"])

    def test_allocated_ids_are_masked_but_real_text_is_not(self):
        masked = harness.mask({"id": "t42", "title": "t42 is a task"}, self.rules())
        self.assertEqual(harness.MASK, masked["id"])
        self.assertEqual("t42 is a task", masked["title"],
                         "a pattern must not rewrite free text")

    def test_an_iso_timestamp_is_masked_by_shape(self):
        masked = harness.mask({"iso": "2026-10-30T06:31:59Z"}, self.rules())
        self.assertEqual(harness.MASK, masked["iso"])

    def test_a_boolean_is_never_mistaken_for_an_epoch(self):
        masked = harness.mask({"ok": True, "count": 3}, self.rules())
        self.assertIs(True, masked["ok"])
        self.assertEqual(3, masked["count"])

    def test_the_shipped_rules_mask_the_fields_the_selfcheck_found(self):
        rules = harness.load_volatile()
        for key in ("created_at", "oldest_unread_s", "checked_at"):
            self.assertIn(key, rules["keys"])
        masked = harness.mask({"id": "i9", "head": "a" * 40}, rules)
        self.assertEqual(harness.MASK, masked["id"])
        self.assertEqual(harness.MASK, masked["head"])


class RoundTripTests(HarnessTestCase):
    """record -> replay against a fake daemon: identical traffic is 0 diffs."""

    def daemon(self, replies):
        return FakeDaemon(replies, self.path("daemon.sock")).start()

    def write_traffic(self, entries, name="traffic.jsonl"):
        path = self.path(name)
        with open(path, "w", encoding="utf-8") as handle:
            for entry in entries:
                handle.write(json.dumps(entry) + "\n")
        return path

    def test_record_forwards_and_writes_scrubbed_lines(self):
        daemon = self.daemon({"status": {"ok": True, "result": {"version": "2.71.4"}}})
        self.addCleanup(daemon.stop)
        out = self.path("traffic.jsonl")
        listen = self.path("proxy.sock")
        ready = threading.Event()

        def on_ready(_path):
            ready.set()

        recorder = threading.Thread(
            target=harness.record,
            args=(daemon.path, listen, out),
            kwargs={"seconds": 3.0, "on_ready": on_ready},
            daemon=True)
        recorder.start()
        self.assertTrue(ready.wait(5), "the proxy never listened")
        reply, _latency = harness.call(
            listen, {"id": 1, "cmd": "status", "args": {}, "service_token": "tok-live"})
        self.assertTrue(reply["ok"])
        recorder.join(10)
        lines = [json.loads(line) for line in open(out, encoding="utf-8")]
        self.assertEqual(1, len(lines))
        self.assertEqual("status", lines[0]["cmd"])
        self.assertTrue(lines[0]["service_token_scrubbed"],
                        "the fixture must record THAT a token was stripped")
        self.assertNotIn("tok-live", open(out, encoding="utf-8").read())
        self.assertEqual({"version": "2.71.4"}, lines[0]["response"]["result"])

    def test_replay_of_identical_traffic_reports_zero_diffs(self):
        daemon = self.daemon({"status": {"ok": True, "result": {"version": "2.71.4",
                                                               "created_at": 1}}})
        self.addCleanup(daemon.stop)
        traffic = self.write_traffic([
            {"ts": 1, "cmd": "status", "args": {}, "latency_ms": 5.0,
             "response": {"ok": True, "result": {"version": "2.71.4", "created_at": 2}}}])
        result = harness.replay(harness.load_traffic(traffic), daemon.path,
                                volatile=self.write_volatile(), verbose=False)
        self.assertEqual(0, len(result["diffs"]))
        self.assertEqual(1, result["sent"])
        self.assertEqual([], result["errors"])

    def test_a_real_difference_is_reported_as_a_readable_diff(self):
        daemon = self.daemon({"status": {"ok": True, "result": {"version": "0.0.1"}}})
        self.addCleanup(daemon.stop)
        traffic = self.write_traffic([
            {"ts": 1, "cmd": "status", "args": {}, "latency_ms": 5.0,
             "response": {"ok": True, "result": {"version": "2.71.4"}}}])
        result = harness.replay(harness.load_traffic(traffic), daemon.path,
                                volatile=self.write_volatile(), verbose=False)
        self.assertEqual(1, len(result["diffs"]))
        cmd, text = result["diffs"][0]
        self.assertEqual("status", cmd)
        self.assertIn("2.71.4", text)
        self.assertIn("0.0.1", text)

    def test_the_request_id_echo_is_not_a_behaviour_difference(self):
        daemon = self.daemon({"status": {"ok": True, "result": {}, "id": 99}})
        self.addCleanup(daemon.stop)
        traffic = self.write_traffic([
            {"ts": 1, "cmd": "status", "args": {}, "latency_ms": 5.0,
             "response": {"ok": True, "result": {}, "id": 1}}])
        result = harness.replay(harness.load_traffic(traffic), daemon.path,
                                volatile=self.write_volatile(), verbose=False)
        self.assertEqual([], result["diffs"])

    def test_commands_filter_limits_what_is_sent(self):
        daemon = self.daemon({"status": {"ok": True, "result": {}},
                              "lanes": {"ok": True, "result": {}}})
        self.addCleanup(daemon.stop)
        traffic = self.write_traffic([
            {"ts": 1, "cmd": "status", "args": {}, "latency_ms": 1.0,
             "response": {"ok": True, "result": {}}},
            {"ts": 2, "cmd": "lanes", "args": {}, "latency_ms": 1.0,
             "response": {"ok": True, "result": {}}}])
        result = harness.replay(harness.load_traffic(traffic), daemon.path,
                                commands=["lanes"], volatile=self.write_volatile(),
                                verbose=False)
        self.assertEqual(1, result["sent"])
        self.assertEqual(["lanes"], [request["cmd"] for request in daemon.seen])

    def test_a_dead_socket_is_an_error_not_a_pass(self):
        traffic = self.write_traffic([
            {"ts": 1, "cmd": "status", "args": {}, "latency_ms": 1.0,
             "response": {"ok": True, "result": {}}}])
        result = harness.replay(harness.load_traffic(traffic),
                                self.path("no-such.sock"),
                                volatile=self.write_volatile(), verbose=False)
        self.assertEqual(0, len(result["diffs"]))
        self.assertEqual(1, len(result["errors"]),
                         "'0 diffs' must never be reported when nothing answered")
        self.assertEqual(1, harness.main(["replay", "--traffic", traffic,
                                          "--against", self.path("no-such.sock")]))

    def write_volatile(self, name="volatile.json"):
        path = self.path(name)
        payload = {"keys": ["created_at"], "patterns": ["^t[0-9]+$"],
                   "epoch_keys": ["created_at"]}
        with open(path, "w", encoding="utf-8") as handle:
            json.dump(payload, handle)
        return path


class GoldenEntryTests(HarnessTestCase):
    def test_a_missing_snapshot_is_a_named_refusal(self):
        traffic = self.path("empty.jsonl")
        open(traffic, "w").close()
        code = harness.main(["golden", "--traffic", traffic,
                             "--state", self.path("no-such-state")])
        self.assertEqual(2, code)

    def test_a_malformed_traffic_line_says_which_line(self):
        path = self.path("bad.jsonl")
        with open(path, "w", encoding="utf-8") as handle:
            handle.write('{"cmd": "status"}\n{not json}\n')
        with self.assertRaises(SystemExit) as caught:
            harness.load_traffic(path)
        self.assertIn(":2:", str(caught.exception.code))

    def test_a_meta_line_is_skipped_not_replayed(self):
        path = self.path("meta.jsonl")
        with open(path, "w", encoding="utf-8") as handle:
            handle.write('{"_meta": true, "note": "recorded 2026-10-30"}\n')
            handle.write('{"cmd": "status"}\n')
        self.assertEqual(["status"], [entry["cmd"] for entry in harness.load_traffic(path)])


if __name__ == "__main__":
    unittest.main()
