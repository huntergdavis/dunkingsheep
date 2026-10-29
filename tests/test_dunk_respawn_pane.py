"""t398: guarded same-pane respawn. No live pane or raw tmux is used."""
import json
import os
import sqlite3
import sys
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
from dunk_callers import process_start  # noqa: E402
from dunk_core import DunkError, Flock  # noqa: E402
from fakes import FakeHerdr  # noqa: E402
from dunk_terminal import TmuxBackend  # noqa: E402


class RespawnPaneTests(unittest.TestCase):
    def setUp(self):
        self.herdr = FakeHerdr()
        self.pane_id = "w2:p1"
        self.pane = next(p for p in self.herdr.panes if p["pane_id"] == self.pane_id)
        self.pane["agent"] = "codex"
        self.pane["pane_pid"] = os.getpid()
        self.herdr.screens[self.pane_id] = "\u276f \n Ask Codex to do anything\n"
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)
        self.flock.require_superuser = lambda *_a, **_k: None
        self.flock.delivery_state = lambda _pane: {
            "state": "ready", "reason": None, "foreign_text": False}
        self.flock.inbox_count = lambda *_a, **_k: {"unread": 0}
        self.flock.tasks.leases_for_pane = lambda _pane: []
        self.flock.AGENT_VERIFY_S = 0.1
        self.flock._agent_evidence = lambda *_a: ["codex"]
        self.flock.turns = {}
        self.flock.outbox = {}
        self.old_argv = ["codex", "-C", "/tmp", "-s", "danger-full-access",
                         "-a", "never", "resume", "session-old"]
        self.rollback = ["codex", "-C", "/tmp", "-s", "danger-full-access",
                         "-a", "never", "resume", "session-old"]
        self.codex_tmp = tempfile.TemporaryDirectory(prefix="t416-codex-home-")
        self.addCleanup(self.codex_tmp.cleanup)
        self.codex_home = os.path.join(self.codex_tmp.name, "codex-home")
        os.makedirs(self.codex_home)
        self.rollout = os.path.join(self.codex_home, "rollout-session-old.jsonl")
        with open(self.rollout, "w") as handle:
            handle.write(json.dumps({"type": "session_meta", "payload": {
                "session_id": "session-old", "id": "session-old",
                "cwd": "/tmp"}}) + "\n")
        state = sqlite3.connect(os.path.join(self.codex_home, "state_5.sqlite"))
        state.execute("create table threads (id text primary key, rollout_path text, cwd text)")
        state.execute("insert into threads values (?,?,?)",
                      ("session-old", self.rollout, "/tmp"))
        state.commit()
        state.close()
        self.new_argv = ["env", f"CODEX_HOME={self.codex_home}",
                         "codex", "--no-daemon", "-C", "/tmp", "-s",
                         "danger-full-access", "-a", "never", "resume", "session-old"]
        self.flock.launches[self.pane_id] = {
            "agent": "codex", "argv": self.old_argv,
            "command": "old command", "cwd": "/tmp"}
        self.calls = []
        self.fail_after_kill = False
        self.fail_child = False
        self.new_pid = os.getpid() + 1000

        def respawn(pane_id, argv, expected_pid=None, expected_start=None):
            self.calls.append({"pane_id": pane_id, "argv": list(argv),
                               "expected_pid": expected_pid,
                               "expected_start": expected_start})
            if len(self.calls) == 1 and self.fail_after_kill:
                for pane in self.herdr.panes:
                    if pane["pane_id"] == pane_id:
                        pane["pane_pid"] = self.new_pid
                return False, "simulated startup failure after kill"
            if len(self.calls) == 1:
                for pane in self.herdr.panes:
                    if pane["pane_id"] == pane_id:
                        pane["pane_pid"] = self.new_pid
                if self.fail_child:
                    self.herdr.screens[pane_id] = ("command not found\n[agent exited rc=127]\n"
                                                   "hunter$ ")
            else:
                for pane in self.herdr.panes:
                    if pane["pane_id"] == pane_id:
                        pane["pane_pid"] = self.new_pid
                self.herdr.screens[pane_id] = "\u276f \n Ask Codex to do anything\n"
            return True, {"pane_id": pane_id, "pane_pid": self.new_pid,
                          "old_pid": expected_pid}

        self.herdr.respawn_pane = respawn
        self.start = process_start(os.getpid())

    def tearDown(self):
        self.flock.close()

    def _respawn(self, **kw):
        args = dict(pane_id=self.pane_id, argv=self.new_argv,
                    rollback_argv=self.rollback, expected_pid=os.getpid(),
                    expected_start=self.start, confirm_idle=True,
                    reason="t398 test", self_pane_id="%2")
        args.update(kw)
        with patch("dunk_callers.process_alive", return_value=True):
            return self.flock.respawn_pane(**args)

    def set_native_turns(self, *events):
        with open(self.rollout, "w") as handle:
            handle.write(json.dumps({"type": "session_meta", "payload": {
                "session_id": "session-old", "id": "session-old",
                "cwd": "/tmp"}}) + "\n")
            for event, turn_id in events:
                handle.write(json.dumps({
                    "timestamp": "2026-10-29T06:06:32Z",
                    "type": "event_msg",
                    "payload": {"type": event, "turn_id": turn_id,
                                "thread_id": "session-old"}}) + "\n")

    def test_success_preserves_old_launch_and_records_new_argv(self):
        result = self._respawn()
        self.assertEqual(os.getpid(), result["old_pid"])
        self.assertEqual(self.new_pid, result["pane_pid"])
        entry = self.flock.launches[self.pane_id]
        self.assertEqual(self.new_argv, entry["argv"])
        self.assertEqual("respawn_pane", entry["source"])
        self.assertEqual(self.old_argv, entry["previous"]["argv"])
        self.assertEqual(self.new_argv, self.calls[0]["argv"])

    def test_native_session_active_old_turn_refuses_before_mutation(self):
        self.set_native_turns(("task_started", "turn-old"))
        with self.assertRaisesRegex(DunkError, "unfinished turn.*turn-old"):
            self._respawn()
        self.assertEqual([], self.calls)

    def test_native_session_completed_old_turn_allows(self):
        self.set_native_turns(("task_started", "turn-old"),
                              ("task_complete", "turn-old"))
        result = self._respawn()
        self.assertEqual(self.new_pid, result["pane_pid"])
        self.assertEqual(1, len(self.calls))

    def test_requires_positive_idle_confirmation(self):
        with self.assertRaisesRegex(DunkError, "confirm-idle"):
            self._respawn(confirm_idle=False)
        self.assertEqual([], self.calls)

    def test_wrong_pid_and_start_refuse_without_mutation(self):
        with self.assertRaisesRegex(DunkError, "expected"):
            self._respawn(expected_pid=os.getpid() + 1)
        self.assertEqual([], self.calls)
        with self.assertRaisesRegex(DunkError, "process start"):
            self._respawn(expected_start="not-the-start")
        self.assertEqual([], self.calls)

    def test_active_turn_unread_outbox_lease_and_nonready_refuse(self):
        cases = []
        self.flock.turns[self.pane_id] = {"started_at": 1, "expires_at": 2,
                                          "ended_at": None}
        cases.append(self.flock.turns)
        with self.assertRaisesRegex(DunkError, "active turn"):
            self._respawn()
        self.flock.turns = {}
        self.flock.inbox_count = lambda *_a, **_k: {"unread": 2}
        with self.assertRaisesRegex(DunkError, "unread"):
            self._respawn()
        self.flock.inbox_count = lambda *_a, **_k: {"unread": 0}
        self.flock.outbox[self.pane_id] = [SimpleNamespace(status="queued")]
        with self.assertRaisesRegex(DunkError, "outbox"):
            self._respawn()
        self.flock.outbox = {}
        self.flock.tasks.leases_for_pane = lambda _pane: [{"task_id": "t1"}]
        with self.assertRaisesRegex(DunkError, "live task lease"):
            self._respawn()
        self.flock.tasks.leases_for_pane = lambda _pane: []
        self.flock.delivery_state = lambda _pane: {
            "state": "unknown", "reason": "ambiguous", "foreign_text": False}
        with self.assertRaisesRegex(DunkError, "confirmed idle"):
            self._respawn()
        self.assertEqual([], self.calls)

    def test_shared_codex_home_is_refused(self):
        shared = os.path.expanduser("~/.codex")
        argv = ["env", f"CODEX_HOME={shared}", "codex", "--no-daemon"]
        with self.assertRaisesRegex(DunkError, "isolated per-lane"):
            self._respawn(argv=argv)
        self.assertEqual([], self.calls)

    def test_backend_refusal_keeps_the_old_launch_record(self):
        def refuse(pane_id, argv, expected_pid=None, expected_start=None):
            self.calls.append({"pane_id": pane_id, "argv": list(argv)})
            return False, "refused before mutation"
        self.herdr.respawn_pane = refuse
        with self.assertRaisesRegex(DunkError, "refused before mutation"):
            self._respawn()
        self.assertEqual(self.old_argv, self.flock.launches[self.pane_id]["argv"])

    def test_backend_without_respawn_refuses(self):
        del self.herdr.respawn_pane
        with self.assertRaisesRegex(DunkError, "does not support"):
            self._respawn()
        self.assertEqual([], self.calls)

    def test_failed_start_rolls_back_to_rollback_argv(self):
        self.fail_after_kill = True
        with self.assertRaisesRegex(DunkError, "rolled back"):
            self._respawn()
        self.assertEqual(2, len(self.calls), "one new command + one rollback")
        self.assertEqual(self.rollback, self.calls[1]["argv"])
        self.assertEqual(self.old_argv, self.flock.launches[self.pane_id]["argv"],
                         "failed launch must not replace the old launch record")

    def test_live_wrapper_with_failed_child_rolls_back(self):
        self.fail_child = True
        with self.assertRaisesRegex(DunkError, "rolled back"):
            self._respawn()
        self.assertEqual(2, len(self.calls))
        self.assertEqual(self.rollback, self.calls[1]["argv"])
        self.assertEqual(self.old_argv, self.flock.launches[self.pane_id]["argv"],
                         "failed child must not write a launch record")

    def test_live_wrapper_without_agent_child_rolls_back(self):
        self.flock._agent_evidence = lambda *_a: []
        with self.assertRaisesRegex(DunkError, "live wrapper|no codex child|rollback not verified"):
            self._respawn()
        self.assertEqual(2, len(self.calls))
        self.assertEqual(self.old_argv, self.flock.launches[self.pane_id]["argv"])

    def test_second_respawn_with_stale_pid_refuses(self):
        self._respawn()
        with self.assertRaisesRegex(DunkError, "expected"):
            self._respawn(expected_pid=os.getpid())
        self.assertEqual(1, len(self.calls), "stale pid must not trigger a second mutation")

    def test_dead_replacement_rolls_back(self):
        with patch("dunk_callers.process_alive", side_effect=[False, True]):
            with self.assertRaisesRegex(DunkError, "rolled back"):
                self.flock.respawn_pane(
                    self.pane_id, self.new_argv, rollback_argv=self.rollback,
                    expected_pid=os.getpid(), expected_start=self.start,
                    confirm_idle=True, reason="dead", self_pane_id="%2")
        self.assertEqual(2, len(self.calls))
        self.assertEqual(self.rollback, self.calls[1]["argv"])


class AgentEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.flock = Flock(herdr=FakeHerdr(), persist=False, restore=False)

    def tearDown(self):
        self.flock.close()

    def test_zombie_child_is_not_running_agent_evidence(self):
        rows = {123: {"pid": 123, "ppid": 1, "comm": "sh", "state": "S"},
                456: {"pid": 456, "ppid": 123, "comm": "codex", "state": "Z"}}
        with patch("os.listdir", return_value=["123", "456"]), \
             patch("dunk_callers._stat", side_effect=rows.get):
            self.assertEqual([], self.flock._agent_evidence(123, {"codex"}))

    def test_stopped_child_is_not_evidence(self):
        rows = {123: {"pid": 123, "ppid": 1, "comm": "sh", "state": "S", "start": "1"},
                456: {"pid": 456, "ppid": 123, "comm": "codex", "state": "T", "start": "200"}}
        with patch("os.listdir", return_value=["123", "456"]), \
             patch("dunk_callers._stat", side_effect=rows.get):
            self.assertEqual([], self.flock._agent_evidence(123, {"codex"}))

    def test_leftover_child_started_before_respawn_is_not_evidence(self):
        rows = {123: {"pid": 123, "ppid": 1, "comm": "sh", "state": "S", "start": "100"},
                456: {"pid": 456, "ppid": 123, "comm": "codex", "state": "S", "start": "50"}}
        with patch("os.listdir", return_value=["123", "456"]), \
             patch("dunk_callers._stat", side_effect=rows.get):
            self.assertEqual([], self.flock._agent_evidence(123, {"codex"}, "100"))

    def test_live_child_started_after_respawn_is_evidence(self):
        rows = {123: {"pid": 123, "ppid": 1, "comm": "sh", "state": "S", "start": "100"},
                456: {"pid": 456, "ppid": 123, "comm": "codex", "state": "S", "start": "100"}}
        with patch("os.listdir", return_value=["123", "456"]), \
             patch("dunk_callers._stat", side_effect=rows.get):
            self.assertEqual([456], self.flock._agent_evidence(123, {"codex"}, "100"))

    def test_proc_walk_race_is_not_evidence(self):
        with patch("os.listdir", side_effect=OSError("vanished")):
            self.assertEqual([], self.flock._agent_evidence(123, {"codex"}, "100"))

    def test_live_child_is_production_helper_evidence(self):
        rows = {123: {"pid": 123, "ppid": 1, "comm": "sh", "state": "S"},
                456: {"pid": 456, "ppid": 123, "comm": "codex", "state": "S"}}
        with patch("os.listdir", return_value=["123", "456"]), \
             patch("dunk_callers._stat", side_effect=rows.get):
            self.assertEqual([456], self.flock._agent_evidence(123, {"codex"}))


