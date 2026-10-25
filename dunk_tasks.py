#!/usr/bin/env python3
"""
Global and per-pane tasks: the coordination backbone beyond context limits.

A task is a row in a SQLite database owned by the daemon, addressed through the
same CLI / socket / MCP surfaces as everything else. Each task has a scope:

- ``global``: visible to every pane and agent in the fleet.
- a pane id (``w8:p3``): visible only to that pane and its agent.

Semantics the fleet relies on:

- **global completion**: a task is done for everyone as soon as any assignee
  completes it. There is no per-assignee copy to drift apart.
- **next**: a task is eligible when its status is ``open`` or ``in_progress``,
  it is not ``blocked``/``cancelled``/``done``, and every task in
  ``depends_on`` is ``done``. Eligible tasks are ordered by priority
  (urgent > high > medium > low), then oldest first.
- **subtasks**: ``parent_id`` links a task to its parent; reading a task returns
  its children, and deleting a parent is refused while children exist.
- **atomic claim**: ``claim_next`` selects and claims a task in one
  ``BEGIN IMMEDIATE`` transaction, so two agents can never take the same
  exclusive task. A ``multi_assignee`` task stays open to additional claimants.

The store knows nothing about panes, agents or herdr; callers resolve a target
to a scope string and hand it over.
"""

import json
import logging
import os
import re
import sqlite3

log = logging.getLogger("dunkingsheep.tasks")
import threading
import time
from collections import Counter

CONFIG_DIR = os.environ.get("DUNKINGSHEEP_DIR") or os.path.expanduser(
    "~/.config/dunkingsheep"
)
TASKS_DB = os.path.join(CONFIG_DIR, "tasks.db")

GLOBAL = "global"
STATUSES = ("open", "in_progress", "blocked", "done", "cancelled")
PRIORITIES = ("low", "medium", "high", "urgent")
PRIORITY_RANK = {"urgent": 0, "high": 1, "medium": 2, "low": 3}
DEFAULT_STATUS = "open"
DEFAULT_PRIORITY = "medium"
# How long a claim stays valid without a heartbeat. A dead or stalled agent's
# task returns to the queue after this, or as soon as its pane disappears.
DEFAULT_LEASE_S = float(os.environ.get("DUNKINGSHEEP_LEASE_S", "1800"))

# Pane ids are "<workspace>:<pane>" (w8:p3). Scope is either "global", "self"
# (resolved by the caller) or a pane id.
# A pane id is whatever the current backend calls a pane: herdr's
# 'workspace:pane' (w8:p8) or tmux's '%N'. Refusing tmux ids made every
# per-pane task scope unusable the day the backend flipped (2026-10-24).
_PANE_ID = re.compile(r"^(?:[A-Za-z0-9_.-]+:[A-Za-z0-9_.-]+|%\d+)$")

SCHEMA = """
CREATE TABLE IF NOT EXISTS tasks (
    id            TEXT PRIMARY KEY,
    scope         TEXT NOT NULL,
    title         TEXT NOT NULL,
    body          TEXT NOT NULL DEFAULT '',
    status        TEXT NOT NULL DEFAULT 'open',
    priority      TEXT NOT NULL DEFAULT 'medium',
    parent_id     TEXT,
    multi_assignee INTEGER NOT NULL DEFAULT 0,
    attempts      INTEGER NOT NULL DEFAULT 0,
    assignees     TEXT NOT NULL DEFAULT '[]',
    depends_on    TEXT NOT NULL DEFAULT '[]',
    created_at    REAL NOT NULL,
    updated_at    REAL NOT NULL,
    created_by    TEXT,
    completed_by  TEXT,
    completed_at  REAL,
    cancelled_at  REAL,
    block_reason  TEXT
);
CREATE INDEX IF NOT EXISTS tasks_by_scope
    ON tasks (scope, status, created_at);
CREATE INDEX IF NOT EXISTS tasks_by_parent ON tasks (parent_id);
CREATE TABLE IF NOT EXISTS task_leases (
    task_id     TEXT NOT NULL,
    pane_id     TEXT NOT NULL,
    claimed_at  REAL NOT NULL,
    heartbeat_at REAL NOT NULL,
    expires_at  REAL NOT NULL,
    caller_pid   INTEGER,
    caller_start TEXT,
    caller_name  TEXT,
    PRIMARY KEY (task_id, pane_id)
);
CREATE INDEX IF NOT EXISTS leases_by_pane ON task_leases (pane_id);
CREATE TABLE IF NOT EXISTS task_events (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id     TEXT NOT NULL,
    event       TEXT NOT NULL,
    at          REAL NOT NULL,
    actor       TEXT,
    scope       TEXT,
    priority    TEXT,
    status_from TEXT,
    status_to   TEXT,
    duration_ms INTEGER,
    meta        TEXT
);
CREATE INDEX IF NOT EXISTS task_events_by_time ON task_events (at);
CREATE INDEX IF NOT EXISTS task_events_by_task ON task_events (task_id, at);
CREATE INDEX IF NOT EXISTS task_events_by_event ON task_events (event, at);
CREATE TABLE IF NOT EXISTS counters (name TEXT PRIMARY KEY, value INTEGER NOT NULL);
"""


class TaskError(ValueError):
    """A caller mistake (unknown id, bad status, bad scope). Safe to show."""


def _now():
    return time.time()


