#!/usr/bin/env python3
"""Stdlib-only poll client for the Dunking Sheep daemon.

The sidebar used to start the full CLI three times per poll (`status`,
`inbox count`, `list`). That imports the whole store stack, the MCP surface and
herdr discovery each time; the poll client is one tiny socket round trip to a
daemon that already has everything open.

    dunk-poll                # JSON: status + inbox + unread messages + dunks
    dunk-poll --panes         # also the pane list (startup only; slow)
    dunk-poll --text          # human-readable summary

No `dunk_*` modules are imported: this stays fast and independent of the
daemon's implementation. Pane identity comes from the environment
(HERDR_PANE_ID or TMUX_PANE); it never asks the terminal for `pane current`.
"""

import argparse
import json
import os
import socket
import sys

DEFAULT_SOCKET = os.path.join(
    os.environ.get("DUNKINGSHEEP_DIR") or os.path.expanduser("~/.config/dunkingsheep"),
    "dunkingsheep.sock",
)


def request(payload, socket_path, timeout=10.0):
    """Send one newline-delimited JSON request and read one response line."""
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as sock:
        sock.settimeout(timeout)
        sock.connect(socket_path)
        sock.sendall((json.dumps(payload) + "\n").encode("utf-8"))
        chunks = []
        while True:
            data = sock.recv(65536)
            if not data:
                break
            chunks.append(data)
            if data.endswith(b"\n"):
                break
    raw = b"".join(chunks).strip()
    if not raw:
        raise ValueError("daemon closed the connection without a response")
    return json.loads(raw.decode("utf-8"))


def poll(socket_path=None, self_pane=None, timeout=10.0, **options):
    socket_path = socket_path or os.environ.get("DUNKINGSHEEP_SOCKET") or DEFAULT_SOCKET
    self_pane = self_pane or os.environ.get("HERDR_PANE_ID") or os.environ.get("TMUX_PANE")
    payload = {"id": 1, "cmd": "poll", "args": options,
               "self_pane_id": self_pane}
    response = request(payload, os.path.expanduser(socket_path), timeout=float(timeout))
    if not response.get("ok"):
        raise RuntimeError(response.get("error") or "poll failed")
    return response.get("result") or {}


def render_text(result):
    status = result.get("status") or {}
    count = result.get("inbox_count") or {}
    lines = [
        "self %s  v%s  dunks %s/%s running  inbox %s unread / %s" % (
            status.get("self_pane_id") or "?", status.get("version"),
            status.get("running", 0), status.get("dunkers", 0),
            count.get("unread", 0), count.get("total", 0)),
    ]
    if result.get("error"):
        lines.append("  note: " + str(result["error"]))
    for message in (result.get("dunks") or {}).get("messages", [])[:5]:
        lines.append("  #%s %s: %s" % (
            message.get("message_id"), message.get("sender"),
            message.get("subject") or "(no subject)"))
    for dunk in (result.get("dunks") or {}).get("dunks", [])[:10]:
        lines.append("  %s %s every %sm -> %s" % (
            dunk.get("id"), "running" if dunk.get("running") else "stopped",
            dunk.get("interval_minutes"),
            dunk.get("target_label") or dunk.get("target_pane_id")))
    return "\n".join(lines)


def main(argv=None):
    parser = argparse.ArgumentParser(
        prog="dunk-poll",
        description="One batched read from the Dunking Sheep daemon (stdlib only).")
    parser.add_argument("--socket", help="daemon socket (default DUNKINGSHEEP_SOCKET)")
    parser.add_argument("--self-pane", help="caller pane (default HERDR_PANE_ID/TMUX_PANE)")
    parser.add_argument("--mailbox", help="mailbox to poll (default: the caller's)")
    parser.add_argument("--limit", type=int, default=5,
                        help="unread messages to return (default 5)")
    parser.add_argument("--panes", action="store_true",
                        help="include the pane list (slow; startup only)")
    parser.add_argument("--tasks", action="store_true",
                        help="include the caller's task queue")
    parser.add_argument("--task-scope", choices=["global", "self"])
    parser.add_argument("--include-done", action="store_true")
    parser.add_argument("--check-herdr", action="store_true",
                        help="probe the terminal backend (slow; default off)")
    parser.add_argument("--timeout", type=float, default=10.0)
    parser.add_argument("--text", action="store_true", help="human-readable output")
    parser.add_argument("--compact", action="store_true",
                        help="single-line JSON (default: indented)")
    args = parser.parse_args(argv)
    options = {"limit": args.limit, "panes": args.panes, "tasks": args.tasks,
               "include_done": args.include_done,
               "check_herdr": args.check_herdr}
    if args.mailbox:
        options["mailbox"] = args.mailbox
    if args.task_scope:
        options["task_scope"] = args.task_scope
    socket_path = args.socket or os.environ.get("DUNKINGSHEEP_SOCKET") or DEFAULT_SOCKET
    try:
        result = poll(socket_path, self_pane=args.self_pane, timeout=args.timeout,
                      **options)
    except FileNotFoundError:
        print("dunk-poll: daemon socket not found at %s; is the daemon running?"
              % os.path.expanduser(socket_path), file=sys.stderr)
        return 2
    except (OSError, ValueError, RuntimeError) as error:
        print("dunk-poll: %s" % error, file=sys.stderr)
        return 1
    if args.text:
        print(render_text(result))
    elif args.compact:
        print(json.dumps(result, separators=(",", ":"), sort_keys=True))
    else:
        print(json.dumps(result, indent=2, sort_keys=True))
    return 0


if __name__ == "__main__":
    sys.exit(main())
