"""Per-caller CPU from /proc, with a pid+starttime guard and process trees.

Identity is the registered caller (pid + starttime). A recycled pid produces no
sample: the stored starttime must equal /proc/<pid>/stat field 22. CPU is the
caller's PROCESS TREE (the caller plus every descendant), found by walking the
ppid map, because a lane's work happens in children (a compile, a DOSBox run, a
free-model call). A child that reparents to init escapes the tree: that is the
stated limit, and the unattributed remainder is what makes it visible.

No subprocesses. One /proc stat walk and N small reads per interval.
"""

from __future__ import annotations

import json
import os
import time
from pathlib import Path

TICKS = os.sysconf("SC_CLK_TCK") if hasattr(os, "sysconf") else 100
STATE = Path(os.environ.get(
    "DUNKINGSHEEP_LOAD_STATE",
    Path(os.environ.get("DUNKINGSHEEP_DIR", str(Path.home() / ".config" / "dunkingsheep")))
    / "load-state.json"))


class LoadError(ValueError):
    """A named refusal for a bad sampler input."""


def _stat(pid):
    """Fields of /proc/<pid>/stat we need, or None when the process is gone.

    The comm may contain spaces and parentheses, so everything is parsed after
    the LAST ')'. Index 0 after that is field 3 (state).
    """
    try:
        raw = Path(f"/proc/{pid}/stat").read_text()
    except (OSError, ValueError):
        return None
    try:
        rest = raw.rsplit(")", 1)[1].split()
        return {
            "state": rest[0],
            "ppid": int(rest[1]),
            "utime": int(rest[11]),
            "stime": int(rest[12]),
            "starttime": int(rest[19]),
            "rss_pages": int(rest[21]),
        }
    except (IndexError, ValueError):
        return None


def _ppid_map():
    """One walk: pid -> ppid for every visible process."""
    out = {}
    try:
        entries = os.listdir("/proc")
    except OSError:
        return out
    for name in entries:
        if not name.isdigit():
            continue
        info = _stat(name)
        if info is not None:
            out[int(name)] = info["ppid"]
    return out


def tree_of(pid, ppids):
    """The caller plus descendants, via the ppid map. Sorted for stable output."""
    children = {}
    for child, parent in ppids.items():
        children.setdefault(parent, []).append(child)
    seen, stack = set(), [pid]
    while stack:
        current = stack.pop()
        if current in seen:
            continue
        seen.add(current)
        stack.extend(children.get(current, ()))
    return sorted(seen)


def tree_cpu(pid, ppids):
    """Summed utime+stime and RSS for the tree, or None if the root is gone."""
    members = tree_of(pid, ppids)
    root = _stat(pid)
    if root is None:
        return None
    total = 0
    rss = 0
    live = 0
    for member in members:
        info = _stat(member)
        if info is None:
            continue
        total += info["utime"] + info["stime"]
        rss += info["rss_pages"]
        live += 1
    return {"ticks": total, "rss_pages": rss, "process_count": live,
            "starttime": root["starttime"]}


def registered_callers(tasks):
    """[(name, pid, starttime)] from task leases that carry caller identity."""
    seen = {}
    for lease in tasks:
        pid = lease.get("caller_pid")
        start = lease.get("caller_start")
        name = lease.get("caller_name") or (f"pid {pid}" if pid else None)
        if not pid or start is None:
            continue
        seen[(pid, start)] = name
    return [(name, pid, start) for (pid, start), name in seen.items()]


def sample(callers, now=None):
    """One sample: per caller, CPU ticks now and the pid-reuse verdict.

    Returns {"at", "callers": [...], "skipped": [...]}. The caller dict carries
    cpu_ticks and the guard result; the delta is computed by the caller of this
    function against the previous sample.
    """
    now = now if now is not None else time.time()
    ppids = _ppid_map()
    rows, skipped = [], []
    for name, pid, start in callers:
        info = _stat(pid)
        if info is None:
            skipped.append({"name": name, "pid": pid, "reason": "process gone"})
            continue
        if info["starttime"] != start:
            skipped.append({"name": name, "pid": pid,
                            "reason": "pid reused (starttime differs)"})
            continue
        cpu = tree_cpu(pid, ppids)
        if cpu is None:
            skipped.append({"name": name, "pid": pid, "reason": "process gone"})
            continue
        rows.append({"name": name, "pid": pid, "starttime": start,
                     "cpu_ticks": cpu["ticks"], "rss_pages": cpu["rss_pages"],
                     "process_count": cpu["process_count"]})
    return {"at": now, "callers": rows, "skipped": skipped}


