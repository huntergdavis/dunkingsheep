import json
import os
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import dunk_core  # noqa: E402
from dunk_core import (  # noqa: E402
    DunkError, Dunker, Flock, expand_template, format_interval, parse_interval,
)
from fakes import (  # noqa: E402
    FakeHerdr, claude_box_collapsed, claude_box_collapsed_ansi, claude_box_empty,
    claude_box_empty_ansi, claude_box_hint_ansi, claude_box_pasted_unsubmitted,
    claude_box_pending, claude_box_typing_ansi, codex_box_empty_with_transcript_collapse,
    codex_box_hint_ansi, codex_box_rotating_hint, codex_box_typing_ansi,
)


def wait_until(predicate, timeout=5.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if predicate():
            return True
        time.sleep(0.02)
    return False


class ParseIntervalTests(unittest.TestCase):
    def test_numbers_and_suffixes(self):
        self.assertEqual(10.0, parse_interval(10))
        self.assertEqual(10.0, parse_interval("10"))
        self.assertEqual(1.5, parse_interval("90s"))
        self.assertEqual(90.0, parse_interval("1.5h"))
        self.assertEqual(2.0, parse_interval("2m"))
        self.assertEqual(0.5, parse_interval(0.5))

    def test_format_round_trips(self):
        for minutes in (10, 1.5, 0.5, 1 / 12, 120):
            self.assertEqual(float(minutes), parse_interval(format_interval(minutes)))
        self.assertEqual("30s", format_interval(0.5))
        self.assertEqual("5s", format_interval(1 / 12))
        self.assertEqual("10", format_interval(10))
        self.assertEqual("1.5", format_interval(1.5))

    def test_rejects_bad_values(self):
        for bad in ("0", -1, "abc", "", True, "10x"):
            with self.assertRaises(DunkError):
                parse_interval(bad)


class TemplateTests(unittest.TestCase):
    def test_known_placeholders_expand_and_others_survive(self):
        dunker = Dunker("d7", name="nightly", interval_minutes=60, target_pane_id="w1:p2",
                        target_tab="Claude Proj", target_agent="claude", send_count=2)
        text = 'dunk {id} "{name}" #{count} -> {target} every {interval}m {"json": 1} {unknown}'
        out = expand_template(text, dunker)
        self.assertEqual(
            'dunk d7 "nightly" #3 -> Claude Proj / claude every 60m {"json": 1} {unknown}', out
        )

    def test_name_falls_back_to_id(self):
        self.assertEqual("d1", expand_template("{name}", Dunker("d1")))


class FlockTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.state = os.path.join(self.tmp.name, "dunks.json")
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, state_file=self.state)

    def tearDown(self):
        self.flock.close()
        self.tmp.cleanup()

    # -- CRUD -----------------------------------------------------------------

    def test_add_assigns_ids_and_resolves_target_labels(self):
        dunk = self.flock.add(target="w1:p2", text="go", interval_minutes=5)
        self.assertEqual("d1", dunk["id"])
        self.assertEqual("Claude Proj / claude", dunk["target_label"])
        self.assertEqual("alpha", dunk["target_workspace"])
        self.assertFalse(dunk["running"])
        second = self.flock.add(target="codex", interval_minutes="30s")
        self.assertEqual("d2", second["id"])
        self.assertEqual(0.5, second["interval_minutes"])

    def test_resolve_target_by_self_substring_and_terminal_id(self):
        self.assertEqual("w1:p1", self.flock.resolve_target("self", self_pane_id="w1:p1")["pane_id"])
        self.assertEqual("w2:p1", self.flock.resolve_target("codex site")["pane_id"])
        self.assertEqual("w1:p2", self.flock.resolve_target("term_b")["pane_id"])
        # "alpha" matches both alpha panes, but only one hosts an agent.
        self.assertEqual("w1:p2", self.flock.resolve_target("alpha")["pane_id"])
        with self.assertRaisesRegex(DunkError, "no herdr pane matches"):
            self.flock.resolve_target("nothing-here")
        with self.assertRaisesRegex(DunkError, "needs HERDR_PANE_ID"):
            self.flock.resolve_target("self")
        with self.assertRaisesRegex(DunkError, "ambiguous"):
            self.flock.resolve_target("/home/x")

    def test_exact_tab_label_beats_title_substring_on_an_agent_pane(self):
        # "Sheep" is the exact label of a shell tab and a substring of an agent
        # pane's title; the exact label must win despite the agent tie-break.
        self.assertEqual("w2:p2", self.flock.resolve_target("sheep")["pane_id"])
        self.assertEqual("w2:p2", self.flock.resolve_target("Sheep")["pane_id"])
        # A substring that only the title matches still resolves.
        self.assertEqual("w2:p3", self.flock.resolve_target("release work")["pane_id"])
        # Exact workspace label with one agent inside picks that agent.
        self.assertEqual("w1:p2", self.flock.resolve_target("alpha")["pane_id"])

    def test_get_accepts_bare_number(self):
        self.flock.add(target="w1:p1")
        self.assertEqual("d1", self.flock.get("1")["id"])
        with self.assertRaises(DunkError):
            self.flock.get("d9")

    def test_update_changes_fields_and_clears_optionals(self):
        dunk = self.flock.add(target="w1:p1", only_when="idle", max_sends=3)
        updated = self.flock.update(dunk["id"], text="new", interval_minutes="2h",
                                    name="n", only_when=None, max_sends=None,
                                    target="codex")
        self.assertEqual("new", updated["text"])
        self.assertEqual(120.0, updated["interval_minutes"])
        self.assertEqual("n", updated["name"])
        self.assertIsNone(updated["only_when"])
        self.assertIsNone(updated["max_sends"])
        self.assertEqual("w2:p1", updated["target_pane_id"])
        with self.assertRaises(DunkError):
            self.flock.update(dunk["id"], only_when="busy")
        with self.assertRaises(DunkError):
            self.flock.update(dunk["id"], max_sends=-2)

    def test_start_requires_target(self):
        dunk = self.flock.add()
        with self.assertRaisesRegex(DunkError, "no target"):
            self.flock.start(dunk["id"])
        self.assertFalse(self.flock.get(dunk["id"])["running"])
        # add(start=True) without a target creates it stopped instead of failing.
        stopped = self.flock.add(start=True)
        self.assertFalse(stopped["running"])
        self.assertEqual("No target - press c", stopped["status"])

    def test_remove_stops_and_deletes(self):
        dunk = self.flock.add(target="w1:p1", start=True)
        self.assertTrue(dunk["running"])
        self.flock.remove(dunk["id"])
        self.assertEqual([], self.flock.list())
        self.assertEqual(0, self.flock.status()["running"])

    def test_stop_all_and_toggle(self):
        a = self.flock.add(target="w1:p1", start=True)
        b = self.flock.add(target="w2:p1", start=True)
        self.assertEqual(2, self.flock.status()["running"])
        self.flock.stop_all()
        self.assertEqual(0, self.flock.status()["running"])
        self.assertTrue(self.flock.toggle(a["id"])["running"])
        self.assertFalse(self.flock.toggle(a["id"])["running"])
        self.assertFalse(self.flock.get(b["id"])["running"])

    # -- sending --------------------------------------------------------------

    def test_fire_sends_expanded_text_synchronously(self):
        dunk = self.flock.add(target="w1:p2", text="hello from {id}")
        result = self.flock.fire(dunk["id"])
        self.assertTrue(result["ok"])
        self.assertEqual([("w1:p2", "hello from d1")], self.herdr.sends)
        self.assertEqual(1, self.flock.get(dunk["id"])["send_count"])
        self.assertTrue(self.flock.get(dunk["id"])["status"].startswith("Tested"))

    def test_fire_reports_failure(self):
        self.herdr.fail_sends = True
        dunk = self.flock.add(target="w1:p2")
        result = self.flock.fire(dunk["id"])
        self.assertFalse(result["ok"])
        self.assertEqual("boom", self.flock.get(dunk["id"])["last_error"])

    def test_send_now_and_read_pane(self):
        # Nobody is typing, so it is delivered as soon as the box settles.
        result = self.flock.send_now("codex", "ping", wait_s=5)
        self.assertTrue(result["ok"])
        self.assertEqual("sent", result["status"])
        self.assertEqual([("w2:p1", "ping")], self.herdr.sends)
        read = self.flock.read_pane("w2:p1", lines=5)
        self.assertEqual("ping\n", read["text"])
        with self.assertRaises(DunkError):
            self.flock.send_now("codex", "   ")

    def test_timer_fires_on_schedule_and_reschedules(self):
        # One-second interval (the loop floors at 1s).
        dunk = self.flock.add(target="w1:p1", text="tick {count}", interval_minutes=1 / 60,
                              start=True)
        self.assertTrue(self.herdr.sent.wait(4))
        self.assertEqual(("w1:p1", "tick 1"), self.herdr.sends[0])
        self.assertTrue(wait_until(lambda: len(self.herdr.sends) >= 2, 4))
        state = self.flock.get(dunk["id"])
        self.assertTrue(state["running"])
        self.assertGreaterEqual(state["send_count"], 2)
        self.assertIsNotNone(state["next_send_in_s"])

    def test_max_sends_removes_the_dunk(self):
        self.flock.add(target="w1:p1", interval_minutes=1 / 60, start=True, max_sends=1)
        self.assertTrue(wait_until(lambda: self.flock.list() == [], 5))
        self.assertEqual(1, len(self.herdr.sends))

    def test_only_when_idle_waits_for_the_agent(self):
        with mock.patch.object(dunk_core, "IDLE_POLL_S", 0.05):
            dunk = self.flock.add(target="w1:p2", interval_minutes=1 / 60, start=True,
                                  only_when="idle")
            # w1:p2 is "working" in the fixture; the dunk must hold.
            self.assertTrue(wait_until(
                lambda: self.flock.get(dunk["id"])["status"] == "Busy (working)", 4))
            self.assertEqual([], self.herdr.sends)
            self.herdr.statuses["w1:p2"] = "idle"
            self.assertTrue(self.herdr.sent.wait(3))
            self.assertEqual("w1:p2", self.herdr.sends[0][0])

    def test_interval_change_reschedules_running_dunk(self):
        dunk = self.flock.add(target="w1:p1", interval_minutes=60, start=True)
        before = self.flock.get(dunk["id"])["next_send_in_s"]
        self.assertGreater(before, 3000)
        self.flock.update(dunk["id"], interval_minutes=1)
        after = self.flock.get(dunk["id"])["next_send_in_s"]
        self.assertLessEqual(after, 60)

    # -- backpressure (skip_if_unconsumed) ---------------------------------------
    #
    # These model what `herdr pane read` really returns for Claude Code and
    # Codex panes (see tests/fakes.py): a bordered input box that shows queued
    # input verbatim when short, or a collapsed "Pasted text / +N lines
    # (ctrl+o to expand)" placeholder when long. Consumption is judged from
    # that box, not from whether the agent happens to be busy.

    def _bp(self, target="w2:p1", interval_minutes=1 / 60, **extra):
        return self.flock.add(target=target, text="nudge {count}",
                              interval_minutes=interval_minutes, start=True,
                              skip_if_unconsumed=True, **extra)

    # ---- the classifier, as a pure decision over real pane text ----

    def test_previous_send_consumed_reads_the_input_box(self):
        d = Dunker("d1", target_pane_id="w2:p1", skip_if_unconsumed=True,
                   send_count=1, last_sent_text="Control rounds #1 (dunk d1). Supervision pass "
                   "over the herd, a very long prompt that Claude collapses.")
        # Long prompt collapsed to a placeholder in the box -> not consumed.
        self.herdr.set_screen("w2:p1", claude_box_collapsed())
        self.assertFalse(self.flock._previous_send_consumed(d))
        # The unsubmitted four-hour paste ghost -> not consumed.
        self.herdr.set_screen("w2:p1", claude_box_pasted_unsubmitted())
        self.assertFalse(self.flock._previous_send_consumed(d))
        # Box empty -> consumed.
        self.herdr.set_screen("w2:p1", claude_box_empty())
        self.assertTrue(self.flock._previous_send_consumed(d))
        # A short verbatim prompt still in the box -> not consumed.
        d.last_sent_text = "keep going please"
        self.herdr.set_screen("w2:p1", claude_box_pending("keep going please"))
        self.assertFalse(self.flock._previous_send_consumed(d))

    def test_codex_transcript_collapse_is_not_mistaken_for_queued_input(self):
        # A collapse chip in the OUTPUT area ("+28 lines (ctrl + t to view
        # transcript)") must not read as pending input; the box is empty.
        d = Dunker("d1", target_pane_id="w2:p1", skip_if_unconsumed=True,
                   send_count=1, last_sent_text="Backlog check #1 (dunk d1). Project work.")
        self.herdr.set_screen("w2:p1", codex_box_empty_with_transcript_collapse())
        self.assertTrue(self.flock._previous_send_consumed(d))

    def test_rotating_idle_hint_reads_as_clear_not_as_our_prompt(self):
        # Codex rotates idle hints in the box; a hint is not our queued send, so
        # the send proceeds (documented: we detect our own text, not arbitrary
        # box content).
        d = Dunker("d1", target_pane_id="w2:p1", skip_if_unconsumed=True,
                   send_count=1, last_sent_text="Backlog check #1 (dunk d1). Long project prompt.")
        self.herdr.set_screen("w2:p1", codex_box_rotating_hint())
        self.assertTrue(self.flock._previous_send_consumed(d))

    def test_unreadable_pane_is_inconclusive_so_it_skips(self):
        d = Dunker("d1", target_pane_id="w2:p1", skip_if_unconsumed=True,
                   send_count=1, last_sent_text="anything")
        self.herdr.set_screen("w2:p1", None)
        self.assertFalse(self.flock._previous_send_consumed(d))  # skip, not send

    def test_busy_agent_does_not_by_itself_mean_consumed(self):
        # Agent reports working, but the box still holds the collapsed prompt:
        # busyness is unrelated to whether THIS send was taken.
        d = Dunker("d1", target_pane_id="w2:p1", skip_if_unconsumed=True,
                   send_count=1, last_sent_text="a long prompt " * 10)
        self.herdr.statuses["w2:p1"] = "working"
        self.herdr.set_screen("w2:p1", claude_box_collapsed())
        self.assertFalse(self.flock._previous_send_consumed(d))

    # ---- end to end through the running timer ----

    def test_first_send_always_fires_with_backpressure_on(self):
        # Box "full" from the start. Typing-hold is off here so the fixture
        # exercises backpressure alone (a paste we never sent would otherwise
        # read as someone else's draft and hold, see the typing tests).
        self.herdr.set_screen("w2:p1", claude_box_collapsed())
        dunk = self._bp(hold_while_typing=False)
        self.assertTrue(self.herdr.sent.wait(4))
        self.assertEqual(("w2:p1", "nudge 1"), self.herdr.sends[0])
        self.assertEqual(1, self.flock.get(dunk["id"])["send_count"])

    def test_stacking_is_prevented_when_the_box_stays_full(self):
        # The field bug: a long prompt sits collapsed in the box and the next
        # send must be skipped, not stacked on top of it.
        dunk = self._bp()
        self.assertTrue(self.herdr.sent.wait(4))
        self.herdr.set_screen("w2:p1", claude_box_collapsed())  # never picked up
        self.assertTrue(wait_until(
            lambda: self.flock.get(dunk["id"])["skip_count"] >= 2, 6))
        state = self.flock.get(dunk["id"])
        self.assertEqual(1, state["send_count"], "skips must not advance send_count")
        self.assertEqual([("w2:p1", "nudge 1")], self.herdr.sends)
        self.assertEqual("Skipped (unconsumed)", state["status"])
        self.assertIsNone(state["last_error"], "a skip is not an error")
        self.assertIsNotNone(state["last_skipped_at"])
        self.assertTrue(state["running"])
        self.assertIsNotNone(state["next_send_in_s"], "it reschedules, does not stall")

    def test_pane_going_busy_for_an_unrelated_reason_still_skips(self):
        dunk = self._bp()
        self.assertTrue(self.herdr.sent.wait(4))
        self.herdr.statuses["w2:p1"] = "working"          # busy, but...
        self.herdr.set_screen("w2:p1", claude_box_collapsed())  # prompt still queued
        self.assertTrue(wait_until(
            lambda: self.flock.get(dunk["id"])["skip_count"] >= 1, 5))
        self.assertEqual(1, self.flock.get(dunk["id"])["send_count"])

    def test_box_cleared_after_consumption_lets_the_next_send_fire(self):
        dunk = self._bp()
        self.assertTrue(self.herdr.sent.wait(4))
        self.herdr.set_screen("w2:p1", claude_box_empty())  # agent took it
        self.assertTrue(wait_until(lambda: len(self.herdr.sends) >= 2, 5))
        self.assertEqual(("w2:p1", "nudge 2"), self.herdr.sends[1])
        self.assertEqual(0, self.flock.get(dunk["id"])["skip_count"])

    def test_unreadable_pane_skips_rather_than_stacking(self):
        dunk = self._bp()
        self.assertTrue(self.herdr.sent.wait(4))
        self.herdr.set_screen("w2:p1", None)
        self.assertTrue(wait_until(
            lambda: self.flock.get(dunk["id"])["skip_count"] >= 1, 5))
        self.assertEqual(1, self.flock.get(dunk["id"])["send_count"])

    def test_backpressure_off_preserves_existing_behavior(self):
        dunk = self.flock.add(target="w2:p1", text="tick", interval_minutes=1 / 60, start=True,
                              hold_while_typing=False)
        self.assertTrue(wait_until(lambda: len(self.herdr.sends) >= 2, 5))
        state = self.flock.get(dunk["id"])
        self.assertFalse(state["skip_if_unconsumed"])
        self.assertEqual(0, state["skip_count"])
        self.assertGreaterEqual(state["send_count"], 2)
        # With both gates off a dunk never inspects the pane at all.
        self.assertEqual([], self.herdr.reads)
        self.assertEqual(0, self.herdr.status_calls)

    # ---- never type over a human: the classifier, on styled screens ----

    def test_typed_text_tells_a_draft_from_a_dim_hint(self):
        typed = dunk_core._typed_text
        # A human mid-sentence in Claude Code / Codex.
        self.assertEqual("delete numb", typed(claude_box_typing_ansi("delete numb")))
        self.assertEqual("fix the flaky test", typed(codex_box_typing_ansi("fix the flaky test")))
        # Empty boxes, including the dim placeholder hints both agents rotate.
        self.assertEqual("", typed(claude_box_empty_ansi()))
        self.assertEqual("", typed(claude_box_hint_ansi()))
        self.assertEqual("", typed(codex_box_hint_ansi()))
        self.assertEqual("", typed(codex_box_hint_ansi("Use /skills to list available skills")))
        # A collapsed paste is content, not a hint.
        self.assertEqual("[Pasted text #1 +75 lines]", typed(claude_box_collapsed_ansi()))
        # No input box at all (a bare shell): inconclusive.
        self.assertIsNone(typed("hunter@cell:~$ ls\nfoo bar\nhunter@cell:~$ \n"))
        self.assertIsNone(typed(""))

    def test_bright_text_ignores_truecolor_subparameters(self):
        # "38;2;r;g;b" carries a literal 2 that must not read as dim.
        line = "\x1b[38;2;255;255;255mhello\x1b[0m \x1b[2mdim\x1b[22m bright"
        self.assertEqual("hello  bright", dunk_core._bright_text(line))

    def test_human_typing_excludes_our_own_leftover_send(self):
        d = Dunker("d1", target_pane_id="w2:p1", send_count=1,
                   last_sent_text="nudge 1, a long prompt that wraps in the box",
                   awaiting_consumption=True)
        # A stranger's draft -> typing.
        self.herdr.set_screen("w2:p1", claude_box_typing_ansi("delete numb"))
        self.assertTrue(self.flock._human_typing(d))
        # Our own send still sitting there (the visible line is a prefix of it).
        self.herdr.set_screen("w2:p1", claude_box_typing_ansi("nudge 1, a long prompt that"))
        self.assertFalse(self.flock._human_typing(d))
        # Our long send collapsed to a placeholder while we await consumption.
        self.herdr.set_screen("w2:p1", claude_box_collapsed_ansi())
        self.assertFalse(self.flock._human_typing(d))
        # The same placeholder when we have nothing outstanding is someone's paste.
        fresh = Dunker("d2", target_pane_id="w2:p1")
        self.assertTrue(self.flock._human_typing(fresh))
        # Empty / hint / unreadable / shell never hold.
        self.herdr.set_screen("w2:p1", claude_box_hint_ansi())
        self.assertFalse(self.flock._human_typing(d))
        self.herdr.set_screen("w2:p1", None)
        self.assertFalse(self.flock._human_typing(d))
        self.assertFalse(self.flock._human_typing(Dunker("d3", target_pane_id="w1:p1")))

    def test_backpressure_still_reads_styled_screens_as_plain_text(self):
        d = Dunker("d1", target_pane_id="w2:p1", skip_if_unconsumed=True,
                   send_count=1, last_sent_text="keep going please")
        self.herdr.set_screen("w2:p1", claude_box_typing_ansi("keep going please"))
        self.assertFalse(self.flock._previous_send_consumed(d))
        self.herdr.set_screen("w2:p1", claude_box_hint_ansi())
        self.assertTrue(self.flock._previous_send_consumed(d))

    # ---- never type over a human: end to end through the running timer ----

    def test_send_is_held_while_someone_types_and_fires_once_they_finish(self):
        with mock.patch.object(dunk_core, "TYPING_RECHECK_S", 0.2):
            self.herdr.set_screen("w2:p1", claude_box_typing_ansi("delete numb"))
            dunk = self.flock.add(target="w2:p1", text="nudge {count}", interval_minutes=1 / 60,
                                  start=True)
            self.assertTrue(wait_until(
                lambda: self.flock.get(dunk["id"])["status"] == "Held (typing)", 5))
            time.sleep(0.8)  # several re-checks go by
            state = self.flock.get(dunk["id"])
            self.assertEqual([], self.herdr.sends, "must not type over a draft")
            self.assertEqual(1, state["hold_count"], "one hold, however many re-checks")
            self.assertIsNotNone(state["last_held_at"])
            self.assertIsNone(state["last_error"], "a hold is not an error")
            self.assertGreater(len(self.herdr.ansi_reads), 1, "re-checks read the styled box")
            # They finish and submit; the box shows only a dim hint now.
            self.herdr.set_screen("w2:p1", claude_box_hint_ansi())
            self.assertTrue(self.herdr.sent.wait(3))
            self.assertEqual([("w2:p1", "nudge 1")], self.herdr.sends)
            self.assertEqual(1, self.flock.get(dunk["id"])["send_count"])

    def test_idle_gate_runs_again_after_a_typing_hold(self):
        # Finishing typing usually means submitting, so the agent goes busy
        # right as the box clears; an idle-gated dunk must wait for that turn.
        with mock.patch.object(dunk_core, "TYPING_RECHECK_S", 0.2), \
                mock.patch.object(dunk_core, "IDLE_POLL_S", 0.1):
            self.herdr.set_screen("w2:p1", codex_box_typing_ansi("please also"))
            dunk = self.flock.add(target="w2:p1", text="go", interval_minutes=1 / 60,
                                  start=True, only_when="idle")
            self.assertTrue(wait_until(
                lambda: self.flock.get(dunk["id"])["status"] == "Held (typing)", 5))
            self.herdr.statuses["w2:p1"] = "working"
            self.herdr.set_screen("w2:p1", codex_box_hint_ansi())
            self.assertTrue(wait_until(
                lambda: self.flock.get(dunk["id"])["status"] == "Busy (working)", 5))
            self.assertEqual([], self.herdr.sends)
            self.herdr.statuses["w2:p1"] = "idle"
            self.assertTrue(self.herdr.sent.wait(3))

    def test_ignore_typing_sends_over_a_draft(self):
        self.herdr.set_screen("w2:p1", claude_box_typing_ansi("delete numb"))
        dunk = self.flock.add(target="w2:p1", text="go", interval_minutes=1 / 60, start=True,
                              hold_while_typing=False)
        self.assertTrue(self.herdr.sent.wait(4))
        self.assertEqual(0, self.flock.get(dunk["id"])["hold_count"])
        self.assertEqual([], self.herdr.ansi_reads)

    def test_default_dunk_checks_the_box_once_and_sends_when_clear(self):
        self.herdr.set_screen("w2:p1", codex_box_hint_ansi())
        dunk = self.flock.add(target="w2:p1", text="go", interval_minutes=1 / 60, start=True)
        self.assertTrue(self.herdr.sent.wait(5))
        self.assertTrue(dunk["hold_while_typing"])
        # Two reads: clear, then still clear a second later (the settle window).
        self.assertEqual(["w2:p1", "w2:p1"], self.herdr.ansi_reads)
        self.assertEqual(0, self.herdr.status_calls, "no idle gate unless asked")

    def test_hold_while_typing_toggle_round_trip_and_old_files_default_on(self):
        dunk = self.flock.add(target="w2:p1")
        self.assertTrue(dunk["hold_while_typing"])
        self.assertFalse(self.flock.update(dunk["id"], hold_while_typing="no")["hold_while_typing"])
        self.assertTrue(self.flock.update(dunk["id"], hold_while_typing=True)["hold_while_typing"])
        with self.assertRaises(DunkError):
            self.flock.update(dunk["id"], hold_while_typing="sometimes")
        old = Dunker.from_dict({"id": "d9", "target_pane_id": "w2:p1", "text": "x"})
        self.assertTrue(old.hold_while_typing)
        self.assertEqual(0, old.hold_count)
        self.assertIn("hold_while_typing", old.to_dict())
        self.assertIn("hold_count", old.to_dict())

    def test_backpressure_toggle_via_update_and_validation(self):
        dunk = self.flock.add(target="w2:p1")
        self.assertFalse(dunk["skip_if_unconsumed"])
        self.assertTrue(self.flock.update(dunk["id"], skip_if_unconsumed="yes")["skip_if_unconsumed"])
        self.assertFalse(self.flock.update(dunk["id"], skip_if_unconsumed=False)["skip_if_unconsumed"])
        with self.assertRaises(DunkError):
            self.flock.update(dunk["id"], skip_if_unconsumed="maybe")

    def test_backpressure_state_round_trips_and_old_files_default_off(self):
        self.herdr.set_screen("w2:p1", claude_box_empty())
        dunk = self._bp(name="bp")
        self.assertTrue(self.herdr.sent.wait(4))
        self.flock.stop(dunk["id"])
        with open(self.state) as handle:
            saved = json.load(handle)["dunkers"][0]
        self.assertTrue(saved["skip_if_unconsumed"])
        self.assertEqual("nudge 1", saved["last_sent_text"])
        # An old-format file has none of the new fields; it must load with the
        # feature off and no crash.
        for key in ("skip_if_unconsumed", "skip_count", "last_skipped_at",
                    "awaiting_consumption", "last_sent_text"):
            del saved[key]
        with open(self.state, "w") as handle:
            json.dump({"version": 1, "next_id": 2, "dunkers": [saved]}, handle)
        flock2 = Flock(herdr=FakeHerdr(), state_file=self.state)
        try:
            restored = flock2.get("d1")
            self.assertFalse(restored["skip_if_unconsumed"])
            self.assertEqual(0, restored["skip_count"])
            self.assertIsNone(restored["last_sent_text"])
            self.assertEqual("bp", restored["name"])
        finally:
            flock2.close()

    # -- persistence -------------------------------------------------------------

    def test_state_is_saved_and_restored_with_running_dunks(self):
        self.flock.add(target="w1:p1", text="a", interval_minutes=30, start=True, name="one")
        self.flock.add(target="w2:p1", text="b", interval_minutes=1 / 60, start=False)
        with open(self.state) as handle:
            saved = json.load(handle)
        self.assertEqual(3, saved["next_id"])
        self.assertEqual(2, len(saved["dunkers"]))
        self.assertTrue(saved["dunkers"][0]["running"])
        self.flock.close()  # like a daemon exit: running flags stay true

        herdr2 = FakeHerdr()
        flock2 = Flock(herdr=herdr2, state_file=self.state)
        try:
            dunks = flock2.list()
            self.assertEqual(["d1", "d2"], [d["id"] for d in dunks])
            self.assertTrue(dunks[0]["running"])
            self.assertFalse(dunks[1]["running"])
            self.assertEqual("one", dunks[0]["name"])
            # The restored countdown continues from the persisted schedule.
            self.assertGreater(dunks[0]["next_send_in_s"], 1000)
            # New ids continue after the restored ones.
            self.assertEqual("d3", flock2.add(target="w1:p1")["id"])
        finally:
            flock2.close()

    def test_restore_fires_overdue_dunks_after_grace(self):
        self.flock.add(target="w1:p1", interval_minutes=60, start=True)
        self.flock.close()
        with open(self.state) as handle:
            saved = json.load(handle)
        saved["dunkers"][0]["next_send_at"] = time.time() - 100
        with open(self.state, "w") as handle:
            json.dump(saved, handle)
        with mock.patch.object(dunk_core, "RESTORE_GRACE_S", 1):
            herdr2 = FakeHerdr()
            flock2 = Flock(herdr=herdr2, state_file=self.state)
            try:
                self.assertTrue(herdr2.sent.wait(4))
            finally:
                flock2.close()

    def test_corrupt_state_file_is_tolerated(self):
        with open(self.state, "w") as handle:
            handle.write("{not json")
        flock2 = Flock(herdr=self.herdr, state_file=self.state)
        self.assertEqual([], flock2.list())
        flock2.close()


