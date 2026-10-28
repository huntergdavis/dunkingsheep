"""Read-only HTTP surface for the Command Center.

The UI needs four shapes (roster, message state, reply stats, project summary) and
it needs them from `curl` as well as from a browser. This server is a THIN read-only
facade over the daemon socket: no writes, no model, no state of its own, 127.0.0.1
only. Every response is JSON; an object response carries `as_of`, /api/terminals is a
bare array because that is its DS3 contract, and a field that cannot be computed is
null WITH a reason, never a zero.

    dunkingsheep api [--port 8792] [--host 127.0.0.1]

Routes:
    GET /healthz
    GET /api/roster
    GET /api/messages/state?ids=i1,i2
    GET /api/reply-stats[?mailboxes=a,b]
    GET /api/projects/<name>            (name 'global' for the global store)
    GET /api/terminals                  (tmux terminals: name/agent/state/cols/rows)
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


def _unique_names(panes):
    """Give every pane a name a caller can address it by: the agent label, made unique.

    The name a lane is addressed by is the registry label recorded at start_agent
    (falling back to the pane title, then the pane id). That label is NOT unique on
    this host: two panes carry 'opencode-ds41-b' because the label was recorded
    twice. A name that two panes answer to is not an address, so the second and
    later holders get '-2', '-3', ... assigned in the stable order list_panes_grouped
    returns (session, then window) - the same pane gets the same suffix on every call.
    """
    used = set()
    seen = {}
    named = []
    for pane in panes:
        base = (pane.get("label") or pane.get("terminal_title_stripped")
                or pane.get("pane_id") or "terminal")
        base = str(base).strip() or "terminal"
        candidate = base
        while candidate in used:
            seen[base] = seen.get(base, 1) + 1
            candidate = f"{base}-{seen[base]}"
        used.add(candidate)
        named.append((candidate, pane))
    return named


def _lane_states():
    """Pane status from the daemon, plus the reason when it cannot be had.

    A state that cannot be computed is null WITH its reason (the house rule), and a
    daemon outage downgrades one field instead of failing the whole list: the
    terminal grid is still true when the status call is not.
    """
    try:
        payload = _call("list_panes")
    except Exception as error:                     # noqa: BLE001 - a null field, not an outage
        return {}, f"daemon call failed: {error}"
    panes = payload.get("panes") if isinstance(payload, dict) else None
    return ({p.get("pane_id"): p for p in panes or [] if p.get("pane_id")}, None)


def terminals():
    """The tmux terminal list, read from the backend this host actually runs.

    Shape is the DS3 contract: a top-level array of {name, agent, state, cols, rows}.
    `pane_id` rides along so a caller can name a pane when a label collided, and
    `state_reason` exists only when `state` is null.
    """
    from dunk_terminal import get_backend
    panes = get_backend().list_panes_grouped()
    states, reason = _lane_states()
    out = []
    for name, pane in _unique_names(panes):
        status = states.get(pane.get("pane_id")) or {}
        state = status.get("agent_status")
        item = {
            "name": name,
            "agent": pane.get("agent") or None,
            "state": state,
            "cols": pane.get("cols"),
            "rows": pane.get("rows"),
            "pane_id": pane.get("pane_id"),
        }
        if state is None:
            item["state_reason"] = (status.get("agent_status_reason") or reason
                                    or "no activity source for this pane")
        out.append(item)
    return out


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
    if path == "/api/terminals":
        return terminals()
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
