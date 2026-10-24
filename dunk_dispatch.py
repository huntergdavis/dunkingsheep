"""Deterministic feeding: a standby queue and a tick that fills idle lanes.

Every lane went idle and nothing noticed, so a human re-fed them by hand. This
module is the structural fix and it contains no model: a queue of assignments, a
tick that runs on a timer, and a gate that sends only to a lane that can actually
receive a message right now.

The gate is `delivery_state` (ready / working / blocked / dead) plus an empty
mailbox. "Ready with unread mail" is NOT dispatchable - the lane already has an
assignment waiting; handing it a second one is how a queue becomes noise.
"""
import json
import os
import sqlite3
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
        self.db = sqlite3.connect(path, check_same_thread=False)
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
