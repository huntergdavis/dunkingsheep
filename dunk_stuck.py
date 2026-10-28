"""Deterministic recovery of a stuck agent message (t377, owner2281/2282).

THE PROBLEM. A message the daemon typed can sit unsubmitted in a lane's composer
while the lane is idle: the harness swallowed the Enter, or the composer needed a
carriage return instead. The owner sees a lane with mail that never acts, and the
only fix for two years was a human pressing Enter.

THE RULE (deterministic; no model, no agent judgement in the loop). Track the exact
message we own, the complete composer text, the pane generation and the observation
times. If the SAME text is still there unchanged after `threshold_s` (default 120 s)
and the target is idle rather than busy, dialogged or unknown, press Enter ONCE
under the daemon's guard, then verify consumption INDEPENDENTLY - the composer
cleared, or a NEW turn started. "Still working" is not success evidence.

EVERYTHING ELSE RESETS. A text edit, an appended continuation line, a replaced pane,
a cancelled message, a busy turn, an open dialog, an unknown state, or a composer
whose text we cannot prove we own all cancel the clock. Submitting somebody's
half-written sentence once is worse than leaving a stuck nudge stuck, so every
uncertain path fails closed.

This module is the state machine only. It performs no I/O and reads no clock: the
caller injects `now`, which is what makes the fake-clock tests exact.
"""
from __future__ import annotations

import hashlib
import json
import re
from dataclasses import dataclass, field
from typing import Optional

SCHEMA_FIELDS = ("message_id", "generation", "fingerprint", "first_seen",
                 "last_attempt_at", "attempts", "terminal")

ACTIONS = ("wait", "recover", "cooling", "reset", "terminal")

DEFAULT_THRESHOLD_S = 120.0
DEFAULT_MAX_ATTEMPTS = 2
DEFAULT_COOLDOWN_S = 60.0
DEFAULT_OBSERVATION_INTERVAL_S = 20.0

# Statuses that mean "do not touch this pane". `working` is here on purpose: a lane
# mid-turn is not a lane that failed to accept our text, and t130/t271 settled that
# a working lane queues its own input. Acting there would interrupt real work.
BLOCKED_STATUSES = ("blocked", "dead", "unknown", "working")

_WS = re.compile(r"\s+")


def collapse(text):
    """The composer's own wrapping is not content: compare on collapsed text."""
    return _WS.sub(" ", (text or "").strip())


def fingerprint(text):
    """Stable identity of the COMPLETE composer text.

    Exact, not prefix: if a person appended one word to our nudge, this changes and
    the recovery resets. That is the whole guard against submitting a continuation.
    """
    flat = collapse(text)
    return hashlib.sha256(flat.encode("utf-8")).hexdigest()[:16]


def _positive(raw, default, what):
    if raw is None:
        return float(default)
    try:
        value = float(raw)
    except (TypeError, ValueError):
        raise ValueError(f"{what} must be a number, got {raw!r}") from None
    if value <= 0:
        raise ValueError(f"{what} must be positive, got {raw!r}")
    return value


@dataclass(frozen=True)
class Config:
    """Tunables, validated once. Threshold default is the CoS's 120 s.

    `enabled` and `observation_interval_s` are RUNTIME inputs, not decoration: the
    worker checks `enabled` before it does anything, and the interval is what the
    operator is promised (it is validated against the threshold so the bound holds).
    """

    enabled: bool = True
    threshold_s: float = DEFAULT_THRESHOLD_S
    max_attempts: int = DEFAULT_MAX_ATTEMPTS
    cooldown_s: float = DEFAULT_COOLDOWN_S
    observation_interval_s: float = DEFAULT_OBSERVATION_INTERVAL_S

    @classmethod
    def from_mapping(cls, raw):
        raw = raw or {}
        if not isinstance(raw, dict):
            raise ValueError("stuck recovery config must be an object")
        enabled_raw = raw.get("enabled", True)
        if isinstance(enabled_raw, str):
            if enabled_raw.strip().lower() not in ("1", "0", "true", "false", "yes", "no"):
                raise ValueError(f"enabled must be a boolean, got {enabled_raw!r}")
            enabled = enabled_raw.strip().lower() in ("1", "true", "yes")
        elif isinstance(enabled_raw, bool):
            enabled = enabled_raw
        else:
            raise ValueError(f"enabled must be a boolean, got {enabled_raw!r}")
        interval = _positive(raw.get("observation_interval_s"),
                             DEFAULT_OBSERVATION_INTERVAL_S, "observation_interval_s")
        threshold = _positive(raw.get("threshold_s"), DEFAULT_THRESHOLD_S, "threshold_s")
        if enabled and interval > threshold:
            raise ValueError("observation_interval_s must be <= threshold_s, or the bound "
                             "cannot be honoured")
        attempts_raw = raw.get("max_attempts", DEFAULT_MAX_ATTEMPTS)
        try:
            attempts = int(attempts_raw)
        except (TypeError, ValueError):
            raise ValueError(f"max_attempts must be an integer, got {attempts_raw!r}") from None
        if attempts < 1:
            raise ValueError("max_attempts must be at least 1 (0 would disable the action)")
        return cls(enabled=enabled, threshold_s=threshold, max_attempts=attempts,
                   cooldown_s=_positive(raw.get("cooldown_s"), DEFAULT_COOLDOWN_S,
                                        "cooldown_s"),
                   observation_interval_s=interval)


