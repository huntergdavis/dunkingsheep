"""t377 integration: the daemon's stuck-message recovery, end to end on a fake pane.

The state machine has its own fake-clock tests (tests/test_dunk_stuck.py). These
tests prove the WIRING: what reaches the pane, what does not, and what is recorded.
A short threshold/cooldown keeps them fast; nothing here reads the live state.
"""
import json
import os
import sys
import tempfile
import time
import unittest

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import _bootstrap  # noqa: E402,F401  (isolate live state)

import dunk_core  # noqa: E402
import dunk_stuck  # noqa: E402
from dunk_inbox import Inbox  # noqa: E402
from dunk_tasks import Tasks  # noqa: E402
from fakes import FakeHerdr  # noqa: E402

PANE = "w1:p2"
OUR_TEXT = "[inbox] new message for Claude Proj (1 unread). Read it with: dunkingsheep inbox read"


class _RecordingMetrics:
    def __init__(self):
        self.events = []

    def record(self, event, **fields):
        self.events.append(dict(fields, event=event))

    def prune(self, *_a, **_k):
        return 0

    def close(self):
        return None


class StuckRecoveryTests(unittest.TestCase):
    THRESHOLD = 0.05
    COOLDOWN = 0.05

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.herdr = FakeHerdr()
        self.metrics = _RecordingMetrics()
        self.inbox = Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                           blob_dir=os.path.join(self.tmp.name, "blobs"))
        for pane in self.herdr.panes:
            if pane["pane_id"] == PANE:
                pane["pane_pid"] = os.getpid()   # real /proc, so pid+start identity exists
        self.flock = dunk_core.Flock(
            herdr=self.herdr, persist=False, restore=False, inbox=self.inbox,
            tasks=Tasks(path=os.path.join(self.tmp.name, "tasks.db")),
            metrics=self.metrics,
            stuck_config={"threshold_s": self.THRESHOLD, "max_attempts": 2,
                          "cooldown_s": self.COOLDOWN, "observation_interval_s": 0.01})
        # A pane whose composer holds text we typed, with a message still queued.
        self.herdr.screens[PANE] = f"│ ✻ idle\n❯ {OUR_TEXT}\n──────────────\n"
        self.herdr.box[PANE] = OUR_TEXT
        self.message = self.flock._new_message(
            PANE, "Claude Proj", OUR_TEXT, "send_text", lambda: (True, "ok"))
        self.flock._remember_typed(PANE, OUR_TEXT)
        # These tests exercise the RECOVERY wiring. t130's exact matcher has its own
        # tests (tests/test_dunk_composer_region*.py); here it is stubbed True except
        # in the seam test that proves a False from it blocks the press.
        self.flock.composer_region_is_owned = lambda pane_id, screen: True

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def presses(self):
        return [k for pane, k in self.herdr.keys if pane == PANE]

    def recover(self, since=None, screen=None):
        if screen is not None:
            self.herdr.screens[PANE] = screen
        return self.flock._unwedge(PANE, OUR_TEXT, since or (time.time() - 10),
                                   message=self.message)

    def test_before_the_threshold_no_enter_is_pressed(self):
        self.flock.stuck_config = dunk_stuck.Config(threshold_s=600, max_attempts=2,
                                                    cooldown_s=60,
                                                    observation_interval_s=0.01)
        self.assertFalse(self.recover(since=time.time() - 1))
        self.assertEqual([], self.presses(), "nothing may be pressed before the threshold")

    def test_at_the_threshold_one_guarded_submit_is_pressed_and_verified(self):
        self.herdr.box[PANE] = OUR_TEXT          # Enter consumes it
        self.assertTrue(self.recover(since=time.time() - 600))
        self.assertEqual(1, len(self.presses()), "exactly ONE key, never a ladder")
        outcomes = [e for e in self.metrics.events if e["event"] == "stuck.recover"]
        self.assertEqual(["consumed"], [e["outcome"] for e in outcomes])
        self.assertNotIn(PANE, self.flock.stuck.records,
                         "a consumed message is forgotten, not retried")

    def test_a_human_continuation_appended_to_our_text_is_never_submitted(self):
        mixed = OUR_TEXT + "\nand one more thing I was typing"
        self.herdr.screens[PANE] = f"│ ✻ idle\n❯ {mixed}\n──────────────\n"
        self.assertFalse(self.flock._unwedge(PANE, mixed, time.time() - 600,
                                             message=self.message))
        self.assertEqual([], self.presses(),
                         "text that is not exactly ours must never be submitted")

    def test_a_false_from_the_t130_matcher_means_nothing_is_pressed(self):
        # The seam: ownership is t130's exact full-composer matcher. If it says "not
        # ours", the recovery must not press, even though the text looks right here.
        self.flock.composer_region_is_owned = lambda pane_id, screen: False
        self.assertFalse(self.recover())
        self.assertEqual([], self.presses(),
                         "a matcher refusal must fail closed, not fall back to a weaker test")

    def test_a_busy_lane_is_left_alone(self):
        self.herdr.screens[PANE] = ("│ ✻ Working (3m) · esc to interrupt\n"
                                    f"❯ {OUR_TEXT}\n──────────────\n")
        self.herdr.statuses[PANE] = "working"
        self.assertFalse(self.recover())
        self.assertEqual([], self.presses(), "a busy turn resets the clock")

    def test_an_open_dialog_is_left_alone(self):
        self.herdr.screens[PANE] = ("│ Choose a model\n│ ❯ 1. fast\n│   2. smart\n"
                                    "│ Enter to select · esc to cancel\n")
        self.assertFalse(self.recover())
        self.assertEqual([], self.presses(), "a panel is dismissed by a human, not by us")

    def test_a_failed_submit_ends_in_a_terminal_reason_after_bounded_attempts(self):
        # The composer ignores Enter: the pane keeps our text, so nothing is consumed.
        self.herdr.box[PANE] = ""                # Enter does not consume it
        self.herdr.enters_needed[PANE] = 99
        for _ in range(6):
            self.recover(since=time.time() - 600)
            time.sleep(self.COOLDOWN + 0.01)
        outcomes = [e["outcome"] for e in self.metrics.events if e["event"] == "stuck.recover"]
        self.assertEqual(2, len(outcomes), f"attempts must be bounded, got {outcomes}")
        self.assertTrue(all(o != "consumed" for o in outcomes))
        record = self.flock.stuck.records.get(PANE) or {}
        self.assertIn("did not submit", str(record.get("terminal")),
                      "a failure must end in a named terminal reason, not a silent loop")


