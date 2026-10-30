"""Pages the phone did not get are retried, not dropped (t667).

Three findings from the t594 review, each pinned here:

1. `_legacy_pages` advanced the page watermark BEFORE delivery, so a page held by
   the three-per-hour cap - or one whose send raised and stopped the batch - was
   never selected again. "Held, not dropped" was a claim the code did not keep.
2. a photo or file WITH A CAPTION was silently lost: `text` is the caption, the
   text branch returned before the attachment was downloaded, and the owner was
   told the message had been recorded.
3. the first tick after the upgrade had no `last_page_id`, so it scanned the
   oldest 20 agent rows and would page history to the phone.
"""

import os
import sys
import tempfile
import types
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))
sys.path.insert(0, os.path.join(os.path.dirname(os.path.dirname(__file__)), "bridges"))

import _bootstrap  # noqa: F401


class FakeBot:
    def __init__(self, fail_on=None):
        self.sent = []
        self.fail_on = fail_on

    def call(self, method, **kwargs):
        if method != "sendMessage":
            return {}
        if self.fail_on and self.fail_on in kwargs.get("text", ""):
            raise RuntimeError("Telegram sendMessage: 502")
        self.sent.append(kwargs.get("text"))
        return {}

    def download(self, file_id):
        return b"photo-bytes"


class FakeClient:
    def __init__(self):
        self.calls = []

    def call(self, name, **kwargs):
        self.calls.append((name, kwargs))
        if name == "owner_outbox":
            return {"messages": []}
        if name == "owner_ingest":
            return {"message": {"message_id": "i7"}, "duplicate": False}
        return {}


def _bridge():
    import telegram_bridge
    return telegram_bridge


class PageWatermarkTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.bridge = _bridge()
        # A desk store with four agent rows, three of them pages.
        self.rows = [{"id": 11, "body": "PAGE OWNER outage: web desk down"},
                     {"id": 12, "body": "PAGE OWNER money: provider cap hit"},
                     {"id": 13, "body": "an ordinary agent row, not a page"},
                     {"id": 14, "body": "PAGE OWNER security: token exposed"}]
        self.config_path = os.path.join(self.tmp.name, "telegram.json")
        self.saved = []

        bridge = self.bridge
        test = self               # the fake reads test.rows, so a test can replace it

        class FakeConn:
            """Honours what the query asks for: the id floor, the LIKE and the LIMIT.

            The first version of this fake returned every row past the watermark and
            ignored the LIMIT, which is exactly why it could not see the starvation
            the reviewer found (t678): the bug lived in the interaction between the
            SQL window and the Python filter.
            """

            def execute(self, sql, params=()):
                rows = test.rows
                if "COALESCE(MAX(id)" in sql:
                    newest = max([row["id"] for row in rows] or [0])
                    return types.SimpleNamespace(fetchone=lambda: {"newest": newest})
                watermark = params[1]
                needle = params[2].strip("%").upper() if len(params) > 2 else None
                limit = 20
                selected = [row for row in rows if row["id"] > watermark]
                if needle is not None:
                    selected = [row for row in selected
                                if needle in str(row["body"]).upper()]
                selected = sorted(selected, key=lambda row: row["id"])[:limit]
                return types.SimpleNamespace(fetchall=lambda: list(selected))

            def __enter__(self):
                return self

            def __exit__(self, *exc):
                return False

        bridge.desk._db = lambda: FakeConn()
        bridge.save_config = lambda config: self.saved.append(dict(config))
        bridge._prepare_route = lambda config: "cos"
        bridge._channel_name = lambda channel: "CoS"
        bridge._write_status_file = lambda last_id, channel: None
        self._saved = self.saved

    def tearDown(self):
        self.tmp.cleanup()

    def test_reading_pages_does_not_move_the_watermark(self):
        config = {"chat_id": 1, "last_page_id": 10}
        pages = self.bridge._legacy_pages(config, "cos")
        self.assertEqual(["page-11", "page-12", "page-14"],
                         [page["message_id"] for page in pages])
        self.assertEqual(10, config["last_page_id"],
                         "the watermark moved before anything was delivered")
        self.assertEqual([], self._saved, "reading pages wrote the config")

    def test_a_held_page_is_selected_again_on_the_next_tick(self):
        config = {"chat_id": 1, "last_page_id": 10,
                  # Three pages in the window: the budget is spent, so every page
                  # read now is held.
                  "page_sent_at": [self.bridge.time.time()] * 3}
        bot, client = FakeBot(), FakeClient()
        self.bridge.send_owner_outbox(config, bot, client=client)
        self.assertEqual([], bot.sent, "a held page must not be sent")
        self.assertEqual(10, config["last_page_id"],
                         "a held page advanced the watermark and is gone forever")
        again = self.bridge._legacy_pages(config, "cos")
        self.assertIn("page-11", [page["message_id"] for page in again],
                      "the held page was not offered again")

    def test_a_delivered_page_moves_the_watermark_and_the_next_one_follows(self):
        config = {"chat_id": 1, "last_page_id": 10}
        bot, client = FakeBot(), FakeClient()
        self.bridge.send_owner_outbox(config, bot, client=client)
        self.assertEqual(3, len(bot.sent))
        self.assertEqual(14, config["last_page_id"])
        self.assertEqual([], self.bridge._legacy_pages(config, "cos"),
                         "delivered pages came back")

    def test_a_failed_send_does_not_advance_past_the_page_that_failed(self):
        config = {"chat_id": 1, "last_page_id": 10}
        bot = FakeBot(fail_on="money")
        # A send that raises stops the batch and propagates: the tick loop above
        # decides what a failed transport means, not this function.
        with self.assertRaises(RuntimeError):
            self.bridge.send_owner_outbox(config, bot, client=FakeClient())
        self.assertEqual(11, config["last_page_id"],
                         "the batch stopped at id 12, so 12 must stay unread")
        pending = [page["message_id"] for page in self.bridge._legacy_pages(config, "cos")]
        self.assertEqual(["page-12", "page-14"], pending)

    def test_pages_delivered_in_one_tick_each_spend_the_hourly_budget(self):
        """t685: three pages in one tick left ONE timestamp, so the next tick sent two more.

        The cap is the safety property of the one path the owner kept for pages: five
        pages inside the hour against a stated cap of three.
        """
        config = {"chat_id": 1, "last_page_id": 10}
        bot, client = FakeBot(), FakeClient()
        self.bridge.send_owner_outbox(config, bot, client=client)
        self.assertEqual(3, len(bot.sent), "tick 1 should deliver the three pages")
        self.assertEqual(3, len(config["page_sent_at"]),
                         "three delivered pages must be three entries in the budget")
        self.rows += [{"id": 15, "body": "PAGE OWNER outage: a fourth page"},
                      {"id": 16, "body": "PAGE OWNER money: a fifth page"}]
        self.bridge.send_owner_outbox(config, bot, client=client)
        self.assertEqual(3, len(bot.sent),
                         "the budget was spent in tick 1: tick 2 must hold, not send")
        self.assertEqual(14, config["last_page_id"], "held pages must stay unread")
        pending = [page["message_id"] for page in self.bridge._legacy_pages(config, "cos")]
        self.assertEqual(["page-15", "page-16"], pending)

    def test_a_page_older_than_the_window_does_not_count_after_a_tick(self):
        old = self.bridge.time.time() - 2 * 3600
        config = {"chat_id": 1, "last_page_id": 10, "page_sent_at": [old, old]}
        bot = FakeBot()
        self.bridge.send_owner_outbox(config, bot, client=FakeClient())
        self.assertEqual(3, len(bot.sent), "stale entries must not spend the budget")
        self.assertEqual(3, len(config["page_sent_at"]),
                         "stale entries are pruned; only the three new pages remain")

    def test_a_first_run_carries_the_old_watermark_over(self):
        config = {"chat_id": 1, "last_brief_id": 13}
        pages = self.bridge._legacy_pages(config, "cos")
        self.assertEqual(13, config["last_page_id"])
        self.assertEqual(["page-14"], [page["message_id"] for page in pages])

    def test_a_first_run_with_no_marker_starts_from_now_not_from_history(self):
        config = {"chat_id": 1}
        pages = self.bridge._legacy_pages(config, "cos")
        self.assertEqual(14, config["last_page_id"])
        self.assertEqual([], pages, "the first tick paged history to the phone")


    def test_a_page_is_not_starved_by_ordinary_rows_ahead_of_it(self):
        """t678: more non-page agent rows than the window must not hide a page."""
        self.rows = [{"id": 100 + n, "body": f"an ordinary agent row {n}"}
                     for n in range(25)]
        self.rows.append({"id": 200, "body": "PAGE OWNER security: token exposed"})
        config = {"chat_id": 1, "last_page_id": 10}
        pages = self.bridge._legacy_pages(config, "cos")
        self.assertEqual(["page-200"], [page["message_id"] for page in pages],
                         "25 ordinary rows starved the page behind them")
        self.assertEqual(10, config["last_page_id"],
                         "reading must not move the watermark")

    def test_ordinary_rows_never_move_the_watermark(self):
        """The watermark follows PAGES; ordinary rows are simply not selected."""
        self.rows = [{"id": 30, "body": "a report"}, {"id": 31, "body": "a web echo"},
                     {"id": 32, "body": "PAGE OWNER outage: desk down"}]
        config = {"chat_id": 1, "last_page_id": 10}
        bot, client = FakeBot(), FakeClient()
        self.bridge.send_owner_outbox(config, bot, client=client)
        self.assertEqual(32, config["last_page_id"])
        self.assertEqual(1, len(bot.sent))
        self.assertEqual([], self.bridge._legacy_pages(config, "cos"))