class TmuxRespawnGuardTests(unittest.TestCase):
    def setUp(self):
        import tempfile
        self.tmp = tempfile.TemporaryDirectory(prefix="t398-tmux-")
        self.addCleanup(self.tmp.cleanup)
        self.backend = TmuxBackend(tmux_bin="/bin/true",
                                   registry_path=f"{self.tmp.name}/panes.json")
        self.backend._managed = lambda *_a: True
        self.backend._tmux = lambda *_a, **_k: (0, "", "")

    def test_argv_is_shell_quoted_and_pane_id_is_preserved(self):
        state = {"pid": 200}
        calls = []
        def tmux(*args, **_kw):
            calls.append(args)
            state["pid"] = 201
            return 0, "", ""
        self.backend._tmux = tmux
        self.backend.get_pane = lambda _pane: {"pane_id": "w1:p1",
                                               "pane_pid": state["pid"]}
        ok, result = self.backend.respawn_pane(
            "w1:p1", ["codex", "; touch /tmp/pwned", "--no-daemon"],
            expected_pid=200)
        self.assertTrue(ok, result)
        self.assertEqual("w1:p1", result["pane_id"])
        self.assertEqual("w1:p1", result["pane_id"])
        command = calls[0][-1]
        self.assertIn("sh -c", command)
        self.assertIn("touch /tmp/pwned", command)
        self.assertNotIn("; touch /tmp/pwned", command.replace("'" + "; touch /tmp/pwned" + "'", ""))

    def test_pane_replaced_between_core_check_and_backend_mutation_refuses(self):
        state = {"pid": 200}
        calls = []

        def tmux(*args, **_kw):
            calls.append(args)
            return 0, "", ""

        self.backend._tmux = tmux
        self.backend._managed = lambda *_a: True
        pids = iter([200, 201])
        self.backend.get_pane = lambda _pane: {"pane_id": "w1:p1",
                                               "pane_pid": next(pids)}
        ok, message = self.backend.respawn_pane(
            "w1:p1", ["codex", "--no-daemon"], expected_pid=200)
        self.assertFalse(ok)
        self.assertIn("replaced between check", message)
        self.assertEqual([], calls, "no kill/spawn after the pane pid changed")

    def test_unmanaged_or_wrong_pid_refuses(self):
        self.backend.get_pane = lambda _pane: {"pane_id": "w1:p1",
                                               "pane_pid": 200}
        self.backend._managed = lambda *_a: False
        ok, message = self.backend.respawn_pane("w1:p1", ["codex"])
        self.assertFalse(ok)
        self.assertIn("did not create", message)
        self.backend._managed = lambda *_a: True
        ok, message = self.backend.respawn_pane("w1:p1", ["codex"], expected_pid=201)
        self.assertFalse(ok)
        self.assertIn("expected 201", message)


