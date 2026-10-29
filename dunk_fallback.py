"""t359: deterministic model-fallback planning.

The owner's spec (i2249/i2250) asks for a function that, given the SAME credit
numbers the operations board reads (``dunkingsheep usage --json`` ->
``dunk_usage``/``tools/usage_providers``), decides which subscription a pane
should be relaunched on when its own is exhausted.

Two properties matter more than the feature:

* **Reproducible.** The decision is a pure function of the credit snapshot and
  the pane state. No clock, no randomness, no dict iteration order, no model in
  the loop. Any two readers of the same numbers pick the same target.
* **Fail closed.** A pane is only ever swapped when the target is positively
  known to have credit AND the pane is positively known to be between turns.
  Unknown credit is not credit; unknown turn state is not idle.

This module only PLANS. It does not relaunch anything, touch a pane, write
config or read the live daemon: the planner is the reviewable part, and the
live-swap acceptance (before/after model, mailbox intact) needs its own
coordinated authorisation.
"""

from __future__ import annotations

import math

# The owner's tier order. A pane walks FORWARD through this list only: a lane on
# codex may fall back to pi or free, never back to claude, so a swap cannot
# oscillate between two subscriptions that both have credit.
TIER_ORDER = ("claude", "codex", "pi", "free")

# Every quota window a provider is REQUIRED to report. A reading that omits one
# is not partially good news: the missing window could be the empty one, so the
# whole provider is "unknown". (CoS review i33219 finding 3.)
REQUIRED_WINDOWS = {
    "claude": ("five_hour", "weekly_all"),
    "codex": ("weekly",),
    "opencode-go": ("five_hour", "weekly", "monthly"),
}

# Free rows are usable only when a snapshot positively says so (a "free"
# provider reading with credit). The planner never assumes free capacity,
# because "everything is spent, so move it to free" is exactly the unsafe swap
# the owner forbade and CoS reproduced.
FREE_PROVIDER = "free"

# Which usage provider each tier's credit is read from. "free" reads a "free"
# provider row: it is NOT assumed available. Assuming it was the bug CoS caught
# (an all-spent snapshot moved panes onto a resource nobody had measured).
TIER_PROVIDER = {
    "claude": ("claude",),
    "codex": ("codex",),
    # The board's "pi" reading is a LOCAL SESSION ESTIMATE (dollars, context
    # tokens), not plan quota - it can never say whether the tier has credit.
    # The pi tier actually runs on opencode-go rows (muse-spark / deepseek), so
    # its credit is read from THAT provider only. The local pi estimate is a
    # cost summary; folding it in would make "exhausted" unreachable.
    "pi": ("opencode-go",),
    "free": (FREE_PROVIDER,),
}

# Per-harness relaunch facts from the owner's spec. Kept as data so the planner
# can report them rather than having them hard-coded in a relauncher.
HARNESS_WIRING = {
    "codex": {
        "args": ["-s", "danger-full-access", "-a", "never"],
        # t310/t264 recorded that codex could not emit turn events; the paired
        # native producers accepted since then supersede that, so a relauncher
        # must not skip wiring codex's producer.
        "emits_turn_events": True,
        "note": "codex needs -s danger-full-access -a never; paired native "
                "turn producers now exist",
    },
    "pi": {
        "args": [],
        "emits_turn_events": True,
        "note": "pi needs its turn-generation producer and can emit turn events",
    },
    "claude": {
        "args": [],
        "emits_turn_events": True,
        # No static reset date: claude's quota comes from the credit snapshot
        # (five_hour / weekly_all), so a hardcoded date can only go stale.
        "note": "claude quota is read from the credit snapshot",
    },
}

# A window at or below this percentage is spent. Zero-tolerance would accept
# 0.0001% as "credit"; the board reports one decimal, so anything below one
# tenth of a percent is indistinguishable from empty.
SPENT_PERCENT = 0.1

# A TARGET tier must have more than this much credit left in every window
# (owner rule, VP i33250: "above 5 reserve"). This is deliberately a different
# number from SPENT_PERCENT and must not be conflated with it:
#   - SPENT_PERCENT decides whether the pane's CURRENT tier is exhausted, which
#     is about what has already happened;
#   - RESERVE_PERCENT decides whether a TARGET is worth moving to, which is
#     about not landing a lane on a subscription that is about to run dry.
# A tier at 1% weekly is therefore not "exhausted" (nothing has failed yet) and
# is also not a legal target.
RESERVE_PERCENT = 5.0



class FallbackError(ValueError):
    """A caller error, not a decision: bad input shapes are refused."""