if __name__ == "__main__":
    unittest.main()


class TypingSettleTests(unittest.TestCase):
    """The box must be clear twice, a second apart, before anything is sent."""

    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)

    def tearDown(self):
        self.flock.close()

    def test_clear_then_typing_within_the_window_is_not_clear(self):
        with mock.patch.object(dunk_core, "TYPING_SETTLE_S", 0.15):
            self.herdr.set_screen("w2:p1", claude_box_hint_ansi())

            def start_typing():
                time.sleep(0.05)
                self.herdr.set_screen("w2:p1", claude_box_typing_ansi("wait, also"))

            threading.Thread(target=start_typing, daemon=True).start()
            # First check sees an empty box; by the second the human has typed.
            self.assertFalse(self.flock._clear_to_send("w2:p1"))

    def test_two_clear_checks_a_second_apart_allow_the_send(self):
        self.herdr.set_screen("w2:p1", claude_box_hint_ansi())
        started = time.time()
        self.assertTrue(self.flock._clear_to_send("w2:p1"))
        self.assertGreaterEqual(time.time() - started, dunk_core.TYPING_SETTLE_S)
        self.assertEqual(2, len(self.herdr.ansi_reads))

    def test_typing_on_the_first_check_returns_at_once(self):
        self.herdr.set_screen("w2:p1", claude_box_typing_ansi("mid sentence"))
        started = time.time()
        self.assertFalse(self.flock._clear_to_send("w2:p1"))
        self.assertLess(time.time() - started, dunk_core.TYPING_SETTLE_S)
        self.assertEqual(1, len(self.herdr.ansi_reads), "no need to wait it out")

    def test_a_stopped_dunk_does_not_finish_its_settle_wait(self):
        stop = threading.Event()
        self.herdr.set_screen("w2:p1", claude_box_hint_ansi())
        stop.set()
        self.assertFalse(self.flock._clear_to_send("w2:p1", stop_event=stop))


