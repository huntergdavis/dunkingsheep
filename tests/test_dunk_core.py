import json
import os
import sys
import tempfile
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
    FakeHerdr, claude_box_collapsed, claude_box_empty, claude_box_pasted_unsubmitted,
    claude_box_pending, codex_box_empty_with_transcript_collapse,
    codex_box_rotating_hint,
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
        result = self.flock.send_now("codex", "ping")
        self.assertTrue(result["ok"])
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
        self.herdr.set_screen("w2:p1", claude_box_collapsed())  # box "full" from the start
        dunk = self._bp()
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
        dunk = self.flock.add(target="w2:p1", text="tick", interval_minutes=1 / 60, start=True)
        self.assertTrue(wait_until(lambda: len(self.herdr.sends) >= 2, 5))
        state = self.flock.get(dunk["id"])
        self.assertFalse(state["skip_if_unconsumed"])
        self.assertEqual(0, state["skip_count"])
        self.assertGreaterEqual(state["send_count"], 2)
        # A plain dunk never inspects the pane for consumption.
        self.assertEqual([], self.herdr.reads)
        self.assertEqual(0, self.herdr.status_calls)

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
