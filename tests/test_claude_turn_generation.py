"""Exercise the in-pane Claude turn producer without Claude or the daemon.

The producer's two external dependencies are injected: `_root` (the kernel
ancestry lookup) and `_run` (the daemon CLI).  Everything else is the real code
that a Claude hook executes.  Claude exposes no turn id, so correlation is by
(session_id, starter pid/start) plus a Stop-shaped payload gate.
"""
import importlib.util
import json
import os
import stat
import tempfile
import threading
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PRODUCER = ROOT / "integrations" / "claude" / "turn-generation.py"


def load_producer():
    spec = importlib.util.spec_from_file_location("claude_turn_generation", PRODUCER)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class ClaudeTurnGenerationTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.old_state = os.environ.get("DUNKINGSHEEP_CLAUDE_STATE_DIR")
        os.environ["DUNKINGSHEEP_CLAUDE_STATE_DIR"] = str(Path(self.tmp.name) / "state")
        self.addCleanup(self._restore_state)
        self.producer = load_producer()
        self.producer.END_WAIT_S = 0.2
        self.calls = []
        self.root = {"pid": 250, "start": "b", "comm": "claude"}
        self.producer._root = lambda pid=None: dict(self.root)

    def _restore_state(self):
        if self.old_state is None:
            os.environ.pop("DUNKINGSHEEP_CLAUDE_STATE_DIR", None)
        else:
            os.environ["DUNKINGSHEEP_CLAUDE_STATE_DIR"] = self.old_state

    def start_event(self, session="sess-1", prompt="do work"):
        return {
            "hook_event_name": "UserPromptSubmit",
            "session_id": session,
            "transcript_path": "/tmp/transcript",
            "cwd": str(ROOT),
            "permission_mode": "default",
            "prompt": prompt,
        }

    def stop_event(self, session="sess-1", subagent=False):
        event = {
            "hook_event_name": "SubagentStop" if subagent else "Stop",
            "session_id": session,
            "transcript_path": "/tmp/transcript",
            "cwd": str(ROOT),
        }
        if not subagent:
            event["stop_hook_active"] = False
        return event

    def fake_run(self, result=None):
        def run(args, cwd):
            self.calls.append(list(args))
            if result is not None:
                return result
            return 0, {"turn_id": "daemon-" + str(len(self.calls)),
                       "pane_id": "%99", "ended": True}, ""
        return run

    def state_files(self):
        return sorted((Path(self.tmp.name) / "state").glob("*.json"))

    def test_start_then_end_uses_token_and_never_a_pane_label(self):
        self.producer._run = self.fake_run()
        self.producer.start(self.start_event())
        self.producer.end(self.stop_event())
        self.assertEqual(2, len(self.calls))
        self.assertEqual(["--json", "turn", "start", "--source", "claude-hook",
                          "--ttl-s", "1800"], self.calls[0])
        self.assertEqual(["--json", "turn", "end", "--turn-id", "daemon-1",
                          "--reason", "stop"], self.calls[1])
        self.assertFalse(any("--pane" in arg for call in self.calls for arg in call))
        self.assertEqual([], self.state_files(), "the end consumes the token")

    def test_state_file_is_owner_only(self):
        self.producer._run = self.fake_run()
        self.producer.start(self.start_event())
        files = self.state_files()
        self.assertEqual(1, len(files))
        mode = stat.S_IMODE(files[0].stat().st_mode)
        self.assertEqual(0o600, mode, "token state must not be group/world readable")

    def test_duplicate_start_keeps_the_token(self):
        self.producer._run = self.fake_run()
        self.producer.start(self.start_event())
        self.producer.start(self.start_event())
        self.assertEqual(1, len(self.calls), "no second daemon turn opened")
        self.producer.end(self.stop_event())
        self.assertEqual(2, len(self.calls))
        self.assertIn("daemon-1", self.calls[1])

    def test_replaced_root_starts_fresh(self):
        self.producer._run = self.fake_run()
        self.producer.start(self.start_event())
        self.root = {"pid": 251, "start": "c", "comm": "claude"}
        self.producer.start(self.start_event())
        self.assertEqual(2, len(self.calls), "respawned root opens a new turn")
        self.assertEqual(1, len(self.state_files()))

    def test_replaced_root_refuses_the_old_end(self):
        self.producer._run = self.fake_run()
        self.producer.start(self.start_event())
        self.root = {"pid": 251, "start": "c", "comm": "claude"}
        self.producer.end(self.stop_event())
        self.assertEqual(1, len(self.calls), "old root cannot end the turn")

    def test_failed_start_never_produces_an_end(self):
        self.producer._run = self.fake_run(result=(1, None, "refused"))
        self.producer.start(self.start_event())
        self.producer.end(self.stop_event())
        self.assertEqual(1, len(self.calls), "no unbound end")
        self.assertEqual([], self.state_files())

    def test_end_without_start_is_skipped(self):
        self.producer._run = self.fake_run()
        self.producer.end(self.stop_event())
        self.assertEqual([], self.calls)
        self.assertEqual([], self.state_files())

    def test_replay_after_end_is_skipped(self):
        self.producer._run = self.fake_run()
        self.producer.start(self.start_event())
        self.producer.end(self.stop_event())
        self.producer.end(self.stop_event())
        self.assertEqual(2, len(self.calls), "consumed token cannot replay")
        self.assertEqual([], self.state_files())

    def test_expired_turn_is_refused_and_cleaned(self):
        self.producer._run = self.fake_run()
        self.producer.start(self.start_event())
        path = self.state_files()[0]
        state = json.loads(path.read_text())
        state["expires_at"] = 0.0
        path.write_text(json.dumps(state))
        self.producer.end(self.stop_event())
        self.assertEqual(1, len(self.calls), "expired token never sent")
        self.assertEqual([], self.state_files())

    def test_subagent_stop_is_noop_and_end_refuses_subagent_shape(self):
        self.producer._run = self.fake_run()
        self.producer.start(self.start_event())
        self.producer.noop(self.stop_event(subagent=True))
        self.assertEqual(1, len(self.calls), "noop makes no CLI call")
        self.assertEqual(1, len(self.state_files()), "noop keeps the token")
        # Even a miswired SubagentStop reaching end() cannot close the turn:
        # it is not Stop-shaped (no stop_hook_active key).
        self.producer.end(self.stop_event(subagent=True))
        self.assertEqual(1, len(self.calls), "subagent-shaped end refused")
        self.assertEqual(1, len(self.state_files()))
        # The genuine Stop still ends.
        self.producer.end(self.stop_event())
        self.assertEqual(2, len(self.calls))

    def test_end_requires_stop_hook_active_key(self):
        self.producer._run = self.fake_run()
        self.producer.start(self.start_event())
        event = self.stop_event()
        del event["stop_hook_active"]
        self.producer.end(event)
        self.assertEqual(1, len(self.calls))
        self.assertEqual(1, len(self.state_files()))

    def test_invalid_session_or_cwd_is_refused(self):
        self.producer._run = self.fake_run()
        bad = self.start_event(session="not a session id!!!")
        self.producer.start(bad)
        self.assertEqual([], self.calls)
        bad_cwd = self.start_event()
        del bad_cwd["cwd"]
        self.producer.start(bad_cwd)
        self.assertEqual([], self.calls)
        self.assertEqual([], self.state_files())

    def test_only_initiating_prompt_handoff_is_echoed(self):
        self.producer._run = self.fake_run()
        handoff = "nj-" + "a" * 32
        event = self.start_event(prompt="[inbox] TASK t2. Handoff " + handoff + ".")
        self.producer.start(event)
        self.assertEqual(["--handoff-id", handoff], self.calls[0][-2:])
        self.producer.end(self.stop_event())
        plain = self.start_event(prompt="ordinary follow-up")
        self.producer.start(plain)
        self.assertNotIn("--handoff-id", self.calls[-1])

    def test_old_tokenless_end_shape_is_refused(self):
        """The installed script's failure mode: end with no token anywhere.

        The legacy hook calls turn end without --turn-id and keeps no state,
        so the daemon can never authenticate it.  The new producer never
        emits an unbound end: with no start token on disk, nothing is sent.
        """
        self.producer._run = self.fake_run()
        self.producer.end(self.stop_event(session="never-started"))
        self.assertEqual([], self.calls)

    def doc_shaped_subagent_stop(self, session="sess-1"):
        # Shape per the official hooks reference: a SubagentStop carries
        # session identity AND stop_hook_active (plus agent identity), so
        # neither the key nor the payload shape can distinguish it from a
        # Stop.  Only the hook event name can.
        return {
            "hook_event_name": "SubagentStop",
            "session_id": session,
            "transcript_path": "/tmp/subagent-transcript",
            "cwd": str(ROOT),
            "permission_mode": "default",
            "stop_hook_active": True,
            "agent_id": "subagent",
            "agent_type": "Explore",
        }

    def test_start_requires_user_prompt_submit_name(self):
        self.producer._run = self.fake_run()
        misrouted = self.stop_event()
        misrouted["prompt"] = "a stop reaching start"
        self.producer.start(misrouted)
        self.assertEqual([], self.calls)
        self.assertEqual([], self.state_files())

    def test_end_requires_stop_name(self):
        self.producer._run = self.fake_run()
        self.producer.start(self.start_event())
        self.producer.end(self.start_event())
        self.assertEqual(1, len(self.calls), "a start is not an end")
        self.assertEqual(1, len(self.state_files()))

    def test_doc_shaped_subagent_stop_never_ends(self):
        self.producer._run = self.fake_run()
        self.producer.start(self.start_event())
        self.producer.end(self.doc_shaped_subagent_stop())
        self.assertEqual(1, len(self.calls))
        self.assertEqual(1, len(self.state_files()))
        self.producer.noop(self.doc_shaped_subagent_stop())
        self.assertEqual(1, len(self.calls))
        self.assertEqual(1, len(self.state_files()))
        self.producer.end(self.stop_event())
        self.assertEqual(2, len(self.calls))

    def test_old_end_never_consumes_successor(self):
        """t439 follow-up: an end that first observed an older generation must
        not consume a newer one published before its decision lock runs.
        Mirrors the independent probe: swap the token right after the first
        lock exit and assert the replacement survives with no end RPC."""
        self.producer._run = self.fake_run()
        old = {
            "session_id": "sess-1", "cwd": str(ROOT),
            "daemon_turn_id": "old-token", "started_at": 1000.0,
            "root_pid": 250, "root_start": "b",
            "expires_at": time.time() + 300,
        }
        path = Path(self.tmp.name) / "state" / "sess-1.json"
        self.producer._write_state(path, old)
        real_lock = self.producer._Lock
        prod = self.producer
        count = [0]

        class Interleave(real_lock):
            def __exit__(self, *args):
                try:
                    return super().__exit__(*args)
                finally:
                    count[0] += 1
                    if count[0] == 1:
                        prod._write_state(path, dict(old, daemon_turn_id="new-token"))

        self.producer._Lock = Interleave
        try:
            self.producer.end(self.stop_event())
        finally:
            self.producer._Lock = real_lock
        ends = [c for c in self.calls if c[1:3] == ["turn", "end"]]
        self.assertEqual([], ends, "successor token must not be spent")
        current = json.loads(path.read_text())
        self.assertEqual("new-token", current["daemon_turn_id"],
                         "replacement state preserved")

    def test_concurrent_ends_send_exactly_one_rpc(self):
        self.producer._run = self.fake_run()
        self.producer.start(self.start_event())
        barrier = threading.Barrier(2)

        def one_end():
            barrier.wait()
            self.producer.end(self.stop_event())

        threads = [threading.Thread(target=one_end) for _ in range(2)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(timeout=30)
        ends = [call for call in self.calls if call[1:3] == ["turn", "end"]]
        self.assertEqual(1, len(ends), "exactly one winner RPCs")
        self.assertEqual([], self.state_files(), "winner consumes the token")

    def test_state_dir_is_owner_only_even_if_precreated_wide(self):
        from pathlib import Path as P
        wide = P(self.tmp.name) / "state"
        wide.mkdir(exist_ok=True)
        os.chmod(wide, 0o755)
        self.producer._run = self.fake_run()
        self.producer.start(self.start_event())
        self.assertEqual(0o700, stat.S_IMODE(wide.stat().st_mode))
        self.assertEqual(1, len(self.state_files()))

    def test_main_dispatches_start_end_noop(self):
        seen = []

        def fake_start(event):
            seen.append(("start", event))

        def fake_end(event):
            seen.append(("end", event))

        def fake_noop(event):
            seen.append(("noop", event))

        self.producer.start, self.producer.end, self.producer.noop = (
            fake_start, fake_end, fake_noop)
        self.producer._read_event = lambda: {"session_id": "s"}
        self.assertEqual(0, self.producer.main(["start"]))
        self.assertEqual(0, self.producer.main(["end"]))
        self.assertEqual(0, self.producer.main(["noop"]))
        self.assertEqual(0, self.producer.main(["bogus"]))
        self.assertEqual(["start", "end", "noop"],
                         [name for name, _event in seen])


if __name__ == "__main__":
    unittest.main()
