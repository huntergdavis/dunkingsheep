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

import _bootstrap  # noqa: F401  (isolate live state)
import dunk_core  # noqa: E402
from dunk_core import (  # noqa: E402
    DunkError, Dunker, Flock, expand_template, format_interval, parse_interval,
)
from fakes import (  # noqa: E402
    FakeHerdr, claude_box_collapsed, claude_box_collapsed_ansi, claude_box_empty,
    claude_box_empty_herdr_095,
    claude_box_empty_ansi, claude_box_hint_ansi, claude_box_pasted_unsubmitted,
    claude_box_pending, claude_box_typing_ansi, codex_box_empty_with_transcript_collapse,
    pi_box_empty, pi_box_pending, pi_box_working_empty,
    codex_approval_dialog, codex_answered_dialog_in_scrollback,
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
        # The real `claude agents --json` is 2.9 s; tests must never run it.
        class _NoClaude:
            def status_for_pane(self, pane, fresh=False):
                return None
        self.flock.claude_status = _NoClaude()

    def tearDown(self):
        self.flock.close()
        self.tmp.cleanup()

    # -- CRUD -----------------------------------------------------------------

    def test_status_can_skip_the_terminal_probe_for_the_sidebar(self):
        with mock.patch.object(self.flock.herdr, "is_available",
                               return_value=True) as probe:
            status = self.flock.status(check_herdr=False)
            probe.assert_not_called()
        self.assertIsNone(status["herdr_available"])
        self.assertIsNone(status["herdr_hint"])

    def test_a_declared_turn_holds_sends_until_turn_end(self):
        self.flock.turn_start("w1:p2", ttl_s=60, source="test")
        self.assertTrue(self.flock.turn_active("w1:p2"))
        held = self.flock.send_now("w1:p2", "GATE-turn-hold", wait_s=0)
        self.assertTrue(held.get("queued"), f"the active turn must hold: {held}")
        self.assertTrue(self.flock.turn_active("w1:p2"))
        self.flock.turn_end("w1:p2")
        self.assertFalse(self.flock.turn_active("w1:p2"))
        # The turn no longer blocks; the pane's own box may still queue, which
        # is the ordinary hold path, not the turn hold.
        after = self.flock.send_now("w1:p2", "GATE-turn-after", wait_s=0)
        self.assertIn(after.get("status"), ("queued", "sent"), after)
        rows = [dict(row) for row in self.flock.metrics.db.execute(
            "SELECT kind FROM events WHERE kind='turn.end'")]
        self.assertTrue(rows)

    def test_a_crashed_turn_releases_after_ttl_and_logs_the_reason(self):
        self.flock.turn_start("w1:p2", ttl_s=0.05, source="test-crash")
        held = self.flock.send_now("w1:p2", "GATE-crash-held", wait_s=0)
        self.assertTrue(held.get("queued"))
        time.sleep(0.08)
        # No turn_end arrives: the TTL must release the hold, not hold forever.
        released = self.flock.turn_expire()
        self.assertEqual("ttl", released[0]["reason"])
        self.assertFalse(self.flock.turn_active("w1:p2"))
        after = self.flock.send_now("w1:p2", "GATE-crash-delivered", wait_s=0)
        self.assertIn(after.get("status"), ("queued", "sent"), after)
        rows = [dict(row) for row in self.flock.metrics.db.execute(
            "SELECT kind, reason, outcome FROM events WHERE kind='turn.expired'")]
        self.assertTrue(rows and rows[-1]["reason"] == "ttl", rows)

    def test_task_claim_carries_the_request_caller_for_proc_liveness(self):
        from dunk_callers import process_start
        self.flock.tasks.add("global", "caller test")
        caller = {"pid": os.getpid(), "start": process_start(os.getpid()),
                  "name": "pi"}
        self.flock.set_caller(caller)
        try:
            claimed = self.flock.task_next(scope="global", lease_seconds=60,
                                           self_pane_id="w1:p2")
        finally:
            self.flock.clear_caller()
        self.assertIsNotNone(claimed["task"])
        lease = self.flock.tasks.leases_for(claimed["task"]["id"])[0]
        self.assertEqual(os.getpid(), lease["caller_pid"])
        self.assertEqual("pi", lease["caller_name"])

    def test_task_reap_refuses_a_fixture_pane_list(self):
        # A fixture pane list must fail loudly, never move real work.
        self.flock.tasks.add("global", "fixture reap")
        with self.assertRaisesRegex(DunkError, "alive_source|fixture"):
            self.flock.task_reap(self_pane_id="w1:p2")

    def test_claude_busy_holds_and_idle_accepts(self):
        from dunk_claude import ClaudeStatus
        class FakeStatus:
            def __init__(self, status): self.status = status; self.calls = 0
            def status_for_pane(self, pane, fresh=False):
                self.calls += 1
                return self.status
        self.flock.claude_status = FakeStatus("busy")
        # w1:p2 is the claude pane in the fixture.
        busy = self.flock.send_now("w1:p2", "GATE-claude-busy", wait_s=0)
        self.assertTrue(busy.get("queued"), busy)
        self.flock.claude_status = FakeStatus("idle")
        idle = self.flock.send_now("w1:p2", "GATE-claude-idle", wait_s=0)
        self.assertIn(idle.get("status"), ("queued", "sent"), idle)
        self.flock.claude_status = FakeStatus(None)
        unknown = self.flock.send_now("w1:p2", "GATE-claude-unknown", wait_s=0)
        self.assertIn(unknown.get("status"), ("queued", "sent"), unknown)

    def test_claude_status_maps_by_cwd_and_reports_unknown_honestly(self):
        from dunk_claude import ClaudeStatus
        calls = []
        def run(argv, **kwargs):
            calls.append(argv)
            class P:
                returncode = 0
                stdout = json.dumps([
                    {"pid": 1, "cwd": "/work/a", "status": "busy", "name": "a"},
                    {"pid": 2, "cwd": "/work/b", "status": "idle", "name": "b"},
                ])
                stderr = ""
            return P()
        with mock.patch.dict(os.environ, {"DUNKINGSHEEP_CLAUDE_STATUS": "1"}), \
                mock.patch("dunk_claude.subprocess.run", side_effect=run):
            status = ClaudeStatus(ttl=60)
            self.assertEqual("busy", status.status_for_pane({"cwd": "/work/a"}))
            self.assertEqual("idle", status.status_for_pane({"cwd": "/work/b"}))
            self.assertIsNone(status.status_for_pane({"cwd": "/work/unknown"}))
            self.assertEqual("busy", status.status_for_pane({"cwd": "/work/a"}))
            self.assertEqual(1, len(calls), "the agents list is cached")
            status.status_for_pane({"cwd": "/work/a"}, fresh=True)
            self.assertEqual(2, len(calls), "fresh bypasses the cache")

    def test_own_mailbox_is_cached_for_the_poll(self):
        calls = []
        original = self.herdr.list_panes_grouped
        self.herdr.list_panes_grouped = lambda: (calls.append(1), original())[1]
        first = self.flock.inbox_count(None, self_pane_id="w1:p2")
        after_first = len(calls)
        self.assertGreater(after_first, 0)
        second = self.flock.inbox_count(None, self_pane_id="w1:p2")
        self.assertEqual(first, second)
        self.assertEqual(after_first, len(calls),
                         "the second self poll must add no pane walk")
        # An explicit target still resolves fresh.
        self.flock.inbox_count("w2:p1", self_pane_id="w1:p2")
        self.assertGreater(len(calls), after_first)

    def test_own_mailbox_cache_expires(self):
        calls = []
        original = self.herdr.list_panes_grouped
        self.herdr.list_panes_grouped = lambda: (calls.append(1), original())[1]
        with mock.patch("dunk_core.MAILBOX_CACHE_S", 0.01):
            self.flock.inbox_count(None, self_pane_id="w1:p2")
            before = len(calls)
            time.sleep(0.02)
            self.flock.inbox_count(None, self_pane_id="w1:p2")
        self.assertGreater(len(calls), before)

    def test_poll_batches_status_inbox_and_dunks_without_marking_read(self):
        from dunk_api import h_poll
        sent = self.flock.inbox_send("w1:p2", "hello", self_pane_id="w2:p1",
                                     notify=False)
        before = self.flock.inbox_count("w1:p2")["unread"]
        result = h_poll(self.flock, {"limit": 5}, "w1:p2")
        self.assertEqual("w1:p2", result["status"]["self_pane_id"])
        self.assertIsNone(result["status"]["herdr_available"])
        self.assertEqual(before, result["inbox_count"]["unread"])
        self.assertEqual(before, self.flock.inbox_count("w1:p2")["unread"])
        ids = [message["message_id"] for message in result["dunks"]["messages"]]
        self.assertIn(sent["message_id"], ids)
        self.assertIn("dunks", result["dunks"])

    def test_poll_reports_a_dead_mailbox_as_an_error_but_still_shows_dunks(self):
        from dunk_api import h_poll
        result = h_poll(self.flock, {}, "wZ:p9")
        self.assertIn("error", result)
        self.assertIn("dunks", result["dunks"])

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
        with self.assertRaisesRegex(DunkError, "no pane matches"):
            self.flock.resolve_target("nothing-here")
        with self.assertRaisesRegex(DunkError, "needs TMUX_PANE"):
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
        self.assertTrue(wait_until(
            lambda: self.flock.get(dunk["id"])["send_count"] >= 2, 6))
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
        # Gating is covered by its own tests; these are about backpressure.
        extra.setdefault("only_when", "any")
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

    def test_herdr_095_empty_box_reads_as_consumed(self):
        # Captured from a real Claude pane after the herdr 0.9.5 upgrade: the
        # box is the last sigil line plus a border/footer, and it is empty.
        d = Dunker("d1", target_pane_id="w2:p1", skip_if_unconsumed=True,
                   send_count=1, last_sent_text="nudge 1")
        self.herdr.set_screen("w2:p1", claude_box_empty_herdr_095())
        self.assertTrue(self.flock._previous_send_consumed(d))

    def test_pi_empty_box_reads_as_consumed(self):
        # pi has no prompt sigil; its empty input box is the blank line between
        # the last two full-width borders.
        d = Dunker("d1", target_pane_id="w2:p1", skip_if_unconsumed=True,
                   send_count=1, last_sent_text="nudge 1")
        self.herdr.set_screen("w2:p1", pi_box_empty())
        self.assertTrue(self.flock._previous_send_consumed(d))

    def test_pi_working_empty_box_reads_as_consumed(self):
        d = Dunker("d1", target_pane_id="w2:p1", skip_if_unconsumed=True,
                   send_count=1, last_sent_text="nudge 1")
        self.herdr.set_screen("w2:p1", pi_box_working_empty())
        self.assertTrue(self.flock._previous_send_consumed(d))

    def test_pi_queued_text_reads_as_pending(self):
        d = Dunker("d1", target_pane_id="w2:p1", skip_if_unconsumed=True,
                   send_count=1, last_sent_text="nudge 1")
        self.herdr.set_screen("w2:p1", pi_box_pending("nudge 1"))
        self.assertFalse(self.flock._previous_send_consumed(d))

    def test_unrecognised_box_caps_skips_and_sends(self):
        d = Dunker("d1", target_pane_id="w2:p1", skip_if_unconsumed=True,
                   send_count=1, last_sent_text="nudge 1")
        self.herdr.set_screen("w2:p1", "no recognisable input box here\n")
        self.assertFalse(self.flock._previous_send_consumed(d))   # 1
        self.assertFalse(self.flock._previous_send_consumed(d))   # 2
        self.assertTrue(self.flock._previous_send_consumed(d))    # 3 -> send
        self.assertTrue(d.detector_fallback)
        self.assertEqual(0, d.unknown_skips)

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
        # Backpressure did not skip it; the text was typed into the pane.
        self.assertEqual(0, self.flock.get(dunk["id"])["skip_count"])
        self.assertIn(("w2:p1", "nudge 1"), self.herdr.text_only)

    def test_stacking_is_prevented_when_the_box_stays_full(self):
        # The field bug: a long prompt sits collapsed in the box and the next
        # send must be skipped, not stacked on top of it.
        dunk = self._bp()
        # Let the first send land and be confirmed, then make the box look as
        # though the agent never picked that prompt up.
        self.assertTrue(wait_until(
            lambda: self.flock.get(dunk["id"])["send_count"] == 1, 6))
        self.herdr.set_screen("w2:p1", claude_box_collapsed())
        self.assertTrue(wait_until(
            lambda: self.flock.get(dunk["id"])["skip_count"] >= 2, 10))
        state = self.flock.get(dunk["id"])
        self.assertEqual(1, state["send_count"], "skips must not advance send_count")
        self.assertEqual([("w2:p1", "nudge 1")], self.herdr.sends)
        self.assertEqual("Skipped (unconsumed)", state["status"])
        self.assertIsNone(state["last_error"], "a skip is not an error")
        self.assertTrue(state["running"])
        self.assertIsNotNone(state["next_send_in_s"], "it reschedules, does not stall")

    def test_pi_box_stacking_is_prevented(self):
        dunk = self._bp(hold_while_typing=False)
        self.assertTrue(wait_until(
            lambda: self.flock.get(dunk["id"])["send_count"] == 1, 6))
        self.herdr.set_screen("w2:p1", pi_box_pending("nudge 1"))
        self.assertTrue(wait_until(
            lambda: self.flock.get(dunk["id"])["skip_count"] >= 1, 8))
        self.assertEqual(1, self.flock.get(dunk["id"])["send_count"])

    def test_historical_pane_error_clears_when_the_pane_is_back(self):
        d = Dunker("d1", target_pane_id="w2:p1", running=True,
                   last_error="target pane not found")
        self.flock.dunkers = [d]
        self.flock._clear_historical_errors({"w2:p1"})
        self.assertIsNone(d.last_error)
        d.last_error = "target pane not found"
        self.flock._clear_historical_errors(set())
        self.assertEqual("target pane not found", d.last_error)

    def test_a_skip_clears_a_stale_pane_error(self):
        dunk = self._bp(hold_while_typing=False)
        self.assertTrue(wait_until(
            lambda: self.flock.get(dunk["id"])["send_count"] == 1, 6))
        with self.flock._lock:
            self.flock._find(dunk["id"]).last_error = "target pane not found"
        self.herdr.set_screen("w2:p1", pi_box_pending("nudge 1"))
        self.assertTrue(wait_until(
            lambda: self.flock.get(dunk["id"])["skip_count"] >= 1, 8))
        self.assertIsNone(self.flock.get(dunk["id"])["last_error"])

    def test_pane_going_busy_for_an_unrelated_reason_still_skips(self):
        dunk = self._bp()
        self.assertTrue(wait_until(
            lambda: self.flock.get(dunk["id"])["send_count"] == 1, 6))
        self.herdr.statuses["w2:p1"] = "working"          # busy, but...
        self.herdr.set_screen("w2:p1", claude_box_collapsed())  # prompt still queued
        self.assertTrue(wait_until(
            lambda: self.flock.get(dunk["id"])["skip_count"] >= 1, 8))
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
        # Let the first send be confirmed before the pane goes unreadable;
        # an unreadable pane cannot confirm a submit at all.
        self.assertTrue(wait_until(
            lambda: self.flock.get(dunk["id"])["send_count"] == 1, 6))
        self.herdr.set_screen("w2:p1", None)
        self.assertTrue(wait_until(
            lambda: self.flock.get(dunk["id"])["skip_count"] >= 1, 5))
        self.assertEqual(1, self.flock.get(dunk["id"])["send_count"])

    def test_backpressure_off_preserves_existing_behavior(self):
        dunk = self.flock.add(target="w2:p1", text="tick", interval_minutes=1 / 60, start=True,
                              hold_while_typing=False, only_when="any")
        self.assertTrue(wait_until(
            lambda: self.flock.get(dunk["id"])["send_count"] >= 2, 7))
        state = self.flock.get(dunk["id"])
        self.assertFalse(state["skip_if_unconsumed"])
        self.assertEqual(0, state["skip_count"])
        self.assertGreaterEqual(state["send_count"], 2)
        # With both gates off no GATE polls the agent's status. t363 adds exactly
        # one pre-send status baseline PER SEND: turn confirmation needs a
        # before/after, so one cheap probe per send is the cost of the proof.
        self.assertGreaterEqual(self.herdr.status_calls, state["send_count"])
        self.assertTrue(all(source == "visible" for _pane, source in self.herdr.reads))

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

    def test_text_in_the_box_no_longer_holds_a_dunk(self):
        """Policy 2026-10-24: nothing else types into a pane, so a non-empty box is not
        a reason to hold. The text is PRESERVED (submitted first), then the dunk goes."""
        with mock.patch.object(dunk_core, "TYPING_RECHECK_S", 0.2):
            self.herdr.set_screen("w2:p1", claude_box_typing_ansi("delete numb"))
            self.herdr.box["w2:p1"] = "delete numb"
            dunk = self.flock.add(target="w2:p1", text="nudge {count}", interval_minutes=1 / 60,
                                  start=True)
            self.assertTrue(self.herdr.sent.wait(10), "the dunk must deliver, not hold")
            self.assertEqual(0, self.flock.get(dunk["id"])["hold_count"],
                             "text in the box is not a hold any more")
            self.assertTrue(wait_until(
                lambda: self.flock.get(dunk["id"])["send_count"] >= 1, 10))

    def test_the_idle_gate_still_waits_for_a_busy_agent(self):
        # The typing hold is gone; the IDLE gate is not. A busy agent is respected
        # where the dunk asked to be (only_when="idle"), which is a delivery-time
        # choice rather than a claim about who is typing.
        with mock.patch.object(dunk_core, "IDLE_POLL_S", 0.1):
            self.herdr.statuses["w2:p1"] = "working"
            self.herdr.set_screen("w2:p1", codex_box_hint_ansi())
            dunk = self.flock.add(target="w2:p1", text="go", interval_minutes=1 / 60,
                                  start=True, only_when="idle")
            self.assertTrue(wait_until(
                lambda: self.flock.get(dunk["id"])["status"] == "Busy (working)", 6))
            self.assertEqual([], self.herdr.sends)
            self.herdr.statuses["w2:p1"] = "idle"
            self.assertTrue(self.herdr.sent.wait(4))

    def test_ignore_typing_sends_over_a_draft(self):
        self.herdr.set_screen("w2:p1", claude_box_typing_ansi("delete numb"))
        dunk = self.flock.add(target="w2:p1", text="go", interval_minutes=1 / 60, start=True,
                              hold_while_typing=False)
        self.assertTrue(self.herdr.sent.wait(4))
        self.assertEqual(0, self.flock.get(dunk["id"])["hold_count"])
        self.assertEqual([], self.herdr.ansi_reads)

    def test_default_dunk_checks_the_box_once_and_sends_when_clear(self):
        self.herdr.set_screen("w2:p1", codex_box_hint_ansi())
        dunk = self.flock.add(target="w2:p1", text="go", interval_minutes=1 / 60, start=True,
                              only_when="any")
        self.assertTrue(self.herdr.sent.wait(5))
        self.assertTrue(dunk["hold_while_typing"])
        # Three reads: clear, still clear a second later (the settle window),
        # then one last look with the send lock held, just before typing.
        self.assertEqual(["w2:p1", "w2:p1", "w2:p1"], self.herdr.ansi_reads)
        # t363: no idle gate, but one pre-send status baseline per send.
        self.assertEqual(1, self.herdr.status_calls, "no idle gate when asked for any time")

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
        self.assertTrue(wait_until(
            lambda: self.flock.get(dunk["id"])["send_count"] == 1, 6))
        saved = self.flock.get(dunk["id"])
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

    def test_typing_between_the_two_reads_no_longer_cancels_the_send(self):
        with mock.patch.object(dunk_core, "TYPING_SETTLE_S", 0.15):
            self.herdr.set_screen("w2:p1", claude_box_hint_ansi())

            def start_typing():
                time.sleep(0.05)
                self.herdr.set_screen("w2:p1", claude_box_typing_ansi("wait, also"))

            threading.Thread(target=start_typing, daemon=True).start()
            # Text is preserved at send time now, so the second read no longer
            # cancels the send; it exists for REDRAW safety.
            self.assertTrue(self.flock._clear_to_send("w2:p1"))

    def test_two_clear_checks_a_second_apart_allow_the_send(self):
        self.herdr.set_screen("w2:p1", claude_box_hint_ansi())
        started = time.time()
        self.assertTrue(self.flock._clear_to_send("w2:p1"))
        self.assertGreaterEqual(time.time() - started, dunk_core.TYPING_SETTLE_S)
        self.assertEqual(2, len(self.herdr.ansi_reads))

    def test_text_in_the_box_no_longer_stops_a_send(self):
        """The hold it used to trigger ('a human is typing') is gone by owner policy."""
        self.herdr.set_screen("w2:p1", claude_box_typing_ansi("mid sentence"))
        started = time.time()
        self.assertTrue(self.flock._clear_to_send("w2:p1"))
        self.assertLess(time.time() - started, 4 * dunk_core.TYPING_SETTLE_S)

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

    def test_a_message_with_text_in_the_box_is_delivered_by_preserving_it(self):
        """Policy 2026-10-24: text in the box is submitted first, never a reason to queue."""
        self.herdr.set_screen("w2:p1", claude_box_typing_ansi("let me think"))
        self.herdr.box["w2:p1"] = "let me think"
        queued = self.flock.send_now("w2:p1", "hello there", hold_while_typing=True, wait_s=4)
        self.assertFalse(queued.get("queued", False),
                         "the message must not be queued behind text any more")
        self.assertEqual("let me think", self.herdr.sends[0][1],
                         "their text went first, unmodified")
        self.assertIn(("w2:p1", "hello there"), self.herdr.text_only)

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
        self.assertEqual("sent", other.get("status"),
                         f"the quiet pane goes now; the busy one cannot delay it ({other})")

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
        self.assertTrue(wait_until(
            lambda: self.flock.get(dunk["id"])["send_count"] == 1, 5))

    def test_fired_dunk_with_typing_ignored_still_sends_now(self):
        self.herdr.set_screen("w2:p1", claude_box_typing_ansi("mid sentence"))
        dunk = self.flock.add(target="w2:p1", text="now", hold_while_typing=False)
        fired = self.flock.fire(dunk["id"])
        self.assertTrue(fired["ok"])
        self.assertFalse(fired["queued"])
        self.assertEqual([("w2:p1", "now")], self.herdr.sends)


class SendRaceTests(unittest.TestCase):
    """The regression from 2026-09-23: a draft that appears between the settle
    check and the keystrokes must still stop the send."""

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

    def test_a_draft_appearing_while_we_wait_is_preserved_then_our_send_goes(self):
        """The race that used to hold the message: the draft is submitted, then ours."""
        self.herdr.set_screen("w2:p1", claude_box_hint_ansi())
        released = threading.Event()

        def hog():
            with self.flock._lock_for("w2:p1"):
                self.herdr.set_screen("w2:p1", claude_box_typing_ansi("I am mid sentence"))
                self.herdr.box["w2:p1"] = "I am mid sentence"
                released.set()
                time.sleep(0.3)

        threading.Thread(target=hog, daemon=True).start()
        released.wait(2)
        sent, ok, detail = self.flock.send_guarded(
            "w2:p1", lambda: self.herdr.send_text_and_enter("w2:p1", "AGENT TEXT"))
        self.assertTrue(sent, detail)
        self.assertTrue(ok, detail)
        self.assertEqual("I am mid sentence", self.herdr.sends[0][1],
                         "their sentence went first, unmodified")
        self.assertIn(("w2:p1", "AGENT TEXT"), self.herdr.text_only,
                      "our text went in afterwards")

    def test_the_guard_sends_when_the_box_is_still_clear(self):
        self.herdr.set_screen("w2:p1", claude_box_hint_ansi())
        sent, ok, _detail = self.flock.send_guarded(
            "w2:p1", lambda: self.herdr.send_text_and_enter("w2:p1", "fine"))
        self.assertTrue(sent)
        self.assertTrue(ok)
        self.assertEqual([("w2:p1", "fine")], self.herdr.sends)

    def test_ignore_typing_still_bypasses_the_guard(self):
        self.herdr.set_screen("w2:p1", claude_box_typing_ansi("drafting"))
        sent, ok, _detail = self.flock.send_guarded(
            "w2:p1", lambda: self.herdr.send_text_and_enter("w2:p1", "barge"),
            hold_while_typing=False)
        self.assertTrue(sent)
        self.assertTrue(ok)

    def test_a_refused_message_keeps_its_place_and_is_delivered_later(self):
        self.herdr.set_screen("w2:p1", claude_box_hint_ansi())
        message = self.flock._new_message(
            "w2:p1", "codex", "hello", source="send_text",
            deliver=lambda: self.herdr.send_text_and_enter("w2:p1", "hello"))
        self.herdr.set_screen("w2:p1", claude_box_typing_ansi("typing now"))
        self.assertFalse(self.flock._deliver(message), "refused, stays queued")
        self.assertEqual("queued", message.status)
        self.assertEqual([], self.herdr.sends)
        self.flock._enqueue(message)
        self.herdr.set_screen("w2:p1", claude_box_hint_ansi())
        self.assertTrue(self.herdr.sent.wait(5))
        self.assertEqual([("w2:p1", "hello")], self.herdr.sends)

    def test_a_late_draft_is_preserved_and_the_dunk_sends(self):
        """Policy 2026-10-24: a draft appearing mid-wait no longer parks the dunk in
        'Held (typing)'. It is preserved (submitted) or deferred by the box's own
        refusal - never held because a person might be there."""
        with mock.patch.object(dunk_core, "TYPING_RECHECK_S", 0.1):
            self.herdr.set_screen("w2:p1", claude_box_hint_ansi())
            self.flock._lock_for("w2:p1").acquire()
            dunk = self.flock.add(target="w2:p1", text="nudge", interval_minutes=1 / 60,
                                  start=True)
            try:
                self.assertTrue(wait_until(
                    lambda: self.flock.get(dunk["id"])["status"] == "Sending...", 5),
                    "dunk should be waiting on the send lock")
                self.herdr.set_screen("w2:p1", claude_box_typing_ansi("a late draft"))
                self.herdr.box["w2:p1"] = "a late draft"
            finally:
                self.flock._lock_for("w2:p1").release()
            self.assertTrue(wait_until(
                lambda: self.flock.get(dunk["id"])["hold_count"] == 0, 10),
                "a 'typing' hold must never appear again")
            self.assertEqual(0, self.flock.get(dunk["id"])["hold_count"])
    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)
        # The real gaps are seconds long (Codex needs a quiet pane); the
        # sequence is what matters here, not the waiting.
        self.gaps = mock.patch.object(dunk_core, "SUBMIT_GAPS", (0.0, 0.02, 0.02, 0.02))
        self.gaps.start()
        self.verify_gaps = mock.patch.object(dunk_core, "SUBMIT_VERIFY_GAPS", (0.01, 0.01))
        self.verify_gaps.start()

    def tearDown(self):
        self.gaps.stop()
        self.verify_gaps.stop()
        self.flock.close()

    def test_one_enter_is_enough_when_it_works(self):
        ok, detail = self.flock.submit_text("w2:p1", "hello")
        self.assertTrue(ok)
        self.assertEqual("sent", detail)
        self.assertEqual([("w2:p1", "hello")], self.herdr.sends)
        self.assertEqual(1, len(self.herdr.keys), "no needless extra Enter")

    def test_the_send_result_carries_a_machine_readable_outcome(self):
        """A caller must not have to parse prose to tell sent from queued from
        unconfirmed from refused (cos measured all three arriving as sentences)."""
        import dunk_core as dc
        sent = self.flock.send_now("w2:p1", "one", wait_s=3)
        self.assertEqual("ok", sent["outcome"])
        self.assertTrue(sent["ok"])
        probe = dc.Message("m1", "w2:p1", "claude", "x", "send_text", lambda: None)
        probe.status, probe.detail = "queued", None
        self.assertEqual("queued", probe.to_dict()["outcome"])
        self.assertIsNone(probe.to_dict()["ok"], "queued is neither sent nor failed")
        probe.status, probe.detail = "failed", "submission unconfirmed: box unreadable"
        self.assertEqual("unconfirmed", probe.to_dict()["outcome"])
        probe.status, probe.detail = "failed", "no such pane"
        self.assertEqual("refused", probe.to_dict()["outcome"])
        self.assertIn("stored", dc.Message.OUTCOMES,
                      "a durable queue row is its own outcome, not 'ok'")

    def test_a_busy_lane_confirms_by_the_turn_it_started(self):
        """The false negative cos measured: two sends to a claude lane reported
        "did not submit" and the receiving lane showed both as turns. A busy box
        is not evidence of failure; A TURN STARTING DURING THE SEND (t363: a
        transition - idle at send start, working after) is evidence of success.
        The stale form of this test (working set BEFORE the send) encoded the
        false-success bug the PR63 review reproduced; the negative regression for
        it lives in tests/test_dunk_turn_confirmation.py."""
        # Baseline read returns "idle"; every later probe (the turn checks inside
        # the ladder) sees "working": a transition, not a pre-existing state.
        reads = iter(["idle"])
        with mock.patch.object(type(self.flock), "_read_status",
                               side_effect=lambda pane_id: next(reads, "working")), \
             mock.patch.object(type(self.flock), "_box_holds", return_value=True):
            ok, detail = self.flock.submit_text("w2:p1", "a nudge")
        self.assertTrue(ok, detail)
        self.assertIn("confirmed by the turn", detail)

    def test_a_composer_that_needs_carriage_return_still_submits(self):
        """Measured 2026-10-24: five of six revived claude lanes sat with our text
        in the composer for four minutes while the send reported success, and
        `tmux send-keys C-m` submitted at once. Enter is not the submit key for
        every TUI, so the retries alternate instead of pressing Enter forever."""
        self.herdr.cm_only.add("w2:p1")
        ok, detail = self.flock.submit_text("w2:p1", "a nudge")
        self.assertTrue(ok, detail)
        pressed = [keys for pane, keys in self.herdr.keys if pane == "w2:p1"]
        self.assertIn(["C-m"], pressed, "the retry must try the carriage return")

    def test_a_pane_that_needs_two_enters_still_submits(self):
        # What a large paste does on Codex: the first Enter is swallowed.
        self.herdr.enters_needed["w2:p1"] = 2
        ok, detail = self.flock.submit_text("w2:p1", "a long message")
        self.assertTrue(ok)
        self.assertEqual("sent (Enter x2)", detail)
        self.assertEqual([("w2:p1", "a long message")], self.herdr.sends)
        self.assertEqual(2, len(self.herdr.keys))

    def test_a_dead_agent_session_is_refused_before_any_enter(self):
        # wR:p4's codex session died and still showed an editable composer; four
        # Enter presses reported "did not submit". The pane's own words are the
        # signal, and this must refuse before pressing anything.
        self.herdr.screens["w2:p1"] = ("■ Server connection could not be restored\n"
                                       "• Reconnect failed - check the endpoint, then relaunch")
        ok, detail = self.flock.submit_text("w2:p1", "a message")
        self.assertFalse(ok)
        self.assertIn("agent session is dead", detail)
        self.assertEqual([], self.herdr.keys)

    def test_a_box_that_reads_clear_then_fills_again_is_not_counted_as_sent(self):
        # What stranded a message in the VP pane: the box read empty for an
        # instant mid-redraw, the send was declared done, and the text that
        # came back blocked everything queued behind it.
        self.herdr.enters_needed["w2:p1"] = 99
        readings = iter([False, True])          # clear, then not clear
        real = self.flock._box_holds

        def flaky(pane_id, text):
            try:
                return next(readings)
            except StopIteration:
                return real(pane_id, text)

        with mock.patch.object(self.flock, "_box_holds", flaky):
            ok, detail = self.flock.submit_text("w2:p1", "fragile")
        self.assertFalse(ok, "a redraw must not count as submitted")
        self.assertIn("did not submit", detail)

    def test_delayed_redraw_after_two_clear_reads_is_not_sent(self):
        self.herdr.enters_needed["w2:p1"] = 99
        readings = iter([False, False, True])
        real = self.flock._box_holds

        def delayed(pane_id, text):
            try:
                return next(readings)
            except StopIteration:
                return real(pane_id, text)

        with mock.patch.object(self.flock, "_box_holds", delayed):
            ok, detail = self.flock.submit_text("w2:p1", "delayed")
        self.assertFalse(ok)
        self.assertIn("did not submit", detail)

    def test_unreadable_box_does_not_claim_success(self):
        with mock.patch.object(self.flock, "_box_holds", return_value=None):
            ok, detail = self.flock.submit_text("w2:p1", "unconfirmed")
        self.assertFalse(ok)
        self.assertIn("unconfirmed", detail)

    def test_text_that_never_submits_is_reported_as_failed_not_sent(self):
        self.herdr.enters_needed["w2:p1"] = 99
        ok, detail = self.flock.submit_text("w2:p1", "stubborn")
        self.assertFalse(ok)
        self.assertIn("did not submit", detail)
        self.assertIn("still in the pane's input box", detail)
        self.assertEqual([], self.herdr.sends)
        self.assertEqual(dunk_core.SUBMIT_ATTEMPTS, len(self.herdr.keys))

    def test_a_dunk_whose_text_never_submits_does_not_count_it_as_sent(self):
        self.herdr.enters_needed["w2:p1"] = 99
        dunk = self.flock.add(target="w2:p1", text="nudge")
        result = self.flock.fire(dunk["id"])
        self.assertFalse(result["ok"])
        state = self.flock.get(dunk["id"])
        self.assertEqual(0, state["send_count"])
        self.assertIn("did not submit", state["last_error"])

    def test_press_enter_false_does_not_submit_or_confirm(self):
        ok, _detail = self.flock.submit_text("w2:p1", "draft only", press_enter=False)
        self.assertTrue(ok)
        self.assertEqual([], self.herdr.keys)
        self.assertEqual("draft only", self.herdr.box["w2:p1"])

    def test_our_stranded_text_is_flushed_before_the_next_send(self):
        # A send that failed to submit leaves our text sitting in the box.
        self.herdr.enters_needed["w2:p1"] = 99
        self.assertFalse(self.flock.submit_text("w2:p1", "stranded")[0])
        self.assertEqual("stranded", self.herdr.box["w2:p1"])
        # It is ours, so it must not look like a human's draft...
        self.assertFalse(self.flock._typing_in("w2:p1"))
        # ...and the next send flushes it first instead of appending to it.
        self.herdr.enters_needed["w2:p1"] = 1
        self.assertTrue(self.flock._ensure_box_clear("w2:p1"))
        self.assertEqual([("w2:p1", "stranded")], self.herdr.sends)
        self.assertEqual({}, self.herdr.box)

    def test_our_text_is_still_recognised_after_a_send_reported_success(self):
        # The wedge: a send reported sent, the daemon forgot the text, and the
        # text that was really still there then looked like a human's draft,
        # parking everything queued behind it (37 minutes, in the field).
        self.flock.submit_text("w2:p1", "first message")
        self.assertEqual([("w2:p1", "first message")], self.herdr.sends)
        # It is still there despite the success, as a bad reading would leave it.
        self.herdr.box["w2:p1"] = "first message"
        self.assertFalse(self.flock._typing_in("w2:p1"), "ours, not a draft")
        self.assertTrue(self.flock._ensure_box_clear("w2:p1"))
        self.assertEqual(2, len(self.herdr.sends), "it gets submitted, not waited on")

    def test_several_recent_sends_are_recognised_not_just_the_last(self):
        for text in ("alpha message", "beta message", "gamma message"):
            self.flock.submit_text("w2:p1", text)
        for text in ("alpha message", "gamma message"):
            self.assertTrue(self.flock._looks_like_ours("w2:p1", text), text)
        self.assertFalse(self.flock._looks_like_ours("w2:p1", "something a human wrote"))

    def test_foreign_text_is_submitted_to_preserve_it_not_sat_through(self):
        """Owner policy 2026-10-24: nothing else types into a pane, so text we did not
        write is PRESERVED by submitting it - never a hold, never clobbered."""
        self.herdr.set_screen("w2:p1", claude_box_typing_ansi("my own sentence"))
        self.herdr.box["w2:p1"] = "my own sentence"      # Enter consumes it
        # After the harness takes the line, the composer redraws empty.
        original = self.herdr.send_keys
        def clearing(pane_id, keys):
            result = original(pane_id, keys)
            if "Enter" in ([keys] if isinstance(keys, str) else list(keys)):
                self.herdr.set_screen(pane_id, claude_box_hint_ansi())
            return result
        self.herdr.send_keys = clearing
        self.assertTrue(self.flock._ensure_box_clear("w2:p1"))
        self.assertEqual([("w2:p1", ["Enter"])], self.herdr.keys,
                         "their text is submitted, not overwritten: Enter only, no typing")
        # The fake records 'Enter submits the box' as a send: that IS the preserved
        # text reaching the agent. What must be empty is the text WE typed over it.
        self.assertEqual([("w2:p1", "my own sentence")], self.herdr.sends)
        self.assertEqual([], self.herdr.text_only, "nothing of ours was typed over it")

    def test_an_unreadable_pane_cannot_confirm_so_the_send_is_not_claimed(self):
        self.herdr.set_screen("w2:p1", None)
        ok, detail = self.flock.submit_text("w2:p1", "into the dark")
        self.assertFalse(ok)
        self.assertIn("unreadable", detail)

    def test_a_pane_with_no_input_box_needs_no_confirmation(self):
        # A bare shell: nothing to read back, so one Enter and done.
        self.herdr.set_screen("w1:p1", "hunter@cell:/tmp$ \n")
        ok, _detail = self.flock.submit_text("w1:p1", "ls -l")
        self.assertTrue(ok)
        self.assertEqual(1, len(self.herdr.keys))


