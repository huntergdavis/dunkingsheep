#!/usr/bin/env python3
"""The reporter: a deterministic status report, no model (FLAT_FLEET.md 7.3).

The VP used to write this by hand, which is why the fleet spent a week building
machinery for a job that is a SQL query and a template. Every number here is
measured at the moment it is read and printed verbatim; the optional prose pass
is *below* the numbers and is not allowed to say anything they do not contain.

Sections, in the order the owner reads them:

    task metrics (24h) | lanes working/idle/dead | needs-owner | review queue
    with ages | poolrad progress | budget | watchdog

A section whose source is unavailable says so ("not measured here") instead of
printing a zero: a zero is a measurement, and this report exists because a
number that was not measured kept being read as one that was.

Where it goes: `~/.config/dunkingsheep/reports/<ts>.md`, and — once the owner
mailbox has a `report` transport (7.2) — a row in that mailbox too, which the
web shows and Telegram does not.
"""

from __future__ import annotations

import json
import os
import re
import time

CONFIG_DIR = os.environ.get("DUNKINGSHEEP_DIR") or os.path.expanduser(
    "~/.config/dunkingsheep")
REPORT_DIR = os.path.join(CONFIG_DIR, "reports")
OWNER_MAILBOX = "owner"
REPORT_TRANSPORT = "report"
WINDOW_S = 24 * 3600

# A task id or a task status in the prose must exist in the report's input: a
# summariser that rounds "merged" up to "done" is the defect this gate exists
# for (FLAT_FLEET.md 7.3).
TASK_ID = re.compile(r"\bt\d{1,5}\b")
STATUSES = ("open", "in_progress", "blocked", "done", "cancelled")


def _short(path):
    return str(path).replace(os.path.expanduser("~"), "~")


def _iso(at):
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(at))


def _age(seconds):
    if seconds is None:
        return "-"
    seconds = int(seconds)
    if seconds < 90:
        return f"{seconds}s"
    if seconds < 5400:
        return f"{seconds // 60}m"
    if seconds < 172800:
        return f"{seconds // 3600}h"
    return f"{seconds // 86400}d"


def task_section(flock, now):
    try:
        metrics = flock.tasks.metrics(window_s=WINDOW_S)
    except Exception as error:  # noqa: BLE001 - a section never kills the report
        return {"error": f"task metrics unavailable: {error}"}
    window = metrics.get("window") or {}
    throughput = metrics.get("throughput") or {}
    # The store's own shape: `current` is the board now, `throughput` the window's
    # flow, `latency_ms` the durations. Read them, never guess a key: the first
    # draft of this section asked for `by_status`/`counts`, which do not exist, and
    # printed "none" beside three Nones for a fleet with work in it.
    current = metrics.get("current") or {}
    return {
        "window_hours": round((window.get("seconds") or WINDOW_S) / 3600, 1),
        "current_status": current.get("by_status") or {},
        "current_priority": current.get("by_priority") or {},
        "flow": {key: throughput.get(key) for key in
                 ("created", "claimed", "completed", "cancelled", "blocked")},
        "latency_ms": metrics.get("latency_ms") or {},
        "coverage": metrics.get("coverage"),
    }


def lanes_section(flock, now):
    """Working / idle / dead, from live panes and the leases they hold."""
    try:
        panes = flock.herdr.list_panes() or []
    except Exception as error:  # noqa: BLE001
        return {"error": f"pane list unavailable: {error}"}
    live = {str(pane.get("pane_id")): pane for pane in panes if pane.get("pane_id")}
    working, idle = {}, {}
    try:
        tasks = flock.tasks.list(scope=None, include_done=False, limit=500)
    except Exception:  # noqa: BLE001
        tasks = []
    for task in tasks:
        for lease in task.get("leases") or []:
            working.setdefault(str(lease.get("pane_id")), []).append(task["id"])
    for pane_id in live:
        if pane_id in working:
            continue
        idle[pane_id] = live[pane_id].get("label") or live[pane_id].get("agent") or "-"
    dead = []
    try:
        registry = flock.roles.describe().get("lanes") or {}
    except Exception:  # noqa: BLE001
        registry = {}
    for pane_id, entry in registry.items():
        if pane_id not in live:
            dead.append({"pane": pane_id, "label": entry.get("label"),
                         "role": entry.get("role")})
    return {"working": [{"pane": p, "tasks": t} for p, t in sorted(working.items())],
            "idle": [{"pane": p, "label": l} for p, l in sorted(idle.items())],
            "dead": dead,
            "live": len(live)}


def needs_owner_section(flock, now):
    try:
        result = flock.task_list(needs_owner=True, include_done=False, limit=50)
    except TypeError:            # older signature
        result = flock.task_list(limit=50)
    rows = []
    for task in (result.get("tasks") or []):
        reason = str(task.get("block_reason") or "")
        if not reason.startswith("needs-owner:"):
            continue
        rows.append({"id": task["id"], "priority": task.get("priority"),
                     "age_s": round(now - task.get("created_at", now)),
                     "question": reason.split(":", 1)[1].strip()})
    return {"questions": rows}


