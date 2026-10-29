"""t416: fail-closed native-session activity guard, saved %30 trace controls."""
import json
import os
import sqlite3
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
from dunk_native_session import (NativeSessionChanged, NativeSessionRefusal,
                                 check_native_session_quiescence,
                                 find_resume_request)

SESSION = "01a173ee-beed-7461-a560-e5f6c1f666a7"
TIMELINE = os.path.join(os.path.dirname(__file__), "fixtures",
                        "t416-native-turn-timeline.json")
OLD = "01a18644-66a3-7772-b01f-9b0f1d25e7fd"
NEW = "01a18645-98e0-7cb0-b14a-bbc93298907e"
CWD = "/home/hunter/workspace/poolrad-codex-review-c"


class NativeSessionTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="t416-native-")
        self.addCleanup(self.tmp.cleanup)
        self.home = os.path.join(self.tmp.name, "codex-home")
        os.makedirs(self.home)
        self.rollout = os.path.join(self.home, f"rollout-{SESSION}.jsonl")
        self.write_rollout()
        db = sqlite3.connect(os.path.join(self.home, "state_5.sqlite"))
        db.execute("create table threads (id text primary key, rollout_path text, cwd text)")
        db.execute("insert into threads values (?,?,?)", (SESSION, self.rollout, CWD))
        db.commit()
        db.close()

    def write_rollout(self, *events, authenticated=True):
        with open(self.rollout, "w") as handle:
            if authenticated:
                handle.write(json.dumps({
                    "timestamp": "2026-10-29T06:06:30Z",
                    "type": "session_meta",
                    "payload": {"session_id": SESSION, "id": SESSION, "cwd": CWD}
                }) + "\n")
            for event in events:
                handle.write(json.dumps({
                    "timestamp": event[2] if len(event) > 2 else "2026-10-29T06:06:32Z",
                    "type": "event_msg",
                    "payload": {"type": event[0], "turn_id": event[1],
                                "thread_id": SESSION}}) + "\n")

    def argv(self, home=None, session=SESSION, cwd=CWD):
        return ["env", f"CODEX_HOME={home or self.home}", "codex", "--no-daemon",
                "-C", cwd, "resume", session, "prompt"]

    def check(self, **kw):
        return check_native_session_quiescence(self.argv(**kw), CWD)

    def test_saved_trace_cut_at_060744_refuses_and_names_old_turn(self):
        # The real %30 trace: old started 06:06:32, replacement at 06:07:44,
        # old did not complete until 06:09:53.
        self.write_rollout(("task_started", OLD, "2026-10-29T06:06:32.125Z"))
        with self.assertRaisesRegex(NativeSessionRefusal, OLD):
            self.check()

    def test_older_turn_completed_before_respawn_allows(self):
        self.write_rollout(
            ("task_started", OLD, "2026-10-29T06:06:32.125Z"),
            ("task_complete", OLD, "2026-10-29T06:07:40.000Z"))
        snapshot = self.check()
        self.assertEqual((), snapshot.active_turns)
        self.assertEqual((OLD,), snapshot.started_turns)

    def test_authenticated_quiet_history_allows(self):
        self.write_rollout()
        snapshot = self.check()
        self.assertEqual((), snapshot.active_turns)
        self.assertEqual((), snapshot.started_turns)

    def test_empty_history_is_not_quiet(self):
        self.write_rollout(authenticated=False)
        with self.assertRaisesRegex(NativeSessionRefusal, "session_meta"):
            self.check()

    def test_wrong_session_meta_refuses(self):
        with open(self.rollout, "w") as handle:
            handle.write(json.dumps({"type": "session_meta", "payload": {
                "session_id": "another-session", "id": "another-session",
                "cwd": CWD}}) + "\n")
        with self.assertRaisesRegex(NativeSessionRefusal, "session_meta names"):
            self.check()

    def test_new_turn_after_completed_old_turn_is_active_and_refuses(self):
        self.write_rollout(
            ("task_started", OLD, "2026-10-29T06:06:32Z"),
            ("task_complete", OLD, "2026-10-29T06:09:53Z"),
            ("task_started", NEW, "2026-10-29T06:10:43Z"))
        with self.assertRaisesRegex(NativeSessionRefusal, NEW):
            self.check()

    def test_requires_explicit_isolated_codex_home(self):
        with self.assertRaisesRegex(NativeSessionRefusal, "CODEX_HOME"):
            check_native_session_quiescence(
                ["env", "CODEX_HOME=", "codex", "resume", SESSION], CWD)
        with self.assertRaisesRegex(NativeSessionRefusal, "shared"):
            self.check(home="~/.codex")

    def test_missing_session_row_refuses(self):
        with self.assertRaisesRegex(NativeSessionRefusal, "not in the private state"):
            self.check(session="01a00000-0000-0000-0000-000000000000")

    def test_cwd_mismatch_refuses(self):
        with self.assertRaisesRegex(NativeSessionRefusal, "cwd mismatch"):
            self.check(cwd="/tmp/other")

    def test_missing_rollout_refuses(self):
        os.remove(self.rollout)
        with self.assertRaisesRegex(NativeSessionRefusal, "rollout does not exist"):
            self.check()

    def test_completion_without_start_refuses(self):
        self.write_rollout(("task_complete", OLD, "2026-10-29T06:07:00Z"))
        with self.assertRaisesRegex(NativeSessionRefusal, "has no start"):
            self.check()

    def test_duplicate_start_refuses(self):
        self.write_rollout(("task_started", OLD, "2026-10-29T06:06:32Z"),
                           ("task_started", OLD, "2026-10-29T06:06:33Z"))
        with self.assertRaisesRegex(NativeSessionRefusal, "duplicate task_started"):
            self.check()

    def test_malformed_rollout_refuses(self):
        with open(self.rollout, "w") as handle:
            handle.write('{"type": "event_msg", "payload": {broken\n')
        with self.assertRaisesRegex(NativeSessionRefusal, "malformed"):
            self.check()

    def test_thread_id_mismatch_refuses(self):
        with open(self.rollout, "w") as handle:
            handle.write(json.dumps({"type": "session_meta", "payload": {
                "session_id": SESSION, "id": SESSION, "cwd": CWD}}) + "\n")
            handle.write(json.dumps({"type": "event_msg", "payload": {
                "type": "task_started", "turn_id": OLD,
                "thread_id": "different-thread"}}) + "\n")
        with self.assertRaisesRegex(NativeSessionRefusal, "names thread"):
            self.check()

    def test_partial_line_refuses(self):
        with open(self.rollout, "w") as handle:
            handle.write('{"type": "event_msg"')
        with self.assertRaisesRegex(NativeSessionRefusal, "partial line"):
            self.check()

    def test_snapshot_refuses_if_rollout_changes(self):
        self.write_rollout()
        snapshot = self.check()
        with open(self.rollout, "a") as handle:
            handle.write(json.dumps({"type": "event_msg", "payload": {
                "type": "task_started", "turn_id": NEW,
                "thread_id": SESSION}}) + "\n")
        with self.assertRaises(NativeSessionChanged):
            snapshot.assert_unchanged()

    def test_saved_real_timeline_cut_before_old_completion_refuses(self):
        with open(TIMELINE) as handle:
            timeline = json.load(handle)
        for entry in timeline:
            if entry["timestamp"] <= "2026-10-29T06:07:44":
                # Reconstruct only the lifecycle records; no private text.
                payload = {"type": entry["type"], "turn_id": entry["turn_id"],
                           "thread_id": SESSION}
                with open(self.rollout, "a") as handle:
                    handle.write(json.dumps({"timestamp": entry["timestamp"],
                                             "type": "event_msg",
                                             "payload": payload}) + "\n")
        with self.assertRaisesRegex(NativeSessionRefusal, "01a18644"):
            self.check()

    def test_resume_request_parser_ignores_prompt_and_env(self):
        request = find_resume_request(self.argv())
        self.assertEqual(SESSION, request.session_id)
        self.assertEqual(self.home, request.codex_home)
        self.assertEqual(CWD, request.cwd)
        self.assertIsNone(find_resume_request(["codex", "exec", "hi"]))


if __name__ == "__main__":
    unittest.main()
