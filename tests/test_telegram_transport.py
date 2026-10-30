"""t594: Telegram carries answers to Telegram questions, and pages. Nothing else.

The bridge used to forward every agent-role row as a "brief", so the owner's phone
received web echoes, reports and digests. These tests pin the policy in
`bridges/telegram_transport.py` and the one send path that uses it, with a fake
Telegram and a fake daemon: no token, no network, no desk.
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(__file__)), "bridges"))

import _bootstrap  # noqa: F401
import telegram_transport as transport  # noqa: E402


class FakeTelegram:
    def __init__(self):
        self.sent = []
        self.fail_next = False

    def send(self, body):
        if self.fail_next:
            self.fail_next = False
            raise RuntimeError("Telegram sendMessage: 502")
        self.sent.append(body)


class FakeDaemon:
    """Stands in for the client: the outbox it would return and the receipts it gets."""

    def __init__(self, replies=(), pages=()):
        self.replies = list(replies)
        self.pages = list(pages)
        self.receipts = []
        self.calls = []

    def outbox(self, transport_name):
        self.calls.append(("owner_outbox", transport_name))
        return {"messages": self.replies}

    def delivered(self, message_id, kind):
        self.calls.append(("owner_delivered", message_id, kind))
        self.receipts.append((message_id, kind))


def reply(message_id, body="answered on telegram"):
    return {"message_id": message_id, "body": body, "transport": "telegram",
            "reply_to": "m1"}


def page(message_id, body="PAGE OWNER outage: web desk down"):
    return {"message_id": message_id, "body": body, "transport": "telegram"}


class PolicyTests(unittest.TestCase):
    def test_a_web_question_produces_nothing_for_telegram(self):
        """The acceptance line: the daemon's telegram outbox holds no web reply, so
        nothing can be sent for one."""
        daemon = FakeDaemon(replies=[], pages=[])
        bot = FakeTelegram()
        items = transport.outbox_items(daemon.outbox("telegram")["messages"], [],
                                       [], now=1000.0)
        self.assertEqual([], items)
        self.assertEqual([], transport.deliver(
            items, bot.send, lambda i, k: daemon.delivered(i, k)))
        self.assertEqual([], bot.sent)

    def test_a_telegram_reply_is_sent_and_receipted(self):
        daemon = FakeDaemon(replies=[reply("m9", "the answer")])
        bot = FakeTelegram()
        items = transport.outbox_items(daemon.outbox("telegram")["messages"], [],
                                       [], now=1000.0)
        sent = transport.deliver(items, bot.send,
                                 lambda i, k: daemon.delivered(i, k))
        self.assertEqual(["the answer"], bot.sent)
        self.assertEqual([("m9", "reply")], daemon.receipts)
        self.assertEqual("reply", sent[0]["kind"])

    def test_a_brief_or_digest_is_never_selected(self):
        """Nothing here reads a brief channel: the only inputs are the transport
        outbox and page rows, so a report/digest/web echo has no path to the phone."""
        daemon = FakeDaemon(replies=[], pages=[])
        bot = FakeTelegram()
        for body in ("BRIEF #42 ...", "DIGEST ...", "web conversation echo"):
            self.assertFalse(transport.is_page(body), body)
        items = transport.outbox_items(daemon.outbox("telegram")["messages"], [],
                                       [], now=1000.0)
        transport.deliver(items, bot.send, lambda i, k: daemon.delivered(i, k))
        self.assertEqual([], bot.sent)

    def test_pages_are_sent_and_classed(self):
        daemon = FakeDaemon(pages=[page("m10")])
        bot = FakeTelegram()
        items = transport.outbox_items([], daemon.pages, [], now=1000.0)
        transport.deliver(items, bot.send, lambda i, k: daemon.delivered(i, k))
        self.assertEqual(1, len(bot.sent))
        self.assertEqual("outage", items[0]["page_class"])
        self.assertEqual([("m10", "page")], daemon.receipts)

    def test_the_fourth_page_in_an_hour_is_held_not_dropped(self):
        now = 10_000.0
        already = [now - 60, now - 120, now - 180]
        items = transport.outbox_items([], [page("m11")], already, now=now)
        self.assertTrue(items[0].get("hold"))
        self.assertIn("3 pages per hour", items[0]["reason"])
        bot = FakeTelegram()
        transport.deliver(items, bot.send, lambda i, k: None)
        self.assertEqual([], bot.sent, "a held page is not sent")

    def test_a_page_older_than_the_window_does_not_count_against_the_budget(self):
        now = 10_000.0
        already = [now - 4000, now - 5000, now - 6000]
        items = transport.outbox_items([], [page("m12")], already, now=now)
        self.assertFalse(items[0].get("hold"))

    def test_a_failed_send_stops_the_batch_and_records_no_receipt(self):
        daemon = FakeDaemon(replies=[reply("m13", "first"), reply("m14", "second")])
        bot = FakeTelegram()
        bot.fail_next = True
        items = transport.outbox_items(daemon.outbox("telegram")["messages"], [],
                                       [], now=1000.0)
        with self.assertRaises(RuntimeError):
            transport.deliver(items, bot.send, lambda i, k: daemon.delivered(i, k))
        self.assertEqual([], daemon.receipts,
                         "no receipt for a send that did not happen")
        self.assertEqual([], bot.sent)


if __name__ == "__main__":
    unittest.main()
