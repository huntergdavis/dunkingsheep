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


class FirstObservationEvidenceTests(unittest.TestCase):
    """CoS i31729: the clock start must be observable from the record itself.

    A recovery's pass/fail needs T0, and neither the reset line nor the eventual key
    line can define it. So the FIRST creation of a tracker record emits one line built
    from the stored record - once per record, never per tick, and without any composer
    text (the log is not a place for pane content).
    """

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
            stuck_config={"threshold_s": 600, "max_attempts": 2, "cooldown_s": 60,
                          "observation_interval_s": 0.01})
        self.flock.composer_region_is_owned = lambda pane_id, screen: True
        self.herdr.screens[PANE] = f"│ ✻ idle\n❯ {OUR_TEXT}\n│ idle · pi\n"
        self.herdr.box[PANE] = OUR_TEXT
        self.flock._remember_typed(PANE, OUR_TEXT)
        self.message = self.flock._new_message(PANE, "Claude Proj", OUR_TEXT,
                                              "send_text", lambda: (True, "ok"))

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def observe(self):
        return self.flock._unwedge(PANE, OUR_TEXT, time.time() - 30, message=self.message)

    def presses(self):
        return [k for pane, k in self.herdr.keys if pane == PANE]

    def test_the_first_observation_emits_one_evidence_line_before_any_press(self):
        with self.assertLogs("dunkingsheep", level="INFO") as captured:
            self.assertFalse(self.observe(), "a 600 s threshold cannot act")
        lines = [line for line in captured.output if "stuck recovery clock starts" in line]
        self.assertEqual(1, len(lines), captured.output)
        line = lines[0]
        record = self.flock.stuck.records[PANE]
        for expected in (PANE, record["message_id"], record["generation"],
                         record["fingerprint"], "threshold=600s"):
            self.assertIn(str(expected), line)
        self.assertIn(f'first_seen={float(record["first_seen"]):.3f}', line)
        self.assertIn("observed=", line)
        self.assertEqual([], self.presses(), "evidence, and no action")

    def test_the_line_carries_no_composer_text(self):
        with self.assertLogs("dunkingsheep", level="INFO") as captured:
            self.observe()
        self.assertNotIn(OUR_TEXT[:40], "\n".join(captured.output),
                         "pane content must not be logged by the evidence line")

    def test_the_logged_first_seen_is_the_stored_one_not_the_tick_clock(self):
        since = time.time() - 300
        with self.assertLogs("dunkingsheep", level="INFO") as captured:
            self.flock._unwedge(PANE, OUR_TEXT, since, message=self.message)
        line = [l for l in captured.output if "stuck recovery clock starts" in l][0]
        record = self.flock.stuck.records[PANE]
        self.assertIn(f'first_seen={float(record["first_seen"]):.3f}', line)
        self.assertAlmostEqual(since, record["first_seen"], delta=2.0,
                               msg="the clock starts at the first sighting, not at this tick")
        self.assertGreater(record["first_seen"], 0.0)

    def test_repeated_waits_do_not_spam_the_evidence_line(self):
        with self.assertLogs("dunkingsheep", level="INFO") as captured:
            for _ in range(4):
                self.flock._stuck_last_check.pop(PANE, None)   # bypass the interval gate
                self.observe()
        lines = [line for line in captured.output if "stuck recovery clock starts" in line]
        self.assertEqual(1, len(lines),
                         f"one line per record, not per tick: {lines}")


