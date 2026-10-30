"""The desk reads the daemon, never a CLI process or the daemon's databases.

FLAT_FLEET.md 9.2 rule 1. `web/app.py` cannot be imported on a host without
FastAPI, so this test reads the source and asserts the shape of the calls
instead of pretending to run the app: an AST walk that fails when a socket-only
read grows a subprocess or a sqlite3 connection back.

The acceptance for t593 is a grep; this is that grep, but as a test that names
the functions, so the next edit that reintroduces a CLI spawn fails here rather
than in a review.
"""

import ast
import os
import sys
import unittest

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
APP = os.path.join(ROOT, "web", "app.py")
BOARD = os.path.join(ROOT, "bridges", "ops_board.py")

# Functions that are socket-only today: each must contain no subprocess and no
# sqlite3. `api_fleet` is NOT in this list yet - it still opens agent-state.db and
# inbox.db for the fleet view's last-activity and unread columns (see
# test_api_fleet_still_has_exactly_the_two_known_database_reads).
SOCKET_ONLY = ("_pending_by_pane", "_capability")


def _tree(path):
    with open(path, encoding="utf-8") as handle:
        return ast.parse(handle.read(), filename=path)


def _function(tree, name):
    for node in ast.walk(tree):
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and node.name == name:
            return node
    return None


def _names(node):
    """Every attribute base and bare name used inside a function."""
    used = set()
    for child in ast.walk(node):
        if isinstance(child, ast.Attribute):
            base = child
            while isinstance(base, ast.Attribute):
                base = base.value
            if isinstance(base, ast.Name):
                used.add(base.id)
            used.add(child.attr)
        elif isinstance(child, ast.Name):
            used.add(child.id)
    return used


class SocketOnlyReads(unittest.TestCase):
    def test_the_socket_only_reads_spawn_nothing_and_open_no_database(self):
        tree = _tree(APP)
        for name in SOCKET_ONLY:
            node = _function(tree, name)
            self.assertIsNotNone(node, f"{name} is gone; update this test's list")
            used = _names(node)
            self.assertNotIn("subprocess", used, f"{name} still spawns a process")
            self.assertNotIn("sqlite3", used, f"{name} still opens a database")

    def test_those_reads_go_through_the_daemon_client(self):
        tree = _tree(APP)
        expected = {"_pending_by_pane": "owner_stats",
                    "_capability": "delivery_capability",
                    "api_fleet": "list_panes"}   # the pane roster, already socket-only
        for name, command in expected.items():
            node = _function(tree, name)
            calls = {child.args[0].value
                     for child in ast.walk(node)
                     if isinstance(child, ast.Call)
                     and isinstance(child.func, ast.Attribute) and child.func.attr == "call"
                     and child.args and isinstance(child.args[0], ast.Constant)}
            self.assertIn(command, calls,
                          f"{name} does not call the daemon's {command}")

    def test_api_fleet_still_has_exactly_the_two_known_database_reads(self):
        """The FF-9d remainder, pinned so it cannot grow silently.

        `api_fleet` reads agent-state.db (per-pane last activity + session id) and
        inbox.db (queued per mailbox). Both have daemon-native replacements
        (views roster for activity/unread; roster needs a session id before the
        third column can move), and the desk's four views have to be re-rendered
        to prove the swap. Until that is done this number stays 2: a third read
        fails here instead of arriving in review.
        """
        node = _function(_tree(APP), "api_fleet")
        self.assertIsNotNone(node)
        connects = sum(1 for child in ast.walk(node)
                       if isinstance(child, ast.Call)
                       and isinstance(child.func, ast.Attribute)
                       and child.func.attr == "connect")
        self.assertEqual(2, connects,
                         "api_fleet's database reads changed: update the FF-9d list")

    def test_the_board_reads_the_daemon_for_every_fleet_source(self):
        """`conn` stays, and it is the private OPERATIONS db, not a daemon store.

        An earlier pass of this task claimed `conn` was unused and dropped it:
        wrong - `_host_board` and `ops_budget.build` take it. Host samples have no
        daemon command yet, so the connection stays; what must not happen is a
        daemon store being opened here.
        """
        text = open(BOARD, encoding="utf-8").read()
        for store in ("inbox.db", "agent-state.db", "tasks.db"):
            self.assertNotIn(store, text, f"the board opens {store}")
        tree = _tree(BOARD)
        for name in ("_task_board", "_pane_board", "_dunk_board", "_health_board"):
            node = _function(tree, name)
            self.assertIsNotNone(node, f"{name} is gone")
            calls = {child.args[0].value for child in ast.walk(node)
                     if isinstance(child, ast.Call)
                     and isinstance(child.func, ast.Attribute) and child.func.attr == "call"
                     and child.args and isinstance(child.args[0], ast.Constant)}
            self.assertTrue(calls, f"{name} does not call the daemon")

    def test_the_desk_still_has_its_own_session_store(self):
        """The exception the rule keeps: the desk's sessions live in its own DB."""
        text = open(APP, encoding="utf-8").read()
        self.assertIn("CREATE TABLE IF NOT EXISTS sessions", text)
        self.assertIn("web.sqlite3", text.replace("messages.sqlite3", "web.sqlite3")
                      if "web.sqlite3" not in text else text)


if __name__ == "__main__":
    unittest.main()