class ModalPaneTests(unittest.TestCase):
    """A panel or dialog is not a half-written sentence, and not our text."""

    PANEL = ("  Current session \u00b7 Resets 4:30am\n  11% used\n"
             "  \u276f  Switch model\n  Esc to cancel\n")

    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)
        self.gaps = mock.patch.object(dunk_core, "SUBMIT_GAPS", (0.0, 0.02, 0.02))
        self.gaps.start()

    def tearDown(self):
        self.gaps.stop()
        self.flock.close()

    def test_a_slash_command_that_opens_a_panel_counts_as_submitted(self):
        # /usage ran, the panel replaced the box, and the daemon used to spend
        # four Enter presses on the panel and then report the send failed.
        self.herdr.set_screen("w2:p1", self.PANEL)
        ok, detail = self.flock.submit_text("w2:p1", "/usage")
        self.assertTrue(ok, detail)
        self.assertEqual(1, len(self.herdr.keys), "no Enter presses into the panel")

    def test_an_open_panel_is_not_someone_typing(self):
        self.herdr.set_screen("w2:p1", self.PANEL)
        self.assertFalse(self.flock._typing_in("w2:p1"),
                         "a dialog's selected row is not a draft")

    def test_keys_meant_for_a_panel_are_not_queued_behind_it(self):
        # Escape to dismiss the panel used to queue for ever, because the
        # panel looked like occupied input.
        self.herdr.set_screen("w2:p1", self.PANEL)
        result = self.flock.send_keys_now("w2:p1", "Escape", wait_s=3)
        self.assertTrue(result["ok"], result["message"])
        self.assertEqual([("w2:p1", ["Escape"])], self.herdr.keys)

    def test_text_is_not_typed_into_an_open_panel(self):
        self.herdr.set_screen("w2:p1", self.PANEL)
        self.assertFalse(self.flock._ensure_box_clear("w2:p1"),
                         "wait for the panel to go rather than type into it")

    def test_a_normal_box_is_unaffected(self):
        self.herdr.set_screen("w2:p1", claude_box_typing_ansi("mid sentence"))
        self.assertTrue(self.flock._typing_in("w2:p1"))
        self.herdr.set_screen("w2:p1", claude_box_hint_ansi())
        self.assertFalse(self.flock._typing_in("w2:p1"))


