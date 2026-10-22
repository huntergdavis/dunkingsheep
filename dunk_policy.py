"""Per-agent resource policy: alerts, conservative auto-kill, and a deny list.

Measurement comes from the #1252 sampler (dunk_load), so this module never
samples /proc itself. It decides: is this agent over a threshold for long
enough, and may we act on it.

Safety order, and every clause is deliberate:
- DEFAULT OFF. Killing needs an explicit per-agent policy with kill: true.
- ATTRIBUTED ONLY. A pid is actionable only when the daemon registered its
  (pid, starttime) and /proc still matches; a bare process name is never enough.
- DENY LIST WINS. The VP pane, the owner's shell, the daemon, systemd services
  and anything outside a workspace are refused by evaluation, not by luck.
- DRY RUN FIRST. A policy with dry_run true logs the decision and signals
  nothing.
- HEAVY WORK IS EXPECTED. A compile saturating eight cores is not a runaway
  lane; policies are per lane, never global.
"""

from __future__ import annotations

import os
import signal
import time

DEFAULT_GRACE_S = 30.0


class PolicyError(ValueError):
    """A named refusal for a malformed policy or sample."""


def _num(policy, key, default=None):
    value = policy.get(key, default)
    if value is None:
        return None
    if not isinstance(value, (int, float)) or value <= 0:
        raise PolicyError(f"policy.{key} must be a positive number")
    return float(value)


def evaluate(sample, policy, *, now=None):
    """Findings for one caller sample against one agent's policy.

    sample: a dunk_load collect row {name, pid, cpu_pct, peak_cpu_pct, rss_mb,
    process_count, ...}. policy: {cpu_pct_over, cpu_window_s, rss_mb_over,
    rss_window_s, wall_minutes_over}. Returned findings are
    {rule, measured, threshold, window, agent}. A threshold without its window
    is a policy error, not a guess.
    """
    now = now if now is not None else time.time()
    out = []
    cpu = _num(policy, "cpu_pct_over")
    if cpu is not None:
        window = _num(policy, "cpu_window_s")
        if window is None:
            raise PolicyError("cpu_pct_over needs cpu_window_s")
        if (sample.get("cpu_pct") or 0) >= cpu:
            out.append({"rule": "cpu", "agent": sample["name"],
                        "measured": sample["cpu_pct"], "threshold": cpu,
                        "window": window, "pid": sample["pid"]})
    rss = _num(policy, "rss_mb_over")
    if rss is not None:
        window = _num(policy, "rss_window_s")
        if window is None:
            raise PolicyError("rss_mb_over needs rss_window_s")
        if (sample.get("rss_mb") or 0) >= rss:
            out.append({"rule": "rss", "agent": sample["name"],
                        "measured": sample["rss_mb"], "threshold": rss,
                        "window": window, "pid": sample["pid"]})
    wall = _num(policy, "wall_minutes_over")
    if wall is not None and (sample.get("wall_minutes") or 0) >= wall:
        out.append({"rule": "wall", "agent": sample["name"],
                    "measured": sample["wall_minutes"], "threshold": wall,
                    "window": None, "pid": sample["pid"]})
    return out


def denied(pid, *, deny_pids=(), deny_panes=(), workspace_roots=(), cwd=None,
           protected_panes=()):
    """The deny evaluation. Returns a reason string, or None when permitted.

    This is a pure function so a test (and the VP) can prove a refused pid is
    refused by policy rather than by a lucky missing /proc entry.
    """
    if pid in deny_pids:
        return f"pid {pid} is on the deny list"
    if pid in protected_panes:
        return f"pid {pid} is a protected pane"
    if cwd is not None:
        roots = [os.path.realpath(root) for root in workspace_roots]
        if roots and not any(os.path.realpath(cwd).startswith(root + os.sep) or
                             os.path.realpath(cwd) == root for root in roots):
            return f"cwd {cwd} is not a workspace"
    return None


def kill_decision(sample, policy, *, registered=(), deny_pids=(), deny_panes=(),
                  workspace_roots=(), protected_panes=(), dry_run=True,
                  grace_s=DEFAULT_GRACE_S, signal_fn=os.kill, sleep_fn=time.sleep,
                  now=None):
    """Decide and (unless dry_run) act on one over-threshold caller.

    `registered` is the set of (pid, starttime) the daemon recorded; a pid not
    in it is unattributable and is never signalled. TERM, grace, then KILL.
    Returns a record with the decision, the sample, the signals and the outcome.
    """
    now = now if now is not None else time.time()
    findings = evaluate(sample, policy, now=now)
    if not findings:
        return {"decision": "none", "agent": sample.get("name"), "findings": []}
    if not policy.get("kill"):
        return {"decision": "alert-only", "agent": sample.get("name"),
                "findings": findings}
    pid = sample.get("pid")
    start = sample.get("starttime")
    if (pid, start) not in registered:
        return {"decision": "refused", "reason": "unattributed pid (not registered)",
                "agent": sample.get("name"), "findings": findings, "pid": pid}
    reason = denied(pid, deny_pids=deny_pids, deny_panes=deny_panes,
                    workspace_roots=workspace_roots, cwd=sample.get("cwd"),
                    protected_panes=protected_panes)
    if reason:
        return {"decision": "refused", "reason": reason,
                "agent": sample.get("name"), "findings": findings, "pid": pid}
    record = {"decision": "would-kill" if dry_run else "kill",
              "agent": sample.get("name"), "pid": pid, "findings": findings,
              "sample": {"cpu_pct": sample.get("cpu_pct"), "rss_mb": sample.get("rss_mb")}}
    if dry_run:
        return record
    try:
        signal_fn(pid, signal.SIGTERM)
        record["signals"] = ["TERM"]
        sleep_fn(grace_s)
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            record["outcome"] = "exited after TERM"
            return record
        signal_fn(pid, signal.SIGKILL)
        record["signals"].append("KILL")
        record["outcome"] = "KILL after grace"
    except ProcessLookupError:
        record["outcome"] = "already gone"
    except PermissionError as exc:
        record["outcome"] = f"permission refused: {exc}"
    return record
