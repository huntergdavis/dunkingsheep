"""Keep the shared T62 HTTP response fixture grounded in the Python route."""

import importlib.util
import asyncio
import json
import os
import pathlib
import subprocess
import sys
import tempfile
import unittest
from urllib.parse import urlsplit
from unittest import mock

ROOT = pathlib.Path(__file__).resolve().parents[1]
FIXTURE = (ROOT / "rust/crates/ds-web/tests/fixtures/login_origin_refused.json")
sys.path.insert(0, str(ROOT))


def _asgi_request(app, request):
    """Dispatch through FastAPI's real router without a listener or HTTP-client extra."""
    body = json.dumps(request.get("json", {}), separators=(",", ":")).encode("utf-8")
    parsed = urlsplit(request["path"])
    path = parsed.path
    scope = {
        "type": "http",
        "asgi": {"version": "3.0", "spec_version": "2.3"},
        "http_version": "1.1",
        "method": request["method"],
        "scheme": "http",
        "path": path,
        "raw_path": path.encode("ascii"),
        "query_string": parsed.query.encode("ascii"),
        "root_path": "",
        "headers": [(name.encode("ascii"), value.encode("latin-1"))
                    for name, value in request["headers"].items()],
        "client": ("testclient", 123),
        "server": ("testserver", 80),
    }
    messages = []
    body_sent = False

    async def receive():
        nonlocal body_sent
        if not body_sent:
            body_sent = True
            return {"type": "http.request", "body": body, "more_body": False}
        return {"type": "http.disconnect"}

    async def send(message):
        messages.append(message)

    asyncio.run(app(scope, receive, send))
    start = next(message for message in messages if message["type"] == "http.response.start")
    raw = b"".join(message.get("body", b"") for message in messages
                   if message["type"] == "http.response.body")
    headers = {name.decode("ascii").lower(): value.decode("latin-1")
               for name, value in start["headers"]}
    return start["status"], headers, raw


@unittest.skipUnless(importlib.util.find_spec("fastapi"), "web.app needs fastapi")
class T62HttpParityFixtureTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory(prefix="t62-http-parity-")
        old_data_dir = os.environ.get("VP_DATA_DIR")
        os.environ["VP_DATA_DIR"] = cls.temp.name
        try:
            from web import app as desk
        finally:
            if old_data_dir is None:
                os.environ.pop("VP_DATA_DIR", None)
            else:
                os.environ["VP_DATA_DIR"] = old_data_dir
        cls.desk = desk
        cls.fixture = json.loads(FIXTURE.read_text(encoding="utf-8"))

    @classmethod
    def tearDownClass(cls):
        cls.temp.cleanup()

    def test_python_login_origin_refusal_matches_fixture(self):
        request = self.fixture["request"]
        with mock.patch.object(
                self.desk, "_sync_edited_credentials",
                side_effect=AssertionError("cross-origin rejection must precede credential access")):
            status, headers, raw = _asgi_request(self.desk.app, request)

        expected = self.fixture["response"]
        self.assertEqual(status, expected["status"])
        for name, value in expected["headers"].items():
            self.assertEqual(headers.get(name), value, name)
        self.assertEqual(raw.decode("utf-8"), expected["body"])
        self.assertEqual(json.loads(raw), expected["json"])
        self.assertEqual(headers.get("set-cookie"), expected["set_cookie"])

    def test_python_owner_capability_matches_fixture(self):
        fixture = json.loads((FIXTURE.parent / "owner_capability.json").read_text(encoding="utf-8"))

        class FakeDaemon:
            def call(self, method, **args):
                if method != fixture["backend"]["method"] or args != fixture["backend"]["args"]:
                    raise AssertionError(f"unexpected daemon call: {method} {args}")
                return fixture["backend"]["response"]

        self.desk._CAP_CACHE.pop("%9", None)
        with mock.patch.object(self.desk, "_flock", return_value=FakeDaemon()):
            status, headers, raw = _asgi_request(self.desk.app, fixture["request"])
        self.desk._CAP_CACHE.pop("%9", None)
        expected = fixture["response"]
        self.assertEqual(status, expected["status"])
        for name, value in expected["headers"].items():
            self.assertEqual(headers.get(name), value, name)
        self.assertEqual(raw.decode("utf-8"), expected["body"])
        self.assertEqual(json.loads(raw), expected["json"])
        self.assertEqual(headers.get("set-cookie"), expected["set_cookie"])


class DiffHarnessHelpTests(unittest.TestCase):
    def test_help_lists_daemon_modes_without_an_http_mode(self):
        result = subprocess.run(
            [sys.executable, str(ROOT / "tools/diff_harness.py"), "--help"],
            cwd=ROOT, check=True, capture_output=True, text=True)
        self.assertIn("{record,replay,golden}", result.stdout)
        self.assertNotIn("--http", result.stdout)


if __name__ == "__main__":
    unittest.main()