class PaneIndependenceTests(unittest.TestCase):
    """One slow pane must not stall every other pane's send."""

    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)

    def tearDown(self):
        self.flock.close()

    def test_a_pane_stuck_in_retries_does_not_block_another_pane(self):
        # Hold one pane's lock, as a long confirm-and-retry sequence does.
        self.flock._lock_for("w2:p1").acquire()
        try:
            started = time.time()
            sent, ok, _detail = self.flock.send_guarded(
                "w2:p3", lambda: self.herdr.send_text_and_enter("w2:p3", "unrelated"))
            self.assertTrue(sent and ok)
            self.assertLess(time.time() - started, 3,
                            "a busy pane must not hold up a different one")
            self.assertEqual([("w2:p3", "unrelated")], self.herdr.sends)
        finally:
            self.flock._lock_for("w2:p1").release()

    def test_two_sends_to_the_same_pane_are_still_serialised(self):
        held = self.flock._lock_for("w2:p1")
        held.acquire()
        done = []

        def attempt():
            self.flock.send_guarded(
                "w2:p1", lambda: self.herdr.send_text_and_enter("w2:p1", "second"))
            done.append(True)

        thread = threading.Thread(target=attempt, daemon=True)
        thread.start()
        time.sleep(0.4)
        self.assertEqual([], done, "the same pane waits its turn")
        held.release()
        thread.join(5)
        self.assertEqual([True], done)


