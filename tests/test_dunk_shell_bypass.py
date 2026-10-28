"""t130 live blocker: the shell bypass that marked a typed next-job prompt SENT.

Measured on the t374 canary (Codex pane %81, 2026-10-28 15:06) and isolated by
CoS (/tmp/cos-shell-bypass-probe.py against ceb136cc): pane metadata reported
agent:"" for the lane while codex ran, so submit_text classified it as a bare
shell - `shell = pane is not None and not pane.get("agent")` - typed the text,
pressed Enter, and returned (True, "sent") BEFORE any box confirmation
(_box_holds call count 0). The prompt then sat typed-but-unsubmitted with no
producer turn: the t372 clause-3 blocker.

The fix: a pane is a bare shell only when it POSITIVELY says so
(Flock._is_empty_shell: no agent field, unknown agent_status, user@host:title
terminal title). An unknown agent takes the confirmed path; a genuine shell
still confirms instantly (it has no input box, so the box read is clear).
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


def _pane(agent="", agent_status="unknown", title=SHELL_TITLE):
    return {"pane_id": "w2:p1", "agent": agent, "agent_status": agent_status,
            "terminal_title_stripped": title, "tab_label": "lane"}


# The measured %81 metadata (dunkingsheep panes --json, 2026-10-28 15:5x):
# agent='', agent_status='idle', title='t374-codex-ceb136c' - NOT a
# user@host:title shell title, and a non-unknown status.
LANE_TITLE = "t374-codex-ceb136c"


class ShellBypassTests(unittest.TestCase):
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

    def _panes_return(self, pane):
        return [dict(pane)]

    def test_unknown_agent_lane_is_never_sent_without_confirmation(self):
        """RED on ceb136cc (CoS repro): agent='' + nothing submits (enters 99)
        + guarded context -> main returned (True, 'sent') with ZERO box reads.
        The unknown-agent lane must take the confirmed path and come back
        not-sent while the composer never clears."""
        pane = _pane(title=LANE_TITLE, agent_status="idle")
        self.herdr.panes = self._panes_return(pane)
        self.herdr.enters_needed["w2:p1"] = 99
        # No static screen: FakeHerdr renders the typed text from the box, so
        # the confirm path sees OUR text in the composer (the real %81 state).
        with mock.patch.object(type(self.flock), "_box_holds",
                               side_effect=self.flock._box_holds) as holds:
            ok, detail = self.flock.submit_text("w2:p1", NJ_TEXT)
        self.assertFalse(ok, f"false SENT via the shell bypass: {detail}")
        self.assertTrue(holds.call_count > 0,
                        "the box reader never ran: the shell bypass fired")

    def test_genuine_shell_still_confirms_instantly(self):
        """A positively-identified empty shell (no agent, unknown status,
        user@host:title title, no input box) keeps the instant path: typed,
        confirmed clear (no box), sent."""
        pane = _pane(title=SHELL_TITLE)
        self.herdr.panes = self._panes_return(pane)
        self.herdr.set_screen("w2:p1", "  \u2514 output\n$ ")   # no input box
        ok, detail = self.flock.submit_text("w2:p1", "echo hi")
        self.assertTrue(ok, detail)
        self.assertEqual("sent", detail)

    def test_named_agent_lane_unchanged(self):
        """A pane with a named agent was never affected: its confirm path is
        unchanged by the fix."""
        pane = _pane(agent="codex", agent_status="idle")
        self.herdr.panes = self._panes_return(pane)
        ok, detail = self.flock.submit_text("w2:p1", "a nudge")
        self.assertTrue(ok, detail)
        self.assertEqual("sent", detail)


if __name__ == "__main__":
    unittest.main()
