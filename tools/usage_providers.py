#!/usr/bin/env python3
"""One real usage command per provider (owner #1110).

Every provider prints ONE JSON object on stdout and exits non-zero on failure.
The object is deliberately small and stable:

  {"provider": "...", "status": "ok"|"unknown"|"error",
   "meaning": "percent_used"|"local_session_estimate"|"unknown",
   "measured_at": <unix seconds>, "source": "...", "notes": [...],
   "windows": [{"name": "...", "percent_used": N, "percent_remaining": N,
                "resets_at": "...", "window_minutes": N}],
   ... provider-specific fields ...}

Rules:
- No cache, no estimate dressed as a reading. If the live number cannot be
  obtained, status is "unknown" or "error" with a reason.
- A stale snapshot is reported as status "unknown" (with the snapshot in
  `last_snapshot` for context), never as a current percentage.
- The command itself is the configurable unit: the usage config maps provider
  to a command line, timeout and parse spec.

Usage:
  python3 tools/usage_providers.py claude
  python3 tools/usage_providers.py codex [--max-age-s N]
  python3 tools/usage_providers.py pi [--max-age-s N]
  python3 tools/usage_providers.py opencode
"""

import argparse
import glob
import json
import os
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone

HOME = os.path.expanduser("~")


def _now():
    return time.time()


def _error(provider, message, **extra):
    result = {"provider": provider, "status": "error", "measured_at": _now(),
              "meaning": "unknown", "error": message}
    result.update(extra)
    return result


def _epoch(value):
    if isinstance(value, (int, float)):
        return float(value)
    try:
        return datetime.fromisoformat(str(value).replace("Z", "+00:00")).timestamp()
    except (ValueError, TypeError):
        return None


# -- claude ---------------------------------------------------------------

def claude():
    """Claude Code subscription usage from the endpoint the CLI itself uses.

    GET https://api.anthropic.com/api/oauth/usage with the OAuth bearer token
    from ~/.claude/.credentials.json (beta oauth-2025-04-20). The response
    carries five_hour and seven_day `utilization`, which is PERCENT USED.
    """
    path = os.path.expanduser("~/.claude/.credentials.json")
    try:
        with open(path, encoding="utf-8") as handle:
            credentials = json.load(handle).get("claudeAiOauth") or {}
    except (OSError, ValueError) as error:
        return _error("claude", f"cannot read Claude credentials: {error}")
    token = credentials.get("accessToken")
    if not token:
        return _error("claude", "no Claude OAuth accessToken in credentials")
    expires = credentials.get("expiresAt")
    expires_s = None
    if expires:
        expires_s = float(expires) / 1000.0 if float(expires) > 1e12 else float(expires)
        if expires_s <= _now() + 60:
            return _error("claude", "OAuth access token expired; run claude to refresh it")
    request = urllib.request.Request(
        "https://api.anthropic.com/api/oauth/usage",
        headers={"Authorization": f"Bearer {token}",
                 "anthropic-version": "2023-06-01",
                 "anthropic-beta": "oauth-2025-04-20",
                 "User-Agent": "dunkingsheep-usage/1"},
    )
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            payload = json.load(response)
    except urllib.error.HTTPError as error:
        body = error.read().decode("utf-8", "replace")[:400]
        return _error("claude", f"usage endpoint HTTP {error.code}: {body}")
    except (urllib.error.URLError, OSError, ValueError) as error:
        return _error("claude", f"usage endpoint failed: {error}")

    windows = []
    for name, key in (("five_hour", "five_hour"), ("weekly_all", "seven_day")):
        block = payload.get(key)
        if not isinstance(block, dict):
            continue
        used = block.get("utilization")
        if not isinstance(used, (int, float)):
            continue
        windows.append({
            "name": name,
            "percent_used": float(used),
            "percent_remaining": round(100.0 - float(used), 1),
            "resets_at": block.get("resets_at"),
        })
    if not windows:
        return _error("claude", "usage endpoint returned no utilization windows")
    extra = payload.get("extra_usage") or {}
    notes = []
    if isinstance(extra, dict) and extra.get("utilization") is not None:
        notes.append(
            f"extra_usage credits {extra.get('used_credits')} of "
            f"{extra.get('monthly_limit')} used ({extra.get('utilization')}% used)")
    return {
        "provider": "claude", "status": "ok", "meaning": "percent_used",
        "measured_at": _now(),
        "source": "https://api.anthropic.com/api/oauth/usage",
        "subscription": credentials.get("subscriptionType"),
        "rate_limit_tier": credentials.get("rateLimitTier"),
        "windows": windows, "extra_usage": extra, "notes": notes,
    }


