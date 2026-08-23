import unittest
from unittest import mock

from herdr_client import HerdrClient, SEND_TEXT_SETTLE_DELAY_S


class SendTextAndEnterTests(unittest.TestCase):
    def test_waits_between_text_and_enter(self):
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


if __name__ == "__main__":
    unittest.main()
