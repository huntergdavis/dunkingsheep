"""Every flag the CLI passes must exist on the parser (t1011's live outage).

`dunkingsheep task complete` called `args.landed` while its subparser had no `--unmerged`
argument, so the command died with AttributeError for the whole fleet - the one command
every lane uses to finish work. The pairing is mechanical, so it is tested mechanically:
parse the complete subparser with no flags and read every attribute the call site uses.
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import _bootstrap  # noqa: F401,E402


class TaskCompleteFlagTests(unittest.TestCase):
    def setUp(self):
        import importlib.util
        path = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                            "dunkingsheep")
        spec = importlib.util.spec_from_loader("cli_under_test", loader=None)
        module = importlib.util.module_from_spec(spec)
        module.__file__ = path
        # The CLI is a script; execute its definitions without running main().
        with open(path, encoding="utf-8") as handle:
            source = handle.read()
        source = source.split('if __name__ == "__main__":')[0]
        exec(compile(source, path, "exec"), module.__dict__)  # noqa: S102
        self.build_parser = module.build_parser

    def test_task_complete_parses_with_no_flags(self):
        parser = self.build_parser()
        args = parser.parse_args(["task", "complete", "t1"])
        # THE DEFECT: args.landed was read by the dispatch and never defined.
        self.assertTrue(hasattr(args, "landed"),
                        "task complete reads args.landed; its parser must define --unmerged")
        self.assertIsNone(args.landed)

    def test_the_flag_marks_a_stacked_completion(self):
        parser = self.build_parser()
        args = parser.parse_args(["task", "complete", "t1", "--unmerged"])
        self.assertIs(False, args.landed, "--unmerged means landed=False")

    def test_every_attribute_the_dispatch_reads_exists(self):
        import re
        path = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                            "dunkingsheep")
        with open(path, encoding="utf-8") as handle:
            source = handle.read()
        call = re.search(r'client\.call\("task_complete"[^)]*\)', source)
        self.assertIsNotNone(call, "the task_complete dispatch call moved")
        parser = self.build_parser()
        args = parser.parse_args(["task", "complete", "t1"])
        for name in re.findall(r"args\.([a-z_]+)", call.group(0)):
            self.assertTrue(hasattr(args, name),
                            f"the dispatch passes args.{name} but the parser has no such flag")


if __name__ == "__main__":
    unittest.main()
