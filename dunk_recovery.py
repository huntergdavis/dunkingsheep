"""t504: bounded, deterministic recovery for a DS-owned prompt in an ambiguous frame.

The measured stall: d82 typed its own prompt into %2 at 08:28 and the lane never
submitted it. The screen showed a prompt row separated from the footer by two
blank rows, so the delivery gate refused with "ambiguous composer/transcript"
and *every* later send to that pane queued behind the hold. The text was ours
(character-for-character equal to the dunk's `last_sent_text`), it had never
entered the lane's session (0 messages in CoS's codex rollout), and nothing in
the daemon could act on either fact - the refusal had no ownership path.

The rule this module implements, in one sentence:

    submit again ONLY when we can prove the box holds exactly the prompt from
    the CURRENT durable attempt, that attempt is still unconsumed, the pane is
    the same generation it was typed into, the lane's own session never
    received it, and the lane is provably idle with no dialog open.

Everything else refuses, with a reason. Two things this deliberately does NOT
do:

* it never relaxes the blank-row rule. An ambiguous frame stays ambiguous; what
  changes is that ambiguity alone is no longer the end of the decision, because
  the composer's *contents* can be matched against a durable attempt.
* it never presses a key on a guess. A human edit, a stale attempt, a consumed
  attempt, a replaced pane, an active turn, a dialog, an unknown generation and
  an exhausted attempt budget all refuse BEFORE any key is authorised.

The recovery is bounded to one submission per attempt (`max_recoveries`), so a
pane that swallows Enter cannot be hammered.
"""

from __future__ import annotations

import re

# The attempt states that may be recovered. "consumed" is a delivery that the
# lane has already taken (nothing to recover), and "cancelled" is a message the
# daemon will never type (dunk_core treats it as terminal). Everything else -
# including states this code does not know - is NOT recoverable, so a new state
# cannot silently become actionable.
RECOVERABLE_ATTEMPT_STATES = frozenset({"pending", "unconfirmed", "typed"})


def _norm(text):
    """Compare what a person sees, not what a terminal wrapped.

    Wrapping and the composer's ellipsis are rendering, so they are removed;
    nothing else is normalised, because a single changed word is a human edit
    and must refuse.
    """
    if text is None:
        return ""
    return re.sub(r"\s+", " ", str(text).replace("…", " ")).strip()


def _refuse(reason, **extra):
    out = {"action": "refuse", "reason": reason}
    out.update(extra)
    return out


def plan_recovery(*, attempt, frame, pane, native_session_text=None,
                  recoveries_done=0, max_recoveries=1):
    """Decide whether to submit an owned, stalled prompt once more.

    ``attempt``  the durable record of what we typed:
                 {"attempt_id", "text", "state", "generation"}
    ``frame``    what the screen shows:
                 {"ambiguous": bool, "prompt_text": str, "dialog_open": bool}
    ``pane``     {"pane_id", "generation", "turn_state"}  turn_state is
                 "idle" | "working" | anything else (unknown)
    ``native_session_text``  the concatenated text of the lane's own harness
                 session, or None when it could not be read. None is UNKNOWN and
                 refuses: "we could not look" is not "it never arrived".
    """
    if not isinstance(attempt, dict) or not isinstance(frame, dict) \
            or not isinstance(pane, dict):
        raise ValueError("attempt, frame and pane must be mappings")

    text = _norm(attempt.get("text"))
    if not text:
        return _refuse("attempt_has_no_text")

    state = str(attempt.get("state") or "").strip()
    if state == "consumed":
        return _refuse("attempt_consumed")
    if state not in RECOVERABLE_ATTEMPT_STATES:
        # Fail closed on unknown states: a state this code does not know might
        # mean the lane already took the text.
        return _refuse("attempt_state_not_recoverable", state=state)

    attempt_generation = attempt.get("generation")
    current_generation = pane.get("generation")
    if not attempt_generation or not current_generation:
        return _refuse("generation_unknown")
    if attempt_generation != current_generation:
        return _refuse("wrong_generation", attempt=attempt_generation,
                       current=current_generation)

    turn = pane.get("turn_state")
    if turn != "idle":
        return _refuse("mid_turn" if turn == "working" else "turn_unknown",
                       turn_state=turn)
    if frame.get("dialog_open"):
        return _refuse("dialog_open")

    if native_session_text is None:
        return _refuse("native_session_unknown")
    if text and text in _norm(native_session_text):
        # The lane's own transcript already contains this text: it WAS
        # submitted, and pressing Enter again would duplicate it.
        return _refuse("already_submitted")

    visible = _norm(frame.get("prompt_text"))
    if visible != text:
        # Covers both directions: a human edit (visible differs) and a frame
        # that is showing something else entirely. The blank-row shape is not
        # consulted - contents decide, not geometry.
        return _refuse("human_edited" if visible else "frame_has_no_text",
                       visible_chars=len(visible), attempt_chars=len(text))

    if recoveries_done >= max_recoveries:
        return _refuse("recovery_exhausted", done=recoveries_done,
                       limit=max_recoveries)

    return {
        "action": "submit_once",
        "reason": "owned_pending_prompt_unsubmitted",
        "attempt_id": attempt.get("attempt_id"),
        "pane_id": pane.get("pane_id"),
        "generation": current_generation,
        "record": ("pane %s: resubmitting attempt %s because the composer holds "
                   "exactly its text, the attempt is %s, the generation is "
                   "unchanged and the lane's session never received it"
                   % (pane.get("pane_id"), attempt.get("attempt_id"), state)),
    }
