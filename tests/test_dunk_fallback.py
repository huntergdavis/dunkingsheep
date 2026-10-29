"""t359: table-driven tests for the deterministic model-fallback planner.

The table IS the specification: every tier transition the owner named, plus the
refusals. Each row states the credit snapshot, the pane state and the decision
that must come back, so a reader can check the rule without reading the code.
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import dunk_fallback as fb  # noqa: E402


WINDOWS = {
    "claude": ("five_hour", "weekly_all"),
    "codex": ("weekly",),
    "opencode-go": ("five_hour", "weekly", "monthly"),
    "free": ("monthly",),
}


def reading(provider, percent, status="ok"):
    """One provider reading shaped like `dunkingsheep usage --json`.

    Every required window for the provider is present, so a test that wants a
    MISSING window has to build the dict itself (see WindowValidationTests).
    """
    names = WINDOWS.get(provider, ("weekly",))
    windows = [] if percent is None else [
        {"name": name, "percent_remaining": percent} for name in names]
    return {"provider": provider, "status": status, "windows": windows}


ALL_FULL = [reading("claude", 100.0), reading("codex", 90.0),
            reading("opencode-go", 80.0)]
CLAUDE_SPENT = [reading("claude", 0.0), reading("codex", 90.0),
                reading("opencode-go", 80.0)]
CLAUDE_CODEX_SPENT = [reading("claude", 0.0), reading("codex", 0.0),
                      reading("opencode-go", 80.0)]
ALL_SPENT = [reading("claude", 0.0), reading("codex", 0.0),
             reading("opencode-go", 0.0), reading("pi", None)]


def pane(tier="claude", turn="idle", owned=True, harness=None, pane_id="%9"):
    return {"pane_id": pane_id, "tier": tier, "turn_state": turn,
            "owned_by_mechanism": owned, "harness": harness or tier}


class CreditStateTests(unittest.TestCase):
    def test_reduces_readings_to_three_states(self):
        state = fb.credit_state(ALL_SPENT + [reading("pi2", 55.0)])
        self.assertEqual("exhausted", state["claude"])
        self.assertEqual("available", state["pi2"])

    def test_failed_reading_is_unknown_not_available(self):
        state = fb.credit_state([reading("codex", None, status="error")])
        self.assertEqual("unknown", state["codex"])

    def test_no_windows_is_unknown_not_plenty(self):
        # pi reports a local session estimate, not plan quota: absence of
        # numbers must never read as "credit available".
        state = fb.credit_state([reading("pi", None)])
        self.assertEqual("unknown", state["pi"])

    def test_rounding_tail_is_treated_as_spent(self):
        state = fb.credit_state([reading("codex", 0.05)])
        self.assertEqual("exhausted", state["codex"])


class WindowValidationTests(unittest.TestCase):
    """CoS review i33219 finding 3: a missing window or a NaN is not credit."""

    def test_missing_required_window_is_unknown(self):
        reading_partial = {"provider": "codex", "status": "ok", "windows": []}
        self.assertEqual("unknown", fb.credit_state([reading_partial])["codex"])

    def test_missing_one_of_several_required_windows_is_unknown(self):
        reading_partial = {"provider": "claude", "status": "ok",
                           "windows": [{"name": "five_hour",
                                        "percent_remaining": 100.0}]}
        self.assertEqual("unknown", fb.credit_state([reading_partial])["claude"])

    def test_nan_window_is_unknown(self):
        reading_nan = {"provider": "codex", "status": "ok",
                       "windows": [{"name": "weekly",
                                    "percent_remaining": float("nan")}]}
        self.assertEqual("unknown", fb.credit_state([reading_nan])["codex"])

    def test_non_numeric_window_is_unknown(self):
        reading_bad = {"provider": "codex", "status": "ok",
                       "windows": [{"name": "weekly",
                                    "percent_remaining": "lots"}]}
        self.assertEqual("unknown", fb.credit_state([reading_bad])["codex"])

    def test_free_capacity_is_never_assumed(self):
        self.assertEqual("unknown", fb.tier_state({}, "free"))
        self.assertEqual("available",
                         fb.tier_state({"free": "available"}, "free"))


class TransitionTableTests(unittest.TestCase):
    """The owner's acceptance table: every tier transition, plus refusals."""

    CASES = [
        # (name, pane, credit, expected action, expected target/reason)
        ("claude has credit: nothing to do",
         pane("claude"), ALL_FULL, "skip", "current_has_credit"),
        ("claude spent -> codex",
         pane("claude"), CLAUDE_SPENT, "swap", "codex"),
        ("claude+codex spent -> pi",
         pane("claude"), CLAUDE_CODEX_SPENT, "swap", "pi"),
        ("claude+codex+pi spent, no free capacity -> NOTHING is swapped",
         pane("claude"), ALL_SPENT, "skip", "no_tier_has_credit"),
        ("claude+codex+pi spent WITH a funded free row -> free",
         pane("claude"), ALL_SPENT + [reading("free", 100.0)], "swap", "free"),
        ("codex spent -> pi",
         pane("codex"), [reading("claude", 100.0), reading("codex", 0.0),
                         reading("opencode-go", 80.0)], "swap", "pi"),
        ("codex spent, pi unknown, no free capacity -> NOTHING is swapped",
         pane("codex"), [reading("claude", 100.0), reading("codex", 0.0),
                         reading("opencode-go", None, status="error")],
         "skip", "no_tier_has_credit"),
        ("pi spent, no free capacity -> NOTHING is swapped",
         pane("pi"), [reading("claude", 100.0), reading("codex", 100.0),
                      reading("opencode-go", 0.0)], "skip", "no_tier_has_credit"),
        ("pi never falls BACK to claude or codex",
         pane("pi"), [reading("claude", 100.0), reading("codex", 100.0),
                      reading("opencode-go", 0.0),
                      reading("free", 100.0)], "swap", "free"),
        ("free is the last tier: nothing after it",
         pane("free"), ALL_SPENT, "skip", "on_last_tier"),
        ("mid-turn is never swapped",
         pane("claude", turn="working"), CLAUDE_SPENT, "skip", "mid_turn"),
        ("unknown turn state is not idle",
         pane("claude", turn=None), CLAUDE_SPENT, "skip", "turn_unknown"),
        ("a lane the mechanism does not own is untouched",
         pane("claude", owned=False), CLAUDE_SPENT, "skip",
         "not_owned_by_mechanism"),
        ("unknown current tier is refused, not guessed",
         pane("mystery"), CLAUDE_SPENT, "skip", "unknown_current_tier"),
    ]

    def test_every_transition_row(self):
        for name, pane_state, credit, action, expected in self.CASES:
            with self.subTest(name):
                decision = fb.plan_swap(pane_state, fb.credit_state(credit))
                self.assertEqual(action, decision["action"], decision)
                if action == "swap":
                    self.assertEqual(expected, decision["to"], decision)
                else:
                    self.assertEqual(expected, decision["reason"], decision)

    def test_exhausted_everywhere_swaps_nothing_and_says_so(self):
        """The owner's third acceptance case: credit exhausted everywhere.

        Nothing moves - not to another provider and NOT to free, because free
        capacity is not assumed. Every refusal is named.
        """
        panes = [pane("claude", pane_id="%1"), pane("codex", pane_id="%2"),
                 pane("pi", pane_id="%3"), pane("free", pane_id="%4")]
        plan = fb.plan_all(panes, fb.credit_state(ALL_SPENT))
        self.assertEqual([], plan["swapped"])
        self.assertEqual("0 swap(s), 4 skip(s)", plan["summary"])
        reasons = {d["pane_id"]: d["reason"] for d in plan["decisions"]}
        self.assertEqual({"no_tier_has_credit", "on_last_tier"},
                         {reasons["%1"], reasons["%2"], reasons["%3"]} |
                         {reasons["%4"]})
        self.assertEqual("on_last_tier", reasons["%4"])

    def test_all_unknown_swaps_nothing(self):
        unknown = [reading("claude", None, status="error"),
                   reading("codex", None, status="error"),
                   reading("opencode-go", None, status="error")]
        plan = fb.plan_all([pane("codex", pane_id="%1")],
                           fb.credit_state(unknown))
        self.assertEqual([], plan["swapped"])
        self.assertEqual("current_credit_unknown",
                         plan["decisions"][0]["reason"])

    def test_all_unknown_credit_never_swaps_to_a_provider(self):
        unknown = [reading("claude", None, status="error"),
                   reading("codex", None, status="error"),
                   reading("opencode-go", None, status="error")]
        decision = fb.plan_swap(pane("codex"), fb.credit_state(unknown))
        self.assertEqual("skip", decision["action"])
        self.assertEqual("current_credit_unknown", decision["reason"])


