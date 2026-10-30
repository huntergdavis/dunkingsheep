"""Provider allowance windows, separate from fixed subscription prices."""
from __future__ import annotations

import json
import math
import os
import re
import sqlite3
import subprocess
import time
import threading
from concurrent.futures import ThreadPoolExecutor
from datetime import datetime, timezone

REFRESH_S = max(1, int(os.getenv("VP_USAGE_REFRESH_S", "1800")))
BACKOFF_S = tuple(max(1, int(value)) for value in os.getenv("VP_USAGE_BACKOFF_S", "300,600,1800").split(","))
MAX_AGE_S = 2 * REFRESH_S
SOURCE = "dunkingsheep --json usage"
PROVIDERS = ("claude", "codex", "pi", "opencode-go")


def _number(value):
    return value if isinstance(value, (int, float)) and not isinstance(value, bool) and math.isfinite(value) else None


def _time(value):
    try:
        if _number(value) is not None:
            return datetime.fromtimestamp(value, timezone.utc)
        parsed = datetime.fromisoformat(str(value).replace("Z", "+00:00"))
        return parsed.replace(tzinfo=timezone.utc) if parsed.tzinfo is None else parsed.astimezone(timezone.utc)
    except (ValueError, TypeError, OSError, OverflowError):
        return None


def _reason(reading, name, state):
    reason = str(reading.get("reason") or reading.get("error") or "").strip()
    if re.search(r"HTTP\s*429\b|rate_limit_error", reason, re.I):
        return f"{name} endpoint rate-limited (HTTP 429)"
    if reason:
        return reason[:240]
    return "" if state == "ok" else f"{name} usage {state}; no further reason supplied"


def _read_provider(provider):
    result = subprocess.run(["dunkingsheep", "--json", "usage", provider, "--max-age", str(MAX_AGE_S)],
                            capture_output=True, text=True, timeout=20)
    try:
        payload = json.loads(result.stdout)
    except ValueError:
        raise ValueError((result.stderr or "usage command returned invalid JSON").strip()[:240]) from None
    # A nonzero exit can mean one provider failed; preserve per-provider statuses.
    if not isinstance(payload, list) or len(payload) != 1 or not isinstance(payload[0], dict):
        raise ValueError("usage command returned an invalid provider list")
    if payload[0].get("provider") != provider:
        raise ValueError("usage command returned the wrong provider")
    return payload[0]


def _attempt(provider):
    try:
        return _read_provider(provider)
    except (OSError, subprocess.SubprocessError, ValueError) as exc:
        return {"provider": provider, "status": "error", "meaning": "unknown",
                "measured_at": time.time(), "error": str(exc)[:240]}


class UsageCache:
    """Provider-only cadence; slow quota endpoints never hold up the operations board."""

    def __init__(self, *, refresh_s=REFRESH_S, backoff_s=BACKOFF_S, reader=None, clock=None):
        self.refresh_s = refresh_s
        self.backoff_s = backoff_s
        self.reader = reader or _attempt
        self.clock = clock or time.time
        self._lock = threading.RLock()
        self._rows = {}
        self._good = {}
        self._due = {}
        self._failures = {}
        self._futures = {}
        self._pool = None

    def _collect(self, provider):
        try:
            row = self.reader(provider)
        except Exception as exc:
            row = {"provider": provider, "status": "error", "meaning": "unknown",
                   "measured_at": self.clock(), "error": str(exc)[:240]}
        with self._lock:
            now = self.clock()
            # Local collectors may return an old, valid observation as unknown.
            snapshot = row.get("last_snapshot") or {}
            has_snapshot = isinstance(snapshot, dict) and _number(snapshot.get("percent_used")) is not None
            has_local = row.get("meaning") == "local_session_estimate" and _number(row.get("session_cost_usd")) is not None
            success = row.get("status") == "ok" or (row.get("status") == "unknown" and (has_snapshot or has_local))
            if success:
                self._good[provider] = row
                self._failures[provider] = 0
                due = now + self.refresh_s
            else:
                failures = self._failures.get(provider, 0) + 1
                self._failures[provider] = failures
                due = now + min(self.refresh_s, self.backoff_s[min(failures - 1, len(self.backoff_s) - 1)])
                if provider in self._good:
                    row = {**row, "last_good": self._good[provider]}
            self._due[provider] = due
            self._rows[provider] = {**row, "next_attempt_at": due}

    def snapshot(self):
        with self._lock:
            if self._pool is None:
                self._pool = ThreadPoolExecutor(max_workers=len(PROVIDERS), thread_name_prefix="vp-usage")
            now = self.clock()
            for provider in PROVIDERS:
                future = self._futures.get(provider)
                if (future is None or future.done()) and now >= self._due.get(provider, 0):
                    self._futures[provider] = self._pool.submit(self._collect, provider)
            return [dict(self._rows.get(provider) or {
                "provider": provider, "status": "not_read_yet", "meaning": "unknown",
                "measured_at": None, "reason": "Waiting for the first provider reading",
            }) for provider in PROVIDERS]

    def stop(self):
        with self._lock:
            pool, self._pool = self._pool, None
        if pool:
            pool.shutdown(wait=True)


CACHE = UsageCache()


def read_usage():
    return CACHE.snapshot()


