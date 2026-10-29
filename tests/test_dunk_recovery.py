"""t504: table tests for the bounded owned-prompt recovery decision.

The table is the contract CoS asked for: recovery only with a matching durable
attempt, the current generation and native-session absence; every negative
prevents submission. The real frame shape from %2 is used as the positive row
(prompt row separated from the footer by two blank rows), because a fixture
that is easier than the incident proves nothing.
"""

import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import dunk_recovery as rec  # noqa: E402

PROMPT = ("Owner2370/2371 CoS two-minute check: answer owner messages FIRST. Inspect "
          "queue and actual worker claims, native starts, completions, stuck deliveries "
          "and coordinator latency.")


def attempt(text=PROMPT, state="unconfirmed", generation="pid:1480236:start:1034306831",
            attempt_id="a-1"):
    return {"attempt_id": attempt_id, "text": text, "state": state,
            "generation": generation}


def frame(prompt_text=PROMPT, dialog_open=False):
    return {"ambiguous": True, "prompt_text": prompt_text,
            "dialog_open": dialog_open}


def pane(generation="pid:1480236:start:1034306831", turn_state="idle"):
    return {"pane_id": "%2", "generation": generation, "turn_state": turn_state}


def session(text=""):
    """The lane's own harness transcript, as concatenated text."""
    return text


class RecoveryTable(unittest.TestCase):
    CASES = [
        # (name, attempt, frame, pane, native_session_text, done, action, reason)
        ("real %2 frame, owned pending prompt, never submitted -> submit once",
         attempt(), frame(), pane(), session("earlier assistant reply only"), 0,
         "submit_once", "owned_pending_prompt_unsubmitted"),
        ("already in the lane's session -> refuse (would duplicate)",
         attempt(), frame(), pane(), session("... " + PROMPT + " ..."), 0,
         "refuse", "already_submitted"),
        ("human edited the text -> refuse",
         attempt(), frame(prompt_text=PROMPT + " and also call Hunter"), pane(),
         session(""), 0, "refuse", "human_edited"),
        ("stale attempt state -> refuse",
         attempt(state="stale"), frame(), pane(), session(""), 0,
         "refuse", "attempt_state_not_recoverable"),
        ("unknown attempt state -> refuse (fail closed)",
         attempt(state="delivered-somewhere"), frame(), pane(), session(""), 0,
         "refuse", "attempt_state_not_recoverable"),
        ("consumed attempt -> refuse",
         attempt(state="consumed"), frame(), pane(), session(""), 0,
         "refuse", "attempt_consumed"),
        ("pane replaced (wrong generation) -> refuse",
         attempt(), frame(), pane(generation="pid:999:start:1"), session(""), 0,
         "refuse", "wrong_generation"),
        ("generation unknown -> refuse",
         attempt(generation=None), frame(), pane(), session(""), 0,
         "refuse", "generation_unknown"),
        ("pane generation unreadable -> refuse",
         attempt(), frame(), pane(generation=None), session(""), 0,
         "refuse", "generation_unknown"),
        ("active turn -> refuse",
         attempt(), frame(), pane(turn_state="working"), session(""), 0,
         "refuse", "mid_turn"),
        ("unknown turn state -> refuse",
         attempt(), frame(), pane(turn_state=None), session(""), 0,
         "refuse", "turn_unknown"),
        ("dialog open -> refuse",
         attempt(), frame(dialog_open=True), pane(), session(""), 0,
         "refuse", "dialog_open"),
        ("native session unreadable -> refuse (unknown is not absent)",
         attempt(), frame(), pane(), None, 0, "refuse", "native_session_unknown"),
        ("recovery budget spent -> refuse",
         attempt(), frame(), pane(), session(""), 1, "refuse", "recovery_exhausted"),
        ("frame shows nothing -> refuse",
         attempt(), frame(prompt_text=""), pane(), session(""), 0,
         "refuse", "frame_has_no_text"),
        ("attempt with no text -> refuse",
         attempt(text="   "), frame(), pane(), session(""), 0,
         "refuse", "attempt_has_no_text"),
    ]

    def test_every_row(self):
        for (name, att, frm, pne, sess, done, action, reason) in self.CASES:
            with self.subTest(name):
                plan = rec.plan_recovery(attempt=att, frame=frm, pane=pne,
                                         native_session_text=sess,
                                         recoveries_done=done)
                self.assertEqual(action, plan["action"], plan)
                self.assertEqual(reason, plan["reason"], plan)

    def test_a_positive_plan_names_its_attempt_and_generation(self):
        plan = rec.plan_recovery(attempt=attempt(), frame=frame(), pane=pane(),
                                 native_session_text=session(""))
        self.assertEqual("a-1", plan["attempt_id"])
        self.assertEqual("pid:1480236:start:1034306831", plan["generation"])
        self.assertIn("resubmitting attempt a-1", plan["record"])

    def test_geometry_is_not_consulted(self):
        """The blank-row shape never decides: contents do.

        The positive row above uses the incident's two-blank-row frame, and the
        refusal rows reuse it unchanged. A frame carrying a different row count
        and the same text must decide the same way, because the rule is the
        text, not where it sits.
        """
        same = frame()
        other_geometry = dict(same, prompt_rows=9, blank_rows=0)
        a = rec.plan_recovery(attempt=attempt(), frame=same, pane=pane(),
                              native_session_text="")
        b = rec.plan_recovery(attempt=attempt(), frame=other_geometry, pane=pane(),
                              native_session_text="")
        self.assertEqual(a["action"], b["action"])
        self.assertEqual(a["reason"], b["reason"])

    def test_wrapping_and_ellipsis_do_not_cause_a_false_human_edit(self):
        wrapped = PROMPT.replace(" ", "\n  ", 3) + "…"
        plan = rec.plan_recovery(attempt=attempt(), frame=frame(prompt_text=wrapped),
                                 pane=pane(), native_session_text="")
        self.assertEqual("submit_once", plan["action"])

    def test_one_changed_word_is_a_human_edit(self):
        edited = PROMPT.replace("FIRST", "first")
        plan = rec.plan_recovery(attempt=attempt(), frame=frame(prompt_text=edited),
                                 pane=pane(), native_session_text="")
        self.assertEqual("human_edited", plan["reason"])

    def test_budget_is_one_by_default(self):
        plan = rec.plan_recovery(attempt=attempt(), frame=frame(), pane=pane(),
                                 native_session_text="", recoveries_done=0)
        self.assertEqual("submit_once", plan["action"])
        again = rec.plan_recovery(attempt=attempt(), frame=frame(), pane=pane(),
                                  native_session_text="", recoveries_done=1)
        self.assertEqual("recovery_exhausted", again["reason"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
