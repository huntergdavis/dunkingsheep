"""t257: `--json` must work AFTER the subcommand, at any depth.

The CLI header says "`--json` is also accepted after the subcommand (agents tend to put it last)", and
that was true only one level deep: the top-level subparsers got parents=[common], but every NESTED
add_subparsers call fell back to type(parent) - a plain ArgumentParser without the flag - so
`dispatch tick --dry-run --json` and `owner stats --json` exited 2 with "unrecognized arguments".

That mattered live: the desk called `dunkingsheep owner stats --json` (vp-of-agentry/server/app.py:507),
got empty stdout, and rendered pending=0 for every lane (t259).
"""

import ast
import os
import subprocess
import sys
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CLI = os.path.join(ROOT, "dunkingsheep")


def _run(argv):
    """Parse-level check: no daemon, no auto-start, so only argparse's verdict is under test."""
    env = dict(os.environ,
               DUNKINGSHEEP_SOCKET="/tmp/nonexistent-jsonflag.sock",
               DUNKINGSHEEP_DIR="/tmp/jsonflag-cfg",
               DUNKINGSHEEP_TMUX_SOCKET="default")
    return subprocess.run([sys.executable, CLI, "--no-start-daemon", *argv],
                          capture_output=True, text=True, timeout=60, env=env)


class JsonPositionTests(unittest.TestCase):
    def test_trailing_json_is_accepted_by_nested_read_only_actions(self):
        # Read-only commands only: this test must not start a turn, feed a lane or write state.
        for argv in (["owner", "stats"], ["task", "list"], ["inbox", "count"],
                     ["dispatch", "status"], ["lane", "list"], ["retired", "list"]):
            result = _run([*argv, "--json"])
            self.assertNotIn("unrecognized arguments", result.stderr, f"{argv}: {result.stderr}")
            # The daemon is absent on purpose; reaching ITS error means argparse accepted the flag.
            self.assertIn("daemon", result.stderr.lower(), f"{argv}: {result.stderr}")

    def test_the_leading_form_still_works(self):
        result = _run(["--json", "owner", "stats"])
        self.assertNotIn("unrecognized arguments", result.stderr, result.stderr)
        self.assertIn("daemon", result.stderr.lower(), result.stderr)

    def test_every_add_subparsers_call_carries_the_class(self):
        """Structural, because the defect was one missing keyword on twelve call sites, and a
        behavioural test on read-only commands cannot reach every branch."""
        tree = ast.parse(open(CLI).read())
        missing = []
        for node in ast.walk(tree):
            if not isinstance(node, ast.Call):
                continue
            func = node.func
            if not (isinstance(func, ast.Attribute) and func.attr == "add_subparsers"):
                continue
            keywords = {kw.arg for kw in node.keywords if kw.arg}
            if "parser_class" not in keywords:
                missing.append(f"line {node.lineno}")
        self.assertEqual([], missing,
                         "every add_subparsers call must carry parser_class so --json works at any "
                         "depth")


if __name__ == "__main__":
    unittest.main()


class LongDeadlineTests(unittest.TestCase):
    def test_long_daemon_commands_get_a_ten_minute_read_deadline(self):
        import importlib.util
        from importlib.machinery import SourceFileLoader
        loader = SourceFileLoader("ds_cli", os.path.join(ROOT, "dunkingsheep"))
        spec = importlib.util.spec_from_loader("ds_cli", loader)
        cli = importlib.util.module_from_spec(spec)
        loader.exec_module(cli)
        for command in ("poolrad", "review", "start-agent", "respawn-pane"):
            self.assertGreaterEqual(cli.client_timeout_for(command, 15.0), 600.0, command)
        self.assertEqual(15.0, cli.client_timeout_for("status", 15.0))
        self.assertEqual(900.0, cli.client_timeout_for("poolrad", 900.0), "never below the client's own")