class StuckRecoveryRestartTests(unittest.TestCase):
    """The attempt count crosses a restart, so a crashed daemon cannot double-press."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.state = os.path.join(self.tmp.name, "dunks.json")
        self.herdr = FakeHerdr()
        self.inbox = Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                           blob_dir=os.path.join(self.tmp.name, "blobs"))
        self.flock = self.make()

    def make(self):
        for pane in self.herdr.panes:
            if pane["pane_id"] == PANE:
                pane["pane_pid"] = os.getpid()
        return dunk_core.Flock(
            herdr=self.herdr, state_file=self.state, persist=True, restore=True,
            inbox=self.inbox, tasks=Tasks(path=os.path.join(self.tmp.name, "tasks.db")),
            metrics=_RecordingMetrics(),
            stuck_config={"threshold_s": 0.05, "max_attempts": 1, "cooldown_s": 0.05,
                          "observation_interval_s": 0.01})

    def tearDown(self):
        self.flock.close()
        self.inbox.close()
        self.tmp.cleanup()

    def test_a_restart_does_not_buy_a_second_enter(self):
        self.herdr.screens[PANE] = f"│ ✻ idle\n❯ {OUR_TEXT}\n──────────────\n"
        self.herdr.box[PANE] = ""                # Enter never consumes: attempt fails
        self.herdr.enters_needed[PANE] = 99
        self.flock._remember_typed(PANE, OUR_TEXT)
        self.flock.composer_region_is_owned = lambda pane_id, screen: True
        message = self.flock._new_message(PANE, "Claude Proj", OUR_TEXT, "send_text",
                                         lambda: (True, "ok"))
        self.flock._unwedge(PANE, OUR_TEXT, time.time() - 600, message=message)
        first = len([k for pane, k in self.herdr.keys if pane == PANE])
        # ONE recovery ATTEMPT is not one keystroke: the existing submit ladder presses
        # Enter up to len(SUBMIT_GAPS) times to CONFIRM a submit, which is bounded by
        # construction. The dedupe claim is about the restart, asserted below.
        self.assertEqual(1, first, "the recovery presses exactly ONE key per attempt")
        self.flock.close()

        # A NEW daemon on the same state file: max_attempts is already spent.
        self.flock = self.make()
        self.flock.composer_region_is_owned = lambda pane_id, screen: True
        self.assertIn(PANE, self.flock.stuck.records,
                      "the attempt record must be restored from the state file")
        self.flock._remember_typed(PANE, OUR_TEXT)
        self.flock._unwedge(PANE, OUR_TEXT, time.time() - 600, message=message)
        self.assertEqual(first, len([k for pane, k in self.herdr.keys if pane == PANE]),
                         "a restart must not press a second Enter")


class ReviewBlockerTests(unittest.TestCase):
    """The independent review's blockers, each as an executable case (i31531)."""

    THRESHOLD = 0.05

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.herdr = FakeHerdr()
        for pane in self.herdr.panes:
            if pane["pane_id"] == PANE:
                pane["pane_pid"] = os.getpid()
        self.metrics = _RecordingMetrics()
        self.inbox = Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                           blob_dir=os.path.join(self.tmp.name, "blobs"))
        self.flock = self.make()
        self.flock.composer_region_is_owned = lambda pane_id, screen: True
        self.herdr.screens[PANE] = f"│ ✻ idle\n❯ {OUR_TEXT}\n──────────────\n"
        self.herdr.box[PANE] = OUR_TEXT
        self.flock._remember_typed(PANE, OUR_TEXT)
        self.message = self.flock._new_message(PANE, "Claude Proj", OUR_TEXT,
                                              "send_text", lambda: (True, "ok"))

    def make(self, **overrides):
        config = {"threshold_s": self.THRESHOLD, "max_attempts": 2, "cooldown_s": 0.05,
                  "observation_interval_s": 0.01}
        config.update(overrides)
        return dunk_core.Flock(
            herdr=self.herdr, persist=False, restore=False, inbox=self.inbox,
            tasks=Tasks(path=os.path.join(self.tmp.name, "tasks.db")),
            metrics=self.metrics, stuck_config=config)

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def presses(self):
        return [k for pane, k in self.herdr.keys if pane == PANE]

    def test_a_disabled_recovery_presses_nothing_and_tracks_nothing(self):
        flock = self.make(enabled=False)
        try:
            flock._remember_typed(PANE, OUR_TEXT)
            self.assertFalse(flock._unwedge(PANE, OUR_TEXT, time.time() - 600,
                                            message=self.message))
            self.assertEqual([], self.presses(), "disabled means no action at all")
            self.assertEqual({}, flock.stuck.records, "and no tracking either")
        finally:
            flock.close()

    def test_the_env_config_source_changes_real_worker_behaviour(self):
        # One documented runtime source: DUNKINGSHEEP_STUCK_CONFIG (inline JSON).
        previous = os.environ.get("DUNKINGSHEEP_STUCK_CONFIG")
        os.environ["DUNKINGSHEEP_STUCK_CONFIG"] = '{"enabled": false}'
        try:
            flock = dunk_core.Flock(herdr=self.herdr, persist=False, restore=False,
                                    inbox=self.inbox,
                                    tasks=Tasks(path=os.path.join(self.tmp.name, "t2.db")),
                                    metrics=self.metrics)
            self.assertFalse(flock.stuck_config.enabled)
            flock._remember_typed(PANE, OUR_TEXT)
            self.assertFalse(flock._unwedge(PANE, OUR_TEXT, time.time() - 600,
                                            message=self.message))
            self.assertEqual([], self.presses())
            flock.close()
        finally:
            if previous is None:
                os.environ.pop("DUNKINGSHEEP_STUCK_CONFIG", None)
            else:
                os.environ["DUNKINGSHEEP_STUCK_CONFIG"] = previous

    def test_the_attempt_is_reserved_and_saved_before_the_key(self):
        order = []
        original_save = self.flock._save
        original_send = self.herdr.send_keys

        def save():
            order.append("save")
            return original_save()

        def send(pane_id, keys):
            order.append("send")
            return original_send(pane_id, keys)

        self.flock._save = save
        self.herdr.send_keys = send
        self.herdr.box[PANE] = OUR_TEXT
        self.flock._unwedge(PANE, OUR_TEXT, time.time() - 600, message=self.message)
        self.assertIn("send", order)
        self.assertLess(order.index("save"), order.index("send"),
                        "the attempt must be durable BEFORE the external action")
        self.assertEqual(1, self.flock.stuck.reserved(PANE, self.message.id,
                                                     self.flock._pane_generation(PANE),
                                                     dunk_stuck.fingerprint(OUR_TEXT)))

    def test_an_unreadable_pane_after_the_press_is_not_a_cleared_composer(self):
        self.herdr.box[PANE] = OUR_TEXT
        real = self.flock._box_text
        calls = {"n": 0}

        def flaky(pane_id):
            calls["n"] += 1
            return None if calls["n"] > 1 else real(pane_id)   # first read ok, after: unknown

        self.flock._box_text = flaky
        self.assertFalse(self.flock._unwedge(PANE, OUR_TEXT, time.time() - 600,
                                             message=self.message),
                         "a failed read must not be reported as a successful recovery")
        outcomes = [e["outcome"] for e in self.metrics.events if e["event"] == "stuck.recover"]
        self.assertEqual(["unverified"], outcomes)

    def test_a_pane_without_pid_and_start_evidence_is_never_pressed(self):
        for pane in self.herdr.panes:
            if pane["pane_id"] == PANE:
                pane.pop("pane_pid", None)
        self.assertFalse(self.flock._unwedge(PANE, OUR_TEXT, time.time() - 600,
                                             message=self.message))
        self.assertEqual([], self.presses(), "no generation evidence, no action")

    def test_a_text_edited_between_observation_and_press_is_not_pressed(self):
        self.herdr.box[PANE] = OUR_TEXT
        original_save = self.flock._save

        def save_then_edit():
            original_save()
            # A person types in the gap between the decision and the lock.
            edited = OUR_TEXT + " and one more thing"
            self.herdr.box[PANE] = edited
            self.herdr.screens[PANE] = f"│ ✻ idle\n❯ {edited}\n──────────────\n"

        self.flock._save = save_then_edit
        self.assertFalse(self.flock._unwedge(PANE, OUR_TEXT, time.time() - 600,
                                             message=self.message))
        self.assertEqual([], self.presses(), "the press-time revalidation must refuse")

    def test_a_stranded_message_behind_a_queued_head_is_identified_by_its_text(self):
        # B is queued; the box holds A's older text. The identity must be A, not the head.
        older = self.flock._new_message(PANE, "Claude Proj", OUR_TEXT, "send_text",
                                       lambda: (True, "ok"))
        newer = self.flock._new_message(PANE, "Claude Proj", "a later notice", "send_text",
                                       lambda: (True, "ok"))
        self.flock._enqueue(older)               # both queued; the BOX holds older's text
        self.flock._enqueue(newer)
        found = self.flock._stranded_message(PANE, OUR_TEXT)
        self.assertIs(older, found)
        self.assertIsNone(self.flock._stranded_message(PANE, "text nobody queued"))

    def test_a_dunk_notice_with_no_message_row_is_still_tracked_and_recovered(self):
        # No Message row at all: the identity is the text fingerprint, and the text is
        # ours because this pane was sent exactly this.
        self.flock._remember_typed(PANE, OUR_TEXT)
        self.herdr.box[PANE] = OUR_TEXT
        self.assertTrue(self.flock._unwedge(PANE, OUR_TEXT, time.time() - 600))
        self.assertEqual(1, len(self.presses()))
        outcomes = [e["outcome"] for e in self.metrics.events if e["event"] == "stuck.recover"]
        self.assertEqual(["consumed"], outcomes)


