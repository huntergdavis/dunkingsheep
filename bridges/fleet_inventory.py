"""Read-only Linux agent-process coverage beyond the desk's pane roster.

Never read session contents, send signals, or expose full command lines/environment.
An open session file's mtime is evidence of a write, not proof of useful work.
"""
import os
import subprocess
import time
from pathlib import Path

HARNESSES = {"codex", "claude", "pi", "opencode"}


def harness(args, executable):
    names = [Path(arg).name for arg in args[:2]]
    if "codex" in names and len(args) > 1 and args[1] in {"app-server", "mcp-server"}:
        return None  # service processes are not interactive agent lanes
    for name in (Path(executable).name, *names):
        if name in HARNESSES:
            return name
    if len(args) > 1 and "pi-coding-agent" in args[1] and args[1].endswith("/cli.js"):
        return "pi"
    return None


def process_rows(proc=Path("/proc")):
    rows = {}
    for path in proc.iterdir():
        if not path.name.isdigit():
            continue
        try:
            if path.stat().st_uid != os.getuid():
                continue
            stat = (path / "stat").read_text().rsplit(")", 1)[1].split()
            if stat[0] == "Z":
                continue
            args = (path / "cmdline").read_bytes().decode(errors="replace").split("\0")
            agent = harness(args, os.readlink(path / "exe"))
            row = {"pid": int(path.name), "ppid": int(stat[1]), "agent": agent}
            if agent:
                row["cwd"] = os.readlink(path / "cwd")
            rows[row["pid"]] = row
        except (OSError, ValueError, IndexError):
            continue  # process exited or belongs to an inaccessible namespace
    return rows


def session_write(pid, agent, *, proc=Path("/proc"), home=Path.home()):
    root = {"codex": home / ".codex/sessions", "claude": home / ".claude/projects",
            "pi": home / ".pi/agent/sessions"}.get(agent)
    if root is None:
        return None
    try:
        paths = set()
        for fd in (proc / str(pid) / "fd").iterdir():
            try:
                target = Path(os.readlink(fd))
                if target.suffix == ".jsonl" and target.is_relative_to(root):
                    paths.add(target)
            except OSError:
                continue
        # Ambiguous sessions are unknown, rather than attributing another lane's writes.
        return next(iter(paths)).stat().st_mtime if len(paths) == 1 else None
    except OSError:
        return None


def unrouted(processes, pane_pids, routed_ids):
    rows = []
    for pid, process in processes.items():
        if not process.get("agent"):
            continue
        current, seen, nested, pane = pid, set(), False, None
        while current in processes and current not in seen:
            seen.add(current)
            if current != pid and processes[current].get("agent"):
                nested = True
            if current in pane_pids:
                pane = pane_pids[current]
                break
            current = processes[current]["ppid"]
        if pane in routed_ids and not nested:
            continue
        reason = ("Agent process has no independent pane route" if nested else
                  "Pane is absent from the desk's agent roster" if pane else
                  "No pane route found for this process")
        rows.append({**process, "pane_id": pane, "reason": reason,
                     "project": Path(process.get("cwd") or "unknown").name})
    return rows


def collect(routed_ids):
    try:
        result = subprocess.run(["tmux", "list-panes", "-a", "-F", "#{pane_id} #{pane_pid}"],
                                capture_output=True, text=True, timeout=5, check=True)
        pane_pids = {int(line.split()[1]): line.split()[0] for line in result.stdout.splitlines()}
        processes = process_rows()
        rows = unrouted(processes, pane_pids, set(routed_ids))
        now = time.time()
        for row in rows:
            stamp = session_write(row["pid"], row["agent"])
            row["last_session_write_s"] = max(0, int(now-stamp)) if stamp is not None else None
            row["session_age_reason"] = None if stamp is not None else "No unambiguous open session file"
            row.pop("ppid", None)
        return {"state": "ok", "rows": sorted(rows, key=lambda row: (row["project"], row["pid"])),
                "source": "local process metadata and tmux pane ancestry; session-file mtime only",
                "note": "Detected process presence is not proof of active work; unrecognized harnesses are not counted."}
    except (OSError, ValueError, IndexError, subprocess.SubprocessError):
        return {"state": "unavailable", "rows": [], "reason": "Process or pane inventory unavailable"}