class MessageQueueTests(unittest.TestCase):
    """Direct messages queue behind a human who is typing, like dunks do."""

    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)
        self.settle = mock.patch.object(dunk_core, "TYPING_SETTLE_S", 0.05)
        self.recheck = mock.patch.object(dunk_core, "MESSAGE_RECHECK_S", 0.05)
        self.settle.start()
        self.recheck.start()

    def tearDown(self):
        self.settle.stop()
        self.recheck.stop()
        self.flock.close()

    def test_message_waits_while_typing_then_lands_when_the_box_clears(self):
        self.herdr.set_screen("w2:p1", claude_box_typing_ansi("hold on, I am writing"))
        queued = self.flock.send_now("codex", "stop and write tests first", wait_s=0.3)
        self.assertTrue(queued["queued"])
        self.assertIsNone(queued["ok"])
        self.assertEqual("m1", queued["message_id"])
        self.assertIn("someone is typing", queued["message"])
        self.assertEqual([], self.herdr.sends, "must not type over a draft")
        listed = self.flock.list_messages()
        self.assertEqual(["m1"], [m["message_id"] for m in listed["queued"]])
        self.assertEqual([], listed["recent"])
        self.assertEqual(1, self.flock.status()["queued_messages"])
        # They finish; the daemon delivers it.
        self.herdr.set_screen("w2:p1", claude_box_hint_ansi())
        self.assertTrue(self.herdr.sent.wait(5))
        self.assertEqual([("w2:p1", "stop and write tests first")], self.herdr.sends)
        self.assertTrue(wait_until(
            lambda: self.flock.get_message("m1")["status"] == "sent", 5))
        done = self.flock.get_message("m1")
        self.assertTrue(done["ok"])
        self.assertFalse(done["queued"])
        self.assertGreaterEqual(done["holds"], 1)
        self.assertEqual(["m1"], [m["message_id"] for m in self.flock.list_messages()["recent"]])
        self.assertEqual(0, self.flock.status()["queued_messages"])

    def test_queued_messages_keep_their_order_per_pane(self):
        self.herdr.set_screen("w2:p1", claude_box_typing_ansi("typing"))
        for text in ("first", "second", "third"):
            self.flock.send_now("codex", text, wait_s=0)
        self.assertEqual(["first", "second", "third"],
                         [m["text"] for m in self.flock.list_messages()["queued"]])
        self.herdr.set_screen("w2:p1", claude_box_hint_ansi())
        self.assertTrue(wait_until(lambda: len(self.herdr.sends) == 3, 5))
        self.assertEqual(["first", "second", "third"], [t for _p, t in self.herdr.sends])

    def test_messages_to_a_quiet_pane_are_not_delayed_by_a_busy_one(self):
        self.herdr.set_screen("w2:p1", claude_box_typing_ansi("typing"))
        self.herdr.set_screen("w2:p3", claude_box_hint_ansi())
        self.flock.send_now("codex", "waits", wait_s=0)
        other = self.flock.send_now("w2:p3", "goes now", wait_s=2)
        self.assertTrue(other["ok"])
        self.assertEqual([("w2:p3", "goes now")], self.herdr.sends)

    def test_wait_s_blocks_for_delivery_and_zero_returns_immediately(self):
        self.herdr.set_screen("w2:p1", claude_box_hint_ansi())
        sent = self.flock.send_now("codex", "hello", wait_s=3)
        self.assertTrue(sent["ok"])
        self.assertEqual("sent", sent["status"])
        self.herdr.set_screen("w2:p1", claude_box_typing_ansi("busy"))
        pending = self.flock.send_now("codex", "later", wait_s=0)
        self.assertTrue(pending["queued"])

    def test_cancel_a_queued_message(self):
        self.herdr.set_screen("w2:p1", claude_box_typing_ansi("typing"))
        queued = self.flock.send_now("codex", "never mind", wait_s=0)
        cancelled = self.flock.cancel_message(queued["message_id"])
        self.assertEqual("cancelled", cancelled["status"])
        self.herdr.set_screen("w2:p1", claude_box_hint_ansi())
        time.sleep(0.4)
        self.assertEqual([], self.herdr.sends)
        self.assertEqual("cancelled", self.flock.get_message(queued["message_id"])["status"])
        with self.assertRaisesRegex(DunkError, "already cancelled"):
            self.flock.cancel_message(queued["message_id"])
        with self.assertRaisesRegex(DunkError, "no queued message"):
            self.flock.cancel_message("m99")
        with self.assertRaisesRegex(DunkError, "no message with id"):
            self.flock.get_message("m99")

    def test_ignore_typing_sends_straight_over_a_draft(self):
        self.herdr.set_screen("w2:p1", claude_box_typing_ansi("typing"))
        result = self.flock.send_now("codex", "barging in", hold_while_typing=False)
        self.assertTrue(result["ok"])
        self.assertEqual([("w2:p1", "barging in")], self.herdr.sends)
        self.assertEqual([], self.flock.list_messages()["queued"])

    def test_a_failed_send_is_reported_not_retried(self):
        self.herdr.set_screen("w2:p1", claude_box_hint_ansi())
        self.herdr.fail_sends = True
        result = self.flock.send_now("codex", "doomed", wait_s=3)
        self.assertFalse(result["ok"])
        self.assertEqual("failed", result["status"])
        self.assertEqual("boom", result["message"])
        self.assertEqual([], self.flock.list_messages()["queued"])

    def test_fired_dunk_queues_instead_of_typing_over_a_draft(self):
        self.herdr.set_screen("w2:p1", claude_box_typing_ansi("mid sentence"))
        dunk = self.flock.add(target="w2:p1", text="nudge {count}")
        fired = self.flock.fire(dunk["id"])
        self.assertTrue(fired["queued"])
        self.assertEqual([], self.herdr.sends)
        self.assertEqual("Queued (typing)", self.flock.get(dunk["id"])["status"])
        queued = self.flock.list_messages()["queued"]
        self.assertEqual([dunk["id"]], [m["source"] for m in queued])
        self.herdr.set_screen("w2:p1", claude_box_hint_ansi())
        self.assertTrue(self.herdr.sent.wait(5))
        self.assertEqual([("w2:p1", "nudge 1")], self.herdr.sends)
        self.assertEqual(1, self.flock.get(dunk["id"])["send_count"])

    def test_fired_dunk_with_typing_ignored_still_sends_now(self):
        self.herdr.set_screen("w2:p1", claude_box_typing_ansi("mid sentence"))
        dunk = self.flock.add(target="w2:p1", text="now", hold_while_typing=False)
        fired = self.flock.fire(dunk["id"])
        self.assertTrue(fired["ok"])
        self.assertFalse(fired["queued"])
        self.assertEqual([("w2:p1", "now")], self.herdr.sends)