def credit_state(readings):
    """Reduce ``dunkingsheep usage --json`` output to {provider: state}.

    ``state`` is ``"available"``, ``"exhausted"`` or ``"unknown"``. A provider
    is available only when the reading succeeded AND every window it reports
    has credit left. A reading with no windows is ``unknown``: "no numbers"
    must never read as "plenty of numbers".
    """
    state = {}
    headroom = {}
    for reading in readings or []:
        provider = str(reading.get("provider") or "").strip()
        if not provider:
            continue
        if reading.get("status") != "ok":
            state[provider] = "unknown"
            continue
        windows = reading.get("windows") or []
        by_name = {}
        for window in windows:
            name = str(window.get("name") or "").strip()
            value = window.get("percent_remaining")
            if name:
                by_name[name] = value
        if not by_name:
            # pi reports a local session estimate, not plan quota. Its
            # availability cannot be decided from this reading, so it is
            # unknown rather than assumed good.
            state[provider] = "unknown"
            continue
        required = REQUIRED_WINDOWS.get(provider)
        if required is not None and any(name not in by_name for name in required):
            # One missing window is not "the others look fine": the absent one
            # could be the empty one.
            state[provider] = "unknown"
            continue
        numbers = []
        for name, value in by_name.items():
            # ALL-REPORTED-WINDOW CONTRACT (VP i33250): every window the
            # provider reports is evaluated, not only the required ones. An
            # extra window that is empty means the subscription is empty, so
            # skipping it because it was not on our list would be a lie.
            if isinstance(value, bool) or not isinstance(value, (int, float)):
                numbers = None
                break
            if not math.isfinite(float(value)):
                numbers = None
                break
            numbers.append(float(value))
        if not numbers:
            state[provider] = "unknown"
            continue
        state[provider] = "exhausted" if min(numbers) <= SPENT_PERCENT else "available"
        headroom[provider] = min(numbers)
    state["headroom"] = headroom
    return state


def tier_headroom(credit, tier):
    """Lowest FINITE remaining percentage for a tier, or None.

    None means "no usable number", which is not the same as "plenty": a
    hand-built or partially-filled snapshot has no headroom for a tier, so the
    tier cannot be a target.
    """
    providers = TIER_PROVIDER.get(tier, ())
    headroom = credit.get("headroom") or {}
    values = []
    for name in providers:
        value = headroom.get(name)
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            continue
        if not math.isfinite(float(value)):
            continue
        values.append(float(value))
    return min(values) if values else None


def tier_credit(credit, tier):
    """The lowest remaining percentage reported for a tier, or None.

    None means "no numbers", which is never treated as headroom.
    """
    providers = TIER_PROVIDER.get(tier, ())
    if not providers:
        return None
    return credit.get("headroom", {}).get(tier)


def tier_state(credit, tier):
    """Availability of one tier, from the reduced credit state.

    A tier reads from the providers in TIER_PROVIDER. Today every tier reads
    exactly one (the adapter is primary-only); the list form is kept so a tier
    that later reads several rows can be expressed as data, and the rule for
    that case is already fixed: available if ANY has credit, exhausted only if
    ALL are exhausted, unknown otherwise.
    """
    providers = TIER_PROVIDER.get(tier, ())
    if not providers:
        # No provider mapped: nothing measured, so nothing available. (No tier
        # is in this state today; the branch exists so an unconfigured tier can
        # never be treated as free capacity.)
        return "unknown"
    states = [credit.get(name, "unknown") for name in providers]
    if "available" in states:
        return "available"
    if all(state == "exhausted" for state in states):
        return "exhausted"
    return "unknown"


def _provider_label(tier):
    providers = TIER_PROVIDER.get(tier) or ()
    return "+".join(providers) if providers else tier


def _credit_label(credit, tier):
    providers = TIER_PROVIDER.get(tier) or ()
    if not providers:
        return "unknown"
    return "+".join(credit.get(name, "unknown") for name in providers)


def _reason_for_skip(reason, **extra):
    out = {"action": "skip", "reason": reason}
    out.update(extra)
    return out