def _as_list(value):
    if value is None:
        return []
    if isinstance(value, str):
        # Comma-separated is accepted so a CLI/JSON round trip stays friendly.
        parts = value.split(",")
        return [part.strip() for part in parts if part.strip()]
    return [str(item).strip() for item in value if str(item).strip()]


def _decode(raw):
    try:
        data = json.loads(raw or "[]")
    except (TypeError, ValueError):
        return []
    return data if isinstance(data, list) else []


def _encode(values):
    # Stable, de-duplicated order so repeated updates do not churn the row.
    seen = []
    for value in values:
        if value not in seen:
            seen.append(value)
    return json.dumps(seen)


def validate_scope(scope):
    scope = str(scope or "").strip()
    if scope == GLOBAL:
        return GLOBAL
    if _PANE_ID.match(scope):
        return scope
    raise TaskError(
        f"scope must be '{GLOBAL}' or a pane id such as 'w8:p3' or '%4' (got {scope!r})"
    )


def validate_status(status):
    status = str(status or "").strip()
    if status not in STATUSES:
        raise TaskError(f"status must be one of {', '.join(STATUSES)}")
    return status


def validate_priority(priority):
    priority = str(priority or "").strip()
    if priority not in PRIORITIES:
        raise TaskError(f"priority must be one of {', '.join(PRIORITIES)}")
    return priority


