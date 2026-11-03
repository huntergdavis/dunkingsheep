"""Shared Python/Rust fixture for the T62 operations board deferral route."""

import json
import os
import sys
import tempfile
import unittest
import asyncio
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
FIXTURE = ROOT / "rust/crates/ds-web/tests/fixtures/ops_board_not_now.json"
sys.path.insert(0, str(ROOT))
os.environ.setdefault("VP_DATA_DIR", tempfile.mkdtemp(prefix="ops-board-http-test-"))


class OperationsBoardHttpParity(unittest.TestCase):
    def test_python_not_now_route_matches_shared_fixture(self):
        from web import app as desk
        import ops_board

        fixture = json.loads(FIXTURE.read_text(encoding="utf-8"))
        request = fixture["request"]
        expected = fixture["response"]
        calls = []

        class FakeClient:
            def call(self, name, **args):
                calls.append((name, args))
                self.assert_task_update(name, args)
                return {"id": args["id"], "priority": "low"}

            @staticmethod
            def assert_task_update(name, args):
                if name != "task_update" or args != {"id": "t9", "priority": "low"}:
                    raise AssertionError((name, args))

        saved_client = ops_board._client
        ops_board._client = FakeClient
        desk.app.dependency_overrides[desk.require_auth] = lambda: "owner"
        try:
            async def dispatch():
                body = json.dumps(request["json"], separators=(",", ":")).encode()
                headers = [(name.lower().encode(), value.encode())
                           for name, value in request["headers"].items()]
                headers.append((b"content-length", str(len(body)).encode()))
                scope = {
                    "type": "http", "asgi": {"version": "3.0", "spec_version": "2.3"},
                    "http_version": "1.1", "method": request["method"],
                    "scheme": "http", "path": request["path"],
                    "raw_path": request["path"].encode(), "query_string": b"",
                    "root_path": "", "headers": headers,
                    "server": ("testserver", 80), "client": ("127.0.0.1", 4000),
                    "state": {}, "extensions": {},
                }
                sent = []
                received = False

                async def receive():
                    nonlocal received
                    if not received:
                        received = True
                        return {"type": "http.request", "body": body, "more_body": False}
                    return {"type": "http.disconnect"}

                async def send(message):
                    sent.append(message)

                await desk.app(scope, receive, send)
                start = next(message for message in sent if message["type"] == "http.response.start")
                chunks = [message.get("body", b"") for message in sent
                          if message["type"] == "http.response.body"]
                return start, b"".join(chunks)

            response_start, response_body = asyncio.run(dispatch())
        finally:
            ops_board._client = saved_client
            desk.app.dependency_overrides.clear()

        self.assertEqual(expected["status"], response_start["status"])
        response_headers = {name.decode(): value.decode()
                            for name, value in response_start["headers"]}
        for name, value in expected["headers"].items():
            self.assertEqual(value, response_headers.get(name))
        self.assertEqual(expected["set_cookie"], response_headers.get("set-cookie"))
        self.assertEqual(expected["body"], response_body.decode())
        self.assertEqual(expected["json"], json.loads(response_body))
        self.assertEqual([("task_update", {"id": "t9", "priority": "low"})], calls)


if __name__ == "__main__":
    unittest.main()
