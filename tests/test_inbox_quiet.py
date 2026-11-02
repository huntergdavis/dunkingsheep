"""Recipient policy suppresses wake prompts, never durable inbox delivery."""
import os
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))
import _bootstrap  # noqa: F401,E402
from dunk_core import Flock, _inbox_recipient_is_quiet  # noqa: E402
from dunk_inbox import Inbox  # noqa: E402
from dunk_tasks import Tasks  # noqa: E402
from fakes import FakeHerdr  # noqa: E402

KEY = "DUNKINGSHEEP_INBOX_SILENT_MAILBOXES"


class QuietRecipientTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.herdr = FakeHerdr()
        for pane in self.herdr.panes:
            if pane["pane_id"] == "w2:p1":
                pane["pane_id"] = "%1"
        for tab in self.herdr.tabs:
            if tab["tab_id"] == "w2:t1":
                tab["label"] = "codex-rust-owner"
        self.inbox = Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                           blob_dir=os.path.join(self.tmp.name, "blobs"))
        self.tasks = Tasks(path=os.path.join(self.tmp.name, "tasks.db"))
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False,
                           inbox=self.inbox, tasks=self.tasks)
        self.routing = patch("dunk_pm.load_routing", return_value={})
        self.routing.start()
        self.addCleanup(self.routing.stop)
        self.addCleanup(self.tmp.cleanup)
        self.addCleanup(self.tasks.close)
        self.addCleanup(self.inbox.close)
        self.addCleanup(self.flock.close)

    def test_policy_parsing_matches_only_exact_final_destinations(self):
        for policy in (None, "", " , , "):
            self.assertFalse(_inbox_recipient_is_quiet(policy, "owner", "Owner (%1)", "%1"))
        for policy in (" %1 ,other", " CODEX-RUST-OWNER ", "Owner (%1)"):
            self.assertTrue(_inbox_recipient_is_quiet(policy, "codex-rust-owner", "Owner (%1)", "%1"))
        self.assertTrue(_inbox_recipient_is_quiet("strasse", "Straße", "", None))
        self.assertFalse(_inbox_recipient_is_quiet("owner", "other-owner", "other", "%10"))

    def test_quiet_alias_canonical_and_pane_still_store_unread_mail(self):
        for target, policy in (("codex", "codex-rust-owner"),
                               ("codex-rust-owner", "%1"), ("%1", "codex-rust-owner,%1")):
            with self.subTest(target=target), patch.dict(os.environ, {KEY: policy}), \
                    patch.object(self.flock, "send_now") as send, \
                    patch.object(self.flock, "_refresh_queued_notice") as fold, \
                    patch.object(self.flock.metrics, "record", wraps=self.flock.metrics.record) as metric:
                message = self.flock.inbox_send(target, "review complete", subject="result",
                                                 self_pane_id="w1:p2")
                self.assertFalse(message["notified"])
                self.assertEqual("codex-rust-owner", message["mailbox"])
                row = self.inbox.get(message["message_id"])
                self.assertEqual(("queued", "w1:p2", "review complete"),
                                 (row["state"], row["sender"], row["body"]))
                self.assertIsNone(row["read_at"])
                self.assertGreater(self.inbox.count("codex-rust-owner")["unread"], 0)
                self.assertIsNone(self.inbox.status(message["message_id"])["notify"]["state"])
                send.assert_not_called()
                fold.assert_not_called()
                self.assertTrue(any(c.args[0] == "inbox.sent" for c in metric.call_args_list))

    def test_other_recipient_and_empty_policy_keep_waking(self):
        for policy in ("", "codex-rust-owner,%1"):
            with patch.dict(os.environ, {KEY: policy}), \
                    patch.object(self.flock, "send_now", return_value={"queued": False}) as send, \
                    patch.object(self.flock, "_refresh_queued_notice", return_value=None):
                message = self.flock.inbox_send("w1:p2", "hello")
                self.assertTrue(message["notified"])
                send.assert_called_once()
        with patch.dict(os.environ, {KEY: ""}), patch.object(self.flock, "send_now") as send:
            message = self.flock.inbox_send("w1:p2", "explicitly quiet", notify=False)
            self.assertFalse(message["notified"])
            send.assert_not_called()

    def test_redirect_policy_matches_final_recipient_not_original_target(self):
        routing = {"project": {"vp_pane": "%1", "pm": "w1:p2",
                                "members": ["w2:p3"], "mode": "redirect"}}
        for policy, expected_wake in (("%1", True), ("w1:p2", False)):
            with patch("dunk_pm.load_routing", return_value=routing), \
                    patch.dict(os.environ, {KEY: policy}), \
                    patch.object(self.flock, "send_now", return_value={"queued": False}) as send, \
                    patch.object(self.flock, "_refresh_queued_notice", return_value=None):
                message = self.flock.inbox_send("%1", "lane update", self_pane_id="w2:p3")
                self.assertEqual(expected_wake, message["notified"])
                self.assertEqual("w1:p2", message["redirected"]["to"])
                self.assertEqual("Claude Proj", message["mailbox"])
                self.assertEqual(int(expected_wake), send.call_count)
                self.assertEqual("queued", self.inbox.get(message["message_id"])["state"])


if __name__ == "__main__":
    unittest.main()
