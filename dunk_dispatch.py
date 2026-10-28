"""Deterministic feeding: a standby queue and a tick that fills idle lanes.

Every lane went idle and nothing noticed, so a human re-fed them by hand. This
module is the structural fix and it contains no model: a queue of assignments, a
tick that runs on a timer, and a gate that sends only to a lane that can actually
receive a message right now.

The gate is `delivery_state` (ready / working / blocked / dead) plus an empty
mailbox. "Ready with unread mail" is NOT dispatchable - the lane already has an
assignment waiting; handing it a second one is how a queue becomes noise.

The timer is the fallback, not the mechanism: `turn_end_plan` dispatches on the
turn end the harness already reports, because turns last seconds and timers fire
in hours.
"""
import json
import os
import sqlite3
from dunk_sqlite import connect as sqlite_connect
import threading
import time

CONFIG_DIR = os.environ.get("DUNKINGSHEEP_DIR", os.path.expanduser("~/.config/dunkingsheep"))
DB_PATH = os.path.join(CONFIG_DIR, "dispatch.db")
STATE_PATH = os.path.join(CONFIG_DIR, "dispatch-state.json")

SCHEMA = """
CREATE TABLE IF NOT EXISTS standby (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id TEXT,
    brief TEXT NOT NULL,
    lane TEXT,
    created_at REAL NOT NULL,
    taken_at REAL,
    taken_by TEXT,
    sent_message TEXT
);
"""


def _now():
    return time.time()


class DispatchError(ValueError):
    """A named refusal."""


class Queue:
    """The standby list: ordered assignments any ready lane can take."""

    def __init__(self, path=DB_PATH):
        self.path = path
        self._lock = threading.RLock()
        if path != ":memory:":
            os.makedirs(os.path.dirname(path), exist_ok=True)
        self.db = sqlite_connect(path, check_same_thread=False)
        self.db.row_factory = sqlite3.Row
        with self._lock:
            self.db.executescript(SCHEMA)
            self.db.commit()

    def add(self, brief, task_id=None, lane=None):
        brief = (brief or "").strip()
        if not brief:
            raise DispatchError("a standby item needs a one-line brief")
        with self._lock:
            cursor = self.db.execute(
                "INSERT INTO standby (task_id, brief, lane, created_at) VALUES (?,?,?,?)",
                (task_id, brief, lane, _now()))
            self.db.commit()
            return self.get(cursor.lastrowid)

    def get(self, item_id):
        row = self.db.execute("SELECT * FROM standby WHERE id = ?", (item_id,)).fetchone()
        return dict(row) if row else None

    def items(self, *, pending_only=True):
        where = " WHERE taken_at IS NULL" if pending_only else ""
        with self._lock:
            return [dict(row) for row in self.db.execute(
                f"SELECT * FROM standby{where} ORDER BY id")]

    def remove(self, item_id):
        with self._lock:
            changed = self.db.execute("DELETE FROM standby WHERE id = ?", (item_id,)).rowcount
            self.db.commit()
        if not changed:
            raise DispatchError(f"no standby item {item_id!r}")
        return {"removed": item_id}

    def take(self, item_id, lane, sent_message=None, now=None):
        with self._lock:
            row = self.db.execute("SELECT taken_at FROM standby WHERE id = ?", (item_id,)).fetchone()
            if row is None:
                raise DispatchError(f"no standby item {item_id!r}")
            if row["taken_at"] is not None:
                raise DispatchError(f"standby item {item_id} was already taken")
            self.db.execute(
                "UPDATE standby SET taken_at = ?, taken_by = ?, sent_message = ? WHERE id = ?",
                (now or _now(), lane, sent_message, item_id))
            self.db.commit()
        return self.get(item_id)

    def next_for(self, lane):
        """The first pending item assigned to this lane, else the first unassigned."""
        pending = self.items()
        for item in pending:
            if item["lane"] and item["lane"] == lane:
                return item
        for item in pending:
            if not item["lane"]:
                return item
        return None


