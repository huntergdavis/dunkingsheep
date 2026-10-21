import os
import shutil
import subprocess
import unittest
from unittest import mock

from herdr_client import (
    AGENT_KINDS,
    HerdrClient,
    SEND_TEXT_CHUNK_CHARS,
    SEND_TEXT_CHUNK_DELAY_S,
    SEND_TEXT_SETTLE_DELAY_S,
    _text_chunks,
)


class SendTextAndEnterTests(unittest.TestCase):
    def test_short_text_waits_before_enter(self):
        client = HerdrClient(binary="herdr")
        events = []

        def run(args, timeout=None):
            events.append(("run", args))
            return 0, "", ""

        client._run = run
        with mock.patch("herdr_client.time.sleep") as sleep:
            sleep.side_effect = lambda delay: events.append(("sleep", delay))
            result = client.send_text_and_enter("pane-1", "continue")

        self.assertEqual((True, "sent"), result)
        self.assertEqual(
            [
                ("run", ["pane", "send-text", "pane-1", "continue"]),
                ("sleep", SEND_TEXT_SETTLE_DELAY_S),
                ("run", ["pane", "send-keys", "pane-1", "Enter"]),
            ],
            events,
        )

    def test_long_text_is_chunked_before_enter(self):
        client = HerdrClient(binary="herdr")
        text = "a" * SEND_TEXT_CHUNK_CHARS + "second chunk"
        events = []

        def run(args, timeout=None):
            events.append(("run", args))
            return 0, "", ""

        client._run = run
        with mock.patch("herdr_client.time.sleep") as sleep:
            sleep.side_effect = lambda delay: events.append(("sleep", delay))
            result = client.send_text_and_enter("pane-1", text)

        self.assertEqual((True, "sent"), result)
        self.assertEqual(
            [
                (
                    "run",
                    [
                        "pane",
                        "send-text",
                        "pane-1",
                        "a" * SEND_TEXT_CHUNK_CHARS,
                    ],
                ),
                ("sleep", SEND_TEXT_CHUNK_DELAY_S),
                ("run", ["pane", "send-text", "pane-1", "second chunk"]),
                ("sleep", SEND_TEXT_SETTLE_DELAY_S),
                ("run", ["pane", "send-keys", "pane-1", "Enter"]),
            ],
            events,
        )

    def test_chunking_does_not_split_crlf(self):
        text = "a" * (SEND_TEXT_CHUNK_CHARS - 1) + "\r\nrest"
        chunks = list(_text_chunks(text))

        self.assertEqual(text, "".join(chunks))
        self.assertFalse(chunks[0].endswith("\r"))
        self.assertTrue(chunks[1].startswith("\r\n"))

    def test_one_character_chunks_still_keep_crlf_together(self):
        chunks = list(_text_chunks("\r\nx", max_chars=1))

        self.assertEqual(["\r\n", "x"], chunks)

    def test_does_not_wait_or_press_enter_when_text_send_fails(self):
        client = HerdrClient(binary="herdr")
        client._run = mock.Mock(return_value=(1, "", "send failed"))

        with mock.patch("herdr_client.time.sleep") as sleep:
            result = client.send_text_and_enter("pane-1", "continue")

        self.assertEqual((False, "send failed"), result)
        sleep.assert_not_called()
        client._run.assert_called_once_with(
            ["pane", "send-text", "pane-1", "continue"]
        )

    def test_stops_if_a_later_chunk_fails(self):
        client = HerdrClient(binary="herdr")
        text = "a" * SEND_TEXT_CHUNK_CHARS + "tail"
        client._run = mock.Mock(
            side_effect=[(0, "", ""), (1, "", "second chunk failed")]
        )

        with mock.patch("herdr_client.time.sleep") as sleep:
            result = client.send_text_and_enter("pane-1", text)

        self.assertEqual((False, "second chunk failed"), result)
        self.assertEqual(2, client._run.call_count)
        sleep.assert_called_once_with(SEND_TEXT_CHUNK_DELAY_S)