class CaptionedAttachmentTests(unittest.TestCase):
    """Finding 2: a photo with a caption must not be recorded as text only."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.bridge = _bridge()
        self.posted = []
        self.ingested = []

        class FakeClient:
            def call(inner, name, **kwargs):
                if name == "owner_ingest":
                    self.ingested.append(kwargs)
                    return {"message": {"message_id": "i7"}, "duplicate": False}
                return {}

        bridge = self.bridge
        bridge._daemon = lambda: FakeClient()
        bridge._channel = lambda config: "cos"
        bridge._channel_name = lambda channel: "CoS"
        bridge._prepare_route = lambda config: "cos"
        bridge.save_config = lambda config: None
        bridge._save_attachment = lambda filename, mime, data: "att-1"
        bridge.desk.post_message = lambda message, source: self.posted.append(message) or {
            "id": 99, "delivery": "posted"}
        bridge.desk._db = lambda: types.SimpleNamespace(
            __enter__=lambda s: s,
            __exit__=lambda *a: False,
            execute=lambda *a, **k: types.SimpleNamespace(fetchone=lambda: None))

    def tearDown(self):
        self.tmp.cleanup()

    def update(self, **message):
        base = {"message_id": 5, "chat": {"id": 42, "type": "private"}}
        base.update(message)
        return {"update_id": 1001, "message": base}

    def test_a_captioned_photo_keeps_the_attachment(self):
        bot = FakeBot()
        config = {"chat_id": 42}
        self.bridge.process_update(
            self.update(photo=[{"file_id": "f1", "file_size": 10}], caption="look at this"),
            config, bot)
        self.assertEqual([], self.ingested, "the caption was recorded as a text message")
        self.assertEqual(1, len(self.posted), "the attachment was dropped")
        self.assertEqual("look at this", self.posted[0].body)
        self.assertEqual(["att-1"], list(self.posted[0].attachment_ids))

    def test_a_plain_text_message_still_takes_the_text_path(self):
        bot = FakeBot()
        config = {"chat_id": 42}
        self.bridge.process_update(self.update(text="just words"), config, bot)
        self.assertEqual(1, len(self.ingested))
        self.assertEqual([], self.posted)


if __name__ == "__main__":
    unittest.main()