class WorkerLivelockTests(unittest.TestCase):
    """The 27-minute VP wake-up: two checks disagreed and the worker span."""

    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)
        self.patches = [
            mock.patch.object(dunk_core, "TYPING_SETTLE_S", 0.02),
            mock.patch.object(dunk_core, "MESSAGE_RECHECK_S", 0.05),
            mock.patch.object(dunk_core, "SUBMIT_GAPS", (0.0, 0.02, 0.02)),
            mock.patch.object(dunk_core, "SUBMIT_VERIFY_GAPS", (0.01, 0.01)),
            mock.patch.object(dunk_core, "STALE_BOX_S", 0.4),
        ]
        for patch in self.patches:
            patch.start()

    def tearDown(self):
        for patch in self.patches:
            patch.stop()
        self.flock.close()

    def test_a_refused_delivery_recovers_instead_of_spinning(self):
        # Our own text sits in the box and will not submit, so the queue check
        # says "not a human, go ahead" while the send refuses under the lock.
        # That disagreement used to loop with no sleep and no progress.
        self.flock.submit_text("w2:p1", "an earlier send of ours")
        self.herdr.box["w2:p1"] = "an earlier send of ours"
        self.herdr.enters_needed["w2:p1"] = 99        # it will not submit

        queued = self.flock.send_now("codex", "wake up please", wait_s=0)
        reads_at_start = len(self.herdr.reads)
        time.sleep(1.0)
        # The worker must not have burned through hundreds of reads.
        self.assertLess(len(self.herdr.reads) - reads_at_start, 120,
                        "worker is spinning instead of waiting")

        # Once the box will take Enter again, recovery delivers within the
        # stale-box window rather than waiting for a restart.
        self.herdr.enters_needed["w2:p1"] = 1
        self.assertTrue(wait_until(
            lambda: self.flock.get_message(queued["message_id"])["status"] == "sent", 8),
            "the message never got through")
        self.assertIn(("w2:p1", "wake up please"), self.herdr.sends)

    def test_recovery_is_bounded_by_the_stale_box_window(self):
        self.assertLessEqual(dunk_core.STALE_BOX_S, 30.0,
                             "an agent waiting on a notice should not wait longer")


