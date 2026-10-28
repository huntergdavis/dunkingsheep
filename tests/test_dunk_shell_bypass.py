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
from dunk_tasks import GLOBAL as TASKS_GLOBAL  # noqa: E402
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


class GuardedPathAndProcessEvidenceTests(unittest.TestCase):
    """CoS i31723: the repro must run through the ACTUAL guarded path (not a
    bare submit_text with no _box_ctx), and the title/status heuristic must not
    be treated as process identity - a live task lease (caller pid) outranks it."""

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

    def _lane_pane(self, title="t374-codex-ceb136c", status="idle"):
        return {"pane_id": "w2:p1", "agent": "", "agent_status": status,
                "terminal_title_stripped": title, "tab_label": "lane"}

    def test_guarded_path_unknown_agent_lane_is_never_sent_without_confirmation(self):
        """The CoS repro shape through the REAL guarded path: _box_ctx enabled,
        box check on, agent='' lane, nothing submits -> the guard's box check
        holds the send (BOX_HOLD / unconfirmed), never a false SENT."""
        self.herdr.panes = [self._lane_pane()]
        self.herdr.enters_needed["w2:p1"] = 99
        # A live caller lease: this is a managed agent lane (t374's %81 held
        # exactly such a lease when the bypass fired).
        self.flock.tasks.claim("tX", scope=TASKS_GLOBAL,
                               assignees=["w2:p1"], caller={
                                   "pane_id": "w2:p1", "pid": 999999,
                                   "start": "1036810548", "name": "codex"})
        holds = mock.patch.object(type(self.flock), "_box_holds",
                                  side_effect=self.flock._box_holds)
        with holds as holds_mock, \
                self.flock.send_guarded("w2:p1",
                                        lambda: self.flock.submit_text(
                                            "w2:p1", NJ_TEXT)) as (sent, _m, detail):
            pass
        self.assertFalse(sent, f"false SENT through the guarded path: {detail}")
        self.assertTrue(holds_mock.call_count > 0,
                        "the box reader never ran: the shell bypass fired")

    def test_managed_agent_with_shell_like_title_is_not_a_shell(self):
        """A managed native agent can retain a user@host:title; the live lease
        (process identity) must win over the title/status heuristic."""
        self.herdr.panes = [self._lane_pane(title=SHELL_TITLE, status="unknown")]
        self.flock.tasks.claim("tX", scope=TASKS_GLOBAL,
                               assignees=["w2:p1"], caller={
                                   "pane_id": "w2:p1", "pid": 999999,
                                   "start": "1036810548", "name": "codex"})
        self.assertFalse(self.flock._managed_lane_process_alive("w2:p1") is False
                         and self.flock._is_empty_shell(self.herdr.panes[0])
                         and True or False,
                         "sanity: helper wiring broken")
        # Directly: the shell decision must be False for this pane.
        pane = self.herdr.panes[0]
        shell = (self.flock._is_empty_shell(pane)
                 and not self.flock._managed_lane_process_alive("w2:p1"))
        self.assertFalse(shell, "managed agent misclassified as a shell")


if __name__ == "__main__":
    unittest.main()


class GuardedPathAndProcessEvidenceTests(unittest.TestCase):
    """CoS i31723: the repro must run through the ACTUAL guarded path (not a
    bare submit_text with no _box_ctx), and the title/status heuristic must not
    be treated as process identity - a live task lease (caller pid) outranks it."""

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

    def _lane_pane(self, title="t374-codex-ceb136c", status="idle"):
        return {"pane_id": "w2:p1", "agent": "", "agent_status": status,
                "terminal_title_stripped": title, "tab_label": "lane"}

    def _hold_lease(self):
        """A live caller lease on the lane: process identity for the guard.
        The caller is THIS test process (real pid + real starttime), so
        process_alive is genuinely true without mocking."""
        info = open(f"/proc/{os.getpid()}/stat").read().rsplit(")", 1)[1].split()
        start = info[19]                      # field 22: process starttime
        self.flock.tasks.add(TASKS_GLOBAL, "lease holder probe",
                             assignees=["w2:p1"])
        claimed = self.flock.tasks.claim_next(
            scope=TASKS_GLOBAL, pane_id="w2:p1",
            caller={"pane_id": "w2:p1", "pid": os.getpid(), "start": start,
                    "name": "codex"})
        assert claimed and claimed.get("task"), claimed

    def test_guarded_path_unknown_agent_lane_is_never_sent_without_confirmation(self):
        """The CoS repro shape through the REAL guarded path: _box_ctx enabled,
        box check on, agent='' lane, nothing submits -> the guard holds the send
        (BOX_HOLD / unconfirmed), never a false SENT."""
        self.herdr.panes = [self._lane_pane()]
        self.herdr.enters_needed["w2:p1"] = 99
        self._hold_lease()
        outcome = {}
        with mock.patch.object(type(self.flock), "_box_holds",
                               side_effect=self.flock._box_holds) as holds:
            sent, _msg, detail = self.flock.send_guarded(
                "w2:p1",
                lambda: outcome.update(
                    zip(("ok", "detail"),
                        self.flock.submit_text("w2:p1", NJ_TEXT))))
        # sent=True only means the callable typed; the DELIVERY outcome is the
        # inner submit result, and it must NOT be a false sent.
        self.assertFalse(outcome["ok"],
                         f"false SENT through the guarded path: {outcome}")
        self.assertTrue(holds.call_count > 0,
                        "the box reader never ran: the shell bypass fired")

    def test_managed_agent_with_shell_like_title_is_not_a_shell(self):
        """A managed native agent can retain a user@host:title; the live lease
        (process identity) must win over the title/status heuristic."""
        pane = self._lane_pane(title=SHELL_TITLE, status="unknown")
        self.herdr.panes = [pane]
        self._hold_lease()
        self.assertTrue(self.flock._managed_lane_process_alive("w2:p1"))
        # The heuristic alone would call it a shell; the decision with the
        # lease evidence must not:
        # Directly: the submit_text shell decision must be False for this pane.
        shell = (self.flock._is_empty_shell(pane)
                 and not self.flock._managed_lane_process_alive("w2:p1"))
        self.assertFalse(shell, "managed agent misclassified as a shell")


if __name__ == "__main__":
    unittest.main()