def review_section(flock, now):
    try:
        result = flock.review_list()
    except Exception as error:  # noqa: BLE001
        return {"error": f"review queue unavailable: {error}"}
    rows = []
    for row in (result.get("reviews") or [])[:20]:
        rows.append({"task": row.get("task"), "repo": row.get("repo"),
                     "pr": row.get("pr"), "head": (row.get("head") or "")[:12],
                     "age_s": row.get("age_s"), "reviewer": row.get("reviewer")})
    return {"reviews": rows, "count": result.get("count"),
            "oldest_age_s": result.get("oldest_age_s")}


def poolrad_section(flock, now):
    try:
        return flock.poolrad_progress()
    except Exception as error:  # noqa: BLE001
        return {"error": f"poolrad progress unavailable: {error}"}


def budget_section(now):
    """Provider budget, when the bridges' usage cache can be read.

    Not measured is a valid answer: a report that invents $0.00 is worse than one
    that says it does not know.
    """
    path = os.path.join(CONFIG_DIR, "usage.json")
    try:
        with open(path, encoding="utf-8") as handle:
            data = json.load(handle)
    except (OSError, ValueError):
        return {"error": f"no usage cache at {_short(path)}: budget not measured here"}
    providers = []
    for name, entry in sorted((data.get("providers") or data).items()):
        if not isinstance(entry, dict):
            continue
        providers.append({"provider": name,
                          "used": entry.get("used") or entry.get("spent"),
                          "limit": entry.get("limit") or entry.get("budget"),
                          "window": entry.get("window")})
    return {"providers": providers, "as_of": data.get("as_of")}


def watchdog_section(now):
    path = os.path.join(CONFIG_DIR, "watchdog_state.json")
    try:
        with open(path, encoding="utf-8") as handle:
            data = json.load(handle)
    except (OSError, ValueError):
        return {"error": f"no watchdog state at {_short(path)}"}
    return {"state": {key: data[key] for key in sorted(data) if key != "history"},
            "heartbeat_age_s": (round(now - data["heartbeat_at"])
                                if isinstance(data.get("heartbeat_at"), (int, float))
                                else None)}


def collect(flock, now=None):
    now = float(now or time.time())
    return {"generated_at": now, "iso": _iso(now),
            "sections": {"tasks": task_section(flock, now),
                         "lanes": lanes_section(flock, now),
                         "needs_owner": needs_owner_section(flock, now),
                         "reviews": review_section(flock, now),
                         "poolrad": poolrad_section(flock, now),
                         "budget": budget_section(now),
                         "watchdog": watchdog_section(now)}}


def _counts_line(mapping, order=None):
    if not mapping:
        return "none"
    keys = order or sorted(mapping)
    return ", ".join(f"{key} {mapping[key]}" for key in keys if mapping.get(key))


def render(report):
    """The deterministic block. Nothing here is prose and nothing is inferred."""
    sections = report["sections"]
    lines = [f"# Fleet report {report['iso']}", "",
             "Deterministic block: every number below was read from the stores at "
             "generation time. A section that could not be measured says so.", ""]
    tasks = sections["tasks"]
    lines.append(f"## Task metrics (last {tasks.get('window_hours', 24)}h)")
    if tasks.get("error"):
        lines.append(f"- {tasks['error']}")
    else:
        lines.append(f"- board now, by status: "
                     f"{_counts_line(tasks.get('current_status'))}")
        lines.append(f"- board now, by priority: "
                     f"{_counts_line(tasks.get('current_priority'))}")
        flow = tasks.get("flow") or {}
        lines.append("- in the window: " + ", ".join(
            f"{key} {flow[key]}" for key in ("created", "claimed", "completed",
                                             "cancelled", "blocked")
            if flow.get(key) is not None))
        for name, stats in sorted((tasks.get("latency_ms") or {}).items()):
            if isinstance(stats, dict) and stats.get("count"):
                lines.append(f"- {name} latency: p50 {stats.get('p50')}ms, "
                             f"p90 {stats.get('p90')}ms over {stats['count']}")
    lanes = sections["lanes"]
    lines += ["", "## Lanes"]
    if lanes.get("error"):
        lines.append(f"- {lanes['error']}")
    else:
        lines.append(f"- live panes {lanes.get('live')}")
        for row in lanes.get("working") or []:
            lines.append(f"- working: {row['pane']} on {', '.join(row['tasks'])}")
        for row in lanes.get("idle") or []:
            lines.append(f"- idle: {row['pane']} ({row['label']})")
        for row in lanes.get("dead") or []:
            lines.append(f"- dead: {row['pane']} ({row.get('label')}, "
                         f"{row.get('role')}) — registered, no live pane")
    lines += ["", "## Needs the owner"]
    questions = (sections["needs_owner"] or {}).get("questions") or []
    if not questions:
        lines.append("- none")
    for row in questions:
        lines.append(f"- {row['id']} [{row['priority']}] waiting {_age(row['age_s'])}: "
                     f"{row['question'][:200]}")
    reviews = sections["reviews"]
    lines += ["", "## Review queue"]
    if reviews.get("error"):
        lines.append(f"- {reviews['error']}")
    elif not reviews.get("reviews"):
        lines.append("- empty")
    else:
        lines.append(f"- {reviews.get('count')} open, oldest "
                     f"{_age(reviews.get('oldest_age_s'))}")
        for row in reviews["reviews"]:
            lines.append(f"- {row['task']} {row['repo']}#{row['pr']} @{row['head']} "
                         f"age {_age(row['age_s'])} reviewer "
                         f"{', '.join(row['reviewer'] or []) or 'unclaimed'}")
    poolrad = sections["poolrad"]
    lines += ["", "## Poolrad"]
    if poolrad.get("error"):
        lines.append(f"- {poolrad['error']}")
    else:
        lines.append(f"- credited strict bodies {poolrad.get('credited')}, "
                     f"identities {poolrad.get('identities')}, "
                     f"main {poolrad.get('main')}")
        if poolrad.get("ceiling_note"):
            lines.append(f"- {poolrad['ceiling_note']}")
        if poolrad.get("replaced_note"):
            lines.append(f"- {poolrad['replaced_note']}")
    budget = sections["budget"]
    lines += ["", "## Budget"]
    if budget.get("error"):
        lines.append(f"- {budget['error']}")
    elif not budget.get("providers"):
        lines.append("- no providers in the usage cache")
    for row in budget.get("providers") or []:
        lines.append(f"- {row['provider']}: used {row['used']} of {row['limit']}"
                     f" ({row['window'] or 'window unknown'})")
    watchdog = sections["watchdog"]
    lines += ["", "## Watchdog"]
    if watchdog.get("error"):
        lines.append(f"- {watchdog['error']}")
    else:
        lines.append(f"- heartbeat age {_age(watchdog.get('heartbeat_age_s'))}")
        for key, value in (watchdog.get("state") or {}).items():
            lines.append(f"- {key}: {value}")
    return "\n".join(lines) + "\n"