class PiTierTests(unittest.TestCase):
    """The board's "pi" reading is a local session estimate, not plan quota."""

    def test_pi_tier_reads_opencode_go_credit(self):
        credit = fb.credit_state([reading("pi", None),
                                  reading("opencode-go", 96.9)])
        self.assertEqual("available", fb.tier_state(credit, "pi"))
        self.assertEqual("current_has_credit",
                         fb.plan_swap(pane("pi"), credit)["reason"])

    def test_pi_exhausted_only_when_opencode_go_is(self):
        credit = fb.credit_state([reading("pi", None),
                                  reading("opencode-go", 0.0)])
        self.assertEqual("exhausted", fb.tier_state(credit, "pi"))

    def test_pi_unknown_when_the_only_quota_source_is_unknown(self):
        credit = fb.credit_state([reading("pi", None),
                                  reading("opencode-go", None, status="error")])
        self.assertEqual("unknown", fb.tier_state(credit, "pi"))


class ReserveAndWindowPolicyTests(unittest.TestCase):
    """VP i33250: owner above-5 reserve, and the all-reported-window contract."""

    def _codex_at(self, percent):
        return [reading("claude", 0.0), reading("codex", percent)]

    def test_a_one_percent_target_is_not_a_landing_place(self):
        # CoS reproduced 1% weekly codex being chosen. It has credit, so it is
        # not exhausted - and it is still not a target.
        decision = fb.plan_swap(pane("claude"),
                                fb.credit_state(self._codex_at(1.0)))
        self.assertEqual("skip", decision["action"])
        self.assertEqual("targets_below_reserve", decision["reason"])

    def test_exactly_five_percent_is_refused_and_above_it_is_allowed(self):
        at_five = fb.plan_swap(pane("claude"),
                               fb.credit_state(self._codex_at(5.0)))
        self.assertEqual("targets_below_reserve", at_five["reason"])
        above = fb.plan_swap(pane("claude"),
                             fb.credit_state(self._codex_at(5.1)))
        self.assertEqual("swap", above["action"])
        self.assertEqual("codex", above["to"])

    def test_current_exhaustion_and_target_reserve_are_different_questions(self):
        # A current tier at 1% has credit: nothing has failed, so the pane is
        # NOT swapped off it. Same number, opposite decision, because the
        # question is different.
        decision = fb.plan_swap(pane("claude"),
                                fb.credit_state([reading("claude", 1.0),
                                                 reading("codex", 90.0)]))
        self.assertEqual("skip", decision["action"])
        self.assertEqual("current_has_credit", decision["reason"])

    def test_an_exhausted_extra_reported_window_fails_closed(self):
        # CoS reproduced: five_hour 0 with weekly 99 read as "available".
        windows = [{"name": "weekly", "percent_remaining": 99.0},
                   {"name": "five_hour", "percent_remaining": 0.0}]
        state = fb.credit_state([{"provider": "codex", "status": "ok",
                                  "windows": windows}])
        self.assertEqual("exhausted", state["codex"])

    def test_an_unknown_extra_reported_window_fails_closed(self):
        windows = [{"name": "weekly", "percent_remaining": 99.0},
                   {"name": "five_hour", "percent_remaining": float("nan")}]
        state = fb.credit_state([{"provider": "codex", "status": "ok",
                                  "windows": windows}])
        self.assertEqual("unknown", state["codex"])

    def test_a_healthy_extra_window_does_not_break_availability(self):
        windows = [{"name": "weekly", "percent_remaining": 99.0},
                   {"name": "five_hour", "percent_remaining": 80.0}]
        state = fb.credit_state([{"provider": "codex", "status": "ok",
                                  "windows": windows}])
        self.assertEqual("available", state["codex"])

    def test_record_line_reports_headroom_not_just_availability(self):
        decision = fb.plan_swap(pane("claude"),
                                fb.credit_state(self._codex_at(42.0)))
        self.assertIn("codex 42.0% left, above 5 reserve", decision["record"])