class LayoutControlTests(unittest.TestCase):
    """Building the herd: workspaces, tabs, panes and agents through the Flock."""

    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)

    def tearDown(self):
        self.flock.close()

    def test_list_workspaces_nests_tabs_in_order(self):
        workspaces = self.flock.list_workspaces()
        self.assertEqual(["alpha", "beta"], [w["label"] for w in workspaces])
        self.assertEqual(["Codex Site", "Sheep", "Other"],
                         [t["label"] for t in workspaces[1]["tabs"]])

    def test_resolve_workspace_and_tab_by_id_label_substring_and_self(self):
        self.assertEqual("w2", self.flock.resolve_workspace("w2")["workspace_id"])
        self.assertEqual("w2", self.flock.resolve_workspace("BETA")["workspace_id"])
        self.assertEqual("w1", self.flock.resolve_workspace("alp")["workspace_id"])
        self.assertEqual("w1", self.flock.resolve_workspace("self", self_pane_id="w1:p2")["workspace_id"])
        self.assertEqual("w2:t2", self.flock.resolve_tab("Sheep")["tab_id"])
        self.assertEqual("w2:t1", self.flock.resolve_tab("codex")["tab_id"])
        self.assertEqual("w1:t2", self.flock.resolve_tab("self", self_pane_id="w1:p2")["tab_id"])
        with self.assertRaisesRegex(DunkError, "no herdr workspace"):
            self.flock.resolve_workspace("gamma")
        with self.assertRaisesRegex(DunkError, "needs HERDR_PANE_ID"):
            self.flock.resolve_workspace("self")
        with self.assertRaisesRegex(DunkError, "ambiguous"):
            self.flock.resolve_tab("e")  # Codex Site, Sheep, Other

    def test_create_workspace_runs_command_in_its_pane_and_is_dunkable(self):
        created = self.flock.create_workspace(label="Site", cwd="/home/x/site", command="claude")
        self.assertEqual("Site", created["workspace"]["label"])
        self.assertEqual(created["pane"]["pane_id"], created["pane_id"])
        self.assertEqual("claude", created["ran"])
        self.assertEqual([(created["pane_id"], "claude")], self.herdr.runs)
        self.assertEqual(("create_workspace", "Site", "/home/x/site", False),
                         self.herdr.layout_calls[0])
        # The new pane is immediately a valid dunk target, by id and by label.
        dunk = self.flock.add(target="Site", text="go")
        self.assertEqual(created["pane_id"], dunk["target_pane_id"])

    def test_create_tab_defaults_to_the_callers_workspace(self):
        created = self.flock.create_tab(label="writer", self_pane_id="w2:p1")
        self.assertEqual("w2", created["tab"]["workspace_id"])
        self.assertEqual("writer", created["tab"]["label"])
        self.assertIsNone(created["ran"])
        self.assertEqual([], self.herdr.runs)
        # Explicit workspace by label, from anywhere.
        other = self.flock.create_tab(workspace="alpha", label="tests", command="pytest -q")
        self.assertEqual("w1", other["tab"]["workspace_id"])
        self.assertEqual([(other["pane_id"], "pytest -q")], self.herdr.runs)

    def test_split_pane_validates_direction(self):
        created = self.flock.split_pane("Sheep", direction="down", command="htop")
        self.assertEqual("w2:t2", created["pane"]["tab_id"])
        self.assertEqual(("split_pane", "w2:p2", "down", None, False), self.herdr.layout_calls[0])
        with self.assertRaisesRegex(DunkError, "direction"):
            self.flock.split_pane("Sheep", direction="sideways")

    def test_start_agent_registers_by_name_and_places_it(self):
        started = self.flock.start_agent("writer", "claude --model opus", workspace="beta",
                                         cwd="/home/x/site")
        self.assertEqual("writer", started["name"])
        self.assertEqual(["claude", "--model", "opus"], started["argv"])
        call = self.herdr.layout_calls[-1]
        self.assertEqual("start_agent", call[0])
        self.assertEqual(("writer", ["claude", "--model", "opus"], "w2"), call[1:4])
        # herdr now lists it as an agent pane, so it resolves as a target by name.
        pane = self.flock.resolve_target("writer")
        self.assertEqual(started["pane_id"], pane["pane_id"])
        with self.assertRaisesRegex(DunkError, "command is required"):
            self.flock.start_agent("x", "")
        with self.assertRaisesRegex(DunkError, "split must be"):
            self.flock.start_agent("x", "claude", tab="Sheep", split="left")

    def test_start_agent_into_a_tab_with_split(self):
        started = self.flock.start_agent("helper", "codex", tab="Sheep", split="right")
        self.assertEqual("w2:t2", started["agent"]["tab_id"])
        self.assertEqual("w2:t2", self.herdr.layout_calls[-1][4])
        self.assertEqual("right", self.herdr.layout_calls[-1][5])

    def test_rename_focus_and_close_by_kind(self):
        renamed = self.flock.rename("tab", "Sheep", "Sheep Ops")
        self.assertEqual({"kind": "tab", "id": "w2:t2", "label": "Sheep Ops"},
                         {k: renamed[k] for k in ("kind", "id", "label")})
        self.assertEqual("w2:t2", self.flock.resolve_tab("Sheep Ops")["tab_id"])
        self.assertEqual("w1", self.flock.rename("workspace", "alpha", "Alpha Prime")["id"])
        self.assertEqual("w2:p1", self.flock.rename("pane", "codex", "codex main")["id"])
        with self.assertRaisesRegex(DunkError, "label is required"):
            self.flock.rename("tab", "Other", "  ")
        with self.assertRaisesRegex(DunkError, "kind must be"):
            self.flock.rename("window", "Other", "x")
        self.assertEqual({"kind": "tab", "id": "w2:t3", "focused": True},
                         self.flock.focus("tab", "Other"))
        self.assertEqual(("focus", "tab", "w2:t3"), self.herdr.layout_calls[-1])

    def test_close_stops_dunks_aimed_inside_and_refuses_own_container(self):
        dunk = self.flock.add(target="w2:p3", text="go", interval_minutes=60, start=True)
        bystander = self.flock.add(target="w1:p2", text="go", interval_minutes=60, start=True)
        closed = self.flock.close_target("tab", "Other", self_pane_id="w1:p2")
        self.assertEqual({"kind": "tab", "id": "w2:t3", "closed_panes": ["w2:p3"],
                          "stopped_dunks": [dunk["id"]]}, closed)
        self.assertFalse(self.flock.get(dunk["id"])["running"])
        self.assertEqual("Target closed", self.flock.get(dunk["id"])["status"])
        self.assertTrue(self.flock.get(bystander["id"])["running"])
        with self.assertRaisesRegex(DunkError, "no herdr tab"):
            self.flock.resolve_tab("Other")
        # Never close the caller's own pane, tab or workspace.
        for kind, target in (("pane", "self"), ("tab", "self"), ("workspace", "alpha")):
            with self.assertRaisesRegex(DunkError, "refusing to close"):
                self.flock.close_target(kind, target, self_pane_id="w1:p2")
        # Closing a whole workspace stops every dunk inside it.
        other = self.flock.add(target="w2:p1", text="go", interval_minutes=60, start=True)
        closed = self.flock.close_target("workspace", "beta", self_pane_id="w1:p2")
        self.assertEqual(sorted(["w2:p1", "w2:p2"]), sorted(closed["closed_panes"]))
        self.assertEqual([other["id"]], closed["stopped_dunks"])

    def test_herdr_failures_become_dunk_errors(self):
        self.herdr.fail_layout = "workspace limit reached"
        with self.assertRaisesRegex(DunkError, "could not create the workspace x: workspace limit"):
            self.flock.create_workspace(label="x")
        self.herdr.fail_layout = "nope"
        with self.assertRaisesRegex(DunkError, "could not rename tab w2:t2: nope"):
            self.flock.rename("tab", "Sheep", "y")
        self.herdr.fail_layout = "nope"
        with self.assertRaisesRegex(DunkError, "could not start agent 'a': nope"):
            self.flock.start_agent("a", "claude")

    def test_herdr_passthrough_runs_anything_and_parses_json(self):
        result = self.flock.herdr_command("workspace list")
        self.assertTrue(result["ok"])
        self.assertEqual(["alpha", "beta"], [w["label"] for w in result["result"]["workspaces"]])
        self.assertEqual([["workspace", "list"]], self.herdr.raw_runs)
        # A leading 'herdr' is tolerated; quoting is shell-like; 'self' expands.
        result = self.flock.herdr_command("herdr notification show 'All done' --body ok",
                                          self_pane_id="w1:p2")
        self.assertEqual(["notification", "show", "All done", "--body", "ok"], result["argv"])
        result = self.flock.herdr_command("pane zoom self --on", self_pane_id="w1:p2")
        self.assertEqual(["pane", "zoom", "w1:p2", "--on"], result["argv"])
        # For tab / workspace commands, `self` is the caller's tab / workspace.
        self.assertEqual(["tab", "focus", "w1:t2"],
                         self.flock.herdr_command("tab focus self", self_pane_id="w1:p2")["argv"])
        self.assertEqual(["workspace", "get", "w1"],
                         self.flock.herdr_command("workspace get self", self_pane_id="w1:p2")["argv"])
        # herdr's JSON errors (on stderr) come back as error text, not exceptions.
        result = self.flock.herdr_command("tab get bogus")
        self.assertFalse(result["ok"])
        self.assertEqual("tab bogus not found", result["error"])
        self.assertIsNone(result["result"])
        # Usage text for a bare group is an answer, not an error.
        result = self.flock.herdr_command("pane")
        self.assertTrue(result["ok"])
        self.assertIn("herdr pane commands", result["text"])
        # Commands that would take herdr down are refused.
        for blocked in ("server stop", "update", "channel set preview", "session attach x",
                        "agent attach w1:p2", "--session foo"):
            with self.assertRaisesRegex(DunkError, "not allowed"):
                self.flock.herdr_command(blocked)
        with self.assertRaisesRegex(DunkError, "command is required"):
            self.flock.herdr_command("   ")

    def test_herdr_help_overview_and_group(self):
        self.assertIn("Usage: herdr", self.flock.herdr_help()["text"])
        self.assertIsNone(self.flock.herdr_help()["topic"])
        group = self.flock.herdr_help("pane list")
        self.assertEqual("pane list", group["topic"])
        self.assertIn("herdr pane commands", group["text"])
        self.assertEqual(["pane"], self.herdr.raw_runs[-1])
