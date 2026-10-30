"""t1044: a CLI call site must not pass an argument its parser cannot produce.

The bug: `dunkingsheep task complete` passed `landed=args.landed` while the `complete`
subparser defined no `landed` (main's hotfix 20530f2 restored it as `--unmerged`), so EVERY completion crashed with
AttributeError before the request was sent - the terminal verb of every lane. Unit tests
that call `Flock.task_complete` directly cannot see it, because they never build the
parser.

These tests build the real parser and check the two halves agree.
"""

import os
import sys
import unittest
from importlib.machinery import SourceFileLoader

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def load_cli():
    return SourceFileLoader("dunkcli_call_sites", os.path.join(ROOT, "dunkingsheep")).load_module()


class TaskCompleteParserTests(unittest.TestCase):
    def setUp(self):
        self.cli = load_cli()

    def parse(self, argv):
        return self.cli.build_parser().parse_args(argv)

    def test_complete_produces_every_attribute_the_call_site_reads(self):
        """The exact bug: `args.landed` was read and never defined."""
        args = self.parse(["task", "complete", "t1"])
        for name in ("id", "by", "verdict", "landed"):
            self.assertTrue(hasattr(args, name),
                            f"the complete call site reads args.{name}; the parser must define it")

    def test_landed_defaults_to_unset_so_a_plain_completion_is_unchanged(self):
        args = self.parse(["task", "complete", "t1"])
        self.assertIsNone(args.landed, "unset means 'not stated', not False")

    def test_the_unmerged_flag_is_reachable(self):
        """t906's workflow: done on a stacked branch, dependents stay closed. The flag is
        `--unmerged` (main's hotfix 20530f2), stored as landed=False."""
        self.assertFalse(self.parse(["task", "complete", "t1", "--unmerged"]).landed)

    def test_the_handler_defaults_to_landed_and_the_registry_advertises_it(self):
        import dunk_api
        calls = []

        class Flock:
            def task_complete(self, task_id, **kwargs):
                calls.append(kwargs["landed"])

        dunk_api.h_task_complete(Flock(), {"id": "t1"}, None)
        dunk_api.h_task_complete(Flock(), {"id": "t1", "landed": False}, None)
        self.assertEqual([True, False], calls)
        command = dunk_api.COMMANDS_BY_NAME["task_complete"]
        self.assertEqual("boolean", command.params["landed"]["type"])


if __name__ == "__main__":
    unittest.main()