class TransientDraftTests(unittest.TestCase):
    """A single frame of an agent redrawing is not a half-written sentence."""

    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)
        self.gap = mock.patch.object(dunk_core, "DRAFT_CONFIRM_S", 0.02)
        self.gap.start()

    def tearDown(self):
        self.gap.stop()
        self.flock.close()

    def test_a_one_frame_artifact_does_not_refuse_a_send(self):
        # First read catches status text on the prompt line; by the second the
        # pane has finished redrawing. This cost a Codex pane 14 refusals.
        screens = iter([claude_box_typing_ansi("Working (3m 13s)"),
                        claude_box_hint_ansi()])
        real = self.herdr.read_pane

        def flaky(pane_id, lines=40, source=None, ansi=False):
            try:
                return next(screens)
            except StopIteration:
                return real(pane_id, lines=lines, source=source, ansi=ansi)

        with mock.patch.object(self.herdr, "read_pane", flaky):
            self.assertTrue(self.flock._ensure_box_clear("w2:p1"),
                            "a redraw must not look like a draft")

    def test_a_real_draft_still_refuses(self):
        self.herdr.set_screen("w2:p1", claude_box_typing_ansi("I am still writing this"))
        self.assertFalse(self.flock._ensure_box_clear("w2:p1"),
                         "a draft that is still there must hold the send")

    def test_a_draft_that_turns_out_to_be_ours_is_flushed(self):
        self.flock.submit_text("w2:p1", "our own text")
        screens = iter([claude_box_typing_ansi("unrecognised for a frame"),
                        claude_box_typing_ansi("our own text")])
        real = self.herdr.read_pane

        def flaky(pane_id, lines=40, source=None, ansi=False):
            try:
                return next(screens)
            except StopIteration:
                return real(pane_id, lines=lines, source=source, ansi=ansi)

        self.herdr.box["w2:p1"] = "our own text"
        with mock.patch.object(self.herdr, "read_pane", flaky):
            self.assertTrue(self.flock._ensure_box_clear("w2:p1"))
        self.assertIn(("w2:p1", "our own text"), self.herdr.sends)


