#!/usr/bin/env python3
"""Watch temporary storage and sustained host load for the VP desk."""

from __future__ import annotations

import argparse
import fcntl
import json
import os
import subprocess
import sys
import tempfile
import time
from datetime import datetime
from pathlib import Path
from zoneinfo import ZoneInfo


ROOT = Path(__file__).resolve().parent
STATE = ROOT / "data" / "host-watch-state.json"
LOCK = ROOT / "data" / "host-watch.lock"
GIB = 1024**3
LEVELS = {"ok": 0, "warning": 1, "critical": 2}


def cpu_pressure_avg60() -> float:
    try:
        some = Path("/proc/pressure/cpu").read_text().splitlines()[0]
        return float(next(part.split("=", 1)[1] for part in some.split() if part.startswith("avg60=")))
    except (OSError, ValueError, IndexError, StopIteration):
        return 0.0


def sample_host() -> dict:
    stat = os.statvfs("/tmp")
    block = stat.f_frsize
    total = stat.f_blocks * block
    free = stat.f_bavail * block
    used = (stat.f_blocks - stat.f_bfree) * block
    inode_used = stat.f_files - stat.f_ffree if stat.f_files else 0
    load1, load5, load15 = os.getloadavg()
    process_count = zombie_count = 0
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            process_state = (entry / "stat").read_text().rsplit(")", 1)[1].split()[0]
        except (OSError, IndexError):
            continue
        process_count += 1
        zombie_count += process_state == "Z"
    return {
        "at": time.time(),
        "tmp_total_bytes": total,
        "tmp_used_bytes": used,
        "tmp_free_bytes": free,
        "tmp_used_fraction": used / total if total else 1.0,
        "tmp_inode_used_fraction": inode_used / stat.f_files if stat.f_files else 0.0,
        "cpus": os.cpu_count() or 1,
        "load1": load1,
        "load5": load5,
        "load15": load15,
        "cpu_psi_some_avg60": cpu_pressure_avg60(),
        "process_count": process_count,
        "zombie_count": zombie_count,
    }


def tmp_level(sample: dict) -> str:
    if (sample["tmp_used_fraction"] >= 0.85 or sample["tmp_free_bytes"] <= 0.75 * GIB
            or sample["tmp_inode_used_fraction"] >= 0.90):
        return "critical"
    if (sample["tmp_used_fraction"] >= 0.60 or sample["tmp_free_bytes"] <= 2 * GIB
            or sample["tmp_inode_used_fraction"] >= 0.70):
        return "warning"
    return "ok"


def load_candidate(sample: dict) -> str:
    cpus = sample["cpus"]
    psi = sample["cpu_psi_some_avg60"]
    if sample["load1"] >= 2.5 * cpus or psi >= 60:
        return "critical"
    if sample["load5"] >= 1.5 * cpus or sample["load15"] >= 1.5 * cpus or psi >= 35:
        return "warning"
    return "ok"


def load_level(previous: str, history: list[dict]) -> str:
    """Require spaced samples for load alerts and recovery."""
    if len(history) >= 2 and history[-1]["at"] - history[-2]["at"] >= 45:
        if all(load_candidate(row) == "critical" for row in history[-2:]):
            return "critical"
    if len(history) >= 3 and history[-1]["at"] - history[-3]["at"] >= 90:
        if all(LEVELS[load_candidate(row)] >= 1 for row in history[-3:]):
            return "warning"
        if all(load_candidate(row) == "ok" for row in history[-3:]):
            return "ok"
    return previous


def append_sample(history: list[dict], sample: dict) -> list[dict]:
    history = history[-2:]
    if history and sample["at"] - history[-1]["at"] < 45:
        history[-1] = sample
    else:
        history.append(sample)
    return history