class StandaloneObserverTests(unittest.TestCase):
    """CoS i31791: a LONE stalled delivery must be observed by a timer.

    The two-send staging was disproved - the second send flushed the first in 1.81 s
    through `_ensure_box_clear`, with zero recoveries. The real gap: `_outbox_worker`
    returns when the queue is empty, so a message that was typed, reported
    unconfirmed and left in the composer had no observer at all. These tests drive the
    ACTUAL worker step (`_stuck_observer_step`, the body of the daemon thread), with a
    fake clock - not `_unwedge` and not a second message.
    """

    THRESHOLD = 120.0
    T0 = 500_000.0

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
            stuck_config={"threshold_s": self.THRESHOLD, "max_attempts": 1,
                          "cooldown_s": 60, "observation_interval_s": 5})
        self.flock.composer_region_is_owned = lambda pane_id, screen: True
        self.herdr.screens[PANE] = f"│ ✻ idle\n❯ {OUR_TEXT}\n│ idle · pi\n"
        self.herdr.box[PANE] = OUR_TEXT              # typed by us, never submitted
        self.flock._remember_typed(PANE, OUR_TEXT)
        # A delivery that ASKED for Enter and was reported unconfirmed - the owner's case.
        self.message = self.flock._new_message(PANE, "Claude Proj", OUR_TEXT,
                                              "send_text", lambda: (True, "ok"))
        self.message.status = "failed"
        self.message.attempt_generation = self.flock._pane_generation(PANE)
        self.message.detail = "typed, not submitted"
        self.flock._remember(self.message)

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def presses(self):
        return [k for pane, k in self.herdr.keys if pane == PANE]

    def step(self, at):
        # The worker's own step, with the clock injected (the daemon thread passes None).
        import dunk_stuck
        original = dunk_stuck.Tracker.decide
        return self.flock._stuck_observer_step(now=at)

    def test_a_lone_stalled_message_is_the_only_thing_needed(self):
        self.assertEqual(0, self.flock._stuck_observer_step(now=self.T0),
                         "first sighting: evidence and a clock, no action")
        self.assertEqual([], self.presses())
        self.assertIn(PANE, self.flock.stuck.records,
                      "the standalone observer must start the clock on a lone message")

    def test_nothing_happens_before_the_threshold(self):
        self.flock._stuck_observer_step(now=self.T0)
        for at in (self.T0 + 1, self.T0 + 60, self.T0 + 119):
            self.flock._stuck_observer_step(now=at)
        self.assertEqual([], self.presses(), "no key before the threshold")

    def test_exactly_one_action_at_the_threshold_and_no_second_message(self):
        self.flock._stuck_observer_step(now=self.T0)
        before_ids = len(self.flock.message_history)
        acted = self.flock._stuck_observer_step(now=self.T0 + self.THRESHOLD)
        self.assertEqual(1, acted, "the worker reports the action it took")
        self.assertEqual(1, len(self.presses()), "exactly one key, and no other message")
        self.assertEqual(before_ids, len(self.flock.message_history),
                         "recovery must not create or send a second message")
        outcomes = [e["outcome"] for e in self.metrics.events if e["event"] == "stuck.recover"]
        self.assertEqual(["consumed"], outcomes)

    def test_an_explicit_no_enter_send_is_never_recovered(self):
        # `send --no-enter` is an instruction: re-register the intent as no-enter, the way
        # the daemon does when that delivery is decided.
        self.message.press_enter = False
        self.flock.stuck_candidates.clear()
        self.flock._remember(self.message)
        for at in (self.T0, self.T0 + self.THRESHOLD, self.T0 + 10_000):
            self.flock._stuck_observer_step(now=at)
        self.assertEqual([], self.presses(),
                         "an explicit --no-enter is not a stalled delivery")
        self.assertNotIn(PANE, self.flock.stuck.records,
                         "and it must not even start a clock")

    def test_consumption_stops_the_observation(self):
        self.flock._stuck_observer_step(now=self.T0)
        # The lane finally took it: the composer is empty on the SCREEN too, which is
        # what the reader actually reads.
        self.herdr.box[PANE] = ""
        self.herdr.screens[PANE] = "│ ✻ idle\n❯\u00a0\n\x1b[2midle · pi\x1b[0m\n"
        self.flock._stuck_observer_step(now=self.T0 + 5)
        self.assertNotIn(PANE, self.flock.stuck.records, "consumed text is forgotten")
        self.assertEqual([], self.presses())

    def test_a_cancelled_message_is_not_observed(self):
        self.message.status = "cancelled"
        for at in (self.T0, self.T0 + self.THRESHOLD):
            self.flock._stuck_observer_step(now=at)
        self.assertEqual([], self.presses())
        self.assertNotIn(PANE, self.flock.stuck.records)

    def test_a_dead_pane_is_not_observed(self):
        self.herdr.unreadable.add(PANE)             # the pane cannot be read at all
        for at in (self.T0, self.T0 + self.THRESHOLD, self.T0 + 200):
            self.flock._stuck_observer_step(now=at)
        self.assertEqual([], self.presses(), "an unreadable pane is never pressed")

    def test_a_pane_with_a_queued_message_is_left_to_its_outbox_worker(self):
        follower = self.flock._new_message(PANE, "Claude Proj", "a later notice",
                                          "send_text", lambda: (True, "ok"))
        self.flock._enqueue(follower)               # a real queued row
        for at in (self.T0, self.T0 + self.THRESHOLD):
            self.flock._stuck_observer_step(now=at)
        self.assertEqual([], self.presses(),
                         "exactly one observer: the outbox worker owns this pane")
        self.assertNotIn(PANE, self.flock.stuck.records)

    def test_the_ordinary_two_send_flush_stays_a_separate_control(self):
        # Documented as the control CoS measured: a queued message behind our text is
        # the outbox worker's job (it flushes), not the standalone observer's.
        follower = self.flock._new_message(PANE, "Claude Proj", "second message",
                                          "send_text", lambda: (True, "ok"))
        self.flock._enqueue(follower)
        self.assertEqual(0, self.flock._stuck_observer_step(now=self.T0),
                         "the standalone observer declines a pane with a queue")


