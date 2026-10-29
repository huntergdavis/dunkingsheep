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

# The owner's tier order. A pane walks FORWARD through this list only: a lane on
# codex may fall back to pi or free, never back to claude, so a swap cannot
# oscillate between two subscriptions that both have credit.
TIER_ORDER = ("claude", "codex", "pi", "free")

# Which usage provider each tier's credit is read from. "free" rows have no
# provider reading of their own: they are the fallback of last resort and are
# treated as always available, which is exactly why they are last.
TIER_PROVIDER = {
    "claude": ("claude",),
    "codex": ("codex",),
    # The board's "pi" reading is a LOCAL SESSION ESTIMATE (dollars, context
    # tokens), not plan quota - it can never say whether the tier has credit.
    # The pi tier actually runs on opencode-go rows (muse-spark / deepseek), so
    # its credit is read from THAT provider only. The local pi estimate is a
    # cost summary; folding it in would make "exhausted" unreachable.
    "pi": ("opencode-go",),
    "free": (),
}

# Per-harness relaunch facts from the owner's spec. Kept as data so the planner
# can report them rather than having them hard-coded in a relauncher.
HARNESS_WIRING = {
    "codex": {
        "args": ["-s", "danger-full-access", "-a", "never"],
        "emits_turn_events": False,
        "note": "codex cannot emit turn events (t310/t264)",
    },
    "pi": {
        "args": [],
        "emits_turn_events": True,
        "note": "pi needs its turn-generation producer and can emit turn events",
    },
    "claude": {
        "args": [],
        "emits_turn_events": True,
        "note": "claude is out of weekly quota until 2026-10-29T08:59:59Z",
    },
}

# A window at or below this percentage is spent. Zero-tolerance would accept
# 0.0001% as "credit"; the board reports one decimal, so anything below one
# tenth of a percent is indistinguishable from empty.
SPENT_PERCENT = 0.1


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
    for reading in readings or []:
        provider = str(reading.get("provider") or "").strip()
        if not provider:
            continue
        if reading.get("status") != "ok":
            state[provider] = "unknown"
            continue
        windows = reading.get("windows") or []
        remaining = [w.get("percent_remaining") for w in windows]
        numbers = [float(v) for v in remaining if v is not None]
        if not numbers:
            # pi reports a local session estimate, not plan quota. Its
            # availability cannot be decided from this reading, so it is
            # unknown rather than assumed good.
            state[provider] = "unknown"
            continue
        state[provider] = "exhausted" if min(numbers) <= SPENT_PERCENT else "available"
    return state


def tier_state(credit, tier):
    """Availability of one tier, from the reduced credit state.

    A tier may read from more than one provider (pi: opencode-go rows plus the
    local pi estimate). It is available if ANY of them has credit, exhausted
    only if ALL are exhausted, and unknown otherwise - so one silent provider
    never turns into a swap on its own.
    """
    providers = TIER_PROVIDER.get(tier, ())
    if not providers:
        return "available"          # free rows: last resort, no provider
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
    if tier_state(credit, current) == "available":
        return _reason_for_skip("current_has_credit", pane_id=pane_id,
                                current=current)

    start = TIER_ORDER.index(current)
    for tier in TIER_ORDER[start + 1:]:
        state = tier_state(credit, tier)
        if state == "available":
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
                           f" ({tier} available)"),
            }

    # Every later tier is exhausted or unknown. Leave the pane alone and say
    # exactly why, which is the owner's "nothing is swapped and the report says
    # so" case.
    remaining = {tier: tier_state(credit, tier) for tier in TIER_ORDER[start + 1:]}
    return _reason_for_skip("no_tier_has_credit", pane_id=pane_id,
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