# -- codex ----------------------------------------------------------------

def _last_rate_limits(path):
    """The last rate_limits event in a rollout file, scanning backwards."""
    chunk = 8 * 1024 * 1024
    with open(path, "rb") as handle:
        handle.seek(0, 2)
        position = handle.tell()
        carry = b""
        while position > 0:
            size = min(chunk, position)
            position -= size
            handle.seek(position)
            data = handle.read(size) + carry
            lines = data.split(b"\n")
            carry = lines[0] if position > 0 else b""
            for line in reversed(lines[1:] if position > 0 else lines):
                if b"rate_limits" not in line:
                    continue
                try:
                    event = json.loads(line)
                except ValueError:
                    continue
                limits = _find_rate_limits(event)
                if limits is not None:
                    event["rate_limits"] = limits
                    return event
    return None


def _find_rate_limits(value):
    if isinstance(value, dict):
        if isinstance(value.get("rate_limits"), dict):
            return value["rate_limits"]
        for child in value.values():
            found = _find_rate_limits(child)
            if found is not None:
                return found
    elif isinstance(value, list):
        for child in value:
            found = _find_rate_limits(child)
            if found is not None:
                return found
    return None


def codex(max_age_s=3600.0):
    """OpenAI Codex weekly limits from the newest rate_limits event.

    Codex writes a RateLimitSnapshot into its rollout JSONL on each turn. The
    primary window is the weekly limit (10080 minutes); used_percent is PERCENT
    USED. A snapshot older than max_age_s is reported as unknown, because Codex
    exposes no account usage endpoint this command can query.
    """
    pattern = os.path.expanduser("~/.codex/sessions/**/*.jsonl")
    files = sorted(glob.glob(pattern, recursive=True), key=os.path.getmtime, reverse=True)[:8]
    candidates = []
    for path in files:
        try:
            event = _last_rate_limits(path)
        except OSError:
            continue
        if not event:
            continue
        limits = event.get("rate_limits") or {}
        primary = limits.get("primary") or {}
        used = primary.get("used_percent")
        if not isinstance(used, (int, float)):
            continue
        measured = _epoch(event.get("timestamp")) or os.path.getmtime(path)
        candidates.append((measured, limits, primary, path))
    if not candidates:
        return _error("codex", "no Codex rate_limits snapshot in ~/.codex/sessions")
    measured, limits, primary, path = max(candidates, key=lambda item: item[0])
    age = _now() - measured
    window = primary.get("window_minutes")
    name = "weekly" if window == 10080 else f"{window}min"
    snapshot = {"percent_used": float(primary["used_percent"]),
                "window_minutes": window, "resets_at": primary.get("resets_at"),
                "credits": (limits.get("credits") or {}).get("balance"),
                "plan_type": limits.get("plan_type")}
    if age > float(max_age_s):
        return {
            "provider": "codex", "status": "unknown", "meaning": "percent_used",
            "measured_at": measured,
            "source": os.path.relpath(path, HOME),
            "reason": (f"latest Codex snapshot is {age/60:.0f} min old "
                       f"(max {float(max_age_s)/60:.0f}); no live account endpoint"),
            "last_snapshot": snapshot,
        }
    return {
        "provider": "codex", "status": "ok", "meaning": "percent_used",
        "measured_at": measured, "source": os.path.relpath(path, HOME),
        "snapshot_age_s": round(age, 1),
        "windows": [{"name": name,
                     "percent_used": float(primary["used_percent"]),
                     "percent_remaining": round(100.0 - float(primary["used_percent"]), 1),
                     "resets_at": primary.get("resets_at"),
                     "window_minutes": window}],
        "credits": (limits.get("credits") or {}).get("balance"),
        "plan_type": limits.get("plan_type"),
    }


# -- pi -------------------------------------------------------------------

def _walk_usage(value):
    if isinstance(value, dict):
        usage = value.get("usage")
        if isinstance(usage, dict):
            yield usage
        for child in value.values():
            yield from _walk_usage(child)
    elif isinstance(value, list):
        for child in value:
            yield from _walk_usage(child)