class ObserverBoundaryTests(unittest.TestCase):
    """CoS i31795: three boundary failures reproduced against the frozen observer head.

    1. a newer SENT / no-enter delivery for the same pane must SUPERSEDE an older failed
       candidate with the same composer text;
    2. an unreadable observation must RESET the clock (fresh stable window afterwards);
    3. the candidate must not live in the 50-row display history - ordinary traffic must
       not drop the watchdog's own work.
    """

    THRESHOLD = 120.0
    T0 = 700_000.0

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
            stuck_config={"threshold_s": self.THRESHOLD, "max_attempts": 1,
                          "cooldown_s": 60, "observation_interval_s": 5})
        self.flock.composer_region_is_owned = lambda pane_id, screen: True
        self.herdr.screens[PANE] = f"│ ✻ idle\n❯ {OUR_TEXT}\n│ idle · pi\n"
        self.herdr.box[PANE] = OUR_TEXT
        self.flock._remember_typed(PANE, OUR_TEXT)
        self.failed = self.deliver(OUR_TEXT, status="failed", press_enter=True)

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def deliver(self, text, status="failed", press_enter=True, created_at=None):
        message = self.flock._new_message(PANE, "Claude Proj", text, "send_text",
                                         lambda: (True, "ok"))
        message.status = status
        message.press_enter = press_enter
        message.attempt_generation = self.flock._pane_generation(PANE)
        if created_at is not None:
            message.created_at = created_at
        self.flock._remember(message)
        return message

    def presses(self):
        return [k for pane, k in self.herdr.keys if pane == PANE]

    def test_a_newer_no_enter_delivery_supersedes_an_older_failed_candidate(self):
        self.flock._stuck_observer_step(now=self.T0)      # the failed candidate starts a clock
        self.deliver(OUR_TEXT, status="sent", press_enter=False,
                     created_at=self.failed.created_at + 1)
        for at in (self.T0 + 60, self.T0 + self.THRESHOLD, self.T0 + 600):
            self.flock._stuck_observer_step(now=at)
        self.assertEqual([], self.presses(),
                         "a newer explicit no-enter intent must not be overridden")

    def test_a_newer_sent_delivery_also_supersedes(self):
        self.flock._stuck_observer_step(now=self.T0)
        self.deliver(OUR_TEXT, status="sent", press_enter=True,
                     created_at=self.failed.created_at + 1)
        for at in (self.T0 + self.THRESHOLD, self.T0 + 600):
            self.flock._stuck_observer_step(now=at)
        self.assertEqual([], self.presses(), "the pane's latest intent is a clean send")

    def test_an_unreadable_observation_resets_the_clock(self):
        self.flock._stuck_observer_step(now=self.T0)
        self.herdr.unreadable.add(PANE)
        self.flock._stuck_observer_step(now=self.T0 + 60)      # unknown -> reset
        self.assertNotIn(PANE, self.flock.stuck.records, "unknown state must reset the clock")
        self.herdr.unreadable.discard(PANE)
        self.flock._stuck_observer_step(now=self.T0 + self.THRESHOLD)  # first sighting again
        self.assertEqual([], self.presses(), "no press on the stale pre-unknown clock")
        # A FRESH stable window is required before acting.
        self.flock._stuck_observer_step(now=self.T0 + self.THRESHOLD + self.THRESHOLD)
        self.assertEqual(1, len(self.presses()), "one press after a fresh window, not before")

    def test_display_history_traffic_does_not_drop_the_candidate(self):
        self.flock._stuck_observer_step(now=self.T0)
        # 50 unrelated SENT rows: enough to push the failed message out of a 50-row
        # display buffer (CoS reproduced exactly this).
        for n in range(50):
            other = self.flock._new_message("w1:p1", "shell", f"traffic {n}", "send_text",
                                           lambda: (True, "ok"))
            other.status = "sent"
            self.flock._remember(other)
        self.assertNotIn(self.failed, self.flock.message_history,
                         "premise: the failed message left the display history")
        self.flock._stuck_observer_step(now=self.T0 + self.THRESHOLD)
        self.assertEqual(1, len(self.presses()),
                         "the candidate is independent of the display buffer")

    def test_the_candidate_survives_a_restart(self):
        state = os.path.join(self.tmp.name, "dunks.json")
        flo = dunk_core.Flock(
            herdr=self.herdr, state_file=state, persist=True, restore=True,
            inbox=self.inbox, tasks=Tasks(path=os.path.join(self.tmp.name, "t2.db")),
            metrics=self.metrics,
            stuck_config={"threshold_s": self.THRESHOLD, "max_attempts": 1,
                          "cooldown_s": 60, "observation_interval_s": 5})
        try:
            flo.composer_region_is_owned = lambda pane_id, screen: True
            flo._remember_typed(PANE, OUR_TEXT)
            message = flo._new_message(PANE, "Claude Proj", OUR_TEXT, "send_text",
                                      lambda: (True, "ok"))
            message.status = "failed"
            message.attempt_generation = flo._pane_generation(PANE)
            flo._remember(message)
            self.assertIn(PANE, flo.stuck_candidates)
            flo._save()
        finally:
            flo.close()
        again = dunk_core.Flock(
            herdr=self.herdr, state_file=state, persist=True, restore=True,
            inbox=self.inbox, tasks=Tasks(path=os.path.join(self.tmp.name, "t3.db")),
            metrics=self.metrics,
            stuck_config={"threshold_s": self.THRESHOLD, "max_attempts": 1,
                          "cooldown_s": 60, "observation_interval_s": 5})
        try:
            self.assertIn(PANE, again.stuck_candidates,
                          "the eligibility queue must survive a daemon restart")
            self.assertEqual(OUR_TEXT, again.stuck_candidates[PANE].get("message_id") and OUR_TEXT)
            self.assertTrue(again.stuck_candidates[PANE]["press_enter"])
        finally:
            again.close()

    def test_consumption_retires_the_candidate_to_a_watermark(self):
        self.flock._stuck_observer_step(now=self.T0)
        self.herdr.box[PANE] = ""
        self.herdr.screens[PANE] = "│ ✻ idle\n❯\u00a0\n\x1b[2midle · pi\x1b[0m\n"
        self.flock._stuck_observer_step(now=self.T0 + 5)
        entry = self.flock.stuck_candidates.get(PANE) or {}
        self.assertEqual("consumed", entry.get("status"), "consumption retires the work")
        self.assertFalse(entry.get("eligible"))
        self.assertNotIn(PANE, self.flock.stuck.records)


