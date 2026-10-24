"""Read-only compositions for the Command Center: roster, message state, project summary.

Three consumers (COS chat, the project page, the agent page) each need a shape that
no single store holds, and each shape is assembled here from deterministic reads.
No model, no inference about a pane that was not read, and every number is a cell
{value, as_of, reason} so a missing fact is never a zero.
"""
import os
import time

import dunk_context

CONFIG_DIR = os.environ.get("DUNKINGSHEEP_DIR", os.path.expanduser("~/.config/dunkingsheep"))

# What each harness reports about its model. Unknown is null WITH a reason: a chat UI
# that prints "unknown" as if it were a model name is worse than a gap.
MODEL_SOURCES = {
    "claude": ("claude agents --json", "the session list carries no model field"),
    "codex": ("codex config", "codex does not report its model on the pane record"),
    "pi": ("pi session", "pi does not report its model on the pane record"),
}


def cell(value, as_of=None, reason=None):
    return {"value": value, "as_of": as_of, "reason": reason}


def _iso(at):
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(at)) if at else None


def roster(flock, now=None):
    """One row per lane: identity, state, current task, inbox, cost, cpu.

    No screen reads: this is polled by a UI, and a roster that costs a pane read per
    lane is a roster that times out (the dispatcher taught us that today). `detail`
    states what each field's source is; an unknown field carries a reason.
    """
    now = now or time.time()
    # A bare shell is not an agent: it has no harness and no mail.
    lanes = [lane for lane in flock.resolve_lanes() if lane.get("agent")]
    rows = []
    for lane in lanes:
        pane_id = lane["pane_id"]
        status = (lane.get("agent_status") or "unknown")
        leases = []
        try:
            leases = flock.tasks.leases_for_pane(pane_id, now=now)
        except Exception:                       # noqa: BLE001 - a read must not fail the row
            leases = []
        task = None
        if leases:
            try:
                task_row = flock.tasks.get(leases[0]["task_id"])
                task = {"id": task_row["id"], "title": task_row["title"],
                        "since": _iso(leases[0]["claimed_at"])}
            except Exception:                   # noqa: BLE001
                task = {"id": leases[0]["task_id"], "title": None, "since": None}
        unread, oldest = 0, None
        try:
            box = flock.inbox.count(lane.get("agent_name") or pane_id)
            unread = box.get("unread") or 0
            oldest = box.get("oldest_unread_s")
        except Exception:                       # noqa: BLE001
            unread, oldest = 0, None
        harness = lane.get("agent")
        model_reason = MODEL_SOURCES.get(str(harness or "").lower(), (None, "unknown harness"))
        rows.append({
            "pane_id": pane_id,                       # internal; the UI drops it
            "name": lane.get("agent_name") or pane_id,
            "harness": harness,
            "model": cell(None, None, model_reason[1] if harness else "unknown harness"),
            "state": status,
            "since": cell(None, None, "the pane record carries a status, not when it changed"),
            "task": task,
            "inbox": {"unread": unread,
                      "oldest_unread_s": cell(oldest, None,
                                              None if oldest is not None else "no unread mail")},
            "cost": cell(None, None, "cost is metered per provider, not per lane"),
            "cpu": _cpu_for(flock, leases, now),
        })
    return {"as_of": _iso(now), "lanes": rows}


def _cpu_for(flock, leases, now):
    """CPU for a lane's caller pid, from /proc, or null with a reason."""
    caller = None
    for lease in leases:
        if lease.get("caller_pid"):
            caller = lease
            break
    if caller is None:
        return cell(None, None, "no caller pid on a lease for this lane")
    try:
        import dunk_load
        row = dunk_load.sample([{"caller_pid": caller["caller_pid"],
                                 "caller_start": caller.get("caller_start"),
                                 "caller_name": caller.get("caller_name")}], now=now)
        entry = (row.get("callers") or [None])[0]
        if not entry:
            return cell(None, None, "the caller pid is not running")
        return cell({"cpu_pct": entry.get("cpu_pct"), "rss_mb": entry.get("rss_mb"),
                     "process_count": entry.get("process_count")}, _iso(now), None)
    except Exception as error:                  # noqa: BLE001
        return cell(None, None, f"cpu sample failed: {error}")


def message_state(flock, message_ids, now=None):
    """sent / delivered / seen / replied for owner messages, with timestamps.

    'delivered' is the notice landing in the pane (notify_state), 'seen' is the
    recipient reading it (read_at), 'replied' needs an answer link, which this store
    does not carry - it is the COS/VP application's record, so it is reported as
    null WITH that reason rather than guessed from a later message.
    """
    now = now or time.time()
    out = []
    for message_id in message_ids:
        try:
            row = flock.inbox.get(message_id)
        except Exception:                       # noqa: BLE001
            out.append({"id": message_id, "found": False})
            continue
        if not row:
            out.append({"id": message_id, "found": False})
            continue
        created = row.get("created_at") or now
        out.append({
            "id": row["id"], "found": True,
            "mailbox": row.get("mailbox"),
            "sent": {"at": _iso(created), "by": row.get("sender")},
            "delivered": {"at": _iso(row.get("notified_at")) if row.get("notified_at") else None,
                          "state": row.get("notify_state"),
                          "reason": row.get("notify_reason")},
            "seen": {"at": _iso(row.get("read_at")) if row.get("read_at") else None,
                     "acked_at": _iso(row.get("acked_at")) if row.get("acked_at") else None},
            "replied": cell(None, None, "the answer link lives in the COS/VP application, not this store"),
            "state": row.get("state"),
            "age_s": int(now - created),
        })
    return {"as_of": _iso(now), "messages": out}


