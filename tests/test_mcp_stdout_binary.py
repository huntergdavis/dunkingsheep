"""`dunkingsheep mcp` must start through the real entry point.

_run() used to capture stdout for every command, which left the MCP server
with a StringIO that has no .buffer: CONNECTION_CLOSED for every client.
"""
import json
import os
import subprocess
import sys
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


class McpEntryPointTest(unittest.TestCase):
    def test_initialize_roundtrip_through_cli(self):
        request = json.dumps({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                       "clientInfo": {"name": "t", "version": "0"}}}) + "\n"
        proc = subprocess.run(
            [sys.executable, os.path.join(ROOT, "dunkingsheep"), "mcp"],
            input=request.encode(), capture_output=True, timeout=20)
        self.assertNotIn(b"AttributeError", proc.stderr)
        reply = json.loads(proc.stdout.decode().splitlines()[0])
        self.assertEqual(reply["result"]["serverInfo"]["name"], "dunkingsheep")


if __name__ == "__main__":
    unittest.main()