class CurrentMergedMatcherTests(unittest.TestCase):
    """REAL integration against the matcher that is on main today (t130, PR71).

    NOT the corrected matcher: PR71 is blocked for complete-region extraction holes and
    pi-worker-2 is correcting it, so this is evidence for the CURRENT merged seam, and
    the corrected-matcher run stays a separate required check. Nothing here is stubbed.
    """

    THRESHOLD = 0.05

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.herdr = FakeHerdr()
        for pane in self.herdr.panes:
            if pane["pane_id"] == PANE:
                pane["pane_pid"] = os.getpid()
        self.metrics = _RecordingMetrics()
        self.inbox = Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                           blob_dir=os.path.join(self.tmp.name, "blobs"))
        self.flock = dunk_core.Flock(
            herdr=self.herdr, persist=False, restore=False, inbox=self.inbox,
            tasks=Tasks(path=os.path.join(self.tmp.name, "tasks.db")),
            metrics=self.metrics,
            stuck_config={"threshold_s": self.THRESHOLD, "max_attempts": 2,
                          "cooldown_s": 0.05, "observation_interval_s": 0.01})
        self.flock._remember_typed(PANE, OUR_TEXT)
        self.message = self.flock._new_message(PANE, "Claude Proj", OUR_TEXT,
                                              "send_text", lambda: (True, "ok"))
        self.flock._enqueue(self.message)
        # A status footer with the middle dot is the shape the CURRENT merged matcher
        # excludes from the composer region; see the gap test below for why that matters.
        self.herdr.screens[PANE] = f"│ ✻ idle\n❯ {OUR_TEXT}\n│ idle · pi\n"
        self.herdr.box[PANE] = OUR_TEXT

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def presses(self):
        return [k for pane, k in self.herdr.keys if pane == PANE]

    def test_the_real_merged_matcher_accepts_our_exact_composer(self):
        screen = self.herdr.screens[PANE]
        self.assertTrue(self.flock.composer_region_is_owned(PANE, screen),
                        "the real matcher must agree the composer is ours")
        self.assertTrue(self.flock._unwedge(PANE, OUR_TEXT, time.time() - 600,
                                            message=self.message))
        self.assertEqual(1, len(self.presses()))

    def test_the_real_merged_matcher_refuses_a_human_continuation_below_our_text(self):
        screen = (f"│ ✻ idle\n❯ {OUR_TEXT}\n"
                  "  and one more thing I was typing\n──────────────\n")
        self.assertFalse(self.flock.composer_region_is_owned(PANE, screen))
        self.assertFalse(self.flock._unwedge(PANE, screen_lines(screen), time.time() - 600,
                                             message=self.message))
        self.assertEqual([], self.presses(),
                         "a continuation line means the composer is not ours")

    def test_an_unreadable_screen_is_not_ownership_through_the_real_matcher(self):
        self.assertFalse(self.flock.composer_region_is_owned(PANE, None))

    def test_the_known_extraction_gap_after_PR74_is_pinned_as_a_follow_up(self):
        """Bordered composers: a plain rule line makes the seam refuse (fails closed).

        MEASURED on main 59b627a (PR74 merged). PR74's boundary requires the last
        non-blank row to be a middle-dot status footer, so a bordered composer's plain
        RULE line makes `_composer_region_text` return "" - it does not misread the rule
        as content any more (the pre-PR74 behaviour reproduced in this PR's history),
        and the seam refuses. Recovery is therefore INERT on bordered panes today, and
        that is a missing-functionality case, not a safety defect: nothing is submitted.

        This is pinned, not flipped, and it is a KNOWN-LIMITATION pin in t377's own
        scope: CoS ruled (i31593) that bordered plain-rule geometry is fail-closed
        UNSUPPORTED, not a defect in the accepted safety correction, and pi-worker-2
        will not change the matcher for it. If a live proof later shows bordered pi
        panes need recovery, that is a t377 follow-up with captured real geometry and
        a negative human-rule control - not a reason to reopen the matcher.
        """
        bordered = f"│ ✻ idle\n❯ {OUR_TEXT}\n{'─' * 14}\n"
        self.assertEqual("", dunk_core._composer_region_text(bordered),
                         "PR74 keeps the rule line OUT of the region by returning none")
        self.assertFalse(self.flock.composer_region_is_owned(PANE, bordered),
                         "so the seam refuses our own text on a bordered pane")
        self.herdr.screens[PANE] = bordered       # the pane really shows this shape
        self.assertFalse(self.flock._unwedge(PANE, OUR_TEXT, time.time() - 600,
                                            message=self.message))
        self.assertEqual([], self.presses(),
                         "fail closed: a bordered frame is a known gap, not a green light")

    def test_the_supported_frame_shape_is_exactly_one_sigil_and_a_middle_dot_footer(self):
        """What PR74 supports, measured: the recovery fires there and only there."""
        supported = f"│ ✻ idle\n❯ {OUR_TEXT}\n│ idle · pi\n"
        self.assertEqual(OUR_TEXT, dunk_core._composer_region_text(supported))
        self.assertTrue(self.flock.composer_region_is_owned(PANE, supported))
        self.herdr.screens[PANE] = supported
        self.herdr.box[PANE] = OUR_TEXT
        self.assertTrue(self.flock._unwedge(PANE, OUR_TEXT, time.time() - 600,
                                           message=self.message))
        self.assertEqual(1, len(self.presses()))


