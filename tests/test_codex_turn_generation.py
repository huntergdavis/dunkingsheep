"""Exercise the in-pane Codex turn producer without Codex or the daemon.

The producer's two external dependencies are injected: `_root` (the kernel
ancestry lookup) and `_run` (the daemon CLI).  Everything else is the real code
that a Codex hook executes.
"""
import importlib.util
import json
import os
import tempfile
import threading
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PRODUCER = ROOT / "integrations" / "codex" / "turn-generation.py"


def load_producer():
    spec = importlib.util.spec_from_file_location("codex_turn_generation", PRODUCER)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class CodexTurnGenerationTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.old_state = os.environ.get("DUNKINGSHEEP_CODEX_STATE_DIR")
        os.environ["DUNKINGSHEEP_CODEX_STATE_DIR"] = str(Path(self.tmp.name) / "state")
        self.addCleanup(self._restore_state)
        self.producer = load_producer()
        self.producer.END_WAIT_S = 0.2
        self.calls = []
        self.root = {"pid": 250, "start": "b", "comm": "codex"}
        self.producer._root = lambda pid=None: dict(self.root)

    def _restore_state(self):
        if self.old_state is None:
            os.environ.pop("DUNKINGSHEEP_CODEX_STATE_DIR", None)
        else:
            os.environ["DUNKINGSHEEP_CODEX_STATE_DIR"] = self.old_state

    def event(self, action="start", turn="turn-1", session="sess-1"):
        return {
            "hook_event_name": "UserPromptSubmit" if action == "start" else "Stop",
            "session_id": session,
            "turn_id": turn,
            "cwd": str(ROOT),
            "prompt": "SECRET PROMPT MUST NOT BE LOGGED",
        }

    def fake_run(self, result=None, block=None):
        def run(args, cwd):
            self.calls.append(list(args))
            if block is not None:
                block.wait(5)
            if result is not None:
                return result
            return 0, {"turn_id": "daemon-" + str(len(self.calls)),
                       "pane_id": "%99", "ended": True}, ""
        return run

    def state_files(self):
        return sorted((Path(self.tmp.name) / "state").glob("*.json"))

    def test_start_then_end_uses_token_and_never_a_pane_label(self):
        self.producer._run = self.fake_run()
        self.producer.start(self.event())
        self.producer.end(self.event(action="end"))
        self.assertEqual(2, len(self.calls))
        self.assertEqual(["--json", "turn", "start", "--source", "codex-hook",
                          "--ttl-s", "1800"], self.calls[0])
        self.assertEqual(["--json", "turn", "end", "--turn-id", "daemon-1",
                          "--reason", "stop"], self.calls[1])
        self.assertFalse(any("--pane" in arg for call in self.calls for arg in call))
        self.assertEqual([], self.state_files(), "the end consumes the token")

    def test_duplicate_start_is_idempotent(self):
        self.producer._run = self.fake_run()
        self.producer.start(self.event())
        self.producer.start(self.event())
        self.assertEqual(1, len(self.calls))
        self.assertEqual(1, len(self.state_files()))

    def test_only_initiating_prompt_handoff_is_echoed(self):
        self.producer._run = self.fake_run()
        event = self.event()
        handoff = 'nj-' + 'a' * 32
        event['prompt'] = '[inbox] TASK t2. Handoff ' + handoff + '.'
        self.producer.start(event)
        self.assertEqual(['--handoff-id', handoff], self.calls[0][-2:])
        self.producer.end(self.event(action='end'))
        self.producer.start(self.event(turn='turn-2'))
        self.assertNotIn('--handoff-id', self.calls[-1])
        self.assertEqual(1, len(self.state_files()))

    def test_failed_start_never_produces_an_end(self):
        self.producer._run = self.fake_run(result=(1, None, "refused"))
        self.producer.start(self.event())
        self.producer.end(self.event(action="end"))
        self.assertEqual(1, len(self.calls), "no unbound end")
        self.assertEqual([], self.state_files())

    def test_stale_turn_does_not_end_and_does_not_consume_the_new_token(self):
        self.producer._run = self.fake_run()
        self.producer.start(self.event(turn="turn-1"))
        self.producer.end(self.event(action="end", turn="turn-2"))
        self.assertEqual(1, len(self.calls), "stale Stop is refused")
        self.assertEqual(1, len(self.state_files()))
        self.producer.end(self.event(action="end", turn="turn-1"))
        self.assertEqual(2, len(self.calls), "the correct Stop still ends")

    def test_replaced_root_refuses_and_leaves_the_new_token_alone(self):
        self.producer._run = self.fake_run()
        self.producer.start(self.event())
        self.root = {"pid": 251, "start": "c", "comm": "codex"}
        self.producer.end(self.event(action="end"))
        self.assertEqual(1, len(self.calls), "a replaced root must not end")
        self.assertEqual(1, len(self.state_files()),
                         "the old token is not consumed by the new root")

    def test_expired_token_is_not_ended(self):
        self.producer._run = self.fake_run()
        self.producer.start(self.event())
        path = self.state_files()[0]
        state = json.loads(path.read_text())
        state["expires_at"] = time.time() - 1
        path.write_text(json.dumps(state))
        self.producer.end(self.event(action="end"))
        self.assertEqual(1, len(self.calls), "expired turn has no end")
        self.assertEqual([], self.state_files())

    def test_end_waits_for_a_fast_start_and_then_ends_it_once(self):
        release = threading.Event()
        self.producer._run = self.fake_run(block=release)
        start = threading.Thread(target=self.producer.start, args=(self.event(),))
        start.start()
        end = threading.Thread(target=self.producer.end,
                               args=(self.event(action="end"),))
        end.start()
        time.sleep(0.1)
        self.assertEqual(1, len(self.calls), "end cannot pass an in-flight start")
        release.set()
        start.join(5)
        end.join(5)
        self.assertEqual(2, len(self.calls))
        self.assertEqual("turn", self.calls[1][1])
        self.assertEqual("daemon-1", self.calls[1][4])

    def test_second_end_cannot_reuse_a_consumed_token(self):
        self.producer._run = self.fake_run()
        self.producer.start(self.event())
        self.producer.end(self.event(action="end"))
        self.producer.end(self.event(action="end"))
        self.assertEqual(2, len(self.calls))

    def test_missing_session_or_turn_is_refused_without_a_call(self):
        self.producer._run = self.fake_run()
        self.producer.start({"turn_id": "turn-1", "cwd": str(ROOT)})
        self.producer.start({"session_id": "sess-1", "cwd": str(ROOT)})
        self.assertEqual([], self.calls)

    def test_log_does_not_contain_prompt_text_or_tokens(self):
        self.producer._run = self.fake_run()
        self.producer.start(self.event())
        self.producer.end(self.event(action="end"))
        log = (Path(self.tmp.name) / "codex-turn.log").read_text()
        self.assertNotIn("SECRET PROMPT", log)
        self.assertNotIn("daemon-1", log)


class CodexAncestryTests(unittest.TestCase):
    def test_nearest_codex_ancestor_is_the_stable_root(self):
        producer = load_producer()
        rows = {
            300: {"pid": 300, "ppid": 250, "start": "a", "comm": "sh"},
            250: {"pid": 250, "ppid": 200, "start": "b", "comm": "codex"},
            200: {"pid": 200, "ppid": 1, "start": "c", "comm": "bash"},
        }
        names = {250: "codex"}

        def stat(pid):
            return rows.get(pid)

        def agent_name(pid, _stat):
            return names.get(pid)

        root = producer._root(300, stat=stat, agent_name=agent_name)
        self.assertEqual({"pid": 250, "start": "b", "comm": "codex"}, root)

    def test_no_codex_ancestor_refuses_instead_of_guessing(self):
        producer = load_producer()

        def stat(pid):
            return {300: {"pid": 300, "ppid": 1, "start": "a", "comm": "python3"}}.get(pid)

        def agent_name(pid, _stat):
            return None

        self.assertIsNone(producer._root(300, stat=stat, agent_name=agent_name))


if __name__ == "__main__":
    unittest.main()
