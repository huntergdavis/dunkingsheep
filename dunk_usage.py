"""Configurable real-usage runner (owner #1110).

The usage config maps each provider to a command line, a timeout and a parse
spec. A command must print one canonical JSON object; it may exit non-zero for
an unknown/error state, and the runner surfaces that state instead of hiding
it. There is no cache: every call runs the command again.

Usage config (default ~/.config/dunkingsheep/usage.json):

  {"schema": 1,
   "providers": {
     "claude": {
       "command": "python3 .../tools/usage_providers.py claude",
       "timeout_s": 30,
       "max_age_s": 120,
       "parse": {"windows": "windows", "meaning": "meaning",
                 "measured_at": "measured_at"}}}}

`max_age_s` is passed to the provider command as
`DUNKINGSHEEP_USAGE_MAX_AGE_S`; a provider that cannot honour it still returns
its own status/reason.
"""

import json
import os
import shlex
import subprocess
import time

SCHEMA = 1
_MISSING = object()


class UsageError(ValueError):
    """A config or invocation mistake; safe to show the operator."""


def _number(value, name):
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise UsageError(f"{name} must be a number")
    return float(value)


def load_config(path):
    path = os.path.expanduser(str(path))
    if not os.path.isfile(path):
        raise UsageError(f"no usage config at {path}; copy packaging/usage.example.json")
    try:
        with open(path, encoding="utf-8") as handle:
            config = json.load(handle)
    except (OSError, ValueError) as error:
        raise UsageError(f"cannot read {path}: {error}") from None
    if not isinstance(config, dict) or config.get("schema") != SCHEMA:
        raise UsageError(f"{path}: schema must be {SCHEMA}")
    providers = config.get("providers")
    if not isinstance(providers, dict) or not providers:
        raise UsageError(f"{path}: providers must be a non-empty object")
    for name, spec in providers.items():
        if not isinstance(spec, dict):
            raise UsageError(f"{path}: providers.{name} must be an object")
        command = spec.get("command")
        if not isinstance(command, str) or not command.strip():
            raise UsageError(f"{path}: providers.{name}.command must be a command line")
        try:
            shlex.split(command)
        except ValueError as error:
            raise UsageError(f"{path}: providers.{name}.command: {error}") from None
        timeout = spec.get("timeout_s", 30)
        _number(timeout, f"providers.{name}.timeout_s")
        if float(timeout) <= 0:
            raise UsageError(f"{path}: providers.{name}.timeout_s must be > 0")
        if "max_age_s" in spec:
            _number(spec["max_age_s"], f"providers.{name}.max_age_s")
        parse = spec.get("parse", {})
        if not isinstance(parse, dict) or any(
                not isinstance(key, str) or not isinstance(value, str)
                for key, value in parse.items()):
            raise UsageError(f"{path}: providers.{name}.parse must map names to dotted paths")
    return config


def _path(value, dotted):
    current = value
    for part in dotted.split("."):
        if isinstance(current, list):
            try:
                current = current[int(part)]
            except (ValueError, IndexError):
                return _MISSING
        elif isinstance(current, dict):
            if part not in current:
                return _MISSING
            current = current[part]
        else:
            return _MISSING
    return current


def _error(name, message, **extra):
    result = {"provider": name, "status": "error", "meaning": "unknown",
              "measured_at": time.time(), "error": message}
    result.update(extra)
    return result


def run_provider(name, spec, max_age=None):
    command = spec["command"]
    timeout = float(spec.get("timeout_s", 30))
    environment = dict(os.environ)
    age = spec.get("max_age_s", 3600) if max_age is None else max_age
    environment["DUNKINGSHEEP_USAGE_MAX_AGE_S"] = str(age)
    try:
        done = subprocess.run(shlex.split(command), capture_output=True,
                              text=True, timeout=timeout, env=environment)
    except subprocess.TimeoutExpired:
        return _error(name, f"command timed out after {timeout:g}s", command=command)
    except OSError as error:
        return _error(name, f"cannot run command: {error}", command=command)

    stdout = done.stdout.strip()
    stderr = done.stderr.strip()
    parsed = None
    if stdout:
        try:
            parsed = json.loads(stdout)
        except ValueError:
            parsed = None
    if not isinstance(parsed, dict):
        return _error(name, f"command exited {done.returncode} without canonical JSON",
                      command=command, stderr=stderr[:1000], exit_code=done.returncode)
    parsed.setdefault("provider", name)
    parsed.setdefault("measured_at", time.time())
    parsed.setdefault("meaning", "unknown")
    parsed.setdefault("status", "error" if done.returncode else "unknown")
    parsed["exit_code"] = done.returncode
    if stderr and parsed.get("status") != "ok":
        parsed["stderr"] = stderr[:1000]
    for label, dotted in (spec.get("parse") or {}).items():
        value = _path(parsed, dotted)
        if value is _MISSING:
            # A successful reading must carry every parsed field. An
            # unknown/error result may legitimately omit value fields
            # (e.g. no `windows` for a stale snapshot); keep its reason.
            if parsed.get("status") == "ok":
                return _error(name, f"parse {label}: path {dotted!r} is missing",
                              command=command, stderr=stderr[:1000],
                              exit_code=done.returncode)
            continue
        parsed.setdefault("parsed", {})[label] = value
    return parsed


def run(config, only=None, max_age=None):
    providers = config["providers"]
    if only:
        if only not in providers:
            raise UsageError(f"unknown provider {only!r}; known: {', '.join(providers)}")
        names = [only]
    else:
        names = list(providers)
    return [run_provider(name, providers[name], max_age=max_age) for name in names]


def _windows(reading):
    parts = []
    for window in reading.get("windows") or []:
        used = window.get("percent_used")
        remaining = window.get("percent_remaining")
        reset = window.get("resets_at")
        text = f"{window.get('name')}: "
        if used is not None:
            text += f"{float(used):.1f}% used"
        if remaining is not None:
            text += f" / {float(remaining):.1f}% left"
        if reset:
            text += f" (resets {reset})"
        parts.append(text)
    return "; ".join(parts)


def render_text(results):
    lines = [f"{'PROVIDER':<10} {'STATUS':<8} {'MEANING':<22} READING"]
    for reading in results:
        status = reading.get("status", "error")
        if status == "ok":
            if reading.get("meaning") == "percent_used" and reading.get("windows"):
                body = _windows(reading)
            elif reading.get("meaning") == "local_session_estimate":
                body = (f"${reading.get('session_cost_usd')} session; "
                        f"{reading.get('last_context_tokens')} ctx tokens; "
                        f"{reading.get('session_messages_with_usage')} messages "
                        "(local; not plan quota)")
            else:
                body = json.dumps({k: v for k, v in reading.items()
                                   if k not in ("provider", "status", "meaning",
                                                "measured_at", "parsed")},
                                  sort_keys=True)[:220]
        elif status == "unknown":
            body = "UNKNOWN: " + (reading.get("reason") or reading.get("error")
                                  or "no reading available")
            snapshot = reading.get("last_snapshot") or {}
            if snapshot.get("percent_used") is not None:
                body += f" | last snapshot {snapshot['percent_used']}% used"
        else:
            body = "UNKNOWN: " + (reading.get("error") or "command failed")
            if reading.get("stderr"):
                body += f" | stderr: {reading['stderr'][:200]}"
        lines.append(f"{str(reading.get('provider')):<10} {status:<8} "
                     f"{str(reading.get('meaning')):<22} {body}")
    return "\n".join(lines)
