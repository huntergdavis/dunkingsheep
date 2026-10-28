"""Run the OpenCode producer's Node test from the Python unittest discovery.

The producer is JavaScript, but the repository's test entry point is
`python3 -m unittest discover -s tests`; this wrapper keeps the Node suite in
that path instead of relying on someone remembering a second command.
"""
import shutil
import subprocess
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SUITE = ROOT / "tests" / "test_opencode_turn_generation.mjs"


class OpenCodeTurnGenerationTests(unittest.TestCase):
    @unittest.skipUnless(shutil.which("node"), "node required for the OpenCode producer test")
    def test_plugin_contract(self):
        run = subprocess.run(["node", "--test", str(SUITE)],
                             capture_output=True, text=True, timeout=120)
        self.assertEqual(0, run.returncode, run.stdout + run.stderr)
        self.assertIn("# fail 0", run.stdout)


if __name__ == "__main__":
    unittest.main()