class ReproducibilityTests(unittest.TestCase):
    def test_same_numbers_same_target_twice(self):
        credit = fb.credit_state(CLAUDE_CODEX_SPENT)
        first = fb.plan_swap(pane("claude"), credit)
        second = fb.plan_swap(dict(reversed(list(pane("claude").items()))), credit)
        self.assertEqual(first, second)

    def test_plan_all_is_ordered_by_pane_id(self):
        panes = [pane("claude", pane_id="%3"), pane("claude", pane_id="%1"),
                 pane("claude", pane_id="%2")]
        plan = fb.plan_all(panes, fb.credit_state(CLAUDE_SPENT))
        self.assertEqual(["%1", "%2", "%3"],
                         [d["pane_id"] for d in plan["decisions"]])

    def test_credit_order_in_the_input_does_not_matter(self):
        a = fb.credit_state(CLAUDE_CODEX_SPENT)
        b = fb.credit_state(list(reversed(CLAUDE_CODEX_SPENT)))
        self.assertEqual(a, b)
        self.assertEqual(fb.plan_swap(pane("claude"), a),
                         fb.plan_swap(pane("claude"), b))


class WiringTests(unittest.TestCase):
    def test_codex_swap_reports_its_required_args(self):
        decision = fb.plan_swap(pane("claude"), fb.credit_state(CLAUDE_SPENT))
        self.assertEqual("codex", decision["to"])
        self.assertEqual(["-s", "danger-full-access", "-a", "never"],
                         decision["wiring"]["args"])
        # i33219 finding 4: paired native producers now exist, so a relauncher
        # must wire codex's turn producer rather than skip it.
        self.assertTrue(decision["wiring"]["emits_turn_events"])

    def test_no_static_reset_date_is_asserted(self):
        for tier, wiring in fb.HARNESS_WIRING.items():
            self.assertNotIn("2026-", wiring["note"], tier)

    def test_pi_swap_reports_a_turn_producer(self):
        decision = fb.plan_swap(pane("codex"), fb.credit_state(CLAUDE_CODEX_SPENT))
        self.assertEqual("pi", decision["to"])
        self.assertTrue(decision["wiring"]["emits_turn_events"])

    def test_record_line_names_pane_from_to_and_credit_reason(self):
        decision = fb.plan_swap(pane("claude", pane_id="%42"),
                                fb.credit_state(CLAUDE_SPENT))
        record = decision["record"]
        self.assertTrue(record.startswith("pane %42: claude -> codex because "),
                        record)
        self.assertIn("claude credit exhausted", record)


class OwnershipTests(unittest.TestCase):
    def test_allowlist_is_authoritative_when_given(self):
        decision = fb.plan_swap(pane("claude"), fb.credit_state(CLAUDE_SPENT),
                                allowed_panes=["%1", "%2"])
        self.assertEqual("not_owned_by_mechanism", decision["reason"])

    def test_allowlist_member_is_planned(self):
        decision = fb.plan_swap(pane("claude", pane_id="%2"),
                                fb.credit_state(CLAUDE_SPENT),
                                allowed_panes=["%2"])
        self.assertEqual("swap", decision["action"])


class InputValidationTests(unittest.TestCase):
    def test_pane_without_id_is_refused(self):
        with self.assertRaises(fb.FallbackError):
            fb.plan_swap({"tier": "claude"}, {})

    def test_credit_must_be_a_mapping(self):
        with self.assertRaises(fb.FallbackError):
            fb.plan_swap(pane("claude"), ["claude"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