def lane_plan(panes, *, delivery, unread, lanes=None):
    """Classify every lane for one tick. Pure: everything is injected.

    panes:   [{"pane_id", "agent_status", "agent_name"?}]
    delivery(pane) -> {"state", "reason"}
    unread(pane)   -> int
    lanes:   optional allowlist of pane ids

    Returns {"ready": [...], "blocked": [...], "dead": [...], "working": [...]}.
    A ready pane with unread mail is reported as `has_mail`, not as ready: it
    already has an assignment waiting.
    """
    out = {"ready": [], "has_mail": [], "blocked": [], "dead": [], "working": []}
    for pane in panes:
        pane_id = pane.get("pane_id")
        if not pane_id or (lanes is not None and pane_id not in lanes):
            continue
        state = delivery(pane) or {}
        name = state.get("state")
        entry = {"pane_id": pane_id, "name": pane.get("agent_name") or pane_id,
                 "reason": state.get("reason")}
        if name == "dead":
            out["dead"].append(entry)
        elif name == "blocked":
            out["blocked"].append(entry)
        elif name == "working":
            out["working"].append(entry)
        elif name == "ready":
            waiting = unread(pane)
            if waiting:
                out["has_mail"].append(dict(entry, unread=waiting))
            else:
                out["ready"].append(entry)
        else:
            out["blocked"].append(dict(entry, reason=state.get("reason") or "unknown state"))
    return out


PRIORITY_ORDER = {"urgent": 0, "high": 1, "medium": 2, "low": 3}
# Kept as documentation of the marker: a held task carries a block_reason like any
# other blocked task, and the dispatcher skips every block_reason.
HELD_PREFIX = "HELD FOR OWNER DECISION"


def derive_board_items(tasks, *, dispatched_task_ids=(), now=None, coarse_chars=90):
    """Standby items DERIVED from the task board.

    The hand-written queue is a hand-maintained list, and a hand-maintained list is
    a future omission: it ran dry twice while open tasks sat on the board. So the
    board is the primary source, and `queue empty` then means `no open unblocked
    work` - a real finding - rather than `somebody forgot to top it up`.

    A task is dispatchable when it is open, not held for an owner decision, has no
    unfinished dependency, is not already being worked (a live lease), is not in the
    hand queue, and was not dispatched inside the cooldown.
    """
    now = now or _now()
    done = {str(task.get("id")) for task in tasks
            if str(task.get("status")) in ("done", "cancelled")}
    taken = {str(task_id) for task_id in dispatched_task_ids}
    items, coarse = [], []
    for task in tasks:
        status = str(task.get("status") or "")
        if status != "open":
            continue
        # Any declared block reason means the task is not dispatchable: a task held
        # for the owner and a task waiting on a vendor are both waiting on somebody.
        if str(task.get("block_reason") or "").strip():
            continue
        depends = [str(dep) for dep in (task.get("depends_on") or [])]
        if any(dep not in done for dep in depends):
            continue
        if task.get("leases"):
            continue                      # somebody is on it
        task_id = str(task.get("id"))
        if task_id in taken:
            continue                      # dispatched recently; do not stack
        assignees = task.get("assignees") or []
        if assignees and str(task.get("status")) == "in_progress":
            continue
        title = " ".join(str(task.get("title") or "").split())
        if not title:
            continue
        item = {"id": f"board:{task_id}", "task_id": task_id, "brief": title,
                "lane": None, "derived": True,
                "priority": PRIORITY_ORDER.get(str(task.get("priority")), 9),
                "created_at": task.get("created_at") or now}
        if len(title) > coarse_chars:
            # An umbrella too broad to act on: a signal to split it, not a reason
            # to keep a parallel hand-written list.
            coarse.append(task_id)
        items.append(item)
    items.sort(key=lambda item: (item["priority"], item["created_at"]))
    return items, coarse


def dispatch_text(item):
    """What the lane is sent. One line, the task id first so the reply can cite it."""
    prefix = f"[standby {item['task_id']}] " if item.get("task_id") else "[standby] "
    first = " ".join(str(item["brief"]).split())
    return prefix + first