class RestartRecoveryTests(unittest.TestCase):
    """CoS i31799 #1: after a restart the candidate existed but ownership could not be
    proven (no message_history, no sent_text_by_pane), so nothing ever pressed."""

    THRESHOLD = 120.0
    T0 = 900_000.0

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.state = os.path.join(self.tmp.name, "dunks.json")
        self.herdr = FakeHerdr()
        for pane in self.herdr.panes:
            if pane["pane_id"] == PANE:
                pane["pane_pid"] = os.getpid()
        self.metrics = _RecordingMetrics()
        self.inbox = Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                           blob_dir=os.path.join(self.tmp.name, "blobs"))
        self.herdr.screens[PANE] = f"│ ✻ idle\n❯ {OUR_TEXT}\n│ idle · pi\n"
        self.herdr.box[PANE] = OUR_TEXT
        first = self.flock()
        try:
            first.composer_region_is_owned = lambda pane_id, screen: True
            first._remember_typed(PANE, OUR_TEXT)
            message = first._new_message(PANE, "Claude Proj", OUR_TEXT, "send_text",
                                        lambda: (True, "ok"))
            message.status = "failed"
            message.attempt_generation = first._pane_generation(PANE)
            first._remember(message)                 # records the candidate
            first._save()
        finally:
            first.close()

    def flock(self):
        return dunk_core.Flock(
            herdr=self.herdr, state_file=self.state, persist=True, restore=True,
            inbox=self.inbox, tasks=Tasks(path=os.path.join(self.tmp.name, "tasks.db")),
            metrics=self.metrics,
            stuck_config={"threshold_s": self.THRESHOLD, "max_attempts": 1,
                          "cooldown_s": 60, "observation_interval_s": 5})

    def tearDown(self):
        self.inbox.close()
        self.tmp.cleanup()

    def presses(self):
        return [k for pane, k in self.herdr.keys if pane == PANE]

    def test_the_restarted_worker_actually_recovers(self):
        fresh = self.flock()
        try:
            # NO STUB: this is the production authority path. The real matcher
            # (`_region_is_ours`) reads sent_text_by_pane / the outbox, so the restart
            # must REHYDRATE the persisted text for the press to be authorised at all.
            self.assertIn(PANE, fresh.stuck_candidates, "the candidate persisted")
            self.assertEqual([], fresh.sent_text_by_pane.get(PANE) or [],
                             "premise: a restart remembers no sends by itself")
            fresh._stuck_observer_step(now=self.T0)                 # binds + starts the window
            self.assertEqual([], self.presses(), "the first step is only evidence")
            # THE FIX UNDER TEST: once the candidate is generation-bound, the persisted
            # exact text becomes ownership evidence, so the REAL matcher authorises.
            self.assertIn(OUR_TEXT, fresh.sent_text_by_pane.get(PANE) or [],
                          "the persisted text must be rehydrated once it is bound")
            self.assertTrue(fresh.composer_region_is_owned(PANE, self.herdr.screens[PANE]),
                            "the REAL matcher must authorise this composer after a restart")
            fresh._stuck_observer_step(now=self.T0 + self.THRESHOLD)
            self.assertEqual(1, len(self.presses()),
                             "the restarted worker must press, using the persisted text")
            outcomes = [e["outcome"] for e in self.metrics.events
                        if e["event"] == "stuck.recover"]
            self.assertEqual(["consumed"], outcomes)
        finally:
            fresh.close()

    def test_a_human_continuation_is_refused_after_a_restart_by_the_real_matcher(self):
        # NO stub here: the real matcher reads the COMPLETE composer region, so a
        # continuation line below our text makes the frame not-ours and nothing presses.
        fresh = self.flock()
        try:
            self.herdr.screens[PANE] = (f"│ ✻ idle\n❯ {OUR_TEXT}\n"
                                        "  and one more thing I was typing\n│ idle · pi\n")
            self.assertFalse(fresh.composer_region_is_owned(
                PANE, self.herdr.screens[PANE]), "premise: the real matcher refuses")
            fresh._stuck_observer_step(now=self.T0)
            fresh._stuck_observer_step(now=self.T0 + self.THRESHOLD)
            fresh._stuck_observer_step(now=self.T0 + self.THRESHOLD * 2)
            self.assertEqual([], self.presses(),
                             "the persisted text must not authorise a modified composer")
        finally:
            fresh.close()