class StalePanelTests(unittest.TestCase):
    """A dialog is a person's to answer; the daemon waits and says so."""

    PANEL = ("  Current session \u00b7 Resets 4:30am\n  11% used\n  Esc to cancel\n")

    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)
        self.patches = [
            mock.patch.object(dunk_core, "TYPING_SETTLE_S", 0.02),
            mock.patch.object(dunk_core, "MESSAGE_RECHECK_S", 0.05),
            mock.patch.object(dunk_core, "SUBMIT_GAPS", (0.0, 0.02, 0.02)),
            mock.patch.object(dunk_core, "TYPING_RECHECK_S", 0.1),
            mock.patch.object(dunk_core, "MODAL_WAIT_S", 0.3),
        ]
        for patch in self.patches:
            patch.start()

    def tearDown(self):
        for patch in self.patches:
            patch.stop()
        self.flock.close()

    def test_the_two_gates_disagree_about_a_panel(self):
        # The disagreement itself: the queue check sees no draft, the send
        # check refuses. That is what span for 34 minutes.
        self.herdr.set_screen("w2:p1", self.PANEL)
        self.assertFalse(self.flock._typing_in("w2:p1"), "a panel is not a draft")
        self.assertFalse(self.flock._ensure_box_clear("w2:p1"), "but cannot be typed into")

    def test_a_message_waits_for_a_person_to_answer_the_dialog(self):
        # The daemon must not answer it. Pressing Escape on a Codex approval
        # prompt chooses "no" for the human, and interrupts a working pane.
        self.herdr.set_screen("w2:p1", self.PANEL)
        queued = self.flock.send_now("codex", "behind a panel", wait_s=0)
        time.sleep(1.0)
        self.assertEqual([], self.herdr.keys, "nothing may be pressed at a dialog")
        self.assertEqual("queued",
                         self.flock.get_message(queued["message_id"])["status"])
        # Once a person answers it, delivery resumes on its own.
        self.herdr.set_screen("w2:p1", claude_box_hint_ansi())
        self.assertTrue(wait_until(
            lambda: self.flock.get_message(queued["message_id"])["status"] == "sent", 8))

    def test_a_panel_being_navigated_is_left_alone(self):
        for n in range(6):
            self.herdr.set_screen("w2:p1", self.PANEL.replace("11%", f"1{n}%"))
            time.sleep(0.08)
        self.assertNotIn(("w2:p1", ["Escape"]), self.herdr.keys,
                         "a changing panel is someone using it")

    def test_a_dunk_waits_at_a_dialog_rather_than_answering_it(self):
        self.herdr.set_screen("w2:p1", self.PANEL)
        dunk = self.flock.add(target="w2:p1", text="nudge", interval_minutes=1 / 60,
                              start=True, only_when="any")
        self.assertTrue(wait_until(
            lambda: self.flock.get(dunk["id"])["hold_count"] >= 1, 8))
        self.assertEqual([], self.herdr.keys, "a dunk may not answer a dialog either")
        self.assertEqual(0, self.flock.get(dunk["id"])["send_count"])
        # And it goes through once the dialog is gone.
        self.herdr.set_screen("w2:p1", claude_box_hint_ansi())
        self.assertTrue(wait_until(
            lambda: self.flock.get(dunk["id"])["send_count"] >= 1, 8))


class DialogScopeTests(unittest.TestCase):
    """A dialog counts only while it is the live screen, and is never answered."""

    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)
        self.wait = mock.patch.object(dunk_core, "MODAL_WAIT_S", 0.1)
        self.wait.start()

    def tearDown(self):
        self.wait.stop()
        self.flock.close()

    def test_a_live_dialog_is_recognised(self):
        self.assertTrue(dunk_core._modal_open(codex_approval_dialog()))

    def test_an_answered_dialog_in_the_scrollback_is_not(self):
        # Scanning the whole screen matched history as well as the live
        # interface, so a pane that had ever shown a prompt stayed "modal".
        screen = codex_answered_dialog_in_scrollback()
        self.assertIn("Press Enter to confirm", screen, "the history is still there")
        self.assertFalse(dunk_core._modal_open(screen),
                         "only the bottom of the screen is the live interface")

    def test_a_dialog_is_reported_and_never_answered(self):
        # 2.9.0 pressed Escape, which answers a Codex approval prompt with
        # "no" and interrupts a working pane. It did so 1042 times.
        self.herdr.set_screen("w2:p1", codex_approval_dialog())
        signature = self.flock._box_signature("w2:p1")
        self.assertEqual("modal", signature[0])
        time.sleep(0.15)
        self.assertFalse(
            self.flock._note_stale_panel("w2:p1", signature, time.time() - 5),
            "a dialog is for a person to answer")
        self.assertEqual([], self.herdr.keys, "nothing may be pressed there")

    def test_nothing_is_typed_into_a_live_dialog(self):
        self.herdr.set_screen("w2:p1", codex_approval_dialog())
        self.assertFalse(self.flock._ensure_box_clear("w2:p1"))
        self.assertFalse(self.flock._typing_in("w2:p1"), "a dialog is not a draft")

    def test_an_ordinary_pane_under_old_history_still_takes_a_send(self):
        self.herdr.set_screen("w2:p1", codex_answered_dialog_in_scrollback())
        self.assertTrue(self.flock._ensure_box_clear("w2:p1"),
                        "history must not block the pane")


