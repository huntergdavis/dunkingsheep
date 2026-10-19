import io
import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

from dunk_api import (COMMANDS_BY_NAME, dispatch, grouped_mcp_tools,  # noqa: E402
                      mcp_commands, mcp_tool_map)
from dunk_core import Flock  # noqa: E402
from dunk_mcp import McpServer  # noqa: E402
from dunk_superusers import Superusers  # noqa: E402
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
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.superusers = Superusers(
            path=os.path.join(self.tmp.name, "superusers.json"))
        self.superusers.add("w1:p2")
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False,
                           superusers=self.superusers)
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
        self.assertIn('action="help"', result["instructions"])
        # Unknown versions fall back to our newest supported one.
        fallback = self.rpc("initialize", {"protocolVersion": "1999-01-01"})["result"]
        self.assertEqual("2025-11-25", fallback["protocolVersion"])

    def test_tools_list_is_the_grouped_surface(self):
        tools = self.rpc("tools/list")["result"]["tools"]
        names = [tool["name"] for tool in tools]
        self.assertEqual([t.name for t in grouped_mcp_tools()], names)
        self.assertEqual(9, len(names))
        # The flat names are merged, not declared; shutdown was never a tool.
        self.assertNotIn("add_dunk", names)
        self.assertNotIn("task_next", names)
        self.assertNotIn("memory_recall", names)
        self.assertNotIn("shutdown", names)
        # Every grouped tool advertises an action enum.
        for tool in tools:
            schema = tool["inputSchema"]
            self.assertEqual(["action"], schema["required"])
            self.assertIn("action", schema["properties"])
            self.assertTrue(schema["properties"]["action"]["enum"])
        dunk = next(t for t in tools if t["name"] == "dunk")
        self.assertIn("only_when", dunk["inputSchema"]["properties"])
        self.assertIn("skip_if_unconsumed", dunk["inputSchema"]["properties"])
        self.assertIn("hold_while_typing", dunk["inputSchema"]["properties"])
        pane = next(t for t in tools if t["name"] == "pane")
        self.assertIn("send", pane["inputSchema"]["properties"]["action"]["enum"])
        self.assertIn("queued", pane["description"])
        self.assertIn("{id}", dunk["inputSchema"]["properties"]["text"]["description"])
        # Flat schema types only, so strict validators (OpenAI/Codex, Gemini) accept them.
        for tool in tools:
            for prop in tool["inputSchema"]["properties"].values():
                self.assertIsInstance(prop["type"], str, msg=tool["name"])
                if prop["type"] == "array":
                    self.assertIn(prop.get("items", {}).get("type"),
                                  ("string", "number", "integer", "boolean"),
                                  msg=tool["name"])
                else:
                    self.assertIn(prop["type"],
                                  ("string", "number", "integer", "boolean"))
                self.assertNotIn(None, prop.get("enum", []))

    def test_every_flat_command_has_a_grouped_home(self):
        tool_map = mcp_tool_map()
        groups = {t.name: t for t in grouped_mcp_tools()}
        for command in mcp_commands():
            entry = tool_map[command.name]
            self.assertIsNotNone(entry["tool"], command.name)
            self.assertIsNotNone(entry["action"], command.name)
            self.assertIn(entry["tool"], groups)
            self.assertEqual(command.name, groups[entry["tool"]].resolve(entry["action"]))
        # Nothing registered is missing from the map either.
        self.assertEqual({c.name for c in COMMANDS_BY_NAME.values()},
                         set(tool_map))

    def test_hot_set_schema_bytes_under_10kb(self):
        tools = self.rpc("tools/list")["result"]["tools"]
        hot = [t for t in tools if t["name"] in ("inbox", "task", "memory")]
        size = len(json.dumps([{"name": t["name"], "inputSchema": t["inputSchema"]}
                               for t in hot]))
        self.assertLess(size, 10 * 1024, f"hot set is {size} bytes")

    def test_help_action_returns_guide_and_registry(self):
        response = self.rpc("tools/call", {"name": "system",
                                           "arguments": {"action": "help"}})
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
        response = self.rpc("tools/call", {"name": "metrics",
                                           "arguments": {"action": "status"}})
        self.assertNotIn("structuredContent", response["result"])
        self.assertEqual("text", response["result"]["content"][0]["type"])
        self.rpc("initialize", {"protocolVersion": "2025-11-25"})
        response = self.rpc("tools/call", {"name": "metrics",
                                           "arguments": {"action": "status"}})
        self.assertIn("structuredContent", response["result"])

    def test_grouped_tool_call_round_trip(self):
        response = self.rpc("tools/call", {
            "name": "dunk",
            "arguments": {"action": "add", "target": "self",
                          "text": "check backlog, remove {id} when done",
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

        fired = self.rpc("tools/call", {"name": "dunk",
                                        "arguments": {"action": "fire", "id": "d1"}})
        self.assertTrue(fired["result"]["structuredContent"]["ok"])
        self.assertEqual([("w1:p2", "check backlog, remove d1 when done")], self.herdr.sends)

        removed = self.rpc("tools/call", {"name": "dunk",
                                          "arguments": {"action": "remove", "id": "d1"}})
        self.assertFalse(removed["result"]["isError"])
        listed = self.rpc("tools/call", {"name": "dunk", "arguments": {"action": "list"}})
        self.assertEqual([], listed["result"]["structuredContent"]["dunks"])

    def test_flat_names_still_callable(self):
        """The merge changed the surface, not the capabilities."""
        response = self.rpc("tools/call", {
            "name": "add_dunk",
            "arguments": {"target": "self", "text": "flat compat",
                          "interval_minutes": 5},
        })
        self.assertFalse(response["result"]["isError"])
        self.assertEqual("d1", response["result"]["structuredContent"]["id"])
        listed = self.rpc("tools/call", {"name": "list_dunks", "arguments": {}})
        self.assertEqual(["d1"], [d["id"] for d in listed["result"]["structuredContent"]["dunks"]])

    def test_errors_are_is_error_results_not_protocol_errors(self):
        # Bad action.
        bad = self.rpc("tools/call", {"name": "dunk", "arguments": {"action": "nope"}})
        self.assertTrue(bad["result"]["isError"])
        self.assertIn("unknown action", bad["result"]["content"][0]["text"])
        # Missing per-action required argument.
        missing = self.rpc("tools/call", {"name": "dunk", "arguments": {"action": "get"}})
        self.assertTrue(missing["result"]["isError"])
        self.assertIn("missing required", missing["result"]["content"][0]["text"])
        # A valid action on a missing record is an isError result, not an RPC error.
        response = self.rpc("tools/call", {"name": "dunk",
                                           "arguments": {"action": "get", "id": "d42"}})
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
            b'{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"metrics","arguments":{"action":"status"}}}\n'
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
