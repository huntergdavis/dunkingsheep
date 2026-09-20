import io
import json
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

from dunk_api import dispatch, mcp_commands  # noqa: E402
from dunk_core import Flock  # noqa: E402
from dunk_mcp import McpServer  # noqa: E402
from fakes import FakeHerdr  # noqa: E402


class InProcessClient:
    """Mimics FlockClient.call() against a local Flock."""

    def __init__(self, flock, self_pane_id="w1:p2"):
        self.flock = flock
        self.self_pane_id = self_pane_id

    def call(self, cmd, **args):
        return dispatch(self.flock, cmd, args, self_pane_id=self.self_pane_id)


class McpServerTests(unittest.TestCase):
    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)
        self.client = InProcessClient(self.flock)
        self.server = McpServer(client_factory=lambda: self.client)

    def tearDown(self):
        self.flock.close()

    def rpc(self, method, params=None, msg_id=1):
        message = {"jsonrpc": "2.0", "id": msg_id, "method": method}
        if params is not None:
            message["params"] = params
        return self.server.handle(message)

    def test_initialize_negotiates_protocol_and_advertises_tools(self):
        result = self.rpc("initialize", {"protocolVersion": "2025-03-26",
                                         "capabilities": {}, "clientInfo": {"name": "t"}})["result"]
        self.assertEqual("2025-03-26", result["protocolVersion"])
        self.assertIn("tools", result["capabilities"])
        self.assertEqual("dunkingsheep", result["serverInfo"]["name"])
        self.assertIn("remove_dunk", result["instructions"])
        # Unknown versions fall back to our newest supported one.
        fallback = self.rpc("initialize", {"protocolVersion": "1999-01-01"})["result"]
        self.assertEqual("2025-11-25", fallback["protocolVersion"])

    def test_tools_list_matches_registry_and_has_schemas(self):
        tools = self.rpc("tools/list")["result"]["tools"]
        names = {tool["name"] for tool in tools}
        self.assertEqual({c.name for c in mcp_commands()}, names)
        self.assertNotIn("shutdown", names)
        add = next(t for t in tools if t["name"] == "add_dunk")
        self.assertEqual(["target", "text", "interval_minutes"], add["inputSchema"]["required"])
        self.assertIn("only_when", add["inputSchema"]["properties"])
        self.assertEqual("boolean", add["inputSchema"]["properties"]["skip_if_unconsumed"]["type"])
        update = next(t for t in tools if t["name"] == "update_dunk")
        self.assertIn("skip_if_unconsumed", update["inputSchema"]["properties"])
        self.assertEqual("boolean", add["inputSchema"]["properties"]["hold_while_typing"]["type"])
        self.assertIn("hold_while_typing", update["inputSchema"]["properties"])
        self.assertIn("list_messages", names)
        self.assertIn("cancel_message", names)
        send = next(t for t in tools if t["name"] == "send_text")
        self.assertIn("hold_while_typing", send["inputSchema"]["properties"])
        self.assertIn("queued", send["description"])
        self.assertIn("{id}", add["inputSchema"]["properties"]["text"]["description"])
        self.assertIn("help", names)
        # Flat schema types only, so strict validators (OpenAI/Codex, Gemini) accept them.
        for tool in tools:
            for prop in tool["inputSchema"]["properties"].values():
                self.assertIsInstance(prop["type"], str, msg=tool["name"])
                self.assertIn(prop["type"], ("string", "number", "integer", "boolean"))
                self.assertNotIn(None, prop.get("enum", []))

    def test_help_tool_returns_guide_and_registry(self):
        response = self.rpc("tools/call", {"name": "help", "arguments": {}})
        result = response["result"]["structuredContent"]
        self.assertIn("Meta-dunking", result["guide"].replace("meta-dunking", "Meta-dunking"))
        self.assertIn("newline-delimited JSON", result["protocol"]["transport"])
        names = [c["name"] for c in result["commands"]]
        self.assertIn("add_dunk", names)
        self.assertIn("shutdown", names)
        shutdown = next(c for c in result["commands"] if c["name"] == "shutdown")
        self.assertFalse(shutdown["mcp_tool"])

    def test_old_protocol_clients_do_not_get_structured_content(self):
        self.rpc("initialize", {"protocolVersion": "2024-11-05"})
        response = self.rpc("tools/call", {"name": "status", "arguments": {}})
        self.assertNotIn("structuredContent", response["result"])
        self.assertEqual("text", response["result"]["content"][0]["type"])
        self.rpc("initialize", {"protocolVersion": "2025-11-25"})
        response = self.rpc("tools/call", {"name": "status", "arguments": {}})
        self.assertIn("structuredContent", response["result"])

    def test_tool_call_round_trip(self):
        response = self.rpc("tools/call", {
            "name": "add_dunk",
            "arguments": {"target": "self", "text": "check backlog, remove {id} when done",
                          "interval_minutes": 60, "only_when": "idle",
                          "skip_if_unconsumed": True},
        })
        result = response["result"]
        self.assertFalse(result["isError"])
        self.assertEqual("d1", result["structuredContent"]["id"])
        self.assertTrue(result["structuredContent"]["skip_if_unconsumed"])
        self.assertEqual("w1:p2", result["structuredContent"]["target_pane_id"])
        self.assertEqual("idle", result["structuredContent"]["only_when"])
        body = json.loads(result["content"][0]["text"])
        self.assertEqual("d1", body["id"])

        fired = self.rpc("tools/call", {"name": "fire_dunk", "arguments": {"id": "d1"}})
        self.assertTrue(fired["result"]["structuredContent"]["ok"])
        self.assertEqual([("w1:p2", "check backlog, remove d1 when done")], self.herdr.sends)

        removed = self.rpc("tools/call", {"name": "remove_dunk", "arguments": {"id": "d1"}})
        self.assertFalse(removed["result"]["isError"])
        listed = self.rpc("tools/call", {"name": "list_dunks", "arguments": {}})
        self.assertEqual([], listed["result"]["structuredContent"]["dunks"])

    def test_tool_errors_are_is_error_results_not_protocol_errors(self):
        response = self.rpc("tools/call", {"name": "get_dunk", "arguments": {"id": "d42"}})
        self.assertIn("result", response)
        self.assertTrue(response["result"]["isError"])
        self.assertIn("no dunk with id", response["result"]["content"][0]["text"])
        unknown = self.rpc("tools/call", {"name": "nope", "arguments": {}})
        self.assertTrue(unknown["result"]["isError"])

    def test_unknown_method_and_notifications(self):
        response = self.rpc("bogus/method")
        self.assertEqual(-32601, response["error"]["code"])
        self.assertIsNone(self.server.handle(
            {"jsonrpc": "2.0", "method": "notifications/initialized"}))
        self.assertTrue(self.server.initialized)
        self.assertEqual({}, self.rpc("ping")["result"])
        self.assertEqual([], self.rpc("prompts/list")["result"]["prompts"])
        self.assertEqual([], self.rpc("resources/list")["result"]["resources"])

    def test_serve_forever_speaks_newline_delimited_json(self):
        stdin = io.BytesIO(
            b'{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}\n'
            b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n'
            b'\n'
            b'garbage\n'
            b'{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"status","arguments":{}}}\n'
        )
        stdout = io.BytesIO()
        server = McpServer(client_factory=lambda: self.client, stdin=stdin, stdout=stdout)
        server.serve_forever()
        lines = [json.loads(line) for line in stdout.getvalue().decode().splitlines()]
        self.assertEqual(3, len(lines))
        self.assertEqual(1, lines[0]["id"])
        self.assertEqual(-32700, lines[1]["error"]["code"])
        self.assertEqual(2, lines[2]["id"])
        self.assertEqual(0, lines[2]["result"]["structuredContent"]["dunkers"])


if __name__ == "__main__":
    unittest.main()
