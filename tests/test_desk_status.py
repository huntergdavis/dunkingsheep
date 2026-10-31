"""tools/desk_status.py: the Front Desk's read-only facts, from a fake database."""

import importlib.util
import os
import sqlite3
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location(
    "desk_status", os.path.join(os.path.dirname(HERE), "tools", "desk_status.py"))
ds = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ds)

ROWS = [
    ("t1", "Rust port: T51 memory", "in_progress", '["%126"]', None),
    ("t2", "Rust parity: dunkingsheep", "open", "[]", None),
    ("t3", "T16 slice: panes.rs", "done", "[]", None),
    ("t4", "memory work with no marker", "blocked", "[]", "DEFERRED by the owner"),   # id in the file
    ("t5", "Fix the web desk", "in_progress", '["%124"]', None),
    ("t6", "Old thing", "cancelled", "[]", None),
    ("t7", "Another plain task", "open", "[]", None),
]


def make_db(path):
    connection = sqlite3.connect(path)
    connection.execute("CREATE TABLE tasks (id TEXT PRIMARY KEY, title TEXT, status TEXT, "
                       "assignees TEXT, block_reason TEXT, depends_on TEXT DEFAULT '[]')")
    for row in ROWS:
        connection.execute("INSERT INTO tasks (id,title,status,assignees,block_reason) VALUES (?,?,?,?,?)", row)
    connection.commit()
    connection.close()


class DeskStatusTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.db = os.path.join(self.tmp.name, "tasks.db")
        make_db(self.db)
        self.ids = {"t4"}

    def test_rust_is_the_id_file_or_a_rust_title(self):
        tasks = ds.load_tasks(self.db)
        rust = {t["id"] for t in tasks if ds.is_rust(t, self.ids)}
        self.assertEqual({"t1", "t2", "t3", "t4"}, rust)

    def test_the_rust_report_counts_by_status_and_names_the_work_in_progress(self):
        lines = ds.rust_report(ds.load_tasks(self.db), self.ids)
        text = "\n".join(lines)
        self.assertIn("Rust tasks: 4 total", text)
        self.assertIn("remaining (not done or cancelled): 3", text)
        self.assertIn("t1 (%126)", text)
        self.assertIn("Blocked: 1 (t4: DEFERRED by the owner)", text)

    def test_the_work_report_lists_non_rust_live_tasks_only(self):
        lines = ds.work_report(ds.load_tasks(self.db), self.ids)
        self.assertEqual("Non-Rust tasks open or in progress: 2", lines[0])
        text = "\n".join(lines)
        self.assertIn("t5 in_progress (%124)", text)
        self.assertIn("t7 open", text)
        self.assertNotIn("t6", text, "cancelled is not live")

    def test_headroom_is_parsed_from_the_usage_command(self):
        out = ("PROVIDER STATUS\nclaude ok percent_used five_hour: 8.0% used / 92.0% left "
               "(resets 2026-10-31T13:59:59.1+00:00); weekly_all: 59.0% used / 41.0% left "
               "(resets 2026-11-05T08:59:59.2+00:00)")
        lines = ds.headroom_report(lambda: out)
        self.assertEqual("Claude five-hour window: 8.0% used, 92.0% left (resets 2026-10-31T13:59Z)", lines[0])
        self.assertIn("59.0% used, 41.0% left", lines[1])

    def test_a_failure_says_unknown_instead_of_guessing(self):
        def boom():
            raise OSError("no such command")
        self.assertIn("unknown", ds.headroom_report(boom)[0])
        self.assertIn("unknown", ds.headroom_report(lambda: "claude error: no data")[0])

    def test_the_database_is_opened_read_only(self):
        connection = sqlite3.connect(f"file:{self.db}?mode=ro", uri=True)
        with self.assertRaises(sqlite3.OperationalError):
            connection.execute("DELETE FROM tasks")
        connection.close()
        before = ds.load_tasks(self.db)
        ds.load_tasks(self.db)
        self.assertEqual(before, ds.load_tasks(self.db))

    def test_an_unreadable_database_is_reported_not_raised(self):
        with self.assertRaises(sqlite3.Error):
            ds.load_tasks(os.path.join(self.tmp.name, "missing", "tasks.db"))


if __name__ == "__main__":
    unittest.main()