def gpu():
    """Per-process GPU here is a NULL with a reason, never a fabricated number.

    The host is an Intel UHD 620 (Kaby Lake-R) with no nvidia-smi, so there is
    no per-process GPU counter. DRM fdinfo may expose drm-engine-* lines; we
    report what i915 actually gives rather than inventing a percentage.
    """
    try:
        for entry in os.listdir("/proc"):
            if not entry.isdigit():
                continue
            fdinfo = f"/proc/{entry}/fdinfo"
            if not os.path.isdir(fdinfo):
                continue
            for fd in os.listdir(fdinfo):
                try:
                    text = Path(f"{fdinfo}/{fd}").read_text(errors="replace")
                except OSError:
                    continue
                if "drm-engine-" in text:
                    return {"value": None, "reason": "drm fdinfo exposes engine time, "
                                                     "not a per-process utilisation percentage"}
    except OSError:
        pass
    return {"value": None, "reason": "no per-process GPU counter on this hardware (Intel UHD 620)"}


def _load_state():
    try:
        return json.loads(STATE.read_text())
    except (OSError, ValueError):
        return {}


def _save_state(state):
    STATE.parent.mkdir(parents=True, exist_ok=True)
    temporary = STATE.with_suffix(".json.tmp")
    temporary.write_text(json.dumps(state))
    os.replace(temporary, STATE)


def collect(callers, now=None):
    """Sample and turn ticks into a window percentage of one core."""
    now = now if now is not None else time.time()
    state = _load_state()
    previous = state.get("last") or {}
    previous_at = state.get("at")
    current = sample(callers, now=now)
    rows = []
    elapsed = (now - previous_at) if previous_at else None
    for row in current["callers"]:
        key = f"{row['pid']}:{row['starttime']}"
        before = (previous.get("callers") or {}).get(key)
        pct = None
        if before is not None and elapsed and elapsed > 0:
            delta = row["cpu_ticks"] - int(before.get("cpu_ticks") or 0)
            if delta >= 0:
                pct = round(100.0 * delta / (elapsed * TICKS), 1)
        peak = max([pct or 0.0, float(before.get("peak_cpu_pct") or 0.0)]) if before else (pct or 0.0)
        rows.append({
            "name": row["name"], "pid": row["pid"],
            "cpu_pct": pct, "peak_cpu_pct": peak,
            "rss_mb": round(row["rss_pages"] * os.sysconf("SC_PAGE_SIZE") / 2**20, 1),
            "process_count": row["process_count"],
            "gpu_pct": gpu(),
        })
    state_now = {"at": now, "callers": {f"{r['pid']}:{r['starttime']}": {
        "cpu_ticks": r["cpu_ticks"],
        "peak_cpu_pct": next((x["peak_cpu_pct"] for x in rows
                              if x["pid"] == r["pid"]), 0.0)} for r in current["callers"]}}
    _save_state(state_now)
    # The honest remainder: host load minus what we attributed (in cores).
    try:
        load1 = os.getloadavg()[0]
    except OSError:
        load1 = None
    attributed = round(sum((row["cpu_pct"] or 0.0) for row in rows) / 100.0, 2)
    unattributed = round(max(0.0, (load1 or 0.0) - attributed), 2) if load1 is not None else None
    return {"at": now, "elapsed_s": round(elapsed, 1) if elapsed else None,
            "callers": rows, "skipped": current["skipped"],
            "host_load1": load1, "attributed_cores": attributed,
            "unattributed_cores": unattributed}
