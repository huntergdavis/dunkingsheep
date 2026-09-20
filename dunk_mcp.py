#!/usr/bin/env python3
"""
Dunking Sheep MCP server (stdio transport, standard library only).

Speaks JSON-RPC 2.0 over newline-delimited stdin/stdout, as the Model Context
Protocol's stdio transport requires. Every command in `dunk_api` marked
`mcp=True` becomes a tool; calls are forwarded to the Dunking Sheep daemon
over its unix socket (starting the daemon if needed).

Register with Claude Code:

    claude mcp add --scope user dunkingsheep -- python3 /path/to/dunkingsheep mcp

Because Claude Code spawns this process inside its own herdr pane, it inherits
HERDR_PANE_ID, so the agent can target "self" to dunk on itself.
"""

import json
import logging
import os
import sys

from dunk_api import mcp_commands
from dunk_client import DaemonUnavailable, FlockClient
from dunk_core import VERSION, DunkError

SUPPORTED_PROTOCOLS = ("2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05")
# structuredContent was introduced in 2025-06-18; older clients get text only.
STRUCTURED_CONTENT_SINCE = "2025-06-18"

INSTRUCTIONS = """\
Dunking Sheep sends text (plus Enter) into herdr terminal panes on a schedule.
Each schedule is a "dunk": target pane, text, interval, running flag. Dunks
live in a shared daemon, so what you create here also shows in the human's TUI
and CLI. Call the `help` tool for the full guide.

Typical uses: keep another agent moving (add_dunk target=<its pane>), message
an agent right now (send_text), inspect what an agent is doing (read_pane), or
schedule a future nudge to yourself (target "self").

Nothing ever types over a human. If someone is composing in the target pane,
both dunks and direct messages wait until the input box has been clear for a
second; send_text then returns queued=true with a message_id, and the daemon
delivers it (in order) when they finish. list_messages, get_message and
cancel_message manage that queue.

Building the herd: create_workspace / create_tab / split_pane open named
spaces, tabs and panes (optionally running a command such as "claude" in the
new pane); start_agent launches an agent registered with herdr by name; rename,
focus and close manage them (close refuses your own pane/tab/workspace). The
`herdr` tool runs any other herdr subcommand ("pane zoom self --on",
"notification show Done", "api schema --json") and `herdr_help` returns herdr's
own usage so you can discover what it offers.

Meta-dunking recipe: add_dunk(target="self", interval_minutes=60,
only_when="idle", skip_if_unconsumed=true, text="Backlog check #{count}: if
every backlog item is done, call remove_dunk('{id}') and stop; otherwise
implement the next item."). {id} expands to the dunk's own id at send time, so
the future you knows exactly which dunk to remove. only_when="idle" waits until
you are not mid-task; skip_if_unconsumed skips a send while the previous one is
still unread in the pane (prevents prompts piling up in an unattended session);
max_sends=1 makes a one-off reminder. Every dunk also holds, by default, while
a human is mid-sentence in the target's input box (hold_while_typing), so a
scheduled send never splices itself into what they are typing.
"""


log = logging.getLogger("dunkingsheep.mcp")


