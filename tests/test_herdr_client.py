import unittest
from unittest import mock

from herdr_client import (
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


if __name__ == "__main__":
    unittest.main()