def prose_problems(prose, report):
    """Why `prose` may not be posted, or an empty list.

    The rule is narrow on purpose: a summariser may not name a task the report
    does not mention, and may not claim a status the report does not report. It
    can say anything else, because the deterministic block above it is what the
    owner reads first.
    """
    text = prose or ""
    known_ids = set()
    known_status = set()
    for row in (report["sections"]["reviews"].get("reviews") or []):
        if row.get("task"):
            known_ids.add(str(row["task"]))
    for row in ((report["sections"]["needs_owner"] or {}).get("questions") or []):
        known_ids.add(str(row["id"]))
    for task in (report["sections"]["lanes"].get("working") or []):
        known_ids.update(str(t) for t in task.get("tasks") or [])
    for status, count in (report["sections"]["tasks"].get("current_status") or {}).items():
        if count:
            known_status.add(status)
    problems = []
    for match in sorted(set(TASK_ID.findall(text))):
        if match not in known_ids:
            problems.append(f"prose names {match}, which this report does not mention")
    for status in STATUSES:
        if re.search(rf"\b{status}\b", text) and status not in known_status:
            problems.append(f"prose says {status!r}, which this report does not report")
    return problems


def write(report, text, out_dir=None):
    out_dir = out_dir or REPORT_DIR
    os.makedirs(out_dir, exist_ok=True)
    stamp = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime(report["generated_at"]))
    path = os.path.join(out_dir, f"{stamp}.md")
    # Never overwrite a report: two runs in one second are two reports, and the
    # second one silently replacing the first is how a record stops being one.
    suffix = 2
    while os.path.exists(path):
        path = os.path.join(out_dir, f"{stamp}-{suffix}.md")
        suffix += 1
    with open(path, "w", encoding="utf-8") as handle:
        handle.write(text)
    return path


def list_reports(out_dir=None):
    out_dir = out_dir or REPORT_DIR
    try:
        names = sorted(n for n in os.listdir(out_dir) if n.endswith(".md"))
    except OSError:
        return {"dir": _short(out_dir), "reports": [], "count": 0}
    reports = [{"name": name, "path": os.path.join(out_dir, name),
                "size": os.path.getsize(os.path.join(out_dir, name)),
                "at": os.path.getmtime(os.path.join(out_dir, name))}
               for name in names]
    reports.sort(key=lambda row: row["at"], reverse=True)
    return {"dir": _short(out_dir), "reports": reports, "count": len(reports)}


def run(flock, now=None, out_dir=None, post=True, prose=None, post_fn=None):
    """Generate, write, and (when the owner mailbox exists) post one report."""
    report = collect(flock, now=now)
    text = render(report)
    problems = prose_problems(prose, report) if prose else []
    if prose and not problems:
        text = text + "\n## In plain language\n\n" + prose.strip() + "\n"
    path = write(report, text, out_dir=out_dir)
    posted = None
    if post and post_fn is not None:
        try:
            posted = post_fn(report, text)
        except Exception as error:  # noqa: BLE001 - a failed post keeps the file
            posted = {"error": str(error)}
    return {"path": path, "text": text, "report": report,
            "prose_rejected": problems, "posted": posted}