if __name__ == "__main__":
    unittest.main()


class NativeBusySocketBoundaryTests(unittest.TestCase):
    """t142: a busy native state DB must serialize as retryable, never raw text.

    Consumer boundary: reader -> Flock.respawn_pane -> API dispatch ->
    FlockServer.handle_line. Private fixture only; the backend is a Mock
    that must never be called (busy refuses before any mutation).
    """
    def setUp(self):
        from unittest.mock import Mock
        self.herdr = FakeHerdr()
        self.pane_id = "w2:p1"
        self.pane = next(p for p in self.herdr.panes
                         if p["pane_id"] == self.pane_id)
        self.pane["agent"] = "codex"
        self.pane["pane_pid"] = os.getpid()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)
        self.flock.require_superuser = lambda *_a, **_k: None
        self.flock.delivery_state = lambda _pane: {
            "state": "ready", "reason": None, "foreign_text": False}
        self.flock.inbox_count = lambda *_a, **_k: {"unread": 0}
        self.flock.tasks.leases_for_pane = lambda _pane: []
        self.flock.turns = {}
        self.flock.outbox = {}
        self.backend = Mock(name="herdr.respawn_pane")
        self.herdr.respawn_pane = self.backend
        self.codex_tmp = tempfile.TemporaryDirectory(
            prefix="t142-native-boundary-")
        self.addCleanup(self.codex_tmp.cleanup)
        self.codex_home = os.path.join(self.codex_tmp.name, "codex-home")
        os.makedirs(self.codex_home)
        db_path = os.path.join(self.codex_home, "state_5.sqlite")
        setup = sqlite3.connect(db_path)
        setup.execute("create table threads "
                      "(id text primary key, rollout_path text, cwd text)")
        setup.execute("insert into threads values (?,?,?)",
                      ("session-old", os.path.join(self.codex_home, "rollout.jsonl"),
                       "/tmp"))
        setup.commit()
        setup.close()
        self.writer = sqlite3.connect(db_path, timeout=30)
        self.writer.execute("BEGIN EXCLUSIVE")
        self.writer.execute("UPDATE threads SET cwd='/tmp' "
                            "WHERE id='session-old'")
        self.addCleanup(self.writer.close)
        self.addCleanup(self.writer.rollback)
        self.addCleanup(self.flock.close)
        self.start = process_start(os.getpid())
        self.argv = ["codex", f"CODEX_HOME={self.codex_home}",
                     "resume", "session-old"]

    def test_busy_native_db_replies_retryable_without_raw_lock_text(self):
        from dunk_server import FlockServer
        server = object.__new__(FlockServer)
        server.flock = self.flock
        line = json.dumps({"id": 7, "cmd": "respawn_pane", "args": {
            "pane": self.pane_id, "expected_pid": os.getpid(),
            "expected_start": self.start, "argv": self.argv,
            "rollback_argv": self.argv, "confirm_idle": True}})
        reply, stop = server.handle_line(line)
        self.assertFalse(stop)
        self.assertFalse(reply["ok"])
        self.assertTrue(reply.get("retryable"),
                        f"busy must serialize retryable: {reply}")
        self.assertNotIn("database is locked", reply["error"])
        self.assertIn("retry the request", reply["error"])
        self.assertEqual([], self.backend.call_args_list,
                         "busy refuses before any backend mutation")