class IntentOrderingTests(unittest.TestCase):
    """CoS i31799 #2: completion order is not decision order."""

    THRESHOLD = 120.0
    T0 = 950_000.0

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
            stuck_config={"threshold_s": self.THRESHOLD, "max_attempts": 1,
                          "cooldown_s": 60, "observation_interval_s": 5})
        self.flock.composer_region_is_owned = lambda pane_id, screen: True
        self.herdr.screens[PANE] = f"│ ✻ idle\n❯ {OUR_TEXT}\n│ idle · pi\n"
        self.herdr.box[PANE] = OUR_TEXT
        self.flock._remember_typed(PANE, OUR_TEXT)

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def decide(self, created_at, status, press_enter=True, text=OUR_TEXT):
        message = self.flock._new_message(PANE, "Claude Proj", text, "send_text",
                                         lambda: (True, "ok"))
        message.status = status
        message.press_enter = press_enter
        message.attempt_generation = self.flock._pane_generation(PANE)
        message.created_at = created_at
        self.flock._remember(message)
        return message

    def presses(self):
        return [k for pane, k in self.herdr.keys if pane == PANE]

    def test_a_late_older_failure_cannot_undo_a_newer_no_enter_intent(self):
        # The NEWER decision (a no-enter send) is remembered FIRST; the older FAILED one
        # completes and is remembered LATER.
        self.decide(created_at=2_000.0, status="sent", press_enter=False)
        self.decide(created_at=1_000.0, status="failed")
        for at in (self.T0, self.T0 + self.THRESHOLD, self.T0 + 600):
            self.flock._stuck_observer_step(now=at)
        self.assertEqual([], self.presses(), "the newer intent must still stand")

    def test_a_late_older_failure_cannot_undo_a_newer_clean_send(self):
        self.decide(created_at=2_000.0, status="sent", press_enter=True)
        self.decide(created_at=1_000.0, status="failed")
        for at in (self.T0, self.T0 + self.THRESHOLD):
            self.flock._stuck_observer_step(now=at)
        self.assertEqual([], self.presses())

    def test_a_late_older_failure_cannot_undo_a_newer_cancellation(self):
        self.decide(created_at=2_000.0, status="cancelled")
        self.decide(created_at=1_000.0, status="failed")
        for at in (self.T0, self.T0 + self.THRESHOLD):
            self.flock._stuck_observer_step(now=at)
        self.assertEqual([], self.presses())

    def test_the_ordinary_order_still_recovers(self):
        self.decide(created_at=1_000.0, status="failed")
        for at in (self.T0, self.T0 + self.THRESHOLD):
            self.flock._stuck_observer_step(now=at)
        self.assertEqual(1, len(self.presses()), "a plain old failure still gets its press")


class RevokedAuthorityTests(unittest.TestCase):
    """CoS i31802 #2: the registry must AUTHORISE the press, not just name the id."""

    THRESHOLD = 120.0
    T0 = 1_100_000.0

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
            stuck_config={"threshold_s": self.THRESHOLD, "max_attempts": 1,
                          "cooldown_s": 60, "observation_interval_s": 5})
        self.flock.composer_region_is_owned = lambda pane_id, screen: True
        self.herdr.screens[PANE] = f"│ ✻ idle\n❯ {OUR_TEXT}\n│ idle · pi\n"
        self.herdr.box[PANE] = OUR_TEXT
        self.flock._remember_typed(PANE, OUR_TEXT)
        self.message = self.flock._new_message(PANE, "Claude Proj", OUR_TEXT, "send_text",
                                              lambda: (True, "ok"))
        self.message.status = "failed"
        self.message.attempt_generation = self.flock._pane_generation(PANE)
        self.flock._remember(self.message)

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def presses(self):
        return [k for pane, k in self.herdr.keys if pane == PANE]

    def test_authority_revoked_after_the_decision_prevents_the_press(self):
        original_save = self.flock._save

        def revoke_then_save():
            entry = dict(self.flock.stuck_candidates.get(PANE) or {})
            entry.update({"eligible": False, "status": "cancelled"})
            self.flock.stuck_candidates[PANE] = entry
            return original_save()

        self.flock._save = revoke_then_save
        self.flock._stuck_observer_step(now=self.T0)
        self.flock._stuck_observer_step(now=self.T0 + self.THRESHOLD)
        self.assertEqual([], self.presses(),
                         "naming the id is not authorising the press")

    def test_authority_intact_still_presses(self):
        self.flock._stuck_observer_step(now=self.T0)
        self.flock._stuck_observer_step(now=self.T0 + self.THRESHOLD)
        self.assertEqual(1, len(self.presses()))