# -- the turn-end trigger ------------------------------------------------------
#
# Measured 2026-10-27: a lane's turns lasted 33-448 s while the keep-moving dunks
# fired every 60-120 MINUTES, so a lane sat unprompted ~95% of the time BY
# CONSTRUCTION. The harness already tells us when a turn ends (pi/claude emit
# turn.start/turn.end today); the fix is to act on that instead of waiting for a
# timer. Do NOT shorten the timers instead: 1,541 dunks recently held on an open
# dialog and 738 on a draft, so a faster timer only makes more blocked sends.
#
# The policy lives here, pure, so the guardrails can be read and tested without a
# daemon: a turn end earns ONE prompt, only when the lane has CLAIMED work (a live
# lease held by that pane), only when it has no unread mail (it is already fed), and
# not twice in a row for the same task. The delivery itself goes through the same
# guarded send as everything else, so a human typing is never typed over.
TURN_END_MIN_GAP_S = float(os.environ.get("DUNKINGSHEEP_TURN_END_MIN_GAP_S", "30"))
# THE SAME TASK IS NOT RE-PROMPTED ON EVERY TURN END: a lane that ends a 20-second turn
# 30 times while holding one task must not become a nudge loop. This is a LOOP GUARD, not
# a cadence, and it is deliberately SHORT (5 minutes): a long value would re-create the
# hourly-timer problem one layer down, because a cooldown only suppresses prompts - it
# cannot wake an idle lane. Worst case per lane and task is therefore 12 prompts/hour,
# each of which is one turn; a different held task is not suppressed by it at all.
TURN_END_TASK_COOLDOWN_S = float(
    os.environ.get("DUNKINGSHEEP_TURN_END_TASK_COOLDOWN_S", "300"))


def claimed_tasks(tasks, pane_id):
    """The tasks `pane_id` has CLAIMED (holds a live lease on), in the order to offer them.

    CLAIMED, NOT ASSIGNED (CoS ruling, 2026-10-27). An umbrella or programme task sits
    assigned-and-open for days, so prompting every lane that is named as an assignee
    would turn the trigger into a loop in exactly the lanes that must stay quiet
    (reviewer, CoS, VP). A live lease held by THIS pane means the lane said "I am
    working on this now" - the condition under which prompting it again is useful. The
    same rule excludes double-prompting: a task another pane has leased is that pane's,
    whoever else is named on it.

    Every other exclusion is a reason a nudge would be noise rather than work: a task
    that is not in_progress is not claimed work; a declared block reason means it waits
    on somebody else; an unfinished dependency makes it inactionable.

    Order: priority, then age (every task here is in_progress, so "already started" is
    not a distinction any more).
    """
    pane_id = str(pane_id or "")
    if not pane_id:
        return []
    finished = {str(task.get("id")) for task in tasks
                if str(task.get("status") or "") == "done"}
    claimed = []
    for task in tasks:
        if str(task.get("status") or "") != "in_progress":
            continue
        if str(task.get("block_reason") or "").strip():
            continue
        holders = {str(lease.get("pane_id"))
                   for lease in (task.get("leases") or []) if isinstance(lease, dict)}
        if pane_id not in holders:
            continue
        # The task store resolves dependencies outside the current view. A completed
        # prerequisite is absent from an open-task list; absence is not a blocker.
        blocked = (task["blocked_by"] if "blocked_by" in task else
                   [dep for dep in (task.get("depends_on") or [])
                    if str(dep) not in finished])
        if blocked:
            continue
        claimed.append(task)
    claimed.sort(key=lambda task: (PRIORITY_ORDER.get(str(task.get("priority")), 9),
                                   task.get("created_at") or 0))
    return claimed


def claimed_work(tasks, pane_id):
    """The first task the lane has claimed (see `claimed_tasks`), or None."""
    claimed = claimed_tasks(tasks, pane_id)
    return claimed[0] if claimed else None