def project(readings, subscriptions, now, *, error=None):
    generated = now.isoformat().replace("+00:00", "Z")
    prices = {row["provider"]: row for row in subscriptions}
    providers = []
    seen = set()
    for reading in readings:
        name = reading.get("provider")
        if not isinstance(name, str) or not name:
            continue
        seen.add(name)
        last_good = reading.get("last_good")
        observation = last_good if isinstance(last_good, dict) else reading
        measured = _time(observation.get("measured_at"))
        age = max(0, int((now - measured).total_seconds())) if measured else None
        state = reading.get("status") or "unknown"
        meaning = observation.get("meaning")
        snapshot = observation.get("last_snapshot")
        snapshot = snapshot if isinstance(snapshot, dict) else {}
        raw_windows = observation.get("windows")
        if not raw_windows and snapshot and meaning == "percent_used":
            # Codex retains older observations here. Polling must not redate them.
            minutes = snapshot.get("window_minutes")
            used = _number(snapshot.get("percent_used"))
            raw_windows = [{**snapshot, "name": snapshot.get("name") or (
                "weekly" if minutes == 10080 else f"{minutes}min" if minutes else "last window"),
                "percent_remaining": 100 - used if used is not None and 0 <= used <= 100 else None}]
        has_value = bool(raw_windows) or (meaning == "local_session_estimate" and _number(observation.get("session_cost_usd")) is not None)
        if state in {"ok", "unknown"} and has_value:
            state = "stale" if age is None or age > MAX_AGE_S else "ok"
        reason = _reason(reading, name, state)
        failed = _time(reading.get("measured_at")) if state == "error" else None
        windows = []
        if meaning == "percent_used":
            for window in raw_windows if isinstance(raw_windows, list) else []:
                if not isinstance(window, dict):
                    continue
                reset = _time(window.get("resets_at"))
                window_state = "expired" if reset and reset <= now else state
                used, left = _number(window.get("percent_used")), _number(window.get("percent_remaining"))
                if used is None and left is None:
                    window_state = "unknown"
                windows.append({
                    "name": str(window.get("name") or "unnamed"),
                    "state": window_state, "percent_used": used, "percent_remaining": left,
                    "resets_at": reset.isoformat().replace("+00:00", "Z") if reset else None,
                })
        subscription = prices.get("opencode" if name == "opencode-go" else name)
        price = None
        if subscription:
            price = {"monthly_usd": subscription["monthly_usd_cents"] / 100,
                     "state": "ok", "source": "owner-reported subscription",
                     "observed_at": subscription["observed_at"]}
        extra = observation.get("extra_usage")
        extra = ({key: extra.get(key) for key in
                  ("used_credits", "monthly_limit", "utilization", "spend_limit_reached")}
                 if isinstance(extra, dict) else None)
        if extra:
            for key in ("used_credits", "monthly_limit", "utilization"):
                extra[key] = _number(extra[key])
            if not isinstance(extra["spend_limit_reached"], bool):
                extra["spend_limit_reached"] = None
        local = None
        if meaning == "local_session_estimate":
            local = {key: _number(observation.get(key)) for key in
                     ("session_cost_usd", "last_context_tokens", "session_messages_with_usage")}
        providers.append({
            "provider": name, "source": SOURCE, "state": state, "meaning": meaning,
            "generated_at": generated,
            "measured_at": measured.isoformat().replace("+00:00", "Z") if measured else None,
            "age_s": age, "reason": reason,
            "last_good": isinstance(last_good, dict),
            "next_attempt_at": reading.get("next_attempt_at"),
            "failed_at": failed.isoformat().replace("+00:00", "Z") if failed else None,
            "windows": windows, "subscription": price, "extra_usage": extra, "local_estimate": local,
        })
    for name, subscription in prices.items():
        canonical = "opencode-go" if name == "opencode" else name
        if canonical not in seen:
            providers.append({
                "provider": canonical, "source": SOURCE, "state": "unavailable",
                "meaning": "percent_used", "generated_at": generated, "measured_at": None,
                "age_s": None, "reason": error or "no provider reading", "windows": [],
                "extra_usage": None, "local_estimate": None,
                "subscription": {"monthly_usd": subscription["monthly_usd_cents"] / 100,
                                 "state": "ok", "source": "owner-reported subscription",
                                 "observed_at": subscription["observed_at"]},
            })
    return {"source": SOURCE, "state": "unavailable" if error else "ok",
            "generated_at": generated, "error": error, "providers": providers,
            "dollars_remaining": None,
            "dollars_reason": "Dollars remaining and per-agent dollar balances are unknown. Subscriptions are fixed prices; allowance percentages are shared provider limits."}


def build(conn, now):
    from ops_metrics import PROJECT
    subscriptions = []
    subscription_error = None
    try:
        subscriptions = [dict(row) for row in conn.execute(
            "SELECT provider,monthly_usd_cents,observed_at FROM subscription_costs s "
            "WHERE project=? AND id=(SELECT MAX(id) FROM subscription_costs "
            "WHERE project=s.project AND provider=s.provider) ORDER BY provider", (PROJECT,))]
    except sqlite3.Error:
        subscription_error = "Subscription source unavailable"
    try:
        readings, error = read_usage(), None
    except (OSError, subprocess.SubprocessError, ValueError) as exc:
        readings, error = [], str(exc)[:240]
    budget = project(readings, subscriptions, now, error=error)
    budget["subscription_error"] = subscription_error
    return budget