def reply_stats(flock, mailboxes=None, now=None):
    """Time from a message landing to the lane reading it: last and median.

    This is the measurable half of 'reply': how long mail sat before a lane saw it.
    A true reply needs an answer link (null with a reason, as above).
    """
    now = now or time.time()
    rows = flock.inbox.stats() if hasattr(flock.inbox, "stats") else []
    wanted = set(mailboxes or [])
    out = []
    for row in rows:
        mailbox = row.get("mailbox")
        if wanted and mailbox not in wanted:
            continue
        latencies = _seen_latencies(flock.inbox, mailbox)
        out.append({
            "mailbox": mailbox,
            "seen_latency_s": cell(_median(latencies), _iso(now),
                                   None if latencies else "no message has been read yet"),
            "seen_latency_last_s": cell(latencies[-1] if latencies else None, _iso(now),
                                        None if latencies else "no message has been read yet"),
            "samples": len(latencies),
            "reply_latency_s": cell(None, None, "the answer link lives in the application, not this store"),
        })
    out.sort(key=lambda item: item["mailbox"] or "")
    return {"as_of": _iso(now), "mailboxes": out}


def _seen_latencies(inbox, mailbox, limit=200):
    where, params = __import__("dunk_inbox").mailbox_where(mailbox)
    with inbox._lock:
        rows = inbox.db.execute(
            f"SELECT created_at, read_at FROM messages WHERE {where} AND read_at IS NOT NULL "
            f"ORDER BY read_at DESC LIMIT ?", (*params, limit)).fetchall()
    return sorted(max(0.0, row["read_at"] - row["created_at"]) for row in rows)


def _median(values):
    if not values:
        return None
    ordered = sorted(values)
    middle = len(ordered) // 2
    if len(ordered) % 2:
        return round(ordered[middle], 1)
    return round((ordered[middle - 1] + ordered[middle]) / 2, 1)


def project_summary(flock, project=None, path=None, now=None):
    """Everything one project page needs, from the store and the DB.

    project=None means the global store. `path` overrides the project directory.
    """
    now = now or time.time()
    if project in (None, "", "global"):
        store = dunk_context.Store(global_store=True)
        project_dir = None
        label = "global"
    else:
        project_dir = os.path.abspath(os.path.expanduser(path or f"~/workspace/{project}"))
        store = dunk_context.Store(os.path.join(project_dir, ".dunkingsheep"))
        label = project
    stores = dunk_context.select_stores(project_dir) if project_dir else {"global": store}
    counts, truths, articles = {}, [], []
    for name, target in stores.items():
        if not target.exists():
            continue
        stats = target.stats()
        for cls, count in stats["counts"].items():
            counts[cls] = counts.get(cls, 0) + count
        for entry in target.entries():
            if entry.get("error"):
                continue
            item = {"store": name, "class": entry["class"], "slug": entry["slug"],
                    "subject": entry.get("subject"), "authority": entry.get("authority"),
                    "stated": entry.get("stated"),
                    "first_line": (entry["body"].splitlines() or [""])[0][:160]}
            (truths if entry["class"] == "ground-truth" else articles).append(item)
    lanes = [lane for lane in flock.resolve_lanes()
             if lane.get("agent") and project_dir and (lane.get("cwd") or "").startswith(project_dir)]
    tasks = flock.task_list(include_done=False, limit=500).get("tasks") or []
    in_flight = [{"id": t["id"], "title": t["title"], "status": t["status"],
                  "priority": t["priority"], "assignees": t.get("assignees") or []}
                 for t in tasks if t.get("status") == "in_progress"]
    done = flock.task_list(include_done=True, limit=500).get("tasks") or []
    history = []
    for task in done:
        if task.get("status") == "done" and task.get("completed_at"):
            history.append({"at": _iso(task["completed_at"]), "kind": "task.done",
                            "id": task["id"], "title": task["title"],
                            "by": task.get("completed_by")})
    history.sort(key=lambda item: item["at"] or "", reverse=True)
    return {
        "project": label,
        "as_of": _iso(now),
        "path": project_dir,
        "knowledge": {
            "counts": counts,
            "ground_truths": sorted(truths, key=lambda item: item["subject"] or ""),
            "articles": sorted(articles, key=lambda item: (item["class"], item["subject"] or "")),
            "articles_total": len(articles),
        },
        "now": {"agents": [{"name": lane.get("agent_name"), "pane_id": lane["pane_id"],
                            "state": lane.get("agent_status")} for lane in lanes],
                "tasks_in_flight": in_flight},
        "history": history[:50],
        "history_note": ("merges and decisions are the repository's record; this timeline is "
                         "tasks completed here" if history else "no completed task has a timestamp yet"),
    }