def turn_end_plan(*, pane_id, tasks, unread, last_nudge_at=None,
                  task_nudges=None, now=None, min_gap_s=None,
                  task_cooldown_s=None):
    """Decide whether a turn end earns a prompt. Pure: everything is injected.

    Returns {"trigger", "pane_id", "action", "reason", "item"}, where action is
    "nudge" or "skip" and `reason` is a machine-readable code on a skip:

        no_pane           nothing to decide about
        unread_unknown    the mailbox could not be read, so "already fed" is unknown
        unread_mail       the lane already has unread mail; it is fed, do not stack
        no_open_work      no CLAIMED task: assigned-but-unclaimed work is not prompted,
                          so a leader lane holding programme tasks stays quiet
        cooldown          this pane was prompted < min_gap_s ago
        task_cooldown     every task the lane holds was prompted < task_cooldown_s ago

    `unread=None` means the count could not be established, and an unknown is a
    skip: sending a second assignment because a count could not be read is exactly
    how a queue becomes noise.
    """
    now = _now() if now is None else now
    min_gap_s = TURN_END_MIN_GAP_S if min_gap_s is None else float(min_gap_s)
    task_cooldown_s = (TURN_END_TASK_COOLDOWN_S if task_cooldown_s is None
                       else float(task_cooldown_s))
    plan = {"trigger": "turn.end", "pane_id": str(pane_id or ""),
            "action": "skip", "reason": None, "item": None}
    if not pane_id:
        plan["reason"] = "no_pane"
        return plan
    if unread is None:
        plan["reason"] = "unread_unknown"
        return plan
    if unread:
        plan["reason"] = "unread_mail"
        return plan
    held = claimed_tasks(tasks or (), pane_id)
    if not held:
        plan["reason"] = "no_open_work"
        return plan
    if last_nudge_at is not None and min_gap_s > 0 and now - float(last_nudge_at) < min_gap_s:
        plan["reason"] = "cooldown"
        return plan
    # A task prompted a moment ago is being worked on already; a DIFFERENT task the lane
    # also holds has not been mentioned, so it is still eligible.
    nudges = task_nudges or {}
    if task_cooldown_s > 0:
        held = [task for task in held
                if not (nudges.get(str(task.get("id"))) is not None
                        and now - float(nudges[str(task.get("id"))]) < task_cooldown_s)]
        if not held:
            plan["reason"] = "task_cooldown"
            return plan
    plan.update(action="nudge", item=held[0])
    return plan


def turn_end_text(task):
    """What a lane that ended its turn holding work is sent.

    The task id leads so the reply can cite it, and the trigger is stated in the
    text the lane reads: a prompt that arrives seconds after a turn ended should
    say why it is not a timer.
    """
    task_id = str(task.get("id"))
    title = " ".join(str(task.get("title") or "").split())
    status = str(task.get("status") or "open")
    priority = str(task.get("priority") or "medium")
    return "\n".join([
        f"[turn-end {task_id}] {title}",
        f"trigger: turn.end - you ended your turn still holding this task "
        f"({status}, priority {priority}).",
        "Carry on, or close it out:",
        f"  dunkingsheep task note {task_id} --reason \"<what you did / did NOT verify>\"",
        f"  dunkingsheep task complete {task_id}",
    ])


