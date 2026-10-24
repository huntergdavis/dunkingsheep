"""Owner #913: Dunking Sheep's runtime must not depend on deja."""

import os
import subprocess
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _bootstrap  # noqa: F401,E402  (isolate live state)

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
AUDIT = os.path.join(ROOT, "tools", "check_deja_free.py")


class DejaFreeTests(unittest.TestCase):
    def test_runtime_has_no_active_deja_reference(self):
        result = subprocess.run(
            [sys.executable, AUDIT, "--repo-only", "--json"],
            capture_output=True, text=True, timeout=60)
        self.assertEqual(0, result.returncode, result.stdout + result.stderr)
        import json
        report = json.loads(result.stdout)
        self.assertTrue(report["ok"], report["problems"])
        self.assertEqual([], report["problems"])

    def test_help_and_mcp_instructions_say_retired(self):
        from dunk_help import guide
        from dunk_mcp import INSTRUCTIONS
        self.assertNotIn("deja", guide().lower())
        self.assertNotIn("deja", INSTRUCTIONS.lower())


if __name__ == "__main__":
    unittest.main()