def plan_swap(pane, credit, allowed_panes=None):
    """Decide whether ``pane`` should be relaunched on another tier.

    ``pane`` is a mapping with:
        pane_id            stable id; the relaunch must preserve it
        tier               the tier the pane is running now (or None)
        harness            "claude" | "codex" | "pi" | ...
        turn_state         "working" | "idle" | None (unknown)
        owned_by_mechanism whether this pane is managed by the fallback

    ``credit`` is the mapping produced by :func:`credit_state`.

    Returns a decision dict. ``action`` is ``"swap"`` or ``"skip"``; a skip
    always carries a ``reason`` so "nothing to do" and "I refused" never look
    alike. No side effects, no clock, no randomness.
    """
    if not isinstance(pane, dict):
        raise FallbackError("pane must be a mapping")
    pane_id = str(pane.get("pane_id") or "").strip()
    if not pane_id:
        raise FallbackError("pane needs a pane_id")
    if not isinstance(credit, dict):
        raise FallbackError("credit must be a mapping of provider -> state")

    if allowed_panes is not None and pane_id not in set(allowed_panes):
        return _reason_for_skip("not_owned_by_mechanism", pane_id=pane_id)
    if allowed_panes is None and not pane.get("owned_by_mechanism"):
        return _reason_for_skip("not_owned_by_mechanism", pane_id=pane_id)

    turn = pane.get("turn_state")
    if turn != "idle":
        # "working" is mid-turn; None is unknown. Both refuse: the owner's rule
        # is never swap mid-turn, and an unknown turn is not evidence of a gap.
        return _reason_for_skip(
            "mid_turn" if turn == "working" else "turn_unknown", pane_id=pane_id)

    current = pane.get("tier")
    if current not in TIER_ORDER:
        return _reason_for_skip("unknown_current_tier", pane_id=pane_id,
                                current=current)
    if current == "free":
        # Free rows are the end of the line: there is no cheaper tier behind
        # them, so "credit exhausted everywhere" reports this, not a swap.
        return _reason_for_skip("on_last_tier", pane_id=pane_id, current=current)
    current_state = tier_state(credit, current)
    if current_state == "available":
        return _reason_for_skip("current_has_credit", pane_id=pane_id,
                                current=current)
    if current_state != "exhausted":
        # Unknown is not exhausted. Swapping on an unproven exhaustion would
        # move a working lane off a subscription that may still have credit,
        # which is the unsafe fallback this planner exists to prevent.
        return _reason_for_skip("current_credit_unknown", pane_id=pane_id,
                                current=current)

    start = TIER_ORDER.index(current)
    for tier in TIER_ORDER[start + 1:]:
        state = tier_state(credit, tier)
        # "free" is a tier like any other here: its state comes from the
        # snapshot, so an unproven free row is "unknown" and is never chosen.
        if state != "available":
            continue
        left = tier_headroom(credit, tier)
        if left is None:
            # No positive finite number for the target means the reserve is
            # UNPROVEN, not satisfied. A snapshot without headroom data (or
            # with NaN) must refuse rather than compatibility-pass.
            continue
        if left <= RESERVE_PERCENT:
            # Above-5 reserve: a tier that still has credit but is nearly dry
            # is not a landing place. It is not "exhausted" either - those are
            # different questions and this is the second one.
            continue
        return {
            "action": "swap",
            "pane_id": pane_id,
            "from": current,
            "to": tier,
            "harness": pane.get("harness"),
            "wiring": HARNESS_WIRING.get(tier, HARNESS_WIRING["pi"]),
            "record": (f"pane {pane_id}: {current} -> {tier} because "
                       f"{_provider_label(current)} credit "
                       f"{_credit_label(credit, current)}"
                       f" ({tier} {left}% left, above {RESERVE_PERCENT:g} reserve)"),
        }

    # Every later tier is exhausted or unknown. Leave the pane alone and say
    # exactly why, which is the owner's "nothing is swapped and the report says
    # so" case.
    remaining = {}
    for tier in TIER_ORDER[start + 1:]:
        state = tier_state(credit, tier)
        left = tier_headroom(credit, tier)
        remaining[tier] = (f"{state} (headroom unknown)"
                           if left is None else f"{state} ({left}% left)")
    reason = "no_tier_has_credit"
    if any(str(v).startswith("available") for v in remaining.values()):
        # A later tier has credit but not enough of it: that is a reserve
        # refusal, not an exhaustion one, and the operator action differs.
        reason = "targets_below_reserve"
    if any(str(v).startswith("available (headroom unknown")
           for v in remaining.values()):
        # Available but with no number behind it: the reserve is unproven.
        reason = "target_headroom_unknown"
    return _reason_for_skip(reason, pane_id=pane_id,
                            current=current, tiers=remaining)


def plan_all(panes, credit, allowed_panes=None):
    """Plan every pane, in stable pane-id order so two readers agree."""
    decisions = [plan_swap(pane, credit, allowed_panes=allowed_panes)
                 for pane in sorted(panes or [], key=lambda p: str(p.get("pane_id")))]
    swapped = [d for d in decisions if d["action"] == "swap"]
    return {
        "decisions": decisions,
        "swapped": swapped,
        "skipped": [d for d in decisions if d["action"] != "swap"],
        "summary": (f"{len(swapped)} swap(s), "
                    f"{len(decisions) - len(swapped)} skip(s)"),
    }