class StrandedNoticeTests(unittest.TestCase):
    """Our own notice must stay recognisable across a daemon restart."""

    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)
        self.patches = [
            mock.patch.object(dunk_core, "TYPING_SETTLE_S", 0.02),
            mock.patch.object(dunk_core, "MESSAGE_RECHECK_S", 0.05),
            mock.patch.object(dunk_core, "SUBMIT_GAPS", (0.0, 0.02, 0.02)),
            mock.patch.object(dunk_core, "TYPING_RECHECK_S", 0.1),
            mock.patch.object(dunk_core, "STALE_BOX_S", 0.2),
        ]
        for patch in self.patches:
            patch.start()

    def tearDown(self):
        for patch in self.patches:
            patch.stop()
        self.flock.close()

    NOTICE = ("[inbox] 1 unread. Read with the dunkingsheep MCP tool inbox_read, "
              "or: dunkingsheep inbox read")

    def test_a_notice_is_ours_even_with_no_memory_of_sending_it(self):
        # A restart empties what we remember typing. The notice is still ours.
        fresh = Flock(herdr=self.herdr, persist=False, restore=False)
        try:
            self.assertEqual({}, fresh.sent_text_by_pane)
            self.assertTrue(fresh._looks_like_ours("w2:p1", self.NOTICE))
            self.assertFalse(fresh._typing_in("w2:p1") if False else False)
        finally:
            fresh.close()

    def test_a_stranded_notice_is_submitted_rather_than_protected_for_ever(self):
        # Exactly the overnight case: a notice left in the box by a daemon
        # that has since restarted, with nothing remembered about it.
        self.herdr.box["w2:p1"] = self.NOTICE
        self.assertEqual({}, self.flock.sent_text_by_pane)
        self.assertTrue(self.flock._ensure_box_clear("w2:p1"))
        self.assertEqual([("w2:p1", self.NOTICE)], self.herdr.sends)

    def test_a_dunk_recovers_its_own_stranded_text_instead_of_holding(self):
        # A dunk had no recovery at all, so one notice held it for ten hours.
        self.herdr.box["w2:p1"] = self.NOTICE
        dunk = self.flock.add(target="w2:p1", text="capacity check",
                              interval_minutes=1 / 60, start=True, only_when="any")
        self.assertTrue(wait_until(
            lambda: self.flock.get(dunk["id"])["send_count"] >= 1, 10),
            "the dunk never recovered from its own stranded notice")
        self.assertIn(("w2:p1", self.NOTICE), self.herdr.sends)

    def test_foreign_text_after_a_restart_is_preserved_the_same_way(self):
        self.herdr.set_screen("w2:p1", claude_box_typing_ansi("my own half sentence"))
        self.herdr.box["w2:p1"] = "my own half sentence"
        original = self.herdr.send_keys
        def clearing(pane_id, keys):
            result = original(pane_id, keys)
            if "Enter" in ([keys] if isinstance(keys, str) else list(keys)):
                self.herdr.set_screen(pane_id, claude_box_hint_ansi())
            return result
        self.herdr.send_keys = clearing
        fresh = Flock(herdr=self.herdr, persist=False, restore=False)
        try:
            self.assertTrue(fresh._ensure_box_clear("w2:p1"))
            self.assertEqual([("w2:p1", ["Enter"])], self.herdr.keys)
            self.assertEqual([("w2:p1", "my own half sentence")], self.herdr.sends,
                             "their text was submitted, not overwritten")
            self.assertEqual([], self.herdr.text_only)
        finally:
            fresh.close()


class IdleGateDefaultTests(unittest.TestCase):
    """Typing into an agent mid-turn can abort that turn."""

    def setUp(self):
        self.flock = Flock(herdr=FakeHerdr(), persist=False, restore=False)

    def tearDown(self):
        self.flock.close()

    def test_a_schedule_aimed_at_an_agent_waits_for_idle_by_default(self):
        self.assertEqual("idle", self.flock.add(target="w2:p1")["only_when"])

    def test_a_shell_pane_is_left_ungated(self):
        self.assertIsNone(self.flock.add(target="w1:p1")["only_when"])

    def test_the_caller_can_still_ask_for_any_time(self):
        self.assertIsNone(self.flock.add(target="w2:p1", only_when="any")["only_when"])
        self.assertEqual("idle", self.flock.add(target="w2:p1", only_when="idle")["only_when"])

    def test_a_restored_dunk_keeps_whatever_it_had(self):
        restored = Dunker.from_dict({"id": "d9", "target_pane_id": "w2:p1",
                                     "target_agent": "codex", "text": "x"})
        self.assertIsNone(restored.only_when, "an existing schedule is not changed")


class InboxNoticeTests(unittest.TestCase):
    """One notice per pane, carrying the current count."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.herdr = FakeHerdr()
        from dunk_inbox import Inbox
        self.inbox = Inbox(path=os.path.join(self.tmp.name, "i.db"),
                           blob_dir=os.path.join(self.tmp.name, "b"))
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False,
                           inbox=self.inbox)
        self.patches = [
            mock.patch.object(dunk_core, "TYPING_SETTLE_S", 0.02),
            mock.patch.object(dunk_core, "MESSAGE_RECHECK_S", 0.05),
        ]
        for patch in self.patches:
            patch.start()

    def tearDown(self):
        for patch in self.patches:
            patch.stop()
        self.flock.close()
        self.inbox.close()
        self.tmp.cleanup()

    def test_a_notice_names_the_inbox_the_sender_and_the_subject(self):
        line = dunk_core._notice_text("poolrad-codex", 1, "PM", "PR 130 review")
        self.assertIn("new message for poolrad-codex", line)
        self.assertIn("from PM", line)
        self.assertIn("PR 130 review", line)
        self.assertIn("dunkingsheep inbox read", line)
        self.assertLess(len(line), 200, "a notice stays one short line")
        # With more waiting it says so; with nothing else known it still works.
        self.assertIn("(3 unread)", dunk_core._notice_text("vp", 3, "pm", None))
        self.assertIn("new message for writer", dunk_core._notice_text("writer", 1))

    def test_the_notice_typed_into_a_pane_names_the_inbox(self):
        self.herdr.set_screen("w2:p1", claude_box_hint_ansi())
        self.flock.inbox_send("w2:p1", "body", subject="a subject", notify=True)
        self.assertTrue(wait_until(lambda: len(self.herdr.sends) >= 1, 6))
        typed = self.herdr.sends[0][1]
        self.assertTrue(typed.startswith(dunk_core.NOTICE_PREFIX))
        self.assertIn("new message for", typed)
        self.assertIn("a subject", typed)

    def test_notices_fold_into_one_instead_of_stacking(self):
        # Hold the pane so notices queue rather than going straight out.
        self.herdr.set_screen("w2:p1", claude_box_typing_ansi("a human is writing"))
        for _ in range(4):
            self.flock.inbox_send("w2:p1", "another message", notify=True)
        queued = [m for m in self.flock.list_messages()["queued"]
                  if m["pane_id"] == "w2:p1"]
        self.assertEqual(1, len(queued), f"stacked {len(queued)} notices")
        self.assertIn("(4 unread)", queued[0]["text"], "it carries the latest count")

    def test_notify_state_follows_the_notice_instead_of_going_stale(self):
        # status used to say "queued" for ever, because the inbox record was
        # written when the notice was created and never updated.
        self.herdr.set_screen("w2:p1", claude_box_typing_ansi("writing"))
        first = self.flock.inbox_send("w2:p1", "one", notify=True)
        second = self.flock.inbox_send("w2:p1", "two", notify=True)
        self.assertEqual("queued",
                         self.flock.inbox_status(first["message_id"])["notify"]["state"])
        self.herdr.set_screen("w2:p1", claude_box_hint_ansi())
        self.assertTrue(wait_until(
            lambda: self.flock.inbox_status(first["message_id"])["notify"]["state"]
            == "sent", 8))
        # The folded one is updated too: one notice covered both.
        self.assertEqual("sent",
                         self.flock.inbox_status(second["message_id"])["notify"]["state"])

    def test_the_folded_notice_is_what_actually_gets_delivered(self):
        self.herdr.set_screen("w2:p1", claude_box_typing_ansi("writing"))
        self.flock.inbox_send("w2:p1", "first", notify=True)
        self.flock.inbox_send("w2:p1", "second", notify=True)
        self.herdr.set_screen("w2:p1", claude_box_hint_ansi())
        self.assertTrue(wait_until(lambda: len(self.herdr.sends) >= 1, 6))
        self.assertEqual(1, len(self.herdr.sends), "only one notice typed")
        self.assertIn("(2 unread)", self.herdr.sends[0][1])


class GuardedKeysTests(unittest.TestCase):
    """A bare Enter is how a half-written sentence gets submitted."""

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

    def test_an_enter_meant_to_submit_is_confirmed_and_repeated(self):
        # The agent-to-agent pattern: type with no Enter, then send Enter.
        with mock.patch.object(dunk_core, "SUBMIT_GAPS", (0.0, 0.02, 0.02, 0.02)):
            self.flock.submit_text("w2:p1", "a long message", press_enter=False)
            self.assertEqual("a long message", self.herdr.box["w2:p1"])
            self.herdr.enters_needed["w2:p1"] = 3     # the Codex case
            ok, detail = self.flock.press_keys("w2:p1", ["Enter"])
            self.assertTrue(ok)
            self.assertEqual([("w2:p1", "a long message")], self.herdr.sends)
            self.assertEqual({}, self.herdr.box, "nothing left stranded")

    def test_a_bare_enter_on_an_empty_box_is_just_a_key_press(self):
        ok, _detail = self.flock.press_keys("w2:p1", ["Enter"])
        self.assertTrue(ok)
        self.assertEqual([("w2:p1", ["Enter"])], self.herdr.keys)
        self.assertEqual([], self.herdr.sends)

    def test_keys_that_are_not_enter_are_sent_untouched(self):
        self.flock.submit_text("w2:p1", "draft", press_enter=False)
        ok, _detail = self.flock.press_keys("w2:p1", ["Escape"])
        self.assertTrue(ok)
        self.assertEqual([("w2:p1", ["Escape"])], self.herdr.keys)
        self.assertEqual("draft", self.herdr.box["w2:p1"], "Escape must not submit")

    def test_send_keys_submits_the_text_that_was_there_first(self):
        """Policy 2026-10-24: text in the box is preserved by submitting it, so a bare
        Enter press no longer queues behind somebody's sentence forever."""
        self.herdr.set_screen("w2:p1", claude_box_typing_ansi("half a sentence"))
        self.herdr.box["w2:p1"] = "half a sentence"
        self.flock.send_keys_now("codex", "Enter", wait_s=0.2)
        self.assertTrue(wait_until(lambda: self.herdr.sends, 5),
                        "the text that was in the box is submitted first")
        self.assertEqual("half a sentence", self.herdr.sends[0][1])

    def test_send_keys_validates_and_can_barge_in(self):
        with self.assertRaisesRegex(DunkError, "keys are required"):
            self.flock.send_keys_now("codex", "   ")
        self.herdr.set_screen("w2:p1", claude_box_typing_ansi("draft"))
        forced = self.flock.send_keys_now("codex", "Escape", hold_while_typing=False)
        self.assertTrue(forced["ok"])
        self.assertEqual([("w2:p1", ["Escape"])], self.herdr.keys)

    def test_send_text_can_skip_the_enter(self):
        self.herdr.set_screen("w2:p1", claude_box_hint_ansi())
        result = self.flock.send_now("codex", "no newline", wait_s=3, press_enter=False)
        self.assertTrue(result["ok"])
        self.assertEqual([("w2:p1", "no newline")], self.herdr.text_only)
        self.assertEqual([], self.herdr.sends)