def alert_body(sample: dict, old: dict, new: dict) -> str:
    local = datetime.fromtimestamp(sample["at"], ZoneInfo("America/Los_Angeles"))
    stamp = local.strftime("%Y-%m-%d %H:%M:%S %Z")
    changes = ", ".join(f"{key} {old[key].upper()} -> {new[key].upper()}"
                        for key in ("tmp", "load") if old[key] != new[key])
    severity = max(new.values(), key=LEVELS.get).upper()
    return (
        f"HOST HEALTH {severity} | {stamp}\n"
        f"Change: {changes}\n"
        f"/tmp: {sample['tmp_used_bytes'] / GIB:.2f}/{sample['tmp_total_bytes'] / GIB:.2f} GiB "
        f"({sample['tmp_used_fraction']:.0%} used); {sample['tmp_free_bytes'] / GIB:.2f} GiB free; "
        f"inodes {sample['tmp_inode_used_fraction']:.0%} used.\n"
        f"Load 1/5/15m: {sample['load1']:.2f}/{sample['load5']:.2f}/{sample['load15']:.2f} "
        f"on {sample['cpus']} CPUs; CPU pressure 60s: {sample['cpu_psi_some_avg60']:.1f}%.\n"
        "VP will inspect active jobs and temporary files. No files were removed automatically."
    )


def save_state(state: dict) -> None:
    STATE.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix="host-watch-", dir=STATE.parent)
    try:
        with os.fdopen(fd, "w") as output:
            json.dump(state, output, separators=(",", ":"))
            output.write("\n")
        os.chmod(temporary, 0o600)
        os.replace(temporary, STATE)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


ALERT_COOLDOWN_S = 1200  # suppress non-escalating threshold flaps within 20 minutes


def run_once(dry_run: bool) -> int:
    sample = sample_host()
    STATE.parent.mkdir(parents=True, exist_ok=True)
    with LOCK.open("a+") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        state = json.loads(STATE.read_text()) if STATE.exists() else {}
        old = {"tmp": "ok", "load": "ok", **state.get("levels", {})}
        history = append_sample(state.get("history", []), sample)
        new = {"tmp": tmp_level(sample), "load": load_level(old["load"], history)}
        print(json.dumps({"sample": sample, "levels": new, "previous_levels": old}))
        if dry_run:
            return 0
        state.update({"last_sample": sample, "history": history})
        if new != old:
            severity = {"ok": 0, "warning": 1, "critical": 2}
            max_new = max(severity.get(v, 0) for v in new.values())
            recent = sample["at"] - state.get("last_alert_at", 0) < ALERT_COOLDOWN_S
            if recent and max_new < severity["critical"]:
                # A threshold oscillating around its boundary should not page the VP every
                # minute. Adopt the new levels (so the same transition is not re-detected)
                # and record the suppression for --status. Critical escalations always alert.
                state["levels"] = new
                state["last_suppressed_at"] = sample["at"]
                save_state(state)
                return 0
            body = alert_body(sample, old, new)
            try:
                subprocess.run([
                    "/home/hunter/workspace/dunkingsheep/dunkingsheep", "inbox", "send",
                    "π - vp-of-agentry", body, "--subject", "Host health threshold changed",
                ], capture_output=True, check=True, timeout=10)
            except (subprocess.CalledProcessError, subprocess.TimeoutExpired, OSError) as exc:
                # Keep the old levels so the next tick retries the same change, but do
                # not fail the systemd unit: a busy or briefly ambiguous mailbox must not
                # turn the minute timer red. The error is recorded and surfaced by --status.
                state["last_error"] = f"VP inbox alert failed: {exc}"
                save_state(state)
                print(state["last_error"], file=sys.stderr)
                return 0
            state["levels"] = new
            state["last_alert_at"] = sample["at"]
            state.pop("last_error", None)
        else:
            state.setdefault("levels", old)
        save_state(state)
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dry-run", action="store_true", help="sample without saving or alerting")
    parser.add_argument("--status", action="store_true", help="show last saved sample and alert state")
    args = parser.parse_args()
    if args.status:
        print(STATE.read_text() if STATE.exists() else "No samples yet")
        return 0
    return run_once(args.dry_run)


if __name__ == "__main__":
    raise SystemExit(main())