class DaemonStateFileTests(unittest.TestCase):
    """Review i31621: the ledger and config must be wired through the REAL Flock
    save/restore, not merely through Tracker serialisation."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.state = os.path.join(self.tmp.name, "dunks.json")
        self.herdr = FakeHerdr()
        for pane in self.herdr.panes:
            if pane["pane_id"] == PANE:
                pane["pane_pid"] = os.getpid()
        self.inbox = Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                           blob_dir=os.path.join(self.tmp.name, "blobs"))
        self.flock = self.make()

    def make(self, explicit=None):
        config = {"threshold_s": 0.05, "max_attempts": 1, "cooldown_s": 0.05,
                  "observation_interval_s": 0.01}
        if explicit is not None:
            config = explicit
        return dunk_core.Flock(
            herdr=self.herdr, state_file=self.state, persist=True, restore=True,
            inbox=self.inbox, tasks=Tasks(path=os.path.join(self.tmp.name, "tasks.db")),
            metrics=_RecordingMetrics(), stuck_config=config)

    def tearDown(self):
        self.flock.close()
        self.inbox.close()
        self.tmp.cleanup()

    def press_count(self):
        return len([k for pane, k in self.herdr.keys if pane == PANE])

    def strand(self):
        self.flock._remember_typed(PANE, OUR_TEXT)
        self.flock.composer_region_is_owned = lambda pane_id, screen: True
        self.herdr.screens[PANE] = f"│ ✻ idle\n❯ {OUR_TEXT}\n│ idle · pi\n"
        self.herdr.box[PANE] = ""                    # Enter never consumes: attempt fails
        self.herdr.enters_needed[PANE] = 99
        # NOT enqueued: an enqueued row is restored by the outbox worker after the
        # restart and re-sent through the ordinary send path, which would count as a
        # second Enter for a different reason. The stuck identity is passed explicitly,
        # which is the dunk path's shape too.
        return self.flock._new_message(PANE, "Claude Proj", OUR_TEXT, "send_text",
                                       lambda: (True, "ok"))

    def test_the_spent_ledger_really_crosses_the_state_file(self):
        message = self.strand()
        self.flock._unwedge(PANE, OUR_TEXT, time.time() - 600, message=message)
        self.assertEqual(1, self.press_count())
        self.flock.close()

        self.flock = self.make()                     # a NEW daemon on the same file
        self.assertIn(PANE, self.flock.stuck.records)
        self.assertTrue(self.flock.stuck.spent, "the spent ledger must be in the file")
        self.flock._remember_typed(PANE, OUR_TEXT)
        self.flock.composer_region_is_owned = lambda pane_id, screen: True
        self.flock._unwedge(PANE, OUR_TEXT, time.time() - 600, message=message)
        self.assertEqual(1, self.press_count(),
                         "an attempt spent before the restart must not be refunded")

    def test_a_consumed_message_does_not_regain_budget_after_a_restart(self):
        message = self.strand()
        self.herdr.box[PANE] = OUR_TEXT              # this time Enter consumes it
        self.flock._unwedge(PANE, OUR_TEXT, time.time() - 600, message=message)
        self.flock.close()
        self.flock = self.make()
        self.flock._remember_typed(PANE, OUR_TEXT)
        self.flock.composer_region_is_owned = lambda pane_id, screen: True
        # The same text reappears (a new send of identical text): the ledger still
        # knows this identity spent its attempt.
        self.herdr.box[PANE] = ""
        self.herdr.enters_needed[PANE] = 99
        before = self.press_count()
        self.flock._unwedge(PANE, OUR_TEXT, time.time() - 600, message=message)
        self.assertEqual(before, self.press_count(),
                         "a consumed identity that reappears must not get a fresh budget")

    def test_a_declared_config_wins_over_the_saved_one(self):
        self.flock.stuck_config.enabled             # force a save with defaults
        self.flock._save()
        self.flock.close()
        # The operator declares the opposite on the next start: argument wins.
        self.flock = self.make(explicit={"enabled": False})
        self.assertFalse(self.flock.stuck_config.enabled,
                         "a declared config must not be overwritten by the state file")

    def test_the_saved_config_carries_every_field(self):
        self.flock._save()
        with open(self.state, "r", encoding="utf-8") as handle:
            payload = json.load(handle)
        self.assertEqual({"enabled", "threshold_s", "max_attempts", "cooldown_s",
                          "observation_interval_s"}, set(payload["stuck_config"]))
        self.assertIn("records", payload["stuck"])
        self.assertIn("spent", payload["stuck"])

    def test_the_observation_interval_is_consumed_not_just_validated(self):
        message = self.strand()
        self.flock.stuck_config = dunk_stuck.Config(threshold_s=0.0 + 0.01, max_attempts=2,
                                                    cooldown_s=0.01,
                                                    observation_interval_s=600)
        self.flock._unwedge(PANE, OUR_TEXT, time.time() - 600, message=message)
        first = self.press_count()
        self.flock._unwedge(PANE, OUR_TEXT, time.time() - 600, message=message)
        self.assertEqual(first, self.press_count(),
                         "inside the observation interval the worker must not act again")


class FinalBoundaryGuardTests(unittest.TestCase):
    """Review i31646: three ORIGINAL guard requirements, reproduced at the boundary.

    Each case patches a side effect that happens BETWEEN the observation and the press
    (inside the pre-press save, which the recovery performs to reserve the attempt) and
    asserts that nothing is pressed.
    """

    THRESHOLD = 0.05

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.herdr = FakeHerdr()
        for pane in self.herdr.panes:
            if pane["pane_id"] == PANE:
                pane["pane_pid"] = os.getpid()
        self.metrics = _RecordingMetrics()
        self.inbox = Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                           blob_dir=os.path.join(self.tmp.name, "blobs"))
        self.flock = self.make()
        self.flock.composer_region_is_owned = lambda pane_id, screen: True
        self.herdr.screens[PANE] = f"│ ✻ idle\n❯ {OUR_TEXT}\n│ idle · pi\n"
        self.herdr.box[PANE] = OUR_TEXT
        self.flock._remember_typed(PANE, OUR_TEXT)
        self.message = self.flock._new_message(PANE, "Claude Proj", OUR_TEXT, "send_text",
                                              lambda: (True, "ok"))
        self.flock._enqueue(self.message)

    def make(self):
        return dunk_core.Flock(
            herdr=self.herdr, persist=False, restore=False, inbox=self.inbox,
            tasks=Tasks(path=os.path.join(self.tmp.name, "tasks.db")),
            metrics=self.metrics,
            stuck_config={"threshold_s": self.THRESHOLD, "max_attempts": 2,
                          "cooldown_s": 0.05, "observation_interval_s": 0.01})

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def presses(self):
        return [k for pane, k in self.herdr.keys if pane == PANE]

    def recover(self):
        return self.flock._unwedge(PANE, OUR_TEXT, time.time() - 600,
                                   message=self.message)

    def outcomes(self):
        return [e["outcome"] for e in self.metrics.events if e["event"] == "stuck.recover"]

    def test_a_cancellation_after_the_decision_prevents_the_press(self):
        original_save = self.flock._save

        def cancel_then_save():
            self.message.status = "cancelled"        # the sender cancels in the gap
            return original_save()

        self.flock._save = cancel_then_save
        self.assertFalse(self.recover())
        self.assertEqual([], self.presses(),
                         "a cancelled message must never be pressed at the boundary")

    def test_the_target_going_busy_after_the_decision_prevents_the_press(self):
        original_save = self.flock._save

        def busy_then_save():
            self.herdr.statuses[PANE] = "working"    # the lane starts a turn in the gap
            self.herdr.screens[PANE] = ("│ ✻ Working · esc to interrupt\n"
                                        f"❯ {OUR_TEXT}\n│ busy · pi\n")
            return original_save()

        self.flock._save = busy_then_save
        self.assertFalse(self.recover())
        self.assertEqual([], self.presses(),
                         "the status is re-checked at press time, not only at observation")

    def test_a_failed_reservation_save_refuses_to_press(self):
        original_save = self.flock._save
        calls = {"n": 0}

        def save_fails_second_time():
            calls["n"] += 1
            if calls["n"] == 1:                       # the pre-press reservation
                self.flock._last_save_failed = "OSError: disk full"
                return False
            return original_save()

        self.flock._save = save_fails_second_time
        self.assertFalse(self.recover())
        self.assertEqual([], self.presses(),
                         "no durable reservation means no key")
        self.assertIn("refused", self.outcomes())

    def test_the_backends_refusal_is_honoured(self):
        original_send = self.herdr.send_keys

        def refuse(pane_id, keys):
            original_send(pane_id, keys)
            return (False, "pane rejected the key")

        self.herdr.send_keys = refuse
        self.assertFalse(self.recover(),
                         "a (False, reason) from the backend is not a successful press")
        # "refused": the key was not accepted, so nothing was typed and nothing is
        # claimed as consumed. The attempt stays spent (the conservative direction,
        # since a reservation that may have reached the pane must not be refunded).
        self.assertEqual(["refused"], self.outcomes())
        self.assertNotIn("consumed", self.outcomes())

    def test_the_message_identity_is_rechecked_at_the_boundary(self):
        original_save = self.flock._save

        def swap_identity():
            self.message.id = "m-someone-else"
            return original_save()

        self.flock._save = swap_identity
        self.assertFalse(self.recover())
        self.assertEqual([], self.presses(), "a different tracked message is not ours to press")


def screen_lines(screen):
    """The composer text as the pane reader sees it (last prompt line content)."""
    return OUR_TEXT + " and one more thing I was typing"


if __name__ == "__main__":
    unittest.main()
