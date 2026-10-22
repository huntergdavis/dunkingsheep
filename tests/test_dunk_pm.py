"""PM mechanics: one stable digest row per project, lane->PM routing, lane count."""

import json
import tempfile
import unittest
from pathlib import Path

from tests import _bootstrap  # noqa: F401 - temp DUNKINGSHEEP_DIR before dunk_* imports

import dunk_pm
from dunk_inbox import Inbox


class RoutingTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.routing = self.root / "pm-routing.json"
        self.routing.write_text(json.dumps({
            "poolrad": {"pm": "w1:p1", "members": ["w1:p2"], "mode": "redirect",
                        "vp_pane": "w9:pH"},
            "crackdown": {"pm": "w2:p1", "members": ["w2:p2"], "mode": "flag",
                          "vp_pane": "w9:pH"},
        }))

    def load(self):
        return dunk_pm.load_routing(self.routing)

    def test_lane_messages_the_vp_are_redirected_to_its_pm(self):
        routing = self.load()
        self.assertEqual(("w1:p1", "poolrad", "redirect"),
                         dunk_pm.redirect("w1:p2", routing))

    def test_pm_is_never_redirected_to_itself(self):
        self.assertEqual((None, None, None),
                         dunk_pm.redirect("w1:p1", self.load()))

    def test_a_non_member_is_left_alone(self):
        self.assertEqual((None, None, None),
                         dunk_pm.redirect("w9:pZ", self.load()))

    def test_flag_mode_is_reported_not_redirected(self):
        target, project, mode = dunk_pm.redirect("w2:p2", self.load())
        self.assertEqual(("w2:p1", "crackdown", "flag"), (target, project, mode))

    def test_bad_routing_is_a_named_refusal(self):
        self.routing.write_text('{"poolrad": {"members": []}}')
        with self.assertRaises(dunk_pm.PmError):
            self.load()

    def test_missing_routing_is_empty_not_an_error(self):
        self.assertEqual({}, dunk_pm.load_routing(self.root / "absent.json"))


class DigestTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.inbox = Inbox(path=str(self.root / "inbox.db"),
                           blob_dir=str(self.root / "blobs"))

    def test_digest_has_four_sections_and_evidence_refs(self):
        body = dunk_pm.render_digest(
            "poolrad", changed=["strict C 240 -> 252"],
            blocked=["keystone unknown"],
            owner_decisions=["accept the late-answer rule?"],
            numbers=[{"name": "strict C", "value": 252, "unit": "/286",
                      "evidence_ref": "build log line 41"}])
        for heading in ("## Changed", "## Blocked", "## Owner decisions", "## Numbers"):
            self.assertIn(heading, body)
        self.assertIn("build log line 41", body)

    def test_one_digest_row_per_project_updated_in_place(self):
        first = self.inbox.upsert_digest("w9:pH", "pm-digest:poolrad", "v1",
                                         subject="PM digest: poolrad", sender="w1:p1")
        second = self.inbox.upsert_digest("w9:pH", "pm-digest:poolrad", "v2",
                                          subject="PM digest: poolrad", sender="w1:p1")
        self.assertEqual(first["message_id"], second["message_id"])
        self.assertEqual("v2", second["body"])
        self.assertEqual("queued", second["state"])
        self.assertEqual(1, self.inbox.count("w9:pH")["total"])

    def test_lane_origin_count_separates_lane_mail_from_pm_mail(self):
        self.inbox.send("w9:pH", "from a lane", sender="w1:p2", pane_id="w9:pH")
        self.inbox.send("w9:pH", "from the PM", sender="w1:p1", pane_id="w9:pH")
        count = self.inbox.count("w9:pH", lane_panes=["w1:p2"])
        self.assertEqual(2, count["unread"])
        self.assertEqual(1, count["from_lanes"])


if __name__ == "__main__":
    unittest.main()
