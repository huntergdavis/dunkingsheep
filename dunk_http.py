"""Read-only HTTP surface for the Command Center.

The UI needs four shapes (roster, message state, reply stats, project summary) and
it needs them from `curl` as well as from a browser. This server is a THIN read-only
facade over the daemon socket: no writes, no model, no state of its own, 127.0.0.1
only. Every response is JSON and carries `as_of`; a field that cannot be computed is
null WITH a reason, never a zero.

    dunkingsheep api [--port 8792] [--host 127.0.0.1]

Routes:
    GET /healthz
    GET /api/roster
    GET /api/messages/state?ids=i1,i2
    GET /api/reply-stats[?mailboxes=a,b]
    GET /api/projects/<name>            (name 'global' for the global store)
"""
import argparse
import json
import os
import socket
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

DEFAULT_PORT = int(os.environ.get("DUNKINGSHEEP_API_PORT", "8792"))
DEFAULT_HOST = os.environ.get("DUNKINGSHEEP_API_HOST", "127.0.0.1")


class ApiError(Exception):
    def __init__(self, status, message):
        super().__init__(message)
        self.status = status


def _client():
    from dunk_client import DaemonUnavailable, FlockClient
    try:
        return FlockClient.connect_or_start()
    except DaemonUnavailable as error:
        raise ApiError(503, f"Dunking Sheep unavailable: {error}")


def _call(command, **args):
    return _client().call(command, **args)


def route(path, query):
    if path in ("/healthz", "/health"):
        return {"ok": True, "service": "dunkingsheep-api"}
    if path == "/api/roster":
        return _call("roster")
    if path == "/api/messages/state":
        ids = (query.get("ids") or [""])[0]
        if not ids.strip():
            raise ApiError(400, "ids is required, e.g. ?ids=i1,i2")
        return _call("message_state", message_ids=ids)
    if path == "/api/reply-stats":
        boxes = (query.get("mailboxes") or [None])[0]
        return _call("reply_stats", **({"mailboxes": boxes} if boxes else {}))
    if path.startswith("/api/projects/"):
        name = path[len("/api/projects/"):].strip("/")
        if not name:
            raise ApiError(404, "project name required")
        return _call("project_summary", project=name)
    raise ApiError(404, f"no route {path}")


class Handler(BaseHTTPRequestHandler):
    server_version = "dunkingsheep-api/1"

    def do_GET(self):                                   # noqa: N802 - http.server API
        parsed = urlparse(self.path)
        try:
            payload = route(parsed.path, parse_qs(parsed.query))
            status = 200
        except ApiError as error:
            payload, status = {"error": str(error)}, error.status
        except (socket.timeout, OSError) as error:
            payload, status = {"error": f"daemon call failed: {error}"}, 503
        except Exception as error:                      # noqa: BLE001 - answer, never crash
            payload, status = {"error": f"internal error: {error}"}, 500
        body = json.dumps(payload, indent=2).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):                  # quiet by default
        if os.environ.get("DUNKINGSHEEP_API_VERBOSE") == "1":
            super().log_message(fmt, *args)


def serve(host=DEFAULT_HOST, port=DEFAULT_PORT):
    server = ThreadingHTTPServer((host, port), Handler)
    print(f"dunkingsheep api listening on http://{host}:{port}", flush=True)
    server.serve_forever()


def main(argv=None):
    parser = argparse.ArgumentParser(description="Read-only HTTP surface for the Command Center.")
    parser.add_argument("--host", default=DEFAULT_HOST)
    parser.add_argument("--port", type=int, default=DEFAULT_PORT)
    args = parser.parse_args(argv)
    serve(args.host, args.port)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