class Tasks:
    """The task store. One SQLite database beside the inbox database."""

    def __init__(self, path=TASKS_DB):
        self.path = path
        self._lock = threading.RLock()
        if path != ":memory:":
            os.makedirs(os.path.dirname(path), exist_ok=True)
        # ONE CONNECTION PER THREAD, because a sqlite3 connection is not safe to use
        # from two threads at once even with check_same_thread=False: cursors and
        # statement state are shared, and the damage shows up as somebody else's
        # error. Measured 2026-10-24/25: with twelve threads on one connection the
        # daemon logged "InterfaceError: bad parameter or other API misuse", and
        # `leases_for` once saw rows as plain tuples (IndexError: tuple index out of
        # range) because a concurrent statement had reset the cursor under it. The
        # lock alone could not prevent it: several paths (list, stats, metrics,
        # leases_for_pane, _record, _next_id) never took it.
        self._local = threading.local()
        self._connections = []
        self._connections_lock = threading.Lock()
        self.db = self._connect()          # the creating thread's connection
        with self._lock:
            self.db.executescript(SCHEMA)
            self._migrate()
            self.db.commit()

    def _connect(self):
        """This thread's connection, opened on first use and kept for its lifetime."""
        connection = getattr(self._local, "db", None)
        if connection is not None:
            return connection
        connection = sqlite3.connect(self.path, check_same_thread=False)
        connection.row_factory = sqlite3.Row
        self._local.db = connection
        with self._connections_lock:
            self._connections.append(connection)
        return connection

    def __getattribute__(self, name):
        if name == "db":
            return object.__getattribute__(self, "_connect")()
        return object.__getattribute__(self, name)

    def _migrate(self):
        """Add columns introduced after the first database was created."""
        columns = {row["name"] for row in self.db.execute("PRAGMA table_info(tasks)")}
        for name, ddl in (("multi_assignee", "INTEGER NOT NULL DEFAULT 0"),
                          ("attempts", "INTEGER NOT NULL DEFAULT 0")):
            if name not in columns:
                self.db.execute(f"ALTER TABLE tasks ADD COLUMN {name} {ddl}")
        lease_columns = {row["name"] for row in self.db.execute(
            "PRAGMA table_info(task_leases)")}
        for name, ddl in (("caller_pid", "INTEGER"), ("caller_start", "TEXT"),
                          ("caller_name", "TEXT")):
            if name not in lease_columns:
                self.db.execute(f"ALTER TABLE task_leases ADD COLUMN {name} {ddl}")

    def close(self):
        """Close every thread's connection (the store is shared by the daemon)."""
        with self._connections_lock:
            connections, self._connections = self._connections, []
        for connection in connections:
            try:
                connection.close()
            except sqlite3.Error:
                pass
        self._local = threading.local()

    # -- ids ----------------------------------------------------------------

    def _next_id(self):
        # F1: allocate with a single write statement. The old SELECT-then-upsert
        # let two processes compute the same number; the UPSERT starts the write
        # transaction so SQLite serialises concurrent adders.
        row = self.db.execute(
            "INSERT INTO counters (name, value) VALUES ('task', 1) "
            "ON CONFLICT(name) DO UPDATE SET value = value + 1 "
            "RETURNING value").fetchone()
        return f"t{row[0]}"

    def _record(self, task_id, event, at=None, actor=None, scope=None,
                priority=None, status_from=None, status_to=None,
                duration_ms=None, meta=None):
        """Append one immutable event. Metrics are computed from these rows,
        so every mutation records exactly one meaningful transition."""
        self.db.execute(
            "INSERT INTO task_events (task_id, event, at, actor, scope, "
            "priority, status_from, status_to, duration_ms, meta) "
            "VALUES (?,?,?,?,?,?,?,?,?,?)",
            (str(task_id), event, _now() if at is None else at, actor, scope,
             priority, status_from, status_to, duration_ms,
             json.dumps(meta) if meta is not None else None))

    # -- shaping ------------------------------------------------------------

    def _statuses(self, ids):
        if not ids:
            return {}
        placeholders = ",".join("?" for _ in ids)
        rows = self.db.execute(
            f"SELECT id, status FROM tasks WHERE id IN ({placeholders})", ids
        ).fetchall()
        return {row["id"]: row["status"] for row in rows}

    def _decorate(self, row, pane_id=None):
        task = dict(row)
        task["assignees"] = _decode(task.get("assignees"))
        task["depends_on"] = _decode(task.get("depends_on"))
        task["multi_assignee"] = bool(task.get("multi_assignee"))
        task["is_global"] = task["scope"] == GLOBAL
        leases = self.leases_for(task["id"])
        task["leases"] = leases
        task["claimed_by"] = [lease["pane_id"] for lease in leases]
        task["lease_expires_at"] = (
            min(lease["expires_at"] for lease in leases) if leases else None
        )
        statuses = self._statuses(task["depends_on"])
        task["blocked_by"] = [
            dep for dep in task["depends_on"] if statuses.get(dep) != "done"
        ]
        # F2: eligibility is per-viewer. An assigned task is not claimable by
        # anyone except its named assignee, so a generic list must not advertise
        # it as eligible.
        task["claimable"] = (
            self._claimable(task, pane_id) if pane_id
            else (not task["assignees"] or task["multi_assignee"])
        )
        task["eligible"] = (
            task["status"] in ("open", "in_progress")
            and not task["blocked_by"]
            and task["claimable"]
        )
        return task

    def _full(self, row, pane_id=None):
        task = self._decorate(row, pane_id)
        children = self.db.execute(
            "SELECT * FROM tasks WHERE parent_id = ? ORDER BY created_at",
            (task["id"],),
        ).fetchall()
        task["children"] = [self._decorate(child, pane_id) for child in children]
        if task.get("parent_id"):
            parent = self.db.execute(
                "SELECT id, title, status FROM tasks WHERE id = ?",
                (task["parent_id"],),
            ).fetchone()
            task["parent"] = dict(parent) if parent else None
        return task

    def _order_key(self, task):
        return (
            PRIORITY_RANK.get(task["priority"], 99),
            task["created_at"],
            task["id"],
        )

    def _require(self, task_id):
        row = self.db.execute(
            "SELECT * FROM tasks WHERE id = ?", (str(task_id).strip(),)
        ).fetchone()
        if row is None:
            raise TaskError(f"no task with id {task_id!r}")
        return row

    # -- writing ------------------------------------------------------------

    def add(self, scope, title, body="", status=DEFAULT_STATUS,
            priority=DEFAULT_PRIORITY, assignees=None, parent_id=None,
            depends_on=None, created_by=None, multi_assignee=False):
        scope = validate_scope(scope)
        title = str(title or "").strip()
        if not title:
            raise TaskError("a task needs a title")
        status = validate_status(status or DEFAULT_STATUS)
        priority = validate_priority(priority or DEFAULT_PRIORITY)

        parent_id = str(parent_id).strip() if parent_id else None
        if parent_id:
            self._require(parent_id)

        deps = _as_list(depends_on)
        for dep in deps:
            if dep == str(parent_id or ""):
                raise TaskError("a task cannot depend on its parent")
            self._require(dep)

        with self._lock:
            if self.db.in_transaction:
                self.db.commit()
            self.db.execute("BEGIN IMMEDIATE")
            try:
                task_id = self._next_id()
                now = _now()
                self.db.execute(
                    "INSERT INTO tasks (id, scope, title, body, status, priority, "
                    "parent_id, multi_assignee, assignees, depends_on, created_at, "
                    "updated_at, created_by) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)",
                    (task_id, scope, title, "" if body is None else str(body),
                     status, priority, parent_id, 1 if multi_assignee else 0,
                     _encode(_as_list(assignees)), _encode(deps), now, now,
                     created_by),
                )
                self._record(task_id, "created", at=now, actor=created_by,
                             scope=scope, priority=priority, status_to=status)
                self.db.commit()
            except Exception:
                self.db.rollback()
                raise
        return self.get(task_id)

    def update(self, task_id, title=None, body=None, status=None, priority=None,
               parent_id=..., assignees=None, depends_on=None,
               block_reason=..., updated_by=None, multi_assignee=None):
        """Change any mutable field. ``...`` (Ellipsis) means "leave alone"."""
        with self._lock:
            if self.db.in_transaction:
                self.db.commit()
            self.db.execute("BEGIN IMMEDIATE")
            try:
                row = self._require(task_id)
                task_id = row["id"]
                sets = {}
                now = _now()

                if title is not None:
                    title = str(title).strip()
                    if not title:
                        raise TaskError("a task needs a title")
                    sets["title"] = title
                if body is not None:
                    sets["body"] = str(body)
                if priority is not None:
                    sets["priority"] = validate_priority(priority)
                if parent_id is not ...:
                    parent_id = str(parent_id).strip() if parent_id else None
                    if parent_id:
                        if parent_id == task_id:
                            raise TaskError("a task cannot be its own parent")
                        self._require(parent_id)
                    sets["parent_id"] = parent_id
                if assignees is not None:
                    sets["assignees"] = _encode(_as_list(assignees))
                if multi_assignee is not None:
                    sets["multi_assignee"] = 1 if multi_assignee else 0
                if depends_on is not None:
                    deps = _as_list(depends_on)
                    if task_id in deps:
                        raise TaskError("a task cannot depend on itself")
                    for dep in deps:
                        self._require(dep)
                    sets["depends_on"] = _encode(deps)
                if status is not None:
                    status = validate_status(status)
                    sets["status"] = status
                    if status == "done":
                        sets["completed_by"] = updated_by
                        sets["completed_at"] = now
                        sets["cancelled_at"] = None
                        sets["block_reason"] = None
                    elif status == "cancelled":
                        sets["cancelled_at"] = now
                        sets["block_reason"] = None
                    else:
                        sets["completed_by"] = None
                        sets["completed_at"] = None
                        sets["cancelled_at"] = None
                        if status != "blocked":
                            sets["block_reason"] = None
                if block_reason is not ...:
                    sets["block_reason"] = block_reason

                if not sets:
                    self.db.commit()
                    return self.get(task_id)
                sets["updated_at"] = now
                assignments = ", ".join(f"{key} = ?" for key in sets)
                self.db.execute(
                    f"UPDATE tasks SET {assignments} WHERE id = ?",
                    (*sets.values(), task_id),
                )
                if "status" in sets and sets["status"] != row["status"]:
                    new_status = sets["status"]
                    if new_status == "done":
                        event = "completed"
                        duration = int((now - row["created_at"]) * 1000)
                    elif new_status == "cancelled":
                        event, duration = "cancelled", None
                    elif new_status == "blocked":
                        event, duration = "blocked", None
                    elif row["status"] == "blocked":
                        event, duration = "unblocked", None
                    else:
                        event, duration = "status", None
                    self._record(task_id, event, at=now, actor=updated_by,
                                 scope=row["scope"],
                                 priority=sets.get("priority", row["priority"]),
                                 status_from=row["status"], status_to=new_status,
                                 duration_ms=duration)
                if "priority" in sets and sets["priority"] != row["priority"]:
                    self._record(task_id, "priority", at=now, actor=updated_by,
                                 scope=row["scope"], priority=sets["priority"],
                                 meta={"from": row["priority"], "to": sets["priority"]})
                if "assignees" in sets and sets["assignees"] != row["assignees"]:
                    self._record(task_id, "assigned", at=now, actor=updated_by,
                                 scope=row["scope"], priority=row["priority"],
                                 meta={"assignees": _decode(sets["assignees"])})
                self.db.commit()
            except Exception:
                self.db.rollback()
                raise
        return self.get(task_id)

    def assign(self, task_id, assignees, mode="add"):
        incoming = _as_list(assignees)
        if not incoming:
            raise TaskError("at least one assignee is required")
        mode = (mode or "add").strip().lower()
        if mode not in ("add", "append", "remove", "rm", "set", "replace"):
            raise TaskError("assign mode must be add, remove or set")
        # F4: read-modify-write under one write transaction, so a concurrent
        # assign cannot be lost between the read and the update.
        with self._lock:
            if self.db.in_transaction:
                self.db.commit()
            self.db.execute("BEGIN IMMEDIATE")
            try:
                row = self._require(task_id)
                current = _decode(row["assignees"])
                if mode in ("add", "append"):
                    merged = current + [item for item in incoming if item not in current]
                elif mode in ("remove", "rm"):
                    merged = [item for item in current if item not in incoming]
                else:
                    merged = incoming
                now = _now()
                self.db.execute(
                    "UPDATE tasks SET assignees = ?, updated_at = ? WHERE id = ?",
                    (_encode(merged), now, row["id"]),
                )
                self._record(row["id"], "assigned", at=now,
                             scope=row["scope"], priority=row["priority"],
                             meta={"assignees": merged})
                self.db.commit()
            except Exception:
                self.db.rollback()
                raise
            return self.get(row["id"])

    def complete(self, task_id, by=None):
        row = self._require(task_id)
        if row["status"] == "done":
            return self.get(row["id"])
        return self.update(row["id"], status="done", updated_by=by)

    def block(self, task_id, reason=None):
        return self.update(task_id, status="blocked", block_reason=reason)

    def unblock(self, task_id):
        row = self._require(task_id)
        if row["status"] != "blocked":
            return self.get(row["id"])
        return self.update(row["id"], status="open", block_reason=None)

    def cancel(self, task_id, reason=None):
        return self.update(task_id, status="cancelled", block_reason=reason)

    def delete(self, task_id):
        # F4: the checks and the delete are one write transaction.
        with self._lock:
            if self.db.in_transaction:
                self.db.commit()
            self.db.execute("BEGIN IMMEDIATE")
            try:
                row = self._require(task_id)
                task_id = row["id"]
                children = self.db.execute(
                    "SELECT COUNT(*) AS n FROM tasks WHERE parent_id = ?", (task_id,)
                ).fetchone()["n"]
                if children:
                    raise TaskError(
                        f"task {task_id} has {children} child task(s); "
                        "delete or re-parent them first")
                dependents = [
                    candidate["id"]
                    for candidate in self.db.execute(
                        "SELECT id, depends_on FROM tasks").fetchall()
                    if task_id in _decode(candidate["depends_on"])
                ]
                if dependents:
                    raise TaskError(
                        f"task {task_id} is a dependency of {', '.join(dependents)}")
                self._record(task_id, "deleted", at=_now(), scope=row["scope"],
                             priority=row["priority"], status_from=row["status"])
                self.db.execute("DELETE FROM tasks WHERE id = ?", (task_id,))
                self.db.commit()
            except Exception:
                self.db.rollback()
                raise
        return {"deleted": task_id}

    def note(self, task_id, event="note", actor=None, meta=None):
        """Append a history event without changing the task row (or needing it)."""
        with self._lock:
            self._record(str(task_id), event, at=_now(), actor=actor,
                         meta=meta or {})
            self.db.commit()
        return {"noted": str(task_id), "event": event, "actor": actor,
                "meta": meta or {}}

    # -- reading ------------------------------------------------------------

    def get(self, task_id, pane_id=None):
        return self._full(self._require(task_id), pane_id)

    def _visible_scopes(self, scope, self_pane_id):
        """Which scopes a caller may see.

        Global tasks are visible to everyone. A pane sees its own local tasks.
        Asking for a specific pane sees global + that pane; asking for global
        sees only global.
        """
        scope = None if scope is None else str(scope).strip()
        if scope in (None, "", "all"):
            scopes = [GLOBAL]
            if self_pane_id and self_pane_id != GLOBAL:
                scopes.append(self_pane_id)
            return scopes
        if scope == "self":
            if not self_pane_id:
                raise TaskError("scope 'self' requires a known pane id")
            return [GLOBAL, self_pane_id]
        if scope == GLOBAL:
            return [GLOBAL]
        return [GLOBAL, validate_scope(scope)]

    def list(self, scope=None, status=None, assignee=None, include_done=True,
             limit=200, self_pane_id=None):
        scopes = self._visible_scopes(scope, self_pane_id)
        placeholders = ",".join("?" for _ in scopes)
        where = [f"scope IN ({placeholders})"]
        params = list(scopes)
        if status:
            where.append("status = ?")
            params.append(validate_status(status))
        elif not include_done:
            where.append("status NOT IN ('done', 'cancelled')")
        rows = self.db.execute(
            f"SELECT * FROM tasks WHERE {' AND '.join(where)}", params
        ).fetchall()
        tasks = [self._decorate(row, self_pane_id) for row in rows]
        if assignee:
            assignee = str(assignee).strip()
            tasks = [task for task in tasks if assignee in task["assignees"]]
        tasks.sort(key=self._order_key)
        return tasks[: int(limit)]

    def next(self, scope=None, self_pane_id=None):
        """The next eligible task in the global queue and in a pane queue.

        ``pick`` is the higher-priority of the two. Both are returned so an
        agent can choose its lane without asking a supervisor.
        """
        pane_id = self_pane_id if scope in (None, "", "self") else str(scope).strip()
        if pane_id in (GLOBAL, ""):
            pane_id = None

        global_tasks = [
            task for task in self.list(scope=GLOBAL, include_done=False,
                                       self_pane_id=self_pane_id)
            if task["eligible"]
        ]
        pane_tasks = []
        if pane_id:
            pane_tasks = [
                task for task in self.list(scope=pane_id, include_done=False,
                                           self_pane_id=self_pane_id)
                if task["eligible"] and not task["is_global"]
            ]

        global_next = global_tasks[0] if global_tasks else None
        pane_next = pane_tasks[0] if pane_tasks else None
        pick = None
        for candidate in (global_next, pane_next):
            if candidate and (pick is None or self._order_key(candidate) < self._order_key(pick)):
                pick = candidate
        return {
            "pane_id": pane_id,
            "global": global_next,
            "pane": pane_next,
            "pick": pick,
        }

    # -- atomic claiming, leases and reaping --------------------------------

    def _rows(self, sql, params=()):
        """Rows as sqlite3.Row, whatever the connection's factory says today.

        Observed 2026-10-24 20:08: `leases_for` raised IndexError: tuple index out
        of range, i.e. the rows came back as plain TUPLES though __init__ sets
        row_factory. Two calls a minute later were fine, so the cause is a state
        change on the shared connection (one connection, several threads) rather
        than a bad query. Reading by column name must not depend on that state, so
        the factory is re-asserted and the fact is logged when it had drifted.
        """
        rows = self.db.execute(sql, params).fetchall()
        if rows and not isinstance(rows[0], sqlite3.Row):
            self.db.row_factory = sqlite3.Row
            log.warning("task rows came back without a row factory; re-asserted it")
            return self.db.execute(sql, params).fetchall()
        return rows

    def leases_for(self, task_id, now=None):
        """Live leases for a task (expired rows are ignored)."""
        now = _now() if now is None else now
        rows = self._rows("SELECT * FROM task_leases WHERE task_id = ?", (str(task_id),))
        return [dict(row) for row in rows if row["expires_at"] > now]

    def leases_for_pane(self, pane_id, now=None):
        """Live leases held by a pane."""
        now = _now() if now is None else now
        rows = self.db.execute(
            "SELECT * FROM task_leases WHERE pane_id = ?", (str(pane_id),)
        ).fetchall()
        return [dict(row) for row in rows if row["expires_at"] > now]

    def _claimable(self, task, pane_id):
        """Whether ``pane_id`` may take this task right now."""
        if pane_id and any(lease["pane_id"] == pane_id for lease in task["leases"]):
            return False  # already holds this one
        if task["leases"]:
            return task["multi_assignee"]
        assigned_to_me = bool(pane_id and pane_id in task["assignees"])
        if task["status"] == "open":
            # F2: a named assignee can pull its own task; previously any
            # assignee list made the task unclaimable by everyone.
            return (not task["assignees"] or task["multi_assignee"]
                    or assigned_to_me)
        if task["status"] == "in_progress":
            return task["multi_assignee"] or assigned_to_me
        return False

    def claim_next(self, scope=GLOBAL, pane_id=None, lease_seconds=None,
                   caller=None):
        """Atomically claim the next eligible task in ``scope``.

        The whole read-select-update runs under ``BEGIN IMMEDIATE``. SQLite
        serialises writers across processes, so two agents polling at the same
        instant cannot both win the same exclusive task: the second one sees
        the first one's commit and picks the next task (or nothing). The claim
        carries a lease; ``heartbeat`` extends it and ``reap`` returns expired,
        dead-pane or dead-caller work to the queue.

        ``caller`` is ``{"pid", "start", "name"}`` for the stable agent process
        behind the request. The pane id stays the registry key; the pid/start
        is liveness, so a crashed agent's lease can be reclaimed while its
        shell pane still exists. The name is only a label.
        """
        target = validate_scope(scope)
        if not pane_id:
            raise TaskError("claiming a task requires a caller pane id")
        lease_s = float(lease_seconds or DEFAULT_LEASE_S)
        if lease_s <= 0:
            raise TaskError("lease_seconds must be positive")
        caller = caller or {}
        caller_pid = caller.get("pid")
        caller_start = caller.get("start")
        caller_name = caller.get("name")

        with self._lock:
            if self.db.in_transaction:
                self.db.commit()
            self.db.execute("BEGIN IMMEDIATE")
            try:
                rows = self.db.execute(
                    "SELECT * FROM tasks WHERE scope = ? AND "
                    "status IN ('open', 'in_progress') ORDER BY created_at, id",
                    (target,),
                ).fetchall()
                candidates = [self._decorate(row) for row in rows]
                candidates.sort(key=self._order_key)

                chosen = None
                for task in candidates:
                    if task["blocked_by"]:
                        continue
                    if not self._claimable(task, pane_id):
                        continue
                    chosen = task
                    break

                if chosen is None:
                    self.db.commit()
                    return {"task": None, "scope": target, "pane_id": pane_id}

                assignees = list(chosen["assignees"])
                if pane_id not in assignees:
                    assignees.append(pane_id)
                now = _now()
                expires = now + lease_s
                self.db.execute(
                    "INSERT OR REPLACE INTO task_leases "
                    "(task_id, pane_id, claimed_at, heartbeat_at, expires_at, "
                    "caller_pid, caller_start, caller_name) "
                    "VALUES (?,?,?,?,?,?,?,?)",
                    (chosen["id"], pane_id, now, now, expires,
                     caller_pid, caller_start, caller_name),
                )
                self.db.execute(
                    "UPDATE tasks SET status = 'in_progress', assignees = ?, "
                    "attempts = attempts + 1, updated_at = ?, block_reason = NULL "
                    "WHERE id = ?",
                    (_encode(assignees), now, chosen["id"]),
                )
                self._record(
                    chosen["id"], "claimed", at=now, actor=pane_id,
                    scope=target, priority=chosen["priority"],
                    status_from=chosen["status"], status_to="in_progress",
                    duration_ms=int((now - chosen["created_at"]) * 1000),
                    meta={"lease_seconds": lease_s, "caller_pid": caller_pid,
                          "caller_name": caller_name})
                self.db.commit()
            except Exception:
                self.db.rollback()
                raise
        return {"task": self.get(chosen["id"]), "scope": target,
                "pane_id": pane_id, "lease_seconds": lease_s,
                "expires_at": now + lease_s,
                "caller": {"pid": caller_pid, "start": caller_start,
                           "name": caller_name}}

    def heartbeat(self, task_id, pane_id, lease_seconds=None, caller=None):
        """Extend a live lease. Raises when the caller does not hold one.

        A heartbeat refreshes the caller identity too: the pane's agent may
        have been restarted, and liveness should track the current process.
        """
        lease_s = float(lease_seconds or DEFAULT_LEASE_S)
        now = _now()
        caller = caller or {}
        with self._lock:
            if caller.get("pid"):
                cursor = self.db.execute(
                    "UPDATE task_leases SET heartbeat_at = ?, expires_at = ?, "
                    "caller_pid = ?, caller_start = ?, caller_name = ? "
                    "WHERE task_id = ? AND pane_id = ?",
                    (now, now + lease_s, caller.get("pid"),
                     caller.get("start"), caller.get("name"),
                     str(task_id), str(pane_id)),
                )
            else:
                cursor = self.db.execute(
                    "UPDATE task_leases SET heartbeat_at = ?, expires_at = ? "
                    "WHERE task_id = ? AND pane_id = ?",
                    (now, now + lease_s, str(task_id), str(pane_id)),
                )
            self.db.commit()
        if cursor.rowcount == 0:
            raise TaskError(
                f"no active lease for {pane_id} on {task_id}; claim it first")
        return {"task_id": str(task_id), "pane_id": str(pane_id),
                "expires_at": now + lease_s, "lease_seconds": lease_s}

    def release(self, task_id, pane_id):
        """Give a claim back without completing it."""
        with self._lock:
            if self.db.in_transaction:
                self.db.commit()
            self.db.execute("BEGIN IMMEDIATE")
            try:
                row = self._require(task_id)
                task_id = row["id"]
                now = _now()
                self.db.execute(
                    "DELETE FROM task_leases WHERE task_id = ? AND pane_id = ?",
                    (task_id, str(pane_id)),
                )
                resulting = self._after_lease_change(
                    task_id, row["assignees"], now, pane_id)
                self._record(task_id, "released", at=now, actor=pane_id,
                             scope=row["scope"], priority=row["priority"],
                             status_from=row["status"], status_to=resulting)
                self.db.commit()
            except Exception:
                self.db.rollback()
                raise
        return self.get(task_id)

    def reap(self, alive_panes=None, caller_alive=None, now=None,
             alive_source=None):
        """Return expired, dead-pane or dead-caller leases to the queue.

        ``alive_panes`` is the set of pane ids the terminal backend currently
        knows about. ``caller_alive(pid, start)`` is the process check.

        Only the explicit lease deadline or a dead /proc caller requeues a
        claim. A vanished pane is recorded in the event but is **not** evidence
        by itself: the pane reader can lag, a live agent may still own the
        work, and a legacy lease with no caller identity is unattributable
        rather than dead. A task with another live claimant stays in progress
        for that claimant.
        """
        if alive_panes is not None and alive_source != "live":
            raise TaskError(
                "alive_panes must be tagged alive_source='live'; a fixture "
                "pane list must never move real work")
        now = _now() if now is None else now
        requeued = []
        with self._lock:
            if self.db.in_transaction:
                self.db.commit()
            self.db.execute("BEGIN IMMEDIATE")
            try:
                leases = self.db.execute("SELECT * FROM task_leases").fetchall()
                for lease in leases:
                    expired = lease["expires_at"] <= now
                    gone = (alive_panes is not None
                            and lease["pane_id"] not in alive_panes)
                    dead_caller = bool(
                        caller_alive is not None and lease["caller_pid"]
                        and not caller_alive(lease["caller_pid"],
                                             lease["caller_start"]))
                    # BOTH-condition rule: a live agent behind a vanished pane
                    # is not a dead claim; an unattributable lease is not
                    # guessed dead. Only the lease deadline or /proc caller
                    # death are reasons.
                    if not (expired or dead_caller):
                        continue
                    reason = "caller_dead" if dead_caller else "expired"
                    self.db.execute(
                        "DELETE FROM task_leases WHERE task_id = ? AND pane_id = ?",
                        (lease["task_id"], lease["pane_id"]),
                    )
                    row = self.db.execute(
                        "SELECT * FROM tasks WHERE id = ?", (lease["task_id"],)
                    ).fetchone()
                    if row is None:
                        continue
                    resulting = self._after_lease_change(
                        lease["task_id"], row["assignees"], now,
                        lease["pane_id"], row=row,
                    )
                    # F3: a task with another live lease stays in_progress;
                    # only a task that actually reopened counts as requeued.
                    event = "requeued" if resulting == "open" else "released"
                    self._record(
                        lease["task_id"], event, at=now,
                        actor=lease["pane_id"], scope=row["scope"],
                        priority=row["priority"], status_from=row["status"],
                        status_to=resulting,
                        meta={"reason": reason, "pane_gone": gone,
                              "caller_pid": lease["caller_pid"],
                              "caller_name": lease["caller_name"]})
                    # Only a task that actually reopened is a requeue. A
                    # lease released while another claimant keeps the task in
                    # progress is logged as a release by the events, not here.
                    if resulting == "open":
                        requeued.append({
                            "task_id": lease["task_id"],
                            "pane_id": lease["pane_id"],
                            "reason": reason,
                        })
                self.db.commit()
            except Exception:
                self.db.rollback()
                raise
        return requeued

    def _after_lease_change(self, task_id, raw_assignees, now, pane_id, row=None):
        """Remove one claimant from a task; reopen it if no one is left."""
        assignees = [a for a in _decode(raw_assignees) if a != pane_id]
        remaining = self.db.execute(
            "SELECT COUNT(*) AS n FROM task_leases WHERE task_id = ?", (task_id,)
        ).fetchone()["n"]
        status = row["status"] if row is not None else self._require(task_id)["status"]
        if status not in ("done", "cancelled") and remaining == 0:
            status = "open"
        self.db.execute(
            "UPDATE tasks SET assignees = ?, status = ?, attempts = attempts + 1, "
            "updated_at = ? WHERE id = ?",
            (_encode(assignees), status, now, task_id),
        )
        # F3: tell the caller what actually happened; a task with another live
        # lease stays in_progress and its requeue event must say so.
        return status

    def stats(self, scope=None, self_pane_id=None):
        scopes = self._visible_scopes(scope, self_pane_id)
        placeholders = ",".join("?" for _ in scopes)
        rows = self.db.execute(
            "SELECT scope, status, COUNT(*) AS n FROM tasks "
            f"WHERE scope IN ({placeholders}) GROUP BY scope, status",
            scopes,
        ).fetchall()
        by_scope = {}
        totals = {status: 0 for status in STATUSES}
        for row in rows:
            bucket = by_scope.setdefault(
                row["scope"], {status: 0 for status in STATUSES}
            )
            bucket[row["status"]] = int(row["n"])
            totals[row["status"]] += int(row["n"])
        return {"scopes": scopes, "by_scope": by_scope, "totals": totals}

    def metrics(self, window_s=3600, since=None, until=None, scopes=None,
                bucket="hour", scope_label="all"):
        """Timeseries-friendly task metrics from the append-only event log.

        Current state comes from the tasks table; throughput and latency come
        from task_events in the window. ``scopes`` is None (all) or a list of
        scope strings; callers apply privacy before calling.
        """
        until = _now() if until is None else float(until)
        since = (until - float(window_s)) if since is None else float(since)
        bucket_s = 86400 if str(bucket).lower() == "day" else 3600

        tasks = [self._decorate(row) for row in
                 self.db.execute("SELECT * FROM tasks").fetchall()]
        if scopes is not None:
            tasks = [task for task in tasks if task["scope"] in scopes]

        by_status = {status: 0 for status in STATUSES}
        by_priority = {priority: 0 for priority in PRIORITIES}
        by_scope = {"global": 0, "pane": 0}
        by_assignee = {}
        for task in tasks:
            by_status[task["status"]] = by_status.get(task["status"], 0) + 1
            by_priority[task["priority"]] = by_priority.get(task["priority"], 0) + 1
            by_scope["global" if task["is_global"] else "pane"] += 1
            if task["status"] in ("open", "in_progress", "blocked"):
                for assignee in task["assignees"]:
                    by_assignee[assignee] = by_assignee.get(assignee, 0) + 1

        where = "at >= ? AND at < ?"
        params = [since, until]
        if scopes is not None:
            where += " AND scope IN (%s)" % ",".join("?" for _ in scopes)
            params.extend(scopes)
        events = self.db.execute(
            f"SELECT * FROM task_events WHERE {where} ORDER BY at", params
        ).fetchall()

        counts = Counter(event["event"] for event in events)
        bucket_counts = {}
        for event in events:
            slot = int(event["at"] // bucket_s) * bucket_s
            bucket_counts.setdefault(slot, Counter())[event["event"]] += 1

        def latency(event_name):
            values = sorted(
                int(event["duration_ms"]) for event in events
                if event["event"] == event_name
                and event["duration_ms"] is not None)
            if not values:
                return {"count": 0, "avg": None, "p50": None,
                        "p90": None, "max": None}

            def pick(fraction):
                return values[min(len(values) - 1,
                                  int(round(fraction * (len(values) - 1))))]

            return {"count": len(values),
                    "avg": round(sum(values) / len(values), 1),
                    "p50": pick(0.5), "p90": pick(0.9), "max": values[-1]}

        def actors(event_name):
            counter = Counter(event["actor"] for event in events
                              if event["event"] == event_name and event["actor"])
            return [{"actor": actor, "count": count}
                    for actor, count in counter.most_common()]

        first = self.db.execute(
            "SELECT MIN(at) AS first, MAX(at) AS last, COUNT(*) AS n "
            "FROM task_events").fetchone()

        return {
            "schema_version": 1,
            "generated_at": until,
            "scope": scope_label,
            "window": {
                "since": since, "until": until,
                "seconds": int(until - since),
                "bucket_seconds": bucket_s,
                "bucket": "day" if bucket_s == 86400 else "hour",
            },
            "current": {
                "total": len(tasks),
                "by_status": by_status,
                "by_priority": by_priority,
                "by_scope": by_scope,
                "blocked_vs_in_progress": {
                    "blocked": by_status.get("blocked", 0),
                    "in_progress": by_status.get("in_progress", 0),
                },
                "by_assignee": [
                    {"assignee": assignee, "count": count}
                    for assignee, count in sorted(
                        by_assignee.items(), key=lambda item: (-item[1], item[0]))
                ],
            },
            "throughput": {
                "created": counts.get("created", 0),
                "claimed": counts.get("claimed", 0),
                "completed": counts.get("completed", 0),
                "cancelled": counts.get("cancelled", 0),
                "blocked": counts.get("blocked", 0),
                "unblocked": counts.get("unblocked", 0),
                "requeued": counts.get("requeued", 0),
                "buckets": [
                    {"bucket": slot,
                     "created": counter.get("created", 0),
                     "claimed": counter.get("claimed", 0),
                     "completed": counter.get("completed", 0),
                     "cancelled": counter.get("cancelled", 0),
                     "requeued": counter.get("requeued", 0)}
                    for slot, counter in sorted(bucket_counts.items())
                ],
            },
            "latency_ms": {
                "completion": latency("completed"),
                "claim": latency("claimed"),
            },
            "by_actor": {
                "completed": actors("completed"),
                "claimed": actors("claimed"),
            },
            "coverage": {
                "events_since": first["first"],
                "events_until": first["last"],
                "events_total": int(first["n"] or 0),
            },
        }

    def for_pane(self, pane_id, limit=20):
        """The block inbox_read appends: global + own-pane open work + next."""
        return {
            "pane_id": pane_id,
            "global": self.list(scope=GLOBAL, include_done=False, limit=limit,
                                self_pane_id=pane_id),
            "pane": [
                task for task in self.list(scope=pane_id, include_done=False,
                                           limit=limit, self_pane_id=pane_id)
                if not task["is_global"]
            ],
            "next": self.next(self_pane_id=pane_id),
        }
