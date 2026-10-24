"""Every registered command must RUN, not merely be spelled somewhere.

The lesson from the poolrad gate (#1437): `check_checker_builds --compile-only`
proved a module *bound* and then an undefined name reached main, because a hook
only fails when it executes. Our own surface had the same hole - the parity test
greps the CLI help text, which proves a name exists, not that a handler works.

So this executes every command in the registry against a fixture fleet:

- a command with no required parameters must RETURN a result;
- a command that needs parameters must refuse with words, not crash;
- no command may raise NameError, AttributeError or TypeError - those are the
  "it built, it did not run" signatures.

A refusal is a pass: the point is that the code path ran. A crash is the gate.
"""
import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import _bootstrap  # noqa: E402,F401

import dunk_api  # noqa: E402
import dunk_core  # noqa: E402
from fakes import FakeHerdr  # noqa: E402

# Behaviour that is correct but destructive or slow: refused by policy in a test.
SKIPPED = {
    "shutdown": "would stop the fixture server",
    "install": "writes a systemd unit",
    "uninstall": "removes the installed unit",
    "mcp_setup": "prints host configuration only",
}

# A crash signature, as opposed to a refusal. These are exactly the failures a
# compile-only gate cannot see.
# KeyError is in here on purpose: a handler that indexes args["x"] for a key the
# Command does not declare required is a schema that lies about its needs, and it
# reaches a user as 'internal error'. clear_box and delivery were exactly that.
CRASHES = (NameError, AttributeError, TypeError, UnboundLocalError, KeyError)


def refusal_types():
    """Every 'named refusal' the codebase raises on purpose."""
    types = [dunk_core.DunkError, ValueError, RuntimeError, KeyError, SystemExit]
    for module_name in ("dunk_dispatch", "dunk_panes", "dunk_context", "dunk_policy",
                        "dunk_tasks", "dunk_inbox", "dunk_pm"):
        module = __import__(module_name)
        for name in dir(module):
            candidate = getattr(module, name)
            if isinstance(candidate, type) and issubclass(candidate, Exception):
                types.append(candidate)
    return tuple(types)


class CommandSmokeTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.herdr = FakeHerdr()
        self.superusers = dunk_core.Superusers(path=os.path.join(self.tmp.name, "su.json"))
        self.superusers.add("w1:p1")
        self.flock = dunk_core.Flock(herdr=self.herdr, persist=False, restore=False,
                                     superusers=self.superusers)

    def tearDown(self):
        self.tmp.cleanup()

    def call(self, name, args=None):
        return dunk_api.dispatch(self.flock, name, args or {}, self_pane_id="w1:p1")

    def test_every_command_runs_or_refuses_but_never_crashes(self):
        failures = []
        for command in dunk_api.COMMANDS:
            if command.name in SKIPPED:
                continue
            try:
                result = self.call(command.name)
            except CRASHES as crash:
                failures.append(f"{command.name}: {type(crash).__name__}: {crash}")
                continue
            except refusal_types() as refusal:
                # A refusal must carry words: an empty refusal is a crash in a
                # polite costume.
                if not str(refusal).strip():
                    failures.append(f"{command.name}: refused with no message")
                continue
            # A smoke test that accepts an empty answer is not a smoke test: a
            # handler returning {} 'succeeds' while doing nothing at all.
            if result is None or result == {} or result == [] or result == "":
                failures.append(f"{command.name}: returned {result!r}, which proves nothing")
        self.assertEqual([], failures, "commands that did not run:\n" + "\n".join(failures))

    def test_a_command_with_no_required_parameters_returns_a_result(self):
        """The stronger claim: it produced an answer, not just a polite refusal."""
        failures = []
        for command in dunk_api.COMMANDS:
            if command.required or command.name in SKIPPED:
                continue
            try:
                result = self.call(command.name)
            except CRASHES as crash:
                failures.append(f"{command.name}: {type(crash).__name__}: {crash}")
                continue
            except refusal_types() as refusal:
                # A deliberate policy refusal is a pass (a disabled memory service,
                # a fixture pane list); anything unclear is not.
                if "must" not in str(refusal) and "not enabled" not in str(refusal):
                    failures.append(f"{command.name}: refused without parameters: {refusal}")
                continue
            # A smoke test that accepts an empty answer is not a smoke test: a
            # handler returning {} 'succeeds' while doing nothing at all.
            if result is None or result == {} or result == [] or result == "":
                failures.append(f"{command.name}: returned {result!r}, which proves nothing")
        self.assertEqual([], failures,
                         "parameterless commands must answer:\n" + "\n".join(failures))

    def test_the_gate_would_catch_an_unimported_name(self):
        """Prove the smoke case fails for the exact class #1437 let through.

        A handler that reads a name it never imported raises NameError only when
        it executes. This registers one and asserts the smoke gate reports it.
        """
        def broken(flock, args, self_pane_id):
            return UC_X86_REG_DX  # noqa: F821 - deliberately undefined

        original = list(dunk_api.COMMANDS)
        broken_command = dunk_api.Command("smoke_broken", "deliberately broken", broken,
                                          params={}, required=())
        dunk_api.COMMANDS.append(broken_command)
        dunk_api.COMMANDS_BY_NAME["smoke_broken"] = broken_command
        try:
            with self.assertRaises(NameError):
                self.call("smoke_broken")
        finally:
            dunk_api.COMMANDS[:] = original
            dunk_api.COMMANDS_BY_NAME.pop("smoke_broken", None)


if __name__ == "__main__":
    unittest.main()
