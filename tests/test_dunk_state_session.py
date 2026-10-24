"""Capture must identify the session in a pane from the PROCESS, and say so honestly.

The 2026-10-24 failure: every live pane captured session_id NULL with the MODEL reason
string, because a blank tmux field silently nulled pane_pid. These tests pin the three
things that had to be true for that to stop being invisible.
"""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import dunk_state
import dunk_terminal


class ParsePanesKeepsPanePid(unittest.TestCase):
    def test_blank_session_index_does_not_null_the_pid(self):
        """tmux leaves #{session_index} empty on this host; int("") used to kill pane_pid."""
        backend = dunk_terminal.TmuxBackend.__new__(dunk_terminal.TmuxBackend)
        backend.socket_name = None
        backend.registry_path = "/nonexistent/registry.json"
        backend._load_registry = lambda: {"panes": {}}
        line = backend.PANE_SEP.join([
            "%2", "$2", "@2", "/tmp", "pi", "lane", "1",
            "cos", "pi", "1", "", "4242",
        ])
        pane = backend._parse_panes(line)[0]
        self.assertEqual(pane["pane_pid"], 4242)
        self.assertEqual(pane["pane_id"], "%2")


class SessionFromProcess(unittest.TestCase):
    def test_missing_pid_reports_a_reason_instead_of_a_guess(self):
        session, reason = dunk_state.session_of(None, {"session": "recorded-id"})
        self.assertEqual(session, "recorded-id")
        self.assertIn("no pid", reason)

    def test_shell_pane_says_it_has_no_agent(self):
        """A pane running only a shell: no agent process, and it must say so."""
        import subprocess
        proc = subprocess.Popen(["sleep", "5"])
        try:
            session, reason = dunk_state.session_of(proc.pid, None)
            self.assertIsNone(session)
            self.assertIn("no agent process", reason)
        finally:
            proc.kill()
            proc.wait()

    def test_inherited_session_from_another_workspace_is_reported_not_used(self):
        """The real defect: an env that names another workspace's session must not be trusted."""
        original = dunk_state._env_of
        original_kids = dunk_state._proc_children
        original_cmd = dunk_state._cmdline
        try:
            dunk_state._cmdline = lambda pid: ["/usr/bin/pi", "--flag"]
            dunk_state._proc_children = lambda: {}
            dunk_state._env_of = lambda pid: {
                "PI_SESSION_ID": "01a15560-dead-beef",
                "PI_SESSION_FILE": ("/home/hunter/.pi/agent/sessions/"
                                    "--home-hunter-workspace-vp-of-agentry--/x_01a15560.jsonl"),
            }
            session, reason = dunk_state.session_of(
                4242, None, cwd="/home/hunter/workspace/chief-of-staff")
            self.assertIsNone(session)
            self.assertIn("inherited claim", reason)
            session, reason = dunk_state.session_of(
                4242, None, cwd="/home/hunter/workspace/vp-of-agentry")
            self.assertEqual(session, "01a15560-dead-beef")
            self.assertIsNone(reason)
        finally:
            dunk_state._env_of = original
            dunk_state._proc_children = original_kids
            dunk_state._cmdline = original_cmd


class SessionReasonColumn(unittest.TestCase):
    def test_old_database_gains_the_column_without_losing_rows(self):
        import sqlite3
        import tempfile
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "state.db")
            db = sqlite3.connect(path)
            db.executescript("""
                CREATE TABLE agent_state (
                    at REAL NOT NULL, pane_id TEXT NOT NULL, project TEXT, harness TEXT,
                    cwd TEXT, argv TEXT, session_id TEXT, model TEXT, model_reason TEXT,
                    thinking TEXT, thinking_reason TEXT, state TEXT, state_source TEXT,
                    state_reason TEXT, task_id TEXT, source TEXT);
                INSERT INTO agent_state (at, pane_id, session_id)
                    VALUES (1.0, '%9', NULL);
            """)
            db.commit()
            db.close()
            store = dunk_state.StateStore(path)
            cols = {row[1] for row in store.db.execute("PRAGMA table_info(agent_state)")}
            self.assertIn("session_reason", cols)
            rows = list(store.db.execute("SELECT pane_id FROM agent_state"))
            self.assertEqual([row[0] for row in rows], ["%9"])


if __name__ == "__main__":
    unittest.main()
