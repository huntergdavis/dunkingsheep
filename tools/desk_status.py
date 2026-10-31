#!/usr/bin/env python3
"""Read-only status for the Front Desk: what the owner asks most, in plain lines.

The desk role is read-only and the daemon CAPS what it may list (a few dozen rows), so it could
not count 1,100 tasks or tell which are Rust, and it had no way to read Claude usage. This
reads the task database READ-ONLY (sqlite mode=ro, nothing is written, no daemon call) and
asks `dunkingsheep usage claude` for the provider windows. It prints facts only; a number that
cannot be read is printed as 'unknown', never guessed.

    tools/desk_status.py              everything below
    tools/desk_status.py rust         Rust tasks by status, in progress, next claimable
    tools/desk_status.py headroom     Claude five-hour and weekly usage
    tools/desk_status.py work         non-Rust tasks that are open or in progress
"""

import json
import os
import re
import sqlite3
import subprocess
import sys

CONFIG = os.path.expanduser(os.environ.get("DUNKINGSHEEP_DIR", "~/.config/dunkingsheep"))
DB = os.path.join(CONFIG, "tasks.db")
RUST_IDS = os.path.join(CONFIG, "rust_ids.txt")
RUST_TITLE = re.compile(r"Rust|\bT\d+\b|\.rs")


def rust_ids(path=RUST_IDS):
    try:
        with open(path, encoding="utf-8") as handle:
            return set(handle.read().split())
    except OSError:
        return set()


def load_tasks(path=DB):
    connection = sqlite3.connect(f"file:{path}?mode=ro", uri=True, timeout=5)
    connection.row_factory = sqlite3.Row
    try:
        return [dict(row) for row in connection.execute(
            "SELECT id, title, status, assignees, block_reason, depends_on FROM tasks")]
    finally:
        connection.close()


def is_rust(task, ids):
    return task["id"] in ids or bool(RUST_TITLE.search(task["title"]))


def claimants(task):
    try:
        return ", ".join(json.loads(task.get("assignees") or "[]")) or "unassigned"
    except ValueError:
        return "unassigned"


def rust_report(tasks, ids):
    rust = [t for t in tasks if is_rust(t, ids)]
    by = {}
    for task in rust:
        by[task["status"]] = by.get(task["status"], 0) + 1
    lines = [f"Rust tasks: {len(rust)} total - " +
             ", ".join(f"{n} {s}" for s, n in sorted(by.items(), key=lambda kv: -kv[1]))]
    remaining = sum(n for s, n in by.items() if s not in ("done", "cancelled"))
    lines.append(f"Rust tasks remaining (not done or cancelled): {remaining}")
    working = [t for t in rust if t["status"] == "in_progress"]
    if working:
        lines.append("In progress now: " + "; ".join(
            f"{t['id']} ({claimants(t)}) {t['title'][:60]}" for t in working))
    blocked = [t for t in rust if t["status"] == "blocked"]
    if blocked:
        lines.append(f"Blocked: {len(blocked)} (" + "; ".join(
            f"{t['id']}: {(t['block_reason'] or 'no reason')[:50]}" for t in blocked[:5]) + ")")
    return lines


def work_report(tasks, ids):
    live = [t for t in tasks if t["status"] in ("open", "in_progress") and not is_rust(t, ids)]
    if not live:
        return ["Non-Rust tasks open or in progress: none"]
    return [f"Non-Rust tasks open or in progress: {len(live)}"] + [
        f"  {t['id']} {t['status']} ({claimants(t)}) {t['title'][:70]}" for t in live[:15]]


def headroom_report(runner=None):
    try:
        out = (runner or (lambda: subprocess.run(
            ["dunkingsheep", "--no-start-daemon", "usage", "claude"], capture_output=True,
            text=True, timeout=30).stdout))()
    except Exception as error:  # noqa: BLE001 - say unknown, never guess
        return [f"Claude headroom: unknown ({type(error).__name__})"]
    five = re.search(r"five_hour: ([\d.]+)% used / ([\d.]+)% left \(resets ([^)]+)\)", out)
    week = re.search(r"weekly_all: ([\d.]+)% used / ([\d.]+)% left \(resets ([^)]+)\)", out)
    if not five and not week:
        return ["Claude headroom: unknown (the usage command returned no windows)"]
    lines = []
    if five:
        lines.append(f"Claude five-hour window: {five.group(1)}% used, {five.group(2)}% left "
                     f"(resets {five.group(3)[:16]}Z)")
    if week:
        lines.append(f"Claude weekly window: {week.group(1)}% used, {week.group(2)}% left "
                     f"(resets {week.group(3)[:16]}Z)")
    return lines


def main(argv=None):
    what = (argv or sys.argv[1:] or ["all"])[0]
    try:
        tasks = load_tasks()
    except sqlite3.Error as error:
        tasks, note = [], f"task database unreadable: {error}"
    else:
        note = None
    ids = rust_ids()
    out = []
    if what in ("all", "rust"):
        out += [note] if note else rust_report(tasks, ids)
    if what in ("all", "work"):
        out += [note] if note else work_report(tasks, ids)
    if what in ("all", "headroom"):
        out += headroom_report()
    if not out:
        print(__doc__.strip().splitlines()[0])
        return 2
    print("\n".join(out))
    return 0


if __name__ == "__main__":
    sys.exit(main())