def tick(queue, panes, *, delivery, unread, send, now=None, dry_run=False,
         lanes=None, state_path=STATE_PATH, one_per_lane=True, derived=()):
    """Fill idle lanes from the standby queue. Returns the state it wrote.

    `send(pane, text)` must be the guarded send path (the inbox nudge), so the
    dispatcher inherits the typing hold and the dead-session refusal rather than
    typing over anything itself.
    """
    now = now or _now()
    plan = lane_plan(panes, delivery=delivery, unread=unread, lanes=lanes)
    # Derived board items first: they are the real backlog. The hand queue is for
    # work that is not a task yet, so it comes second rather than starving it.
    pending_derived = list(derived)
    dispatched = []
    for lane in plan["ready"]:
        if pending_derived:
            item = pending_derived.pop(0)
            item = dict(item, id=item.get("id") or f"board:{item['task_id']}")
        else:
            item = queue.next_for(lane["pane_id"])
        if item is None:
            break
        text = dispatch_text(item)
        record = {"pane_id": lane["pane_id"], "name": lane["name"], "item": item["id"],
                  "task_id": item.get("task_id"), "text": text, "sent": False}
        if not dry_run:
            try:
                record["message_id"] = send(lane["pane_id"], text)
                record["sent"] = True
                if not item.get("derived"):
                    queue.take(item["id"], lane["pane_id"], record.get("message_id"), now=now)
            except Exception as exc:             # a refused send is a fact, not a crash
                record["error"] = str(exc)
        else:
            record["sent"] = False
        dispatched.append(record)
        if not one_per_lane:
            continue                 # one item per lane per tick is the default

    pending = queue.items()
    unfilled_derived = [item for item in pending_derived]
    filled = {record["pane_id"] for record in dispatched if record["sent"]}
    idle_unfilled = [lane["pane_id"] for lane in plan["ready"] if lane["pane_id"] not in filled]
    state = {
        "tick_at": now,
        "dry_run": bool(dry_run),
        "lanes": len(plan["ready"]) + len(plan["has_mail"]) + len(plan["blocked"])
                 + len(plan["dead"]) + len(plan["working"]),
        "ready": len(plan["ready"]),
        "working": len(plan["working"]),
        "has_mail": len(plan["has_mail"]),
        "blocked": [{"pane_id": b["pane_id"], "name": b["name"], "reason": b["reason"]}
                    for b in plan["blocked"]],
        "dead": [{"pane_id": d["pane_id"], "name": d["name"], "reason": d["reason"]}
                 for d in plan["dead"]],
        "dispatched": dispatched,
        "dispatched_count": sum(1 for record in dispatched if record["sent"]),
        "idle_unfilled": idle_unfilled,
        "queue_depth": len(pending),
        "derived_available": len(derived),
        "derived_dispatched": sum(1 for record in dispatched
                                  if record.get("sent") and str(record["item"]).startswith("board:")),
        # The honest answer when lanes sit idle: there is no open unblocked work
        # anywhere - not in the board and not in the hand queue.
        "queue_empty": (len(pending) == 0 and not unfilled_derived
                        and not derived and bool(idle_unfilled)),
    }
    state["dispatched_1h"] = 0
    if state_path:
        _write_state(state, state_path)      # computes dispatched_1h from the history
    return state
    return state


def _dispatched_in_window(state_path, now, window_s=3600):
    """How many sends this hour, from the history this module keeps."""
    history = _load_state(state_path).get("history") or []
    return sum(1 for entry in history if entry.get("at", 0) >= now - window_s)


def _load_state(path=STATE_PATH):
    try:
        with open(path, encoding="utf-8") as handle:
            return json.load(handle)
    except (OSError, ValueError):
        return {}


def _write_state(state, path=STATE_PATH):
    """Atomic, and it keeps a small history so 'dispatched this hour' is real."""
    previous = _load_state(path)
    history = [h for h in (previous.get("history") or [])
               if h.get("at", 0) >= state["tick_at"] - 24 * 3600]
    for record in state.get("dispatched") or []:
        if record.get("sent"):
            history.append({"at": state["tick_at"], "pane_id": record["pane_id"],
                            "item": record["item"], "task_id": record.get("task_id")})
    state["history"] = history
    # Count AFTER this tick's sends: the number the owner reads includes them.
    state["dispatched_1h"] = sum(1 for entry in history
                                 if entry.get("at", 0) >= state["tick_at"] - 3600)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    temporary = path + ".tmp"
    with open(temporary, "w", encoding="utf-8") as handle:
        json.dump(state, handle, indent=2)
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temporary, path)
    return state


def summary(state):
    """The three numbers the owner asked for, one line each."""
    idle = state.get("idle_unfilled") or []
    blocked = state.get("blocked") or []
    dead = state.get("dead") or []
    reasons = sorted({f"{item['name']}: {item['reason']}" for item in blocked})
    lines = [f"idle lanes: {len(idle)}",
             f"dispatched this hour: {state.get('dispatched_1h')}",
             f"blocked lanes: {len(blocked)}" + (f" ({'; '.join(reasons)})" if reasons else "")]
    if dead:
        names = ", ".join(item["name"] for item in dead)
        lines.append(f"dead lanes: {len(dead)} ({names})")
    if state.get("queue_empty"):
        lines.append(f"the standby queue is empty and {len(idle)} lane(s) are idle")
    return "\n".join(lines)