class StartAgentShapeTests(unittest.TestCase):
    """The grammar `herdr agent start` actually accepts, not the old shape.

    The old client sent --workspace/--tab/--split/--cwd directly to `agent
    start`; herdr 0.9.5 rejects all of them. These tests pin both halves: the
    installed help text, and the argv our client sends.
    """

    @staticmethod
    def _installed_herdr():
        for candidate in (shutil.which("herdr-real"),
                          os.path.expanduser("~/.local/bin/herdr-real")):
            if candidate and os.path.exists(candidate):
                return candidate
        return None

    def test_installed_help_says_kind_and_pane_not_workspace(self):
        binary = self._installed_herdr()
        if not binary:
            self.skipTest("herdr-real is not installed")
        proc = subprocess.run([binary, "agent", "start", "--help"],
                              capture_output=True, text=True, timeout=15)
        help_text = proc.stdout + proc.stderr
        self.assertEqual(0, proc.returncode, help_text)
        for flag in ("--kind", "--pane"):
            self.assertIn(flag, help_text)
        for flag in ("--workspace", "--tab", "--split", "--cwd"):
            self.assertNotIn(flag, help_text,
                             f"installed herdr now accepts {flag}; update start_agent")

    def test_tab_placement_splits_then_calls_agent_start_in_the_new_pane(self):
        client = HerdrClient(binary="herdr")
        client.list_panes = lambda: [{"pane_id": "w1:p1", "tab_id": "w1:t1"}]
        calls = []

        def run(args, timeout=None):
            calls.append(list(args))
            if args[:2] == ["pane", "split"]:
                return True, {"pane": {"pane_id": "w1:p2"}}
            if args[:2] == ["agent", "start"]:
                return True, {"agent": {"pane_id": "w1:p2",
                                         "cwd": "/work"}}
            return True, {}

        client._run_result = run
        ok, result = client.start_agent(
            "writer", ["claude", "--model", "opus"], tab_id="w1:t1",
            cwd="/work")

        self.assertTrue(ok)
        self.assertEqual("w1:p2", result["pane_id"])
        split = next(args for args in calls if args[:2] == ["pane", "split"])
        self.assertEqual("w1:p1", split[2])
        self.assertIn("right", split)
        agent = next(args for args in calls if args[:2] == ["agent", "start"])
        self.assertEqual(
            ["agent", "start", "writer", "--kind", "claude", "--pane"],
            agent[:6])
        self.assertEqual("w1:p2", agent[6])
        self.assertIn("--timeout", agent)
        for flag in ("--workspace", "--tab", "--split", "--cwd"):
            self.assertNotIn(flag, agent)
        self.assertEqual(["--model", "opus"], agent[agent.index("--") + 1:])
        self.assertTrue(any(args[:3] == ["pane", "rename", "w1:p2"]
                            for args in calls))

    def test_workspace_placement_creates_a_tab_then_starts_there(self):
        client = HerdrClient(binary="herdr")
        calls = []

        def run(args, timeout=None):
            calls.append(list(args))
            if args[:2] == ["tab", "create"]:
                return True, {"root_pane": {"pane_id": "w1:p9"}}
            if args[:2] == ["agent", "start"]:
                return True, {"agent": {"pane_id": "w1:p9"}}
            return True, {}

        client._run_result = run
        ok, result = client.start_agent("helper", ["codex"], workspace_id="w1")

        self.assertTrue(ok)
        tab = next(args for args in calls if args[:2] == ["tab", "create"])
        self.assertIn("--workspace", tab)
        self.assertIn("w1", tab)
        agent = next(args for args in calls if args[:2] == ["agent", "start"])
        self.assertEqual("w1:p9", agent[6])
        self.assertEqual([], agent[agent.index("--") + 1:])

    def test_unknown_kind_is_refused_before_creating_anything(self):
        client = HerdrClient(binary="herdr")
        calls = []
        client._run_result = lambda args, timeout=None: (calls.append(args), (True, {}))[1]
        ok, message = client.start_agent("x", ["bash", "-lc", "claude"])
        self.assertFalse(ok)
        self.assertIn("unsupported agent kind", message)
        self.assertEqual([], calls)

    def test_agent_start_failure_closes_the_shell_it_created(self):
        client = HerdrClient(binary="herdr")
        client.list_panes = lambda: [{"pane_id": "w1:p1", "tab_id": "w1:t1"}]
        calls = []

        def run(args, timeout=None):
            calls.append(list(args))
            if args[:2] == ["pane", "split"]:
                return True, {"pane": {"pane_id": "w1:p2"}}
            if args[:2] == ["agent", "start"]:
                return False, "herdr refused"
            return True, {}

        client._run_result = run
        ok, message = client.start_agent("writer", ["claude"], tab_id="w1:t1")
        self.assertFalse(ok)
        self.assertEqual("herdr refused", message)
        self.assertTrue(any(args == ["pane", "close", "w1:p2"] for args in calls))

    def test_every_documented_kind_is_accepted_by_the_client(self):
        self.assertIn("claude", AGENT_KINDS)
        self.assertIn("codex", AGENT_KINDS)
        self.assertIn("pi", AGENT_KINDS)
        self.assertIn("opencode", AGENT_KINDS)


if __name__ == "__main__":
    unittest.main()
