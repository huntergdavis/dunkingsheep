"""t3624: owner escalations go to the owner mailbox, not the desk brief, and the phone stays off.

A critical owner-class escalation with `escalation.owner.channel = "owner-mailbox"` is one
owner-mailbox write: the `watchdog` system sender, the owner mailbox, a plain escalation body.
It is confirmed on post, never sent through a desk brief, and never written to the telegram
transport. Owner paging stays gated by `escalation.owner.enabled`, which this change leaves off.
"""

import copy
import os
import sys
import unittest
from unittest import mock

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
sys.path.insert(0, HERE)
sys.path.insert(0, ROOT)

import _bootstrap  # noqa: F401,E402  (isolate live state)
import dunk_watchdog as wd  # noqa: E402
from test_dunk_watchdog import NOW, Fleet, WatchdogCase  # noqa: E402


class MailboxSend:
    """Stands in for the daemon's inbox_send: records each write, or refuses it."""

    def __init__(self, fail=None):
        self.calls = []
        self.fail = fail

    def __call__(self, body, subject=wd.OWNER_MAILBOX_SUBJECT):
        if self.fail:
            raise self.fail
        self.calls.append((body, subject))
        return {"message_id": f"i{900 + len(self.calls)}"}


class OwnerMailboxTests(WatchdogCase):
    def disk_config(self, hold=1):
        """A critical outage (an owner class) that is held for `hold` runs, then eligible."""
        cfg = self.fleet.config(host__disk_full={"max_used_fraction": 0.98, "paths": ["/"],
                                                 "must_hold_runs": hold})
        self.fleet.hosts = {"/": {"used_fraction": 0.99}}
        cfg = self.live(cfg, classes=["outage"])
        cfg["escalation"]["owner"]["channel"] = "owner-mailbox"
        self.seed_state(cfg)
        return cfg

    def test_a_critical_escalation_is_one_owner_mailbox_write(self):
        cfg = self.disk_config()
        mailbox = MailboxSend()
        report, _ = self.fleet.run(cfg, owner=wd.OwnerMailboxChannel(send=mailbox))
        self.assertEqual("no", report["decisions"][0]["owner"])
        self.assertEqual(1, len(mailbox.calls))
        body, subject = mailbox.calls[0]
        self.assertEqual(wd.OWNER_MAILBOX_SUBJECT, subject)
        self.assertIn("ESCALATION", body)
        self.assertNotIn("PAGE OWNER", body, "an escalation must not carry the page marker")
        key = wd.dedupe_key(report["decisions"][0]["finding"])
        state, _ = wd.load_state(cfg["state_path"])
        self.assertEqual(NOW, state["owner_pages"][key]["confirmed_at"], "confirmed on post")
        self.assertEqual("owner-mailbox", state["owner_pages"][key]["desk_channel"])

    def test_the_owner_escalation_goes_nowhere_else(self):
        cfg = self.disk_config()
        mailbox = MailboxSend()
        _, vp_sent = self.fleet.run(cfg, owner=wd.OwnerMailboxChannel(send=mailbox))
        self.assertEqual(1, len(mailbox.calls), "exactly one owner-mailbox write")
        # The VP notification for the same finding is unchanged: it goes to the VP mailbox only.
        self.assertTrue(vp_sent)
        for to, *_ in vp_sent:
            self.assertEqual(cfg["escalation"]["vp"]["mailbox"], to)

    def test_a_disabled_owner_channel_writes_no_escalation(self):
        cfg = self.disk_config()
        cfg["escalation"]["owner"]["enabled"] = False
        mailbox = MailboxSend()
        report, _ = self.fleet.run(cfg, owner=wd.OwnerMailboxChannel(send=mailbox))
        self.assertEqual("disabled", report["decisions"][0]["owner"])
        self.assertEqual([], mailbox.calls)

    def test_a_refused_mailbox_write_is_undelivered_and_named(self):
        cfg = self.disk_config()
        mailbox = MailboxSend(fail=RuntimeError("daemon down"))
        report, _ = self.fleet.run(cfg, owner=wd.OwnerMailboxChannel(send=mailbox))
        self.assertEqual("transport-error", report["decisions"][0]["owner"])
        self.assertTrue(any("not posted" in p and "UNDELIVERED" in p for p in report["problems"]),
                        report["problems"])
        state, _ = wd.load_state(cfg["state_path"])
        self.assertEqual({}, state["owner_pages"], "a refused write is never recorded as delivered")

    def test_the_desk_brief_channel_still_fails_closed(self):
        cfg = self.disk_config()
        cfg["escalation"]["owner"]["channel"] = "desk-brief"
        report, _ = self.fleet.run(cfg, owner=wd.DeskBriefChannel(os.path.join(self.tmp, "status.json")))
        self.assertEqual("transport-error", report["decisions"][0]["owner"])

    def test_the_channel_enum_names_the_owner_mailbox_and_nothing_else(self):
        cfg = self.fleet.config()
        cfg["escalation"]["owner"]["channel"] = "owner-mailbox"
        wd.validate_config(cfg)
        cfg["escalation"]["owner"]["channel"] = "telegram"
        with self.assertRaises(wd.ConfigError) as caught:
            wd.validate_config(cfg)
        self.assertEqual("escalation.owner.channel", caught.exception.key)

    def test_the_mailbox_write_is_the_daemon_inbox_send_as_the_watchdog(self):
        with mock.patch("dunk_client.FlockClient") as client_class:
            wd.owner_mailbox_send("the body")
        client_class.assert_called_once_with(self_pane_id=None)
        client_class.return_value.call.assert_called_once_with(
            "inbox_send", to="owner", subject="Watchdog escalation", body="the body",
            system_sender="watchdog")



if __name__ == "__main__":
    unittest.main()
