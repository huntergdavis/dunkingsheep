"""t130 (VP t386 adversarial review): no metadata-based shell bypass exists.

The VP reproduced three false SENTs against 6f8f41a: missing, expired and
unreadable lease evidence all granted shell "sent" because the negated lease
check was fail-open. CoS i31749 proposed and the VP accepted the simplest
correct repair: the metadata-based early success is REMOVED entirely - there
is no reliable positive shell identity from pane metadata. Every send takes
the bounded confirmation path; a genuine shell still confirms instantly and
truthfully (no input box -> box read clear -> sent).

These tests pin: (1) no shell bypass for unknown-agent lanes, (2) genuine
shells still send instantly, (3) named-agent lanes unchanged, (4) the three
lease-boundary shapes (expired / absent / unreadable evidence) all stay
not-sent while the composer never clears.
"""

import os
import sys
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401,E402
import dunk_core  # noqa: E402
from dunk_core import Flock  # noqa: E402
from fakes import FakeHerdr  # noqa: E402

NJ_TEXT = ("[inbox] TASK t385 is already claimed for you. Read dunkingsheep inbox read\n"
           "  and execute it now. Handoff nj-01701af2301a4744ac310b761ff20a18.")

SHELL_TITLE = "hunter@cell:~/workspace"
LANE_TITLE = "t374-codex-ceb136c"


def _pane(agent="", agent_status="unknown", title=LANE_TITLE):
    return {"pane_id": "w2:p1", "agent": agent, "agent_status": agent_status,
            "terminal_title_stripped": title, "tab_label": "lane"}


class NoShellBypassTests(unittest.TestCase):
    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)
        self.gaps = mock.patch.object(dunk_core, "SUBMIT_GAPS", (0.0, 0.0, 0.0, 0.0))
        self.verify = mock.patch.object(dunk_core, "SUBMIT_VERIFY_GAPS", (0.0, 0.0))
        self.poll = mock.patch.object(dunk_core, "SUBMIT_POLL_S", (0.0,))
        for p in (self.gaps, self.verify, self.poll):
            p.start()

    def tearDown(self):
        for p in (self.gaps, self.verify, self.poll):
            p.stop()
        self.flock.close()

    def test_unknown_agent_lane_is_never_sent_without_confirmation(self):
        """The canary shape: agent='' lane, nothing submits (enters 99). No
        bypass exists -> confirmed path -> honest not-sent, box reader runs."""
        self.herdr.panes = [_pane()]
        self.herdr.enters_needed["w2:p1"] = 99
        with mock.patch.object(type(self.flock), "_box_holds",
                               side_effect=self.flock._box_holds) as holds:
            ok, detail = self.flock.submit_text("w2:p1", NJ_TEXT)
        self.assertFalse(ok, f"false SENT: {detail}")
        self.assertTrue(holds.call_count > 0, "the box reader never ran")

    def test_genuine_shell_still_confirms_instantly(self):
        """A positively identified shell (user@host:title title, no agent, no
        input box) still sends instantly - through the normal confirmation
        path, not a metadata shortcut."""
        self.herdr.panes = [_pane(title=SHELL_TITLE)]
        self.herdr.set_screen("w2:p1", "  \u2514 output\n$ ")
        ok, detail = self.flock.submit_text("w2:p1", "echo hi")
        self.assertTrue(ok, detail)
        self.assertEqual("sent", detail)

    def test_named_agent_lane_unchanged(self):
        self.herdr.panes = [_pane(agent="codex", agent_status="idle")]
        ok, detail = self.flock.submit_text("w2:p1", "a nudge")
        self.assertTrue(ok, detail)
        self.assertEqual("sent", detail)


class LeaseBoundaryControls(unittest.TestCase):
    """VP t386: missing, expired and unreadable lease evidence must never
    grant shell success. With the bypass removed there is no lease lookup in
    the send path at all - these controls prove the honest outcome anyway."""

    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)
        self.gaps = mock.patch.object(dunk_core, "SUBMIT_GAPS", (0.0, 0.0, 0.0, 0.0))
        self.verify = mock.patch.object(dunk_core, "SUBMIT_VERIFY_GAPS", (0.0, 0.0))
        self.poll = mock.patch.object(dunk_core, "SUBMIT_POLL_S", (0.0,))
        for p in (self.gaps, self.verify, self.poll):
            p.start()

    def tearDown(self):
        for p in (self.gaps, self.verify, self.poll):
            p.stop()
        self.flock.close()

    def _not_sent(self):
        self.herdr.panes = [_pane()]
        self.herdr.enters_needed["w2:p1"] = 99
        ok, _detail = self.flock.submit_text("w2:p1", NJ_TEXT)
        self.assertFalse(ok, "false SENT through a lease-boundary shape")

    def test_expired_lease_evidence_stays_not_sent(self):
        self.flock.sent_text_by_pane["w2:p1"] = []
        # An expired lease record is indistinguishable from absent evidence
        # for the send path (no lease lookup exists); the control is that the
        # send still cannot claim success without a box confirmation.
        self._not_sent()

    def test_absent_lease_evidence_stays_not_sent(self):
        self._not_sent()

    def test_unreadable_lease_store_stays_not_sent(self):
        def raise_store(pane_id):
            raise RuntimeError("store unavailable")
        with mock.patch.object(type(self.flock.tasks), "leases_for_pane",
                               side_effect=raise_store):
            self._not_sent()


if __name__ == "__main__":
    unittest.main()