class SameTimestampOrderTests(unittest.TestCase):
    """CoS i31802 #3: ties must follow the message SEQUENCE, not the string form."""

    THRESHOLD = 120.0
    T0 = 1_200_000.0

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
            stuck_config={"threshold_s": self.THRESHOLD, "max_attempts": 1,
                          "cooldown_s": 60, "observation_interval_s": 5})
        self.flock.composer_region_is_owned = lambda pane_id, screen: True
        self.herdr.screens[PANE] = f"│ ✻ idle\n❯ {OUR_TEXT}\n│ idle · pi\n"
        self.herdr.box[PANE] = OUR_TEXT
        self.flock._remember_typed(PANE, OUR_TEXT)

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def remember(self, message_id, status, press_enter=True, created_at=5_000.0):
        message = self.flock._new_message(PANE, "Claude Proj", OUR_TEXT, "send_text",
                                         lambda: (True, "ok"))
        message.id = message_id                     # the id IS the creation sequence
        message.status = status
        message.press_enter = press_enter
        message.attempt_generation = self.flock._pane_generation(PANE)
        message.created_at = created_at
        self.flock._remember(message)
        return message

    def presses(self):
        return [k for pane, k in self.herdr.keys if pane == PANE]

    def test_same_timestamp_uses_message_sequence_not_lexical_order(self):
        # m10 (newer, no-enter) is decided first; the older m9 FAILED arrives later.
        self.remember("m10", status="sent", press_enter=False)
        self.remember("m9", status="failed")
        for at in (self.T0, self.T0 + self.THRESHOLD, self.T0 + 600):
            self.flock._stuck_observer_step(now=at)
        self.assertEqual([], self.presses(),
                         "'m10' must outrank 'm9' by sequence, not by string order")

    def test_consumption_keeps_the_watermark(self):
        self.remember("m2", status="failed", created_at=1_000.0)
        # Consumption retires the entry to a tombstone instead of discarding it.
        self.herdr.box[PANE] = ""
        self.herdr.screens[PANE] = "│ ✻ idle\n❯\u00a0\n\x1b[2midle · pi\x1b[0m\n"
        self.flock._stuck_observer_step(now=self.T0)
        entry = self.flock.stuck_candidates.get(PANE) or {}
        self.assertEqual("consumed", entry.get("status"))
        self.assertFalse(entry.get("eligible"))
        # A late, older FAILED decision must not resurrect the work.
        self.remember("m1", status="failed", created_at=900.0)
        self.herdr.box[PANE] = OUR_TEXT
        self.herdr.screens[PANE] = f"│ ✻ idle\n❯ {OUR_TEXT}\n│ idle · pi\n"
        for at in (self.T0 + 10, self.T0 + self.THRESHOLD, self.T0 + 600):
            self.flock._stuck_observer_step(now=at)
        self.assertEqual([], self.presses(), "the watermark must survive consumption")


class RetirementRaceTests(unittest.TestCase):
    """CoS i31804: retirement must not delete a NEWER candidate from a stale snapshot."""

    THRESHOLD = 120.0
    T0 = 1_300_000.0

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
            stuck_config={"threshold_s": self.THRESHOLD, "max_attempts": 1,
                          "cooldown_s": 60, "observation_interval_s": 5})
        self.flock.composer_region_is_owned = lambda pane_id, screen: True
        self.herdr.screens[PANE] = f"│ ✻ idle\n❯ {OUR_TEXT}\n│ idle · pi\n"
        self.herdr.box[PANE] = OUR_TEXT
        self.flock._remember_typed(PANE, OUR_TEXT)
        self.old = self.deliver("m1", OUR_TEXT, created_at=1_000.0)

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def deliver(self, message_id, text, created_at):
        message = self.flock._new_message(PANE, "Claude Proj", text, "send_text",
                                         lambda: (True, "ok"))
        message.id = message_id
        message.status = "failed"
        message.attempt_generation = self.flock._pane_generation(PANE)
        message.created_at = created_at
        self.flock._remember(message)
        return message

    def presses(self):
        return [k for pane, k in self.herdr.keys if pane == PANE]

    def test_a_new_candidate_arriving_during_an_empty_read_survives(self):
        # The observer reads the box, gets an EMPTY snapshot (stale), and while it reads
        # a NEWER failed message m2 is remembered. The retirement path must retire only
        # the entry it observed (m1), never m2.
        original_box = self.flock._box_text

        def empty_but_race(pane_id):
            self.deliver("m2", "a newer stalled line", created_at=2_000.0)
            return ""                      # the stale read

        self.flock._box_text = empty_but_race
        self.flock._stuck_observer_step(now=self.T0)
        self.flock._box_text = original_box
        entry = self.flock.stuck_candidates.get(PANE) or {}
        self.assertEqual("m2", entry.get("message_id"),
                         "the newer candidate must survive a stale empty read")
        self.assertTrue(entry.get("eligible"), "and it stays eligible for its own clock")