class McpServer:
    def __init__(self, client_factory=None, stdin=None, stdout=None):
        self._client_factory = client_factory or self._default_client
        self.stdin = stdin or sys.stdin.buffer
        self.stdout = stdout or sys.stdout.buffer
        self.tools = mcp_commands()
        self.initialized = False
        self.protocol_version = SUPPORTED_PROTOCOLS[0]

    @staticmethod
    def _default_client():
        return FlockClient.connect_or_start()

    # -- transport ------------------------------------------------------------

    def serve_forever(self):
        try:
            self._serve_loop()
        except (BrokenPipeError, KeyboardInterrupt):
            pass  # the harness went away; exit quietly

    def _serve_loop(self):
        for raw in self.stdin:
            line = raw.decode("utf-8", "replace").strip()
            if not line:
                continue
            try:
                message = json.loads(line)
            except ValueError:
                self._write({"jsonrpc": "2.0", "id": None,
                             "error": {"code": -32700, "message": "parse error"}})
                continue
            messages = message if isinstance(message, list) else [message]
            for item in messages:
                response = self.handle(item)
                if response is not None:
                    self._write(response)

    def _write(self, payload):
        self.stdout.write((json.dumps(payload) + "\n").encode("utf-8"))
        self.stdout.flush()

    # -- dispatch ---------------------------------------------------------------

    def handle(self, message):
        """Return a response dict, or None for notifications."""
        if not isinstance(message, dict):
            return self._error(None, -32600, "invalid request")
        msg_id = message.get("id")
        method = message.get("method")
        params = message.get("params") or {}
        is_notification = "id" not in message

        if method is None:
            # A response to something we sent (we never send requests) - ignore.
            return None
        try:
            if method == "initialize":
                result = self.initialize(params)
            elif method == "notifications/initialized":
                self.initialized = True
                return None
            elif method.startswith("notifications/"):
                return None
            elif method == "ping":
                result = {}
            elif method == "tools/list":
                result = {"tools": self.list_tools()}
            elif method == "tools/call":
                result = self.call_tool(params.get("name"), params.get("arguments") or {})
            elif method == "resources/list":
                result = {"resources": []}
            elif method == "resources/templates/list":
                result = {"resourceTemplates": []}
            elif method == "prompts/list":
                result = {"prompts": []}
            else:
                if is_notification:
                    return None
                return self._error(msg_id, -32601, f"method not found: {method}")
        except Exception as error:  # noqa: BLE001 - surface as JSON-RPC error
            log.exception("handler for %s failed", method)
            if is_notification:
                return None
            return self._error(msg_id, -32603, f"internal error: {error}")
        if is_notification:
            return None
        return {"jsonrpc": "2.0", "id": msg_id, "result": result}

    @staticmethod
    def _error(msg_id, code, message):
        return {"jsonrpc": "2.0", "id": msg_id, "error": {"code": code, "message": message}}

    # -- methods ------------------------------------------------------------------

    def initialize(self, params):
        requested = params.get("protocolVersion")
        version = requested if requested in SUPPORTED_PROTOCOLS else SUPPORTED_PROTOCOLS[0]
        self.protocol_version = version
        return {
            "protocolVersion": version,
            "capabilities": {"tools": {"listChanged": False}},
            "serverInfo": {"name": "dunkingsheep", "version": VERSION},
            "instructions": INSTRUCTIONS,
        }

    def list_tools(self):
        return [
            {
                "name": command.name,
                "description": command.description,
                "inputSchema": command.input_schema(),
            }
            for command in self.tools
        ]

    def call_tool(self, name, arguments):
        command = next((c for c in self.tools if c.name == name), None)
        if command is None:
            return self._tool_error(f"unknown tool {name!r}")
        if not isinstance(arguments, dict):
            return self._tool_error("arguments must be an object")
        try:
            client = self._client_factory()
            result = client.call(name, **arguments)
        except DunkError as error:
            return self._tool_error(str(error))
        except (DaemonUnavailable, OSError) as error:
            return self._tool_error(f"dunkingsheep daemon unavailable: {error}")
        text = json.dumps(result, indent=2) if not isinstance(result, str) else result
        payload = {"content": [{"type": "text", "text": text}], "isError": False}
        if isinstance(result, dict) and self.protocol_version >= STRUCTURED_CONTENT_SINCE:
            payload["structuredContent"] = result
        return payload

    @staticmethod
    def _tool_error(message):
        return {"content": [{"type": "text", "text": message}], "isError": True}


def main():
    logging.basicConfig(
        level=logging.INFO,
        stream=sys.stderr,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )
    log.info("dunkingsheep MCP server %s starting (self pane %s)",
             VERSION, os.environ.get("HERDR_PANE_ID"))
    McpServer().serve_forever()


if __name__ == "__main__":
    main()