@dataclass
class Observation:
    """One look at one pane, at one instant."""

    pane_id: str
    message_id: Optional[str]
    text: str                      # the COMPLETE composer text as read
    status: str                    # ready | working | blocked | dead | unknown
    owned: bool                    # the matcher says this text is ours
    generation: Optional[str] = None   # pane identity: pid/start, so a replaced pane differs
    dialog: bool = False               # a panel/dialog is open
    cancelled: bool = False            # the sender cancelled the tracked message

    def key(self):
        return (self.pane_id,)


@dataclass
class Decision:
    action: str                    # one of ACTIONS
    reason: str
    attempts: int = 0
    terminal: Optional[str] = None
    fingerprint: Optional[str] = None
    first_seen: Optional[float] = None
    stale_for: float = 0.0

    def __post_init__(self):
        if self.action not in ACTIONS:
            raise ValueError(f"unknown action {self.action!r}")

    @property
    def recovers(self):
        return self.action == "recover"


@dataclass
class Tracker:
    """Per-pane record of the tracked message, serialisable so a restart cannot
    duplicate an Enter: the attempt count survives, so a recovery is bounded across
    process lifetimes, not just within one.

    `spent` is the durable budget ledger, keyed by message identity. A reset (busy
    turn, edited text, pane replaced) or a consumption FORGETS the per-pane record but
    must not hand the same message a fresh budget: the ledger keeps what that message
    already used, so it cannot regain retries by disappearing and reappearing, and a
    crash mid-press cannot refund the attempt it reserved.
    """

    records: dict = field(default_factory=dict)
    spent: dict = field(default_factory=dict)

    # -- persistence ---------------------------------------------------------
    @classmethod
    def from_dict(cls, data):
        records = {}
        if isinstance(data, dict):
            for pane, record in data.items():
                if not isinstance(record, dict):
                    continue
                records[pane] = {name: record.get(name) for name in SCHEMA_FIELDS}
        return cls(records)

    def to_dict(self):
        return {pane: dict(record) for pane, record in self.records.items()}

    # -- the durable budget ledger ------------------------------------------
    @staticmethod
    def ledger_key(pane_id, message_id, generation, fingerprint):
        return "|".join(str(part) for part in (pane_id, message_id, generation, fingerprint))

    def reserved(self, pane_id, message_id, generation, fingerprint):
        return int(self.spent.get(self.ledger_key(pane_id, message_id, generation,
                                                  fingerprint), 0))

    def dump(self):
        return json.dumps({"records": self.to_dict(), "spent": dict(self.spent)},
                          sort_keys=True)

    def as_state(self):
        """The shape stored in the daemon's state file (both halves)."""
        return {"records": self.to_dict(), "spent": dict(self.spent)}

    @classmethod
    def from_state(cls, data):
        if isinstance(data, dict) and "records" in data:
            tracker = cls.from_dict(data.get("records") or {})
            spent = data.get("spent") or {}
            if isinstance(spent, dict):
                tracker.spent = {str(k): int(v) for k, v in spent.items()
                                 if isinstance(v, (int, float))}
            return tracker
        if isinstance(data, dict):               # an older file: records only
            return cls.from_dict(data)
        return cls()

    @classmethod
    def load(cls, text):
        if not text:
            return cls()
        try:
            return cls.from_state(json.loads(text))
        except (ValueError, TypeError):
            return cls()           # a corrupt tracker resets the clock; it never acts

    # -- the state machine ---------------------------------------------------
    def _reset(self, obs, reason):
        self.records.pop(obs.pane_id, None)
        return Decision("reset", reason)

    def decide(self, obs, config=None, now=None):
        """What to do about this observation, and the record it leaves behind.

        Mutating by design: `first_seen`, `attempts` and `last_attempt_at` ARE the
        state that makes the decision deterministic across ticks.
        """
        config = config or Config()
        now = obs_now(now)

        if not config.enabled:
            # A disabled recovery is not a slower recovery: it does not act at all.
            return Decision("reset", "stuck recovery is disabled", attempts=0)

        # 1. Nothing we can PROVE we own -> nothing to do, and forget the clock.
        if not obs.message_id:
            return self._reset(obs, "no tracked message for this pane")
        if obs.cancelled:
            return self._reset(obs, "the tracked message was cancelled")
        if not obs.text:
            return self._reset(obs, "the composer is empty")
        if not obs.owned:
            return self._reset(obs, "the composer text is not provably ours")
        if obs.dialog:
            return self._reset(obs, "a dialog or panel is open")
        if obs.status in BLOCKED_STATUSES:
            return self._reset(obs, f"target is {obs.status}")

        record = self.records.get(obs.pane_id)
        current = fingerprint(obs.text)
        if record and record.get("generation") and obs.generation \
                and record["generation"] != obs.generation:
            return self._reset(obs, "the pane was replaced")
        if record and record.get("message_id") and record["message_id"] != obs.message_id:
            return self._reset(obs, "the tracked message changed")
        if record and record.get("fingerprint") and record["fingerprint"] != current:
            return self._reset(obs, "the composer text changed")

        if not record:
            already = self.reserved(obs.pane_id, obs.message_id, obs.generation, current)
            self.records[obs.pane_id] = {
                "message_id": obs.message_id, "generation": obs.generation,
                "fingerprint": current, "first_seen": now,
                "last_attempt_at": None, "attempts": already, "terminal": None}
            return Decision("wait", "first sighting; the clock starts now",
                            fingerprint=current, first_seen=now)

        if record.get("terminal"):
            return Decision("terminal", record["terminal"], attempts=record["attempts"],
                            terminal=record["terminal"], fingerprint=current,
                            first_seen=record["first_seen"],
                            stale_for=now - (record["first_seen"] or now))

        first_seen = record["first_seen"] if record["first_seen"] is not None else now
        stale_for = now - first_seen
        if stale_for < config.threshold_s:
            return Decision("wait", f"unchanged for {stale_for:.0f}s of "
                                    f"{config.threshold_s:.0f}s", attempts=record["attempts"],
                            fingerprint=current, first_seen=record["first_seen"],
                            stale_for=stale_for)
        if record["attempts"] >= config.max_attempts:
            record["terminal"] = (f"{record['attempts']} recovery attempt(s) did not "
                                  "submit the text")
            self.spent[self.ledger_key(obs.pane_id, obs.message_id, obs.generation,
                                       current)] = record["attempts"]
            return Decision("terminal", record["terminal"], attempts=record["attempts"],
                            terminal=record["terminal"], fingerprint=current,
                            first_seen=first_seen, stale_for=stale_for)
        last = record.get("last_attempt_at")
        if last is not None and now - last < config.cooldown_s:
            return Decision("cooling", f"attempted {now - last:.0f}s ago; cooldown is "
                                       f"{config.cooldown_s:.0f}s", attempts=record["attempts"],
                            fingerprint=current, first_seen=record["first_seen"],
                            stale_for=stale_for)
        record["attempts"] += 1
        record["last_attempt_at"] = now
        # RESERVED BEFORE THE PRESS. The caller must persist this (and save) before it
        # sends a key; if the process dies between the key and the result, the attempt
        # is already spent, which is the only safe direction to be wrong in.
        self.spent[self.ledger_key(obs.pane_id, obs.message_id, obs.generation,
                                   current)] = record["attempts"]
        return Decision("recover", f"unchanged for {stale_for:.0f}s and idle; attempt "
                                   f"{record['attempts']} of {config.max_attempts}",
                        attempts=record["attempts"], fingerprint=current,
                        first_seen=record["first_seen"], stale_for=stale_for)

    def mark_terminal(self, pane_id, reason):
        record = self.records.get(pane_id)
        if record is not None:
            record["terminal"] = reason

    def forget(self, pane_id):
        self.records.pop(pane_id, None)


def obs_now(now):
    if now is None:
        raise ValueError("this state machine takes `now` from the caller; it never reads a clock")
    return float(now)


def verify(before, after):
    """Independent consumption evidence after ONE Enter. Returns (outcome, detail).

    "Still working" is deliberately NOT success: a lane that was already working is
    running somebody's turn, and t130's lesson is that a busy status proves nothing
    about our text.
    """
    if after is None:
        return "unverified", "no observation after the press"
    if after.text is None:
        # A read that FAILED is not an empty composer. Treating None as "" is how a
        # missing pane reported a successful recovery.
        return "unverified", "the pane could not be read after the press"
    if after.dialog:
        return "unverified", "a dialog opened after the press"
    if collapse(before.text) and not collapse(after.text):
        return "consumed", "the composer cleared"
    if before.status != "working" and after.status == "working":
        return "consumed", "a turn started after the press"
    if fingerprint(after.text) == fingerprint(before.text):
        return "unchanged", "the same text is still in the composer"
    return "modified", "the composer now holds different text"