class GenerationBindingTests(unittest.TestCase):
    """CoS i31806: a restored candidate must be bound to the pane generation it belongs
    to. A replacement pane must RETIRE it, never inherit the old text as authority."""

    THRESHOLD = 120.0
    T0 = 1_500_000.0

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.state = os.path.join(self.tmp.name, "dunks.json")
        self.herdr = FakeHerdr()
        for pane in self.herdr.panes:
            if pane["pane_id"] == PANE:
                pane["pane_pid"] = os.getpid()
        self.metrics = _RecordingMetrics()
        self.inbox = Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                           blob_dir=os.path.join(self.tmp.name, "blobs"))
        self.herdr.screens[PANE] = f"│ ✻ idle\n❯ {OUR_TEXT}\n│ idle · pi\n"
        self.herdr.box[PANE] = OUR_TEXT
        first = self.flock()
        try:
            first._remember_typed(PANE, OUR_TEXT)
            message = first._new_message(PANE, "Claude Proj", OUR_TEXT, "send_text",
                                        lambda: (True, "ok"))
            message.status = "failed"
            message.attempt_generation = first._pane_generation(PANE)
            first._remember(message)                 # records the candidate + generation
            first._save()
            self.saved_generation = first.stuck_candidates[PANE]["generation"]
            self.assertTrue(self.saved_generation, "the decision must persist a generation")
        finally:
            first.close()

    def flock(self):
        return dunk_core.Flock(
            herdr=self.herdr, state_file=self.state, persist=True, restore=True,
            inbox=self.inbox, tasks=Tasks(path=os.path.join(self.tmp.name, "tasks.db")),
            metrics=self.metrics,
            stuck_config={"threshold_s": self.THRESHOLD, "max_attempts": 1,
                          "cooldown_s": 60, "observation_interval_s": 5})

    def tearDown(self):
        self.inbox.close()
        self.tmp.cleanup()

    def presses(self):
        return [k for pane, k in self.herdr.keys if pane == PANE]

    def replace_pane_with_a_real_live_process(self):
        """A DIFFERENT live child process at the same pane id (CoS's repro)."""
        import subprocess, sys
        child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
        self.addCleanup(child.wait)
        self.addCleanup(child.terminate)
        for pane in self.herdr.panes:
            if pane["pane_id"] == PANE:
                pane["pane_pid"] = child.pid
        return child.pid

    def test_the_same_generation_still_recovers_with_the_real_matcher(self):
        fresh = self.flock()
        try:
            self.assertEqual(self.saved_generation,
                             fresh.stuck_candidates[PANE]["generation"])
            # Ownership evidence is BOUND to the generation inside the observer step, so
            # the matcher authorises after the first step, not before it.
            fresh._stuck_observer_step(now=self.T0)
            self.assertTrue(fresh.composer_region_is_owned(PANE, self.herdr.screens[PANE]),
                            "bound ownership evidence must let the real matcher authorise")
            fresh._stuck_observer_step(now=self.T0 + self.THRESHOLD)
            self.assertEqual(1, len(self.presses()),
                             "a same-generation restart must still press, once")
        finally:
            fresh.close()

    def test_a_replacement_generation_is_retired_not_adopted(self):
        old_pid = os.getpid()
        new_pid = self.replace_pane_with_a_real_live_process()
        self.assertNotEqual(old_pid, new_pid)
        fresh = self.flock()
        try:
            self.assertNotEqual(self.saved_generation,
                                fresh._pane_generation(PANE),
                                "premise: the generation really changed")
            # The replacement pane really shows the same text - the VISUAL premise holds -
            # and the seam still refuses, because no ownership evidence was ever bound to
            # the new generation. That is the contract.
            self.assertIn(OUR_TEXT, self.herdr.screens[PANE])
            self.assertFalse(fresh.composer_region_is_owned(PANE, self.herdr.screens[PANE]),
                             "the replacement generation must not inherit ownership")
            for at in (self.T0, self.T0 + self.THRESHOLD, self.T0 + self.THRESHOLD * 2):
                fresh._stuck_observer_step(now=at)
            self.assertEqual([], self.presses(),
                             "a replacement pane must never inherit the old authority")
            entry = fresh.stuck_candidates.get(PANE) or {}
            self.assertEqual("generation-changed", entry.get("status"),
                             "the candidate is retired with a named reason, watermark kept")
            self.assertFalse(entry.get("eligible"))
        finally:
            fresh.close()

    def test_an_unbound_candidate_fails_closed(self):
        # A state file from an older build (or a truncated entry) has no generation.
        import json as _json
        payload = _json.load(open(self.state))
        payload["stuck_candidates"][PANE].pop("generation", None)
        open(self.state, "w").write(_json.dumps(payload))
        fresh = self.flock()
        try:
            for at in (self.T0, self.T0 + self.THRESHOLD, self.T0 + self.THRESHOLD * 2):
                fresh._stuck_observer_step(now=at)
            self.assertEqual([], self.presses(),
                             "an unbound candidate must not act on a pane it cannot identify")
        finally:
            fresh.close()


