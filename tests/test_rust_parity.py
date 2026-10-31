"""tools/rust_parity.py on a fake tree: the logic takes plain dicts, so no git is needed."""

import importlib.util
import os
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location(
    "rust_parity", os.path.join(os.path.dirname(HERE), "tools", "rust_parity.py"))
rp = importlib.util.module_from_spec(spec)
spec.loader.exec_module(rp)

BASE = {"dunk_inbox.py": '''
SCHEMA = """CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    body TEXT NOT NULL,
    state TEXT NOT NULL
);"""
class Inbox:
    def count(self):
        return 1
    def old(self):
        return 0
''', "dunk_api.py": 'COMMANDS = [Command("inbox_count", "d", h)]\n'}
HEAD = {"dunk_inbox.py": '''
SCHEMA = """CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    body TEXT NOT NULL,
    state TEXT NOT NULL,
    delivered_at REAL
);"""
MIGRATE = (("notify_at", "REAL"),)
class Inbox:
    def count(self):
        return 2          # behaviour change
    def gate_unread(self):
        return 3
''', "dunk_api.py": 'COMMANDS = [Command("inbox_count", "d", h),\n  Command("owner_report", "d", h)]\n'}


class ExtractTests(unittest.TestCase):
    def test_functions_commands_and_columns_are_found(self):
        items = rp.extract(HEAD)
        for key in ("fn:dunk_inbox.py::Inbox.count", "cmd:owner_report",
                    "col:messages.delivered_at", "col:*.notify_at"):
            self.assertIn(key, items)
        self.assertNotIn("col:messages.PRIMARY", items)

    def test_a_comment_only_edit_is_not_a_change(self):
        a = rp.extract({"dunk_x.py": "def f():\n    return 1\n"})
        b = rp.extract({"dunk_x.py": "def f():\n    # note\n    return 1\n"})
        self.assertEqual({"added": [], "changed": [], "removed": []}, rp.diff_items(a, b))

    def test_the_diff_lists_added_changed_and_removed(self):
        delta = rp.diff_items(rp.extract(BASE), rp.extract(HEAD))
        self.assertIn("fn:dunk_inbox.py::Inbox.gate_unread", delta["added"])
        self.assertEqual(["fn:dunk_inbox.py::Inbox.count"], delta["changed"])
        self.assertEqual(["fn:dunk_inbox.py::Inbox.old"], delta["removed"])


class ClassifyTests(unittest.TestCase):
    def setUp(self):
        self.delta = rp.diff_items(rp.extract(BASE), rp.extract(HEAD))

    def keys(self, report):
        return {key: status for entries in report.values() for status, key in entries}

    def test_unported_items_are_reported_by_module(self):
        report = rp.classify(self.delta, {"count"})
        keys = self.keys(report)
        self.assertIn("fn:dunk_inbox.py::Inbox.gate_unread", keys)
        self.assertIn("cmd:owner_report", keys)
        self.assertIn("col:messages.delivered_at", keys)
        self.assertNotIn("fn:dunk_inbox.py::Inbox.count", keys)   # has a counterpart
        self.assertEqual({"dunk_inbox", "dunk_api", "schema"}, set(report))

    def test_a_ported_item_is_not_reported_and_a_stale_one_is(self):
        names = {"count", "gate_unread", "owner_report", "delivered_at", "notify_at", "old"}
        self.assertEqual(["fn:dunk_inbox.py::Inbox.old"],
                         [k for k in self.keys(rp.classify(self.delta, names))])  # removed, still in Rust
        stale = rp.classify(self.delta, names, stale=lambda key: key.endswith("Inbox.count"))
        self.assertIn("STALE", self.keys(stale)["fn:dunk_inbox.py::Inbox.count"])

    def test_a_leading_underscore_still_matches_a_rust_name(self):
        delta = rp.diff_items({}, rp.extract({"dunk_x.py": "def _helper():\n    pass\n"}))
        self.assertEqual({}, rp.classify(delta, {"helper"}))


class TaskPlanTests(unittest.TestCase):
    def test_one_task_per_module_deduped_by_title(self):
        report = rp.classify(rp.diff_items(rp.extract(BASE), rp.extract(HEAD)), set())
        plan = rp.plan_tasks(report, {"Rust parity: dunk_api"}, "70876c2")
        titles = [title for title, _ in plan]
        self.assertEqual(["Rust parity: dunk_inbox", "Rust parity: schema"], titles)
        self.assertIn("gate_unread", plan[0][1])
        self.assertIn("70876c2", plan[0][1])

    def test_the_manifest_commit_is_read_from_its_first_line(self):
        self.assertEqual("70876c2", rp.manifest_commit("# Port manifest (generated from 70876c2, 2026-10-30)"))
        with self.assertRaises(ValueError):
            rp.manifest_commit("no commit here")

    def test_nothing_unported_reads_clean(self):
        self.assertIn("nothing unported", rp.format_report({}, "abc1234"))


if __name__ == "__main__":
    unittest.main()