def pi(max_age_s=3600.0):
    """Local pi session usage from the harness's own session JSONL.

    pi records provider token usage and a calculated cost per assistant
    message. This is a real local measurement, NOT the OpenCode Go plan quota
    (which exposes no usage endpoint this command can query).
    """
    pattern = os.path.expanduser("~/.pi/agent/sessions/**/*.jsonl")
    files = sorted(glob.glob(pattern, recursive=True), key=os.path.getmtime, reverse=True)[:3]
    if not files:
        return _error("pi", "no pi session JSONL under ~/.pi/agent/sessions")
    path = files[0]
    total_cost = 0.0
    input_tokens = output_tokens = cache_read = cache_write = 0
    count = 0
    last = None
    last_ts = None
    try:
        with open(path, encoding="utf-8", errors="replace") as handle:
            for line in handle:
                if '"usage"' not in line:
                    continue
                try:
                    event = json.loads(line)
                except ValueError:
                    continue
                for usage in _walk_usage(event):
                    count += 1
                    total_cost += float((usage.get("cost") or {}).get("total") or 0)
                    input_tokens += int(usage.get("input") or 0)
                    output_tokens += int(usage.get("output") or 0)
                    cache_read += int(usage.get("cacheRead") or 0)
                    cache_write += int(usage.get("cacheWrite") or 0)
                    last = usage
                    last_ts = _epoch(event.get("timestamp"))
    except OSError as error:
        return _error("pi", f"cannot read pi session: {error}")
    if not count or last is None:
        return _error("pi", "no usage records in the latest pi session")
    measured = last_ts or os.path.getmtime(path)
    age = _now() - measured
    result = {
        "provider": "pi", "status": "ok" if age <= float(max_age_s) else "unknown",
        "meaning": "local_session_estimate",
        "measured_at": measured,
        "source": os.path.relpath(path, HOME),
        "session_messages_with_usage": count,
        "session_cost_usd": round(total_cost, 6),
        "input_tokens": input_tokens, "output_tokens": output_tokens,
        "cache_read_tokens": cache_read, "cache_write_tokens": cache_write,
        "last_context_tokens": last.get("totalTokens"),
        "last_cost_usd": (last.get("cost") or {}).get("total"),
        "notes": ["local pi session token/cost; not the OpenCode Go plan quota"],
    }
    if age > float(max_age_s):
        result["reason"] = (f"latest pi usage is {age/60:.0f} min old "
                            f"(max {float(max_age_s)/60:.0f})")
    return result


# -- opencode / OpenCode Go ------------------------------------------------

def _stat(text, label):
    match = re.search(rf"{label}\s+\$?([0-9]+(?:\.[0-9]+)?)\s*([KM])?", text)
    if not match:
        return None
    value = float(match.group(1))
    if match.group(2) == "K":
        value *= 1000
    elif match.group(2) == "M":
        value *= 1000000
    return value


def opencode():
    """`opencode stats` is LOCAL token/cost, not the Go plan quota.

    The OpenCode Go account exposes no documented usage endpoint this command
    can query; the honest answer is UNKNOWN for the plan, with the local stats
    as context. If OpenCode publishes an endpoint, change the command line in
    the usage config, not this code.
    """
    try:
        done = subprocess.run(["opencode", "stats", "--days", "1"],
                              capture_output=True, text=True, timeout=30)
    except (OSError, subprocess.TimeoutExpired) as error:
        return _error("opencode", f"opencode stats failed: {error}")
    if done.returncode != 0:
        return _error("opencode", f"opencode stats exited {done.returncode}: "
                                  f"{done.stderr.strip()[:300]}")
    text = re.sub(r"\x1b\[[0-9;]*m", "", done.stdout)
    local = {
        "sessions": _stat(text, "Sessions"),
        "messages": _stat(text, "Messages"),
        "total_cost_usd": _stat(text, "Total Cost"),
        "input_tokens": _stat(text, "Input"),
        "output_tokens": _stat(text, "Output"),
        "cache_read_tokens": _stat(text, "Cache Read"),
        "cache_write_tokens": _stat(text, "Cache Write"),
    }
    return {
        "provider": "opencode-go", "status": "unknown", "meaning": "unknown",
        "measured_at": _now(), "source": "opencode stats --days 1",
        "reason": ("opencode stats reports local token/cost, not account quota; "
                   "no documented OpenCode Go usage endpoint was found"),
        "local": local,
    }


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("provider", choices=["claude", "codex", "pi", "opencode"])
    parser.add_argument("--max-age-s", type=float,
                        default=float(os.environ.get("DUNKINGSHEEP_USAGE_MAX_AGE_S", "3600")))
    args = parser.parse_args(argv)
    if args.provider == "claude":
        result = claude()
    elif args.provider == "codex":
        result = codex(args.max_age_s)
    elif args.provider == "pi":
        result = pi(args.max_age_s)
    else:
        result = opencode()
    print(json.dumps(result, indent=2, sort_keys=True))
    return 0 if result.get("status") == "ok" else 1


if __name__ == "__main__":
    sys.exit(main())
