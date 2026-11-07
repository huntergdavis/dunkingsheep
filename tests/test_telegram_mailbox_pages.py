"""t665: owner pages reach Telegram from the owner mailbox, and only from there.

The Rust daemon records a watchdog page as an owner-mailbox row with the owner-page sender
and no reply link. The bridge reads it from owner_outbox(transport="telegram"), sends it,
confirms it with owner_delivered, and spends the same 3-per-hour page budget as a legacy
page. A reply stays a reply. Nothing here touches the desk store.
"""

import importlib.util
import os
import sys
import unittest

HAVE_FASTAPI = importlib.util.find_spec("fastapi") is not None
NEEDS_FASTAPI = unittest.skipUnless(
    HAVE_FASTAPI, "bridges/telegram_bridge imports web.app, which needs fastapi")

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(__file__)), "bridges"))

import _bootstrap  # noqa: F401,E402


class FakeBot:
    def __init__(self):
        self.sent = []

    def call(self, method, **kwargs):
        if method == "sendMessage":
            self.sent.append(kwargs.get("text"))
        return {}


class FakeClient:
    """A daemon that holds the given outbox rows and records every receipt."""

    def __init__(self, rows):
        self.rows = rows
        self.receipts = []

    def call(self, name, **kwargs):
        if name == "owner_outbox":
            return {"messages": list(self.rows)}
        if name == "owner_delivered":
            self.receipts.append(kwargs["message_id"])
            return {}
        return {}


def _page(message_id, body):
    return {"message_id": message_id, "sender": "owner-page", "transport": "telegram",
            "body": body}


def _reply(message_id, body):
    return {"message_id": message_id, "sender": None, "transport": "telegram",
            "reply_to": "q1", "body": body}


@NEEDS_FASTAPI
class MailboxPagesTests(unittest.TestCase):
    def setUp(self):
        from unittest import mock
        import telegram_bridge
        self.bridge = telegram_bridge
        # The desk-side and config seams are stubbed for this test only; the module is
        # shared with the other bridge suites, so every stub is undone at cleanup.
        stubs = (("_prepare_route", lambda config: "cos"),
                 ("_legacy_pages", lambda config, channel: []),
                 ("save_config", lambda config: None),
                 ("_write_status_file", lambda *args, **kwargs: None))
        for name, stub in stubs:
            patcher = mock.patch.object(telegram_bridge, name, stub)
            patcher.start()
            self.addCleanup(patcher.stop)

    def forward(self, rows, config=None):
        config = config if config is not None else {"chat_id": 42}
        bot, client = FakeBot(), FakeClient(rows)
        sent = self.bridge.send_owner_outbox(config, bot, client)
        return config, bot, client, sent

    def test_a_mailbox_page_is_sent_once_and_receipted_by_its_message_id(self):
        _, bot, client, _ = self.forward([_page("i90", "PAGE OWNER security: token exposed")])
        self.assertEqual(["PAGE OWNER security: token exposed"], bot.sent)
        self.assertEqual(["i90"], client.receipts)

    def test_a_reply_is_still_sent_as_a_reply_and_receipted(self):
        _, bot, client, sent = self.forward([_reply("i91", "Answer to your question.")])
        self.assertEqual(["Answer to your question."], bot.sent)
        self.assertEqual(["i91"], client.receipts)
        self.assertEqual(["reply"], [item["kind"] for item in sent])

    def test_mailbox_pages_share_the_hourly_budget_and_the_held_one_gets_no_receipt(self):
        rows = [_page(f"i{n}", f"PAGE OWNER outage: incident {n}") for n in range(1, 5)]
        config, bot, client, _ = self.forward(rows)
        self.assertEqual(3, len(bot.sent), "three pages per hour, the fourth is held")
        self.assertEqual(["i1", "i2", "i3"], client.receipts)
        self.assertEqual(3, len(config["page_sent_at"]), "each delivered page spends the budget")

    def test_a_mailbox_page_does_not_move_the_legacy_page_watermark(self):
        config, _, _, _ = self.forward([_page("i90", "PAGE OWNER money: spend cap")])
        self.assertNotIn("last_page_id", config)


if __name__ == "__main__":
    unittest.main()