def next_job_plan(*, trigger, pane_id, tasks, unread, pending_delivery=False,
                  last_nudge_at=None, task_nudges=None, now=None,
                  min_gap_s=None, task_cooldown_s=None):
    """Decide what a lane does the moment it goes free. Pure: everything is injected.

    t372: the TRIGGER changes, not the eligibility rule (pi-review-1 i31321). On a task
    COMPLETION (and on a genuine turn end) a lane looks for its next job by calling the
    existing atomic claim - it does not get nudged for work it has not claimed. What was
    missing was never permission; it was that nothing asked at the moment the lane went
    free.

    Returns {"trigger", "pane_id", "action", "reason", "task_id"} where action is
    "claim" or "skip". Skip reasons are machine-readable and are the contract:

        no_pane            nothing to decide about
        unread_unknown     the mailbox could not be read; unknown is a skip
        unread_mail        already fed; do not stack a second assignment
        pending_delivery   a prompt is still waiting for the box to clear
        no_assigned_work   clause 5: quiet lanes stay quiet, and this is recorded
        cooldown           this pane was prompted < min_gap_s ago
        task_cooldown      every task it holds was prompted < task_cooldown_s ago

    CLAUSE 5 IS THE POINT OF `no_assigned_work`: reviewer/CoS/VP lanes with nothing
    assigned must record explicit no-work rather than poll, or the fix becomes the
    storm the CoS ruling was written to stop.
    """
    now = _now() if now is None else now
    min_gap_s = TURN_END_MIN_GAP_S if min_gap_s is None else float(min_gap_s)
    task_cooldown_s = (TURN_END_TASK_COOLDOWN_S if task_cooldown_s is None
                       else float(task_cooldown_s))
    plan = {"trigger": str(trigger or ""), "pane_id": str(pane_id or ""),
            "action": "skip", "reason": None, "task_id": None}
    if not pane_id:
        plan["reason"] = "no_pane"
        return plan
    if unread is None:
        plan["reason"] = "unread_unknown"
        return plan
    if unread:
        plan["reason"] = "unread_mail"
        return plan
    if pending_delivery:
        plan["reason"] = "pending_delivery"
        return plan
    candidates = assigned_open_tasks(tasks or (), pane_id)
    if not candidates:
        plan["reason"] = "no_assigned_work"
        return plan
    if last_nudge_at is not None and min_gap_s > 0 and now - float(last_nudge_at) < min_gap_s:
        plan["reason"] = "cooldown"
        return plan
    nudges = task_nudges or {}
    fresh = [t for t in candidates
             if now - float(nudges.get(str(t.get("id")), 0) or 0) >= task_cooldown_s]
    if not fresh:
        plan["reason"] = "task_cooldown"
        return plan
    plan["action"] = "claim"
    plan["task_id"] = str(fresh[0].get("id"))
    return plan


def assigned_open_tasks(tasks, pane_id):
    """Open tasks ASSIGNED to `pane_id`, in board order, excluding blocked/gated ones.

    This is the eligibility the owner's order needs and the CoS ruling withheld: it is
    used ONLY at the moment a lane goes free, and the caller then takes the work through
    the atomic claim (task_next), so a lane never receives a prompt for work it has not
    claimed.
    """
    wanted = str(pane_id or "")
    out = []
    for task in tasks or ():
        if str(task.get("status")) != "open":
            continue
        if task.get("blocked_by") or task.get("block_reason"):
            continue
        assignees = [str(a) for a in (task.get("assignees") or [])]
        if wanted and wanted in assignees:
            out.append(task)
    out.sort(key=lambda t: (PRIORITY_ORDER.get(str(t.get("priority")), 9),
                            float(t.get("created_at") or 0)))
    return out
