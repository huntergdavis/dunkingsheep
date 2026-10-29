"""Installer tests: preserve unrelated hooks and producers' trusted files."""
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CODEX_INSTALL = ROOT / "integrations" / "codex" / "install.py"
OPENCODE_INSTALL = ROOT / "integrations" / "opencode" / "install.py"
CLAUDE_INSTALL = ROOT / "integrations" / "claude" / "install.py"


def run(*args):
    return subprocess.run([sys.executable, *map(str, args)],
                          capture_output=True, text=True, timeout=30)


class CodexInstallerTests(unittest.TestCase):
    def test_fresh_home_gets_files_and_hooks_then_is_idempotent(self):
        with tempfile.TemporaryDirectory() as home:
            first = run(CODEX_INSTALL, "--home", home)
            self.assertEqual(0, first.returncode, first.stderr)
            hooks = json.loads((Path(home) / "hooks.json").read_text())
            self.assertIn("UserPromptSubmit", hooks["hooks"])
            self.assertIn("Stop", hooks["hooks"])
            self.assertTrue(hooks["hooks"]["UserPromptSubmit"][0]["hooks"][0]
                            ["command"].endswith(" start"))
            self.assertTrue(hooks["hooks"]["Stop"][0]["hooks"][0]
                            ["command"].endswith(" end"))
            self.assertTrue((Path(home) / "dunkingsheep-turn.py").is_file())
            self.assertTrue((Path(home) / "dunkingsheep-turn.sh").is_file())
            before = (Path(home) / "hooks.json").read_bytes()
            second = run(CODEX_INSTALL, "--home", home)
            self.assertEqual(0, second.returncode, second.stderr)
            self.assertIn("left unchanged", second.stdout)
            self.assertEqual(before, (Path(home) / "hooks.json").read_bytes())
            check = run(CODEX_INSTALL, "--home", home, "--check")
            self.assertEqual(0, check.returncode, check.stdout + check.stderr)

    def test_unrelated_hooks_are_preserved(self):
        with tempfile.TemporaryDirectory() as home:
            hooks_path = Path(home) / "hooks.json"
            hooks_path.write_text(json.dumps({"hooks": {
                "SessionStart": [{"hooks": [{"command": "herdr session"}]}],
                "Stop": [{"hooks": [{"command": "other-stop"}]}],
            }}))
            result = run(CODEX_INSTALL, "--home", home)
            self.assertEqual(0, result.returncode, result.stderr)
            hooks = json.loads(hooks_path.read_text())["hooks"]
            commands = json.dumps(hooks)
            self.assertIn("herdr session", commands)
            self.assertIn("other-stop", commands)
            self.assertEqual(2, len(hooks["Stop"]))
            self.assertEqual(1, len(list(Path(home).glob("hooks.json.bak-*"))))


class OpenCodeInstallerTests(unittest.TestCase):
    def test_plugin_is_copied_into_the_auto_loaded_directory(self):
        with tempfile.TemporaryDirectory() as config:
            result = run(OPENCODE_INSTALL, "--config-dir", config)
            self.assertEqual(0, result.returncode, result.stderr)
            target = Path(config) / "opencode" / "plugins" / "dunkingsheep-turn.js"
            self.assertTrue(target.is_file())
            check = run(OPENCODE_INSTALL, "--config-dir", config, "--check")
            self.assertEqual(0, check.returncode, check.stdout + check.stderr)


class ClaudeInstallerTests(unittest.TestCase):
    def test_fresh_home_gets_files_and_three_hooks_then_is_idempotent(self):
        with tempfile.TemporaryDirectory() as home:
            first = run(CLAUDE_INSTALL, "--home", home)
            self.assertEqual(0, first.returncode, first.stderr)
            settings = json.loads((Path(home) / "settings.json").read_text())
            for event, action in (("UserPromptSubmit", "start"),
                                  ("Stop", "end"),
                                  ("SubagentStop", "noop")):
                self.assertIn(event, settings["hooks"])
                self.assertTrue(
                    settings["hooks"][event][0]["hooks"][0]["command"].endswith(
                        " " + action),
                    f"{event} must map to {action}, never to end by default")
            self.assertTrue((Path(home) / "dunkingsheep-turn.py").is_file())
            self.assertTrue((Path(home) / "dunkingsheep-turn.sh").is_file())
            before = (Path(home) / "settings.json").read_bytes()
            second = run(CLAUDE_INSTALL, "--home", home)
            self.assertEqual(0, second.returncode, second.stderr)
            self.assertIn("left unchanged", second.stdout)
            self.assertEqual(before, (Path(home) / "settings.json").read_bytes())
            check = run(CLAUDE_INSTALL, "--home", home, "--check")
            self.assertEqual(0, check.returncode, check.stdout + check.stderr)

    def test_unrelated_hooks_are_preserved_and_subagent_never_ends(self):
        with tempfile.TemporaryDirectory() as home:
            settings_path = Path(home) / "settings.json"
            settings_path.write_text(json.dumps({"hooks": {
                "SessionStart": [{"hooks": [{"type": "command",
                                                "command": "echo hi"}]}],
                "Stop": [{"hooks": [{"type": "command",
                                         "command": "other-stop"}]}],
            }}))
            result = run(CLAUDE_INSTALL, "--home", home)
            self.assertEqual(0, result.returncode, result.stderr)
            hooks = json.loads(settings_path.read_text())["hooks"]
            commands = json.dumps(hooks)
            self.assertIn("echo hi", commands)
            self.assertIn("other-stop", commands)
            self.assertEqual(2, len(hooks["Stop"]))
            subagent = hooks["SubagentStop"][0]["hooks"][0]["command"]
            self.assertTrue(subagent.endswith(" noop"),
                            "SubagentStop must never map to end")
            self.assertEqual(1, len(list(Path(home).glob("settings.json.bak-*"))))


if __name__ == "__main__":
    unittest.main()