class AttemptGenerationTests(unittest.TestCase):
    """CoS i31809: the candidate must carry the generation of the ATTEMPT, captured
    before the text/keys, not a pane read taken after the outcome."""

    THRESHOLD = 120.0
    T0 = 1_700_000.0

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
            stuck_config={"threshold_s": self.THRESHOLD, "max_attempts": 1,
                          "cooldown_s": 60, "observation_interval_s": 5})
        self.flock.composer_region_is_owned = lambda pane_id, screen: True
        self.herdr.screens[PANE] = f"│ ✻ idle\n❯ {OUR_TEXT}\n│ idle · pi\n"
        self.herdr.box[PANE] = OUR_TEXT
        self.flock._remember_typed(PANE, OUR_TEXT)

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def presses(self):
        return [k for pane, k in self.herdr.keys if pane == PANE]

    def live_child(self):
        import subprocess, sys
        child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
        self.addCleanup(child.wait)
        self.addCleanup(child.terminate)
        return child.pid

    def delivery_that_replaces_the_pane_mid_attempt(self):
        """The real guarded path: type text, swap the pane BEFORE the outcome lands."""
        original_send_text = self.herdr.send_text

        def send_then_replace(pane_id, text):
            result = original_send_text(pane_id, text)   # the text really goes in
            for pane in self.herdr.panes:                # the pane is replaced now
                if pane["pane_id"] == PANE:
                    pane["pane_pid"] = self.live_child()
            return result

        self.herdr.send_text = send_then_replace
        message = self.flock._new_message(PANE, "Claude Proj", OUR_TEXT, "send_text",
                                         lambda: self.flock.submit_text(PANE, OUR_TEXT))
        self.flock._deliver(message)          # the ACTUAL delivery path
        self.herdr.send_text = original_send_text
        return message

    def failing_delivery_that_replaces_the_pane_mid_attempt(self):
        """Same, but the submit does not take: the message ends FAILED and eligible.

        The callable types the text for real and then reports the mixed outcome the
        production path reports when the box keeps the text; the pane is replaced inside
        the attempt, i.e. BEFORE `_deliver` decides the outcome.
        """
        def deliver_then_replace():
            self.herdr.send_text(PANE, OUR_TEXT)       # the text really goes in
            for pane in self.herdr.panes:
                if pane["pane_id"] == PANE:
                    pane["pane_pid"] = self.live_child()
            return False, "typed, not submitted"       # the shape the real path reports

        message = self.flock._new_message(PANE, "Claude Proj", OUR_TEXT, "send_text",
                                         deliver_then_replace)
        self.flock._deliver(message)
        return message

    def test_the_attempt_identity_is_captured_before_delivery_not_after(self):
        before = self.flock._pane_generation(PANE)
        message = self.delivery_that_replaces_the_pane_mid_attempt()
        after = self.flock._pane_generation(PANE)
        self.assertNotEqual(before, after, "premise: the pane really was replaced")
        self.assertEqual(before, message.attempt_generation,
                         "the attempt identity must be the pane the text went into")
        self.assertEqual(before, self.flock.stuck_candidates[PANE]["generation"],
                         "the registry must carry the ATTEMPT generation")

    def test_a_pane_replaced_during_the_attempt_gets_no_watcher_key(self):
        message = self.failing_delivery_that_replaces_the_pane_mid_attempt()
        self.assertEqual("failed", message.status, "premise: the submit did not take")
        # The DELIVERY's own keys are not the subject here; the watcher must add none.
        baseline = len(self.presses())
        for at in (self.T0, self.T0 + self.THRESHOLD, self.T0 + self.THRESHOLD * 2):
            self.flock._stuck_observer_step(now=at)
        self.assertEqual(baseline, len(self.presses()),
                         "a replacement pane must never inherit the old attempt's authority")
        entry = self.flock.stuck_candidates.get(PANE) or {}
        self.assertEqual("generation-changed", entry.get("status"))
        self.assertFalse(entry.get("eligible"))

    def test_an_unchanged_generation_still_presses_once(self):
        message = self.flock._new_message(PANE, "Claude Proj", OUR_TEXT, "send_text",
                                         lambda: (True, "ok"))
        message.status = "failed"
        message.attempt_generation = self.flock._pane_generation(PANE)
        message.attempt_generation = self.flock._pane_generation(PANE)
        self.flock._remember(message)
        self.flock._stuck_observer_step(now=self.T0)
        self.flock._stuck_observer_step(now=self.T0 + self.THRESHOLD)
        self.assertEqual(1, len(self.presses()))

    def test_a_candidate_with_no_attempt_identity_is_never_eligible(self):
        message = self.flock._new_message(PANE, "Claude Proj", OUR_TEXT, "send_text",
                                         lambda: (True, "ok"))
        message.status = "failed"                  # no attempt_generation recorded
        self.flock._remember(message)
        entry = self.flock.stuck_candidates.get(PANE) or {}
        self.assertFalse(entry.get("eligible"),
                         "unknown attempt identity must fail closed, not guess")
        for at in (self.T0, self.T0 + self.THRESHOLD):
            self.flock._stuck_observer_step(now=at)
        self.assertEqual([], self.presses())


def screen_lines(screen):
    """The composer text as the pane reader sees it (last prompt line content)."""
    return OUR_TEXT + " and one more thing I was typing"


if __name__ == "__main__":
    unittest.main()