class PassthroughTypingTests(unittest.TestCase):
    """The herdr passthrough must not be a way around any of this."""

    def setUp(self):
        self.flock = Flock(herdr=FakeHerdr(), persist=False, restore=False)

    def tearDown(self):
        self.flock.close()

    def test_typing_subcommands_are_refused_with_the_safe_alternative(self):
        for command in ("pane send-text w2:p1 hi", "pane run w2:p1 ls",
                        "agent send codex hi", "pane send-keys w2:p1 Enter"):
            with self.assertRaises(DunkError) as caught:
                self.flock.herdr_command(command)
            self.assertIn("send_text", str(caught.exception))
        # Everything else still goes through.
        self.assertTrue(self.flock.herdr_command("workspace list")["ok"])
        self.assertTrue(self.flock.herdr_command("pane list")["ok"])


class QueueVisibilityTests(unittest.TestCase):
    """Held messages report the cause without exposing draft contents."""

    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)
        self.patches = [
            mock.patch.object(dunk_core, "TYPING_SETTLE_S", 0.02),
            mock.patch.object(dunk_core, "MESSAGE_RECHECK_S", 0.05),
            mock.patch.object(dunk_core, "SUBMIT_GAPS", (0.0, 0.02, 0.02)),
        ]
        for patch in self.patches:
            patch.start()

    def tearDown(self):
        for patch in self.patches:
            patch.stop()
        self.flock.close()

    def test_text_in_the_box_is_delivered_after_preserving_it(self):
        self.herdr.box["w2:p1"] = "private unfinished draft"
        sent = self.flock.send_now("codex", "wait your turn", wait_s=4)
        self.assertEqual("private unfinished draft", self.herdr.sends[0][1],
                         "the draft is submitted to preserve it")
        self.assertIn(("w2:p1", "wait your turn"), self.herdr.text_only)

    def test_list_messages_says_what_is_blocking_a_queue(self):
        self.herdr.box["w2:p1"] = "something in the way"
        self.flock.send_now("codex", "behind it", wait_s=0)
        blocked = self.flock.list_messages()["queued"]
        self.assertEqual(1, len(blocked))
        self.assertEqual("input box contains a draft", blocked[0]["blocked_by"])
        self.flock.submit_text("w2:p3", "ours")
        self.herdr.box["w2:p3"] = "ours"
        self.flock.send_now("w2:p3", "next", wait_s=0)
        mine = [m for m in self.flock.list_messages()["queued"] if m["pane_id"] == "w2:p3"]
        self.assertTrue(mine and mine[0]["blocked_by"] == "guarded send pending")


class UnwedgeTests(unittest.TestCase):
    """A queue must never park for ever on a box that will not clear."""

    def setUp(self):
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)
        self.patches = [
            mock.patch.object(dunk_core, "TYPING_SETTLE_S", 0.02),
            mock.patch.object(dunk_core, "MESSAGE_RECHECK_S", 0.05),
            mock.patch.object(dunk_core, "SUBMIT_GAPS", (0.0, 0.02, 0.02)),
            mock.patch.object(dunk_core, "SUBMIT_VERIFY_GAPS", (0.01, 0.01)),
            mock.patch.object(dunk_core, "STALE_BOX_S", 0.3),
        ]
        for patch in self.patches:
            patch.start()

    def tearDown(self):
        for patch in self.patches:
            patch.stop()
        self.flock.close()

    def test_our_own_stranded_text_is_submitted_so_the_queue_moves(self):
        # The field case: a dunk's text stranded in wN:p1 with three messages
        # waiting 37 minutes behind it.
        self.flock.submit_text("w2:p1", "a send of ours")
        self.herdr.box["w2:p1"] = "a send of ours"        # it never really left
        queued = self.flock.send_now("codex", "please deliver me", wait_s=0)
        self.assertTrue(wait_until(
            lambda: self.flock.get_message(queued["message_id"])["status"] == "sent", 8))
        self.assertEqual("please deliver me", self.herdr.sends[-1][1])

    def test_a_draft_we_did_not_type_is_submitted_to_preserve_it(self):
        """Policy 2026-10-24: text we did not write is PRESERVED by submitting it."""
        self.herdr.box["w2:p1"] = "I was in the middle of saying"
        sent = self.flock.send_now("codex", "behind their draft", wait_s=4)
        self.assertEqual("I was in the middle of saying", self.herdr.sends[0][1],
                         "their text is submitted first, unmodified")
        self.assertIn(("w2:p1", "behind their draft"), self.herdr.text_only)
        self.assertNotEqual("queued", sent.get("status"))
    def test_nothing_is_unwedged_when_no_message_is_waiting(self):
        self.herdr.box["w2:p1"] = "a draft nobody is waiting on"
        time.sleep(0.6)
        self.assertEqual([], self.herdr.sends)


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
        with self.assertRaisesRegex(DunkError, "no workspace matches"):
            self.flock.resolve_workspace("gamma")
        with self.assertRaisesRegex(DunkError, "needs TMUX_PANE"):
            self.flock.resolve_workspace("self")
        with self.assertRaisesRegex(DunkError, "ambiguous"):
            self.flock.resolve_tab("e")  # Codex Site, Sheep, Other

    def test_create_workspace_runs_command_in_its_pane_and_is_dunkable(self):
        created = self.flock.create_workspace(label="Site", cwd="/home/x/site", command="claude")
        self.assertEqual("Site", created["workspace"]["label"])
        self.assertEqual(created["pane"]["pane_id"], created["pane_id"])
        self.assertEqual("claude", created["ran"])
        # Started through the confirmed submit, like any other send.
        self.assertEqual([(created["pane_id"], "claude")], self.herdr.sends)
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
        self.assertEqual([], self.herdr.sends)
        # Explicit workspace by label, from anywhere.
        other = self.flock.create_tab(workspace="alpha", label="tests", command="pytest -q")
        self.assertEqual("w1", other["tab"]["workspace_id"])
        self.assertEqual([(other["pane_id"], "pytest -q")], self.herdr.sends)

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

    def test_a_closed_dunk_never_reports_itself_as_scheduled_again(self):
        # The countdown thread writes dunker.status every second. Close the dunk
        # immediately after starting it: the thread's first tick used to land
        # afterwards and overwrite "Target closed" with "Next: 60:00", so a
        # stopped dunk reported itself as scheduled again.
        dunk = self.flock.add(target="w2:p3", text="go", interval_minutes=60, start=True)
        self.flock.close_target("tab", "Other", self_pane_id="w1:p2")
        time.sleep(1.5)
        self.assertEqual("Target closed", self.flock.get(dunk["id"])["status"])

    def test_close_stops_dunks_aimed_inside_and_refuses_own_container(self):
        dunk = self.flock.add(target="w2:p3", text="go", interval_minutes=60, start=True)
        bystander = self.flock.add(target="w1:p2", text="go", interval_minutes=60, start=True)
        closed = self.flock.close_target("tab", "Other", self_pane_id="w1:p2")
        self.assertEqual({"kind": "tab", "id": "w2:t3", "closed_panes": ["w2:p3"],
                          "stopped_dunks": [dunk["id"]]}, closed)
        self.assertFalse(self.flock.get(dunk["id"])["running"])
        self.assertEqual("Target closed", self.flock.get(dunk["id"])["status"])
        self.assertTrue(self.flock.get(bystander["id"])["running"])
        with self.assertRaisesRegex(DunkError, "no tab matches"):
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
            self.flock.start_agent("a", "claude", workspace="beta")

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
