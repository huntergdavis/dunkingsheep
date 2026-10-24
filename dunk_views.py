"""Read-only compositions for the Command Center: roster, message state, project summary.

Three consumers (COS chat, the project page, the agent page) each need a shape that
no single store holds, and each shape is assembled here from deterministic reads.
No model, no inference about a pane that was not read, and every number is a cell
{value, as_of, reason} so a missing fact is never a zero.
"""
import os
import time

import sqlite3
import subprocess

import dunk_activity
import dunk_context

CONFIG_DIR = os.environ.get("DUNKINGSHEEP_DIR", os.path.expanduser("~/.config/dunkingsheep"))
# The COS/VP application's own message log: it holds the answer link (answer_id ->
# reply row, answered_at). It records ITS ids, not our inbox ids, so the two stores
# are joined by kind rather than pretended to be one.
APP_DB = os.environ.get("DUNKINGSHEEP_APP_DB",
                        os.path.expanduser("~/workspace/vp-of-agentry/data/messages.sqlite3"))
# The application's delivery column does DOUBLE DUTY and must not be passed through
# raw: on an owner message it is TRANSPORT (sent -> read -> acked), on an agent message
# it is REPLY LIFECYCLE (posted). The design lane asked for the split rather than a
# passthrough, because a UI that renders 'acked' next to 'posted' has two meanings in
# one column.
TRANSPORT_STATES = ("sent", "read", "acked")
REPLY_STATES = ("posted",)

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
        # What the lane is DOING comes from its own tool, not from a status field
        # that may not exist (pi has none) - and an absent signal is UNKNOWN.
        try:
            doing = dunk_activity.activity(lane, claude_status=getattr(flock, "claude_status", None),
                                           turns=getattr(flock, "turns", None), now=now)
        except Exception as error:                 # noqa: BLE001 - a row must not fail
            doing = {"state": "unknown", "since": None, "source": "activity",
                     "reason": f"activity probe failed: {error}"}
        model_reason = MODEL_SOURCES.get(str(harness or "").lower(), (None, "unknown harness"))
        rows.append({
            "pane_id": pane_id,                       # internal; the UI drops it
            "name": lane.get("agent_name") or pane_id,
            "harness": harness,
            "model": cell(None, None, model_reason[1] if harness else "unknown harness"),
            "state": doing["state"],
            "state_source": doing["source"],
            "state_reason": doing["reason"],
            "since": cell(doing.get("since"), _iso(doing.get("since")),
                          None if doing.get("since") else "the source reports a state, not when it changed"),
            "pane_status": status,
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


def _app_row(message_id):
    """One row from the application's log, or None. Never raises on a missing DB."""
    try:
        conn = sqlite3.connect(f"file:{APP_DB}?mode=ro", uri=True)
        conn.row_factory = sqlite3.Row
    except sqlite3.Error:
        return None
    try:
        row = conn.execute(
            "SELECT id, role, channel, body, delivery, delivery_detail, created_at, "
            "answer_id, answered_at FROM messages WHERE id = ?", (int(message_id),)).fetchone()
        if row is None:
            return None
        answer = None
        if row["answer_id"]:
            reply = conn.execute("SELECT id, role, channel, body, created_at FROM messages "
                                 "WHERE id = ?", (row["answer_id"],)).fetchone()
            if reply:
                answer = {"id": reply["id"], "role": reply["role"], "channel": reply["channel"],
                          "at": reply["created_at"],
                          "excerpt": (reply["body"] or "").strip().splitlines()[:1][0][:120]
                          if (reply["body"] or "").strip() else None}
        return dict(row, answer=answer)
    except (sqlite3.Error, ValueError):
        return None
    finally:
        conn.close()


def _app_state(row, now):
    """sent / delivered / seen / replied for an APPLICATION message, with evidence.

    replied is `answer_id IS NOT NULL` - NOT answered_at. The design lane measured it:
    456 owner rows all carry answered_at, but 70 have no answer_id, so answered_at is
    'picked up', not 'answered'. Using it made every message look replied.
    """
    delivery = (row.get("delivery") or "").strip().lower()
    role = (row.get("role") or "").strip().lower()
    if role == "owner":
        transport = delivery if delivery in TRANSPORT_STATES else None
        reply_lifecycle = None
        delivery_reason = (None if transport else
                           (f"unexpected transport state {delivery!r}" if delivery
                            else "no transport state recorded"))
    else:
        transport = None
        reply_lifecycle = delivery if delivery in REPLY_STATES else None
        delivery_reason = (None if reply_lifecycle else
                           (f"unexpected reply state {delivery!r}" if delivery
                            else "no reply state recorded"))
    if row.get("answer_id"):
        reply = row.get("answer") or {}
        replied = {"at": reply.get("at"), "reply_id": row["answer_id"],
                   "reply_excerpt": reply.get("excerpt"),
                   "reason": None if reply.get("at") else
                             "answer_id is set but the reply row is missing"}
    else:
        replied = cell(None, None, "awaiting reply (no answer_id yet)")
    picked_up = ({"at": row.get("answered_at"), "reason": None}
                 if row.get("answered_at") else
                 cell(None, None, "not picked up yet"))
    return {
        "id": row["id"], "found": True, "source": "application",
        "mailbox": row.get("channel"),
        "role": row.get("role"),
        "sent": {"at": row.get("created_at"), "by": row.get("role")},
        "delivered": {"ok": bool(transport or reply_lifecycle),
                      "transport": transport,
                      "reply_lifecycle": reply_lifecycle,
                      "detail": row.get("delivery_detail") or None,
                      "reason": delivery_reason},
        "seen": picked_up,
        "replied": replied,
        "state": transport or reply_lifecycle,
    }


def app_messages(channel=None, limit=50):
    """Recent application messages, newest first: what the owner chat renders."""
    try:
        conn = sqlite3.connect(f"file:{APP_DB}?mode=ro", uri=True)
        conn.row_factory = sqlite3.Row
    except sqlite3.Error as error:
        return {"error": f"application message log unavailable: {error}", "messages": []}
    try:
        where, params = "", []
        if channel:
            where, params = " WHERE channel = ?", [channel]
        rows = [dict(r) for r in conn.execute(
            f"SELECT id, role, channel, body, delivery, created_at, answer_id, answered_at "
            f"FROM messages{where} ORDER BY id DESC LIMIT ?", (*params, int(limit)))]
    except sqlite3.Error as error:
        return {"error": f"application message log unreadable: {error}", "messages": []}
    finally:
        conn.close()
    return {"messages": rows, "count": len(rows), "channel": channel}


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
        # An APPLICATION id (numeric) has the answer link; an INBOX id ('i123') has
        # the pane delivery. Report each from the store that knows it, and say which.
        if str(message_id).isdigit():
            app_row = _app_row(message_id)
            if app_row is None:
                out.append({"id": message_id, "found": False, "source": "application",
                            "reason": "no application row with this id"})
            else:
                out.append(_app_state(app_row, now))
            continue
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
            "id": row.get("id") or row.get("message_id"), "found": True,
            "source": "inbox",
            "mailbox": row.get("mailbox"),
            "sent": {"at": _iso(created), "by": row.get("sender")},
            # DELIVERED is about the NUDGE, not the mail: the mail is the row you are
            # reading. notify_state says whether the notice landed; a held or failed
            # notice is delivered:false with the reason the send path recorded.
            "delivered": {"ok": (row.get("notify_state") or "") == "notified",
                          "state": row.get("notify_state"),
                          "reason": row.get("notify_reason"),
                          "at": None if row.get("notify_state") else None,
                          "at_reason": "the store records the notify state, not when it changed"},
            "seen": {"at": _iso(row.get("read_at")) if row.get("read_at") else None,
                     "acked_at": _iso(row.get("acked_at")) if row.get("acked_at") else None},
            "replied": cell(None, None,
                            "the COS/VP application records its own ids; this inbox message has "
                            "no application row (its log has no external_id for inbox ids)"),
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


# The repositories a project page can show merges for. A merge list is the repo's own
# record, so it carries the repo's own as_of rather than the page's.
PROJECT_REPOS = {
    "dunkingsheep": "~/workspace/dunkingsheep",
    "vp-of-agentry": "~/workspace/vp-of-agentry",
    "herdr-webui": "~/workspace/herdr-webui",
}


def git_merges(repo, limit=10, timeout=10):
    """Recent merges for one repository, with its own as_of. Deterministic, no model."""
    path = os.path.expanduser(repo)
    if not os.path.isdir(os.path.join(path, ".git")):
        return {"repo": path, "merges": [], "as_of": None,
                "reason": f"{path} is not a git repository"}
    try:
        proc = subprocess.run(
            ["git", "-C", path, "log", "--merges", f"-n{int(limit)}",
             "--format=%h%x1f%s%x1f%cI"],
            capture_output=True, text=True, timeout=timeout)
    except (OSError, subprocess.SubprocessError) as error:
        return {"repo": path, "merges": [], "as_of": None, "reason": f"git log failed: {error}"}
    if proc.returncode != 0:
        return {"repo": path, "merges": [], "as_of": None,
                "reason": (proc.stderr or "git log failed").strip()[:200]}
    merges = []
    for line in proc.stdout.splitlines():
        parts = line.split("\x1f")
        if len(parts) == 3:
            merges.append({"sha": parts[0], "subject": parts[1], "at": parts[2]})
    return {"repo": path, "merges": merges, "as_of": merges[0]["at"] if merges else None,
            "reason": None if merges else "no merge commits in this repository"}


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
        "merges": {name: git_merges(repo) for name, repo in PROJECT_REPOS.items()
                   if label in ("global", name)},
        "history": history[:50],
        "history_note": ("merges and decisions are the repository's record; this timeline is "
                         "tasks completed here" if history else "no completed task has a timestamp yet"),
    }
