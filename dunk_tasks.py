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
from dunk_pm import DEFAULT_VP_PANE as _default_vp_pane
from dunk_sqlite import connect as sqlite_connect
# t423: in-transaction privilege revalidation must share the locked-read
# protocol and matching semantics with the allowlist store (single source of
# truth); dunk_superusers imports only stdlib, so this edge is acyclic.
from dunk_superusers import (locked_allowlist, match_allowlist,
                             read_panes_locked)

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
DEFAULT_PRIORITY = "low"
# How long a claim stays valid without a heartbeat. A dead or stalled agent's
# task returns to the queue after this, or as soon as its pane disappears.
DEFAULT_LEASE_S = float(os.environ.get("DUNKINGSHEEP_LEASE_S", "1800"))

# Pane ids are "<workspace>:<pane>" (w8:p3). Scope is either "global", "self"
# (resolved by the caller) or a pane id.
# A pane id is whatever the current backend calls a pane: herdr's
# 'workspace:pane' (w8:p8) or tmux's '%N'. Refusing tmux ids made every
# per-pane task scope unusable the day the backend flipped (2026-10-24).
_PANE_ID = re.compile(r"^(?:[A-Za-z0-9_.-]+:[A-Za-z0-9_.-]+|%\d+)$")

# Task kinds (docs/FLAT_FLEET.md 5). `work` is what a worker lane pulls;
# `review` is filed by the review timer for a PR head and pulled only by a
# reviewer lane that is not its author; `verify` and `report` are reserved.
KINDS = ("work", "review", "verify", "report")
DEFAULT_KIND = "work"
VERDICTS = ("accept", "needs-fix")

SCHEMA = """
CREATE TABLE IF NOT EXISTS tasks (
    id            TEXT PRIMARY KEY,
    scope         TEXT NOT NULL,
    title         TEXT NOT NULL,
    body          TEXT NOT NULL DEFAULT '',
    status        TEXT NOT NULL DEFAULT 'open',
    priority      TEXT NOT NULL DEFAULT 'low',
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
    block_reason  TEXT,
    kind          TEXT NOT NULL DEFAULT 'work',
    author        TEXT,
    repo          TEXT,
    pr            INTEGER,
    head          TEXT,
    verdict       TEXT,
    accounting_pr INTEGER
);
CREATE INDEX IF NOT EXISTS tasks_by_scope
    ON tasks (scope, status, created_at);
CREATE INDEX IF NOT EXISTS tasks_by_parent ON tasks (parent_id);
-- One live review per PR head (tasks_one_review_per_head) is created in
-- _migrate, after the kind/repo/pr/head columns exist on older databases: a
-- duplicate is refused by the store, not by a check in the timer (the CoS
-- filed twelve identical reviews for PR #117).
CREATE TABLE IF NOT EXISTS task_leases (
    task_id     TEXT NOT NULL,
    pane_id     TEXT NOT NULL,
    claimed_at  REAL NOT NULL,
    heartbeat_at REAL NOT NULL,
    expires_at  REAL NOT NULL,
    caller_pid   INTEGER,
    caller_start TEXT,
    caller_name  TEXT,
    -- 1 when _claim_locked added this pane to tasks.assignees, 0 when the pane
    -- was already an assignee (the owner's own assignment). release() removes
    -- only what the claim added, so giving a claim back returns the task to the
    -- queue instead of stranding it behind an assignee nobody chose (t595).
    assignee_added INTEGER NOT NULL DEFAULT 0,
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


def validate_kind(kind):
    kind = str(kind or DEFAULT_KIND).strip().lower()
    if kind not in KINDS:
        raise TaskError(f"kind must be one of {', '.join(KINDS)}; got {kind!r}")
    return kind


def validate_verdict(verdict):
    verdict = str(verdict or "").strip().lower()
    if verdict not in VERDICTS:
        raise TaskError(f"verdict must be one of {', '.join(VERDICTS)}; got {verdict!r}")
    return verdict


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
        # thread ident -> connection. A dict, not a list: pruning needs to know whose
        # connection it is closing.
        self._connections = {}
        self._connections_lock = threading.Lock()
        self.db = self._connect()          # the creating thread's connection
        with self._lock:
            self.db.executescript(SCHEMA)
            self._migrate()
            self.db.commit()

    def _connect(self):
        """This thread's connection, opened on first use and CLOSED when the thread ends.

        Pruning happens on EVERY access, including the early return for a thread that
        already has one (reviewer i27680: pruning only on first-open left a burst-then-quiet
        workload holding every dead thread's connection).

        A per-thread connection that is never closed is a leak the moment threads are
        short-lived, and the daemon's are: an outbox worker, a push worker and a reaper
        per pane. Measured 2026-10-25/26: 5,478 open handles on tasks.db with nine live
        threads, ~11 per minute, and the Errno 24 that broke 'send-keys'. So the pool is
        PRUNED on every access: any connection whose thread is no longer alive is closed
        and dropped. The bound is then the number of LIVE threads, not the number of
        threads the process has ever had.
        """
        connection = getattr(self._local, "db", None)
        if connection is not None:
            # PRUNE BEFORE THE EARLY RETURN (reviewer, i27680): returning straight away
            # meant the pool was only cleaned when a NEW thread opened its first
            # connection, so a burst-then-quiet pattern kept every dead thread's
            # connection open. The check is a scan of a dict that stays small BECAUSE it
            # is pruned here.
            with self._connections_lock:
                self._prune_locked(current=id(threading.current_thread()))
            return connection
        connection = sqlite_connect(self.path, check_same_thread=False)
        connection.row_factory = sqlite3.Row
        self._local.db = connection
        # KEYED BY THE THREAD OBJECT, not by its ident: idents are REUSED, and a reused
        # ident overwrote the previous entry in a dict keyed by ident - dropping that
        # connection without closing it. That was the leak inside the fix (21 fds open,
        # 2 dict entries, measured 2026-10-26).
        thread = threading.current_thread()
        with self._connections_lock:
            self._connections[id(thread)] = (thread, connection)
            self._prune_locked(current=id(thread))
        return connection

    def _prune_locked(self, current=None):
        """Close connections whose owning thread has exited. Caller holds the lock."""
        for key, (thread, connection) in list(self._connections.items()):
            if key == current or thread.is_alive():
                continue
            self._connections.pop(key, None)
            try:
                connection.close()
            except sqlite3.Error:
                pass

    def connection_count(self):
        """How many connections are open right now, WITHOUT pruning.

        Measuring must not repair the thing being measured: the reviewer's point (i27680)
        is that a counter which prunes as a side effect makes a leaking pool look clean
        exactly when you look at it.
        """
        with self._connections_lock:
            return len(self._connections)

    def __getattribute__(self, name):
        if name == "db":
            return object.__getattribute__(self, "_connect")()
        return object.__getattribute__(self, name)

    def _migrate(self):
        """Add columns introduced after the first database was created."""
        columns = {row["name"] for row in self.db.execute("PRAGMA table_info(tasks)")}
        for name, ddl in (("multi_assignee", "INTEGER NOT NULL DEFAULT 0"),
                          ("attempts", "INTEGER NOT NULL DEFAULT 0"),
                          ("kind", "TEXT NOT NULL DEFAULT 'work'"),
                          ("author", "TEXT"), ("repo", "TEXT"), ("pr", "INTEGER"),
                          ("head", "TEXT"), ("verdict", "TEXT"),
                          ("accounting_pr", "INTEGER")):
            if name not in columns:
                self.db.execute(f"ALTER TABLE tasks ADD COLUMN {name} {ddl}")
        # The partial unique index needs the columns above to exist first.
        self.db.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS tasks_one_review_per_head "
            "ON tasks (kind, repo, pr, head) WHERE kind = 'review' AND head IS NOT NULL "
            "AND status IN ('open', 'in_progress', 'blocked')")
        lease_columns = {row["name"] for row in self.db.execute(
            "PRAGMA table_info(task_leases)")}
        for name, ddl in (("caller_pid", "INTEGER"), ("caller_start", "TEXT"),
                          ("caller_name", "TEXT"),
                          ("assignee_added", "INTEGER NOT NULL DEFAULT 0")):
            if name not in lease_columns:
                self.db.execute(f"ALTER TABLE task_leases ADD COLUMN {name} {ddl}")

    def close(self):
        """Close every thread's connection (the store is shared by the daemon)."""
        with self._connections_lock:
            connections = [connection for _thread, connection
                           in self._connections.values()]
            self._connections = {}
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

    NEXT_JOB_EVENT = "next_job_handoff"

    # t461: the ONLY handoff state that stops blocking, compared EXACTLY.
    #
    # Reviewer t462 [P2]: this used to normalize with strip().lower(), while the
    # delivery worker (dunk_next_job.NextJobs.tick/deliver) skips only the exact
    # string "cancelled". So a raw state of "CANCELLED" or " cancelled " was
    # treated as terminal HERE and as EXECUTABLE there: the claim gate advanced
    # to new work while the worker still typed the old prompt - two sends for
    # one intent. The two readers must agree, and the cheap way to agree is to
    # compare the same bytes. Normalizing in both places would be a contract
    # spread across files; comparing exactly keeps it in one.
    #
    # Everything else blocks - pending, sending, sent, started, failed,
    # uncertain, undeliverable, a case/whitespace variant of "cancelled", an
    # empty state, and any state this code does not know - because an
    # unrecognised state might still become a typed prompt and the cost of
    # guessing wrong is a second nudge in a working pane.
    NON_BLOCKING_HANDOFF_STATES = frozenset({"cancelled"})

    def record_next_job_handoff(self, task_id, *, pane_id, from_task, message_id=None,
                                attempt=0, reason=None, now=None, details=None):
        """Record that `pane_id` was handed `task_id` after completing `from_task` (t372).

        This is the durable record clauses 1 and 4 are checked against, so it is written
        with a SHAPE the acceptance instrument can validate from outside:

            event  = 'next_job_handoff'   (NOT 'handoff' - that name already means a PR
                                           handoff; 3 such rows exist from 2026-10-25)
            meta   = {from_task, message_id, attempt, reason}

        A record without from_task AND message_id does not count as a handoff, so a stray
        `task note --event handoff "handed to reviewer"` cannot satisfy clause 4. The
        event name is unique and the shape is required: one protects against accidents,
        the other against a future lane deciding the name is convenient.
        """
        now = _now() if now is None else now
        task = self.get(task_id)
        meta = {"from_task": str(from_task) if from_task else None,
                "message_id": str(message_id) if message_id else None,
                "attempt": int(attempt or 0),
                "reason": reason}
        meta.update(details or {})
        self._record(task_id, self.NEXT_JOB_EVENT, at=now, actor=str(pane_id or ""),
                     scope=task.get("scope"), priority=task.get("priority"),
                     status_from=None, status_to=task.get("status"), meta=meta)
        return {"task_id": str(task_id), "pane_id": str(pane_id or ""),
                "from_task": meta["from_task"], "message_id": meta["message_id"],
                "attempt": meta["attempt"], "reason": reason, "at": now}

    def _fire(self, task_id, event, **extra):
        """Run `on_event` callbacks after a commit. Callbacks must be cheap and
        must not write the task store; a failing callback is logged, never raised."""
        for callback in list(getattr(self, "on_event", ()) or ()):
            try:
                callback(task_id, event, **extra)
            except Exception:  # noqa: BLE001 - observers never break the store
                log.exception("task on_event callback failed task=%s event=%s",
                              task_id, event)

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
        try:
            offer = self.db.execute("SELECT owners FROM allocation_offers WHERE task_id=? AND state='ready'",
                                    (task["id"],)).fetchone()
        except sqlite3.OperationalError as exc:
            if "no such table" not in str(exc):
                raise
            offer = None
        task["allocation_waiting"] = bool(offer and sorted(task["assignees"]) == _decode(offer["owners"]))
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
            depends_on=None, created_by=None, multi_assignee=False,
            kind=DEFAULT_KIND, author=None, repo=None, pr=None, head=None):
        scope = validate_scope(scope)
        title = str(title or "").strip()
        if not title:
            raise TaskError("a task needs a title")
        status = validate_status(status or DEFAULT_STATUS)
        priority = validate_priority(priority or DEFAULT_PRIORITY)
        kind = validate_kind(kind)
        author = str(author).strip() if author else None
        repo = str(repo).strip() if repo else None
        head = str(head).strip() if head else None
        if pr not in (None, ""):
            try:
                pr = int(pr)
            except (TypeError, ValueError):
                raise TaskError(f"pr must be a number; got {pr!r}") from None
        else:
            pr = None
        if kind == "review" and not (repo and pr and head):
            raise TaskError("a review task needs repo, pr and head (the exact commit "
                            "under review); the verdict is bound to that head")

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
                try:
                    self.db.execute(
                        "INSERT INTO tasks (id, scope, title, body, status, priority, "
                        "parent_id, multi_assignee, assignees, depends_on, created_at, "
                        "updated_at, created_by, kind, author, repo, pr, head) "
                        "VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                        (task_id, scope, title, "" if body is None else str(body),
                         status, priority, parent_id, 1 if multi_assignee else 0,
                         _encode(_as_list(assignees)), _encode(deps), now, now,
                         created_by, kind, author, repo, pr, head),
                    )
                except sqlite3.IntegrityError as error:
                    if "UNIQUE constraint failed: tasks.kind" not in str(error):
                        raise
                    existing = self.db.execute(
                        "SELECT id FROM tasks WHERE kind = 'review' AND repo = ? AND pr = ? "
                        "AND head = ? AND status IN ('open', 'in_progress', 'blocked')",
                        (repo, pr, head)).fetchone()
                    raise TaskError(
                        f"a live review task for {repo}#{pr}@{head[:12]} already exists"
                        f"{' (' + existing['id'] + ')' if existing else ''}") from None
                self._record(task_id, "created", at=now, actor=created_by,
                             scope=scope, priority=priority, status_to=status,
                             meta={"kind": kind, "author": author} if kind != "work"
                             or author else None)
                self.db.commit()
            except Exception:
                self.db.rollback()
                raise
        self._fire(task_id, "created")
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

    def complete(self, task_id, by=None, verdict=None):
        row = self._require(task_id)
        if row["status"] == "done":
            return self.get(row["id"])
        if verdict is not None:
            self.set_verdict(row["id"], verdict, by=by)
        result = self.update(row["id"], status="done", updated_by=by)
        self._fire(row["id"], "completed", by=by, kind=row["kind"] if "kind" in row.keys() else None)
        return result

    def set_verdict(self, task_id, verdict, by=None):
        """Record a review verdict. Written by the daemon from the bound reviewer
        (Flock.task_complete), never from a free-text note."""
        verdict = validate_verdict(verdict)
        with self._lock:
            if self.db.in_transaction:
                self.db.commit()
            self.db.execute("BEGIN IMMEDIATE")
            try:
                self.db.execute("UPDATE tasks SET verdict = ?, updated_at = ? WHERE id = ?",
                                (verdict, _now(), str(task_id)))
                self._record(task_id, "verdict", actor=by, meta={"verdict": verdict})
                self.db.commit()
            except Exception:
                self.db.rollback()
                raise
        return verdict

    def set_accounting_pr(self, task_id, number):
        """Record the accounting PR a review's accept verdict opened (FLAT_FLEET 6.4).
        The body PR stays in `pr`; both numbers are then on the one review task."""
        with self._lock:
            row = self._require(task_id)
            self.db.execute(
                "UPDATE tasks SET accounting_pr = ?, updated_at = ? WHERE id = ?",
                (int(number), _now(), str(task_id)))
            self._record(str(task_id), "accounting", at=_now(), scope=row["scope"],
                         priority=row["priority"], meta={"accounting_pr": int(number),
                                                          "body_pr": row["pr"]})
            self.db.commit()
        return int(number)

    def notes(self, task_id, event="note"):
        """Every note event on a task, oldest first: [{at, actor, meta, text}].

        A review verdict has to be able to read the evidence the reviewer
        recorded, because "see the task notes" is only true if there are notes
        (t621: two verdicts were filed with none, and the fix tasks they created
        had nothing to work from)."""
        rows = self.db.execute(
            "SELECT at, actor, meta FROM task_events WHERE task_id = ? AND event = ? "
            "ORDER BY at, id", (str(task_id), event)).fetchall()
        out = []
        for row in rows:
            try:
                meta = json.loads(row["meta"] or "{}")
            except ValueError:
                meta = {}
            text = meta.get("reason") or meta.get("text") or meta.get("note") or ""
            out.append({"at": row["at"], "actor": row["actor"], "meta": meta,
                        "text": str(text)})
        return out

    def next_job_handoffs(self, pane_id=None):
        """Latest durable state of each handoff, including terminal receipts."""
        rows = self.db.execute(
            "SELECT * FROM task_events WHERE event = ? ORDER BY id",
            (self.NEXT_JOB_EVENT,)).fetchall()
        latest = {}
        for row in rows:
            meta = json.loads(row["meta"] or "{}")
            if not meta.get("message_id") or "state" not in meta:
                continue  # older notes are not executable intents
            if pane_id is not None and row["actor"] != pane_id:
                continue
            latest[meta["message_id"]] = dict(meta, task_id=row["task_id"],
                                              pane_id=row["actor"], at=row["at"])
        return list(latest.values())

    def active_handoffs(self, pane_id, owned_task_ids):
        """Handoffs that still count as "a prompt is on its way" for this pane.

        t461: a CANCELLED historical intent is excluded. Before this, any
        historical handoff naming a still-owned open task kept
        `pending_delivery` true forever, so the pane never advanced to its next
        eligible job even with no lease, no outbox row and no held work - a
        terminal intent vetoing live work.

        Everything else still blocks (fail closed on unknown states).
        """
        owned = set(owned_task_ids or ())
        active = []
        for handoff in self.next_job_handoffs(pane_id):
            if handoff["task_id"] not in owned:
                continue
            # Exact comparison, deliberately: see NON_BLOCKING_HANDOFF_STATES.
            state = handoff.get("state")
            if isinstance(state, str) and state in self.NON_BLOCKING_HANDOFF_STATES:
                continue
            active.append(handoff)
        return active

    def advance_next_job(self, pane_id, *, trigger, from_task=None, caller=None,
                         unread=0, pending_delivery=False, dispatch_reason=None,
                         operator_proof=None):
        """Complete A, choose/claim B and journal its inbox id in ONE transaction.

        Clauses 1/4: every trigger uses the same decision while holding SQLite's
        writer lock. No socket or terminal I/O occurs inside this transaction.
        The journal is an outbox intent: inbox materialisation is idempotent.
        """
        import uuid
        from dunk_dispatch import next_job_plan
        caller = caller or {}
        with self._lock:
            self.db.execute("BEGIN IMMEDIATE")
            try:
                now = _now()
                completion = None
                if trigger == "completion":
                    row = self._require(from_task)
                    if row["status"] == "done":
                        self.db.commit()
                        return {"action": "skip", "reason": "already_completed",
                                "task": self.get(from_task)}
                    # P2 (t423 REVISE): ownership is rechecked on the freshly-read
                    # row INSIDE this transaction, not just by the caller before it:
                    # a reassignment landing between the wrapper guard and this UPDATE
                    # must not complete as the removed owner. Operator privilege is
                    # revalidated here too, on a fresh allowlist read serialized
                    # against save() replacement (local file lock only - no
                    # socket/terminal I/O in this transaction): a revocation landing
                    # in the same window must not complete on a stale read.
                    owners = set(_decode(row["assignees"]))
                    owners.update(lease["pane_id"]
                                  for lease in self.leases_for(from_task, now)
                                  if lease.get("pane_id"))
                    proof = operator_proof or {}
                    fresh_operator = False
                    allowlist_path = proof.get("allowlist_path")
                    if (allowlist_path and proof.get("pane_id")
                            and proof.get("pane_id") == pane_id):
                        try:
                            panes, names = read_panes_locked(allowlist_path)
                            fresh_operator = match_allowlist(
                                panes, names, pane_id, proof.get("pane_info"))
                        except Exception:             # noqa: BLE001 - a corrupt
                            fresh_operator = False    # allowlist grants nothing
                    if pane_id not in owners and not fresh_operator:
                        raise TaskError(
                            f"task {from_task} is not owned by {pane_id} (owners: "
                            f"{', '.join(sorted(owners)) or 'no one recorded'}); "
                            "completion refused with no effects")
                    self.db.execute(
                        "UPDATE tasks SET status='done', completed_at=?, completed_by=?, "
                        "updated_at=?, cancelled_at=NULL, block_reason=NULL WHERE id=?",
                        (now, pane_id, now, from_task))
                    self._record(from_task, "completed", at=now, actor=pane_id,
                                 scope=row["scope"], priority=row["priority"],
                                 status_from=row["status"], status_to="done",
                                 duration_ms=int((now - row["created_at"]) * 1000))
                    completion = now
                elif from_task:
                    completion = self._require(from_task)["completed_at"]
                else:
                    previous = self.db.execute(
                        "SELECT task_id,at FROM task_events WHERE event='completed' "
                        "AND actor=? ORDER BY id DESC LIMIT 1", (pane_id,)).fetchone()
                    if previous:
                        from_task, completion = previous["task_id"], previous["at"]

                # A concurrent end/restart sees the committed handoff and cannot
                # advance past it, even when delivery failed or its lease expired.
                rows = self.db.execute(
                    "SELECT * FROM tasks WHERE scope IN (?, ?) "
                    "AND status IN ('open','in_progress') ORDER BY created_at,id",
                    (GLOBAL, pane_id)).fetchall()
                tasks = [self._decorate(r, pane_id) for r in rows]
                owned = {t["id"] for t in tasks if pane_id in t["assignees"]}
                active = self.active_handoffs(pane_id, owned)
                held = any(t["status"] == "in_progress" and pane_id in t["claimed_by"]
                           for t in tasks)
                plan = next_job_plan(trigger=trigger, pane_id=pane_id, tasks=tasks,
                                     unread=unread, pending_delivery=(pending_delivery or
                                                                     bool(active) or held),
                                     now=now, dispatch_reason=dispatch_reason)
                plan.update(from_task=from_task, completion_at=completion, caller=caller)
                if plan["action"] == "claim":
                    chosen = next(t for t in tasks if t["id"] == plan["task_id"])
                    # Revalidate the policy choice with the existing claim rules.
                    if not self._claimable(chosen, pane_id):
                        plan.update(action="skip", reason="claim_unavailable")
                    else:
                        self._claim_locked(chosen, pane_id, DEFAULT_LEASE_S, caller, _now())
                        handoff = dict(from_task=from_task, completion_at=completion,
                                       message_id="nj-" + uuid.uuid4().hex, attempt=0,
                                       reason=None, state="pending", trigger=trigger,
                                       caller=caller)
                        self.record_next_job_handoff(
                            chosen["id"], pane_id=pane_id, from_task=from_task,
                            message_id=handoff["message_id"], now=_now(), details=handoff)
                        plan["handoff"] = dict(handoff, task_id=chosen["id"], pane_id=pane_id)
                self._record(from_task or "", "next_job_decision", at=now,
                             actor=pane_id, meta={k: v for k, v in plan.items() if k != "handoff"})
                proof = operator_proof or {}
                allowlist_path = proof.get("allowlist_path")
                if (trigger == "completion" and allowlist_path and pane_id
                        and from_task):
                    # LATE authorization, held through commit (t423/t430): re-read
                    # ownership plus the allowlist under SH and keep it held until
                    # the commit below, so no writer lands between verification and
                    # the durable decision (or its rollback). Refusal here rolls
                    # back status, events and handoff together - zero effects.
                    with locked_allowlist(allowlist_path) as (panes, names):
                        late_row = self._require(from_task)
                        late_owners = set(_decode(late_row["assignees"]))
                        late_owners.update(
                            lease["pane_id"]
                            for lease in self.leases_for(from_task, _now())
                            if lease.get("pane_id"))
                        if (pane_id not in late_owners
                                and not match_allowlist(panes, names, pane_id,
                                                        proof.get("pane_info"))):
                            raise TaskError(
                                f"task {from_task} authorization changed during "
                                f"completion (owners now: {', '.join(sorted(late_owners)) or 'no one recorded'}); "
                                "completion refused with no effects")
                        self.db.commit()
                else:
                    self.db.commit()
            except Exception:
                self.db.rollback()
                raise
        if trigger == "completion":
            plan["task"] = self.get(from_task)
        return plan

    def update_next_job(self, handoff, *, expected=None, **changes):
        """Append progress under the writer lock, preserving the immutable chain."""
        with self._lock:
            self.db.execute("BEGIN IMMEDIATE")
            try:
                latest = next(h for h in self.next_job_handoffs(handoff["pane_id"])
                              if h["message_id"] == handoff["message_id"])
                if expected and any(latest.get(k) != v for k, v in expected.items()):
                    self.db.commit()
                    return None
                latest.update(changes)
                self.record_next_job_handoff(
                    latest["task_id"], pane_id=latest["pane_id"],
                    from_task=latest["from_task"], message_id=latest["message_id"],
                    attempt=latest["attempt"], reason=latest.get("reason"),
                    details={k: v for k, v in latest.items()
                             if k not in ("task_id", "pane_id", "at")})
                self.db.commit()
                return latest
            except Exception:
                self.db.rollback()
                raise

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

    def recent_notes(self, task_id, limit=3):
        """The last `limit` events that carry a reason, oldest first (t605).

        Evidence a lane wrote with `task note` lived only in task_events, so
        `task read` showed a fully-reviewed task as if nobody had looked at it - the
        reviewer's evidence was invisible to the next reader and the queue re-nudged
        finished work. This is the read side of that record: no new storage, no
        mutation, and a caller that asks for 0 gets none.
        """
        limit = int(limit or 0)
        if limit <= 0:
            return []
        rows = self.db.execute(
            "SELECT event, at, actor, meta FROM task_events WHERE task_id = ? "
            "AND meta IS NOT NULL ORDER BY id DESC LIMIT ?",
            (str(task_id), limit * 4)).fetchall()
        out = []
        for row in rows:
            try:
                meta = json.loads(row["meta"] or "{}")
            except (TypeError, ValueError):
                continue
            reason = str((meta or {}).get("reason") or "").strip()
            if not reason:
                continue
            out.append({"event": row["event"], "at": row["at"],
                        "actor": row["actor"], "reason": reason})
            if len(out) >= limit:
                break
        return list(reversed(out))

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
             limit=200, self_pane_id=None, kind=None, needs_owner=False):
        scopes = self._visible_scopes(scope, self_pane_id)
        placeholders = ",".join("?" for _ in scopes)
        where = [f"scope IN ({placeholders})"]
        params = list(scopes)
        if status:
            where.append("status = ?")
            params.append(validate_status(status))
        elif not include_done:
            where.append("status NOT IN ('done', 'cancelled')")
        if kind:
            where.append("kind = ?")
            params.append(validate_kind(kind))
        if needs_owner:
            # The one way a lane asks the owner anything (FLAT_FLEET.md 5).
            where.append("status = 'blocked' AND block_reason LIKE 'needs-owner:%'")
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
        if task.get("allocation_waiting"):
            return False
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

    def _busy_task_ids(self, pane_id, now):
        """Ids of in-progress tasks this pane holds a live lease on."""
        rows = self.db.execute(
            "SELECT DISTINCT lease.task_id FROM task_leases AS lease "
            "JOIN tasks AS task ON task.id = lease.task_id "
            "WHERE lease.pane_id = ? AND lease.expires_at > ? "
            "AND task.status = 'in_progress' ORDER BY lease.task_id",
            (str(pane_id), now),
        ).fetchall()
        return [row["task_id"] for row in rows]

    def _refusal(self, candidates, pane_id):
        """Why nothing was claimable, as a reason plus the evidence for it."""
        ids = [task["id"] for task in candidates]
        if not candidates:
            return {"reason": "empty", "task_ids": []}
        blocked = [task["id"] for task in candidates if task["blocked_by"]]
        if len(blocked) == len(candidates):
            return {"reason": "blocked", "task_ids": blocked}
        owned = [task for task in candidates if task["assignees"]
                 and not task["multi_assignee"] and pane_id not in task["assignees"]]
        if owned:
            return {"reason": "owned_elsewhere",
                    "task_ids": [task["id"] for task in owned],
                    "owned_by": sorted({name for task in owned
                                        for name in task["assignees"]})}
        leased = [task["id"] for task in candidates if task["leases"]]
        if leased:
            return {"reason": "leased", "task_ids": leased}
        return {"reason": "not_claimable", "task_ids": ids}

    def _log_refusal(self, pane_id, target, refused):
        log.info("task claim refused: pane=%s scope=%s reason=%s tasks=%s",
                 pane_id, target, refused["reason"],
                 ",".join(refused.get("task_ids") or []) or "-")

    def claim_next(self, scope=GLOBAL, pane_id=None, lease_seconds=None,
                   caller=None, allow_busy=False, kinds=None, exclude_author=None):
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

        A pane already holding in-progress work (a live lease) is REFUSED a
        second task unless ``allow_busy`` is set: parallel work is an explicit
        decision, not something a poll can take by accident (t145). Every
        refusal returns ``refused`` with a reason and is logged, so "nothing to
        do" and "you were refused" never look alike.
        """
        mine_only = str(scope or "").strip() == "mine"
        if mine_only:
            # t425: explicit own-assigned-work selection. Scope picks the
            # queue; ownership (assignees) is a separate axis, so a pane id
            # as the positional is never reinterpreted as "my work" here.
            # "mine" claims only tasks that name this pane in assignees,
            # from the global queue and the pane's own queue. Unowned leader
            # backlog and other lanes' tasks are never candidates.
            if not pane_id:
                raise TaskError("scope 'mine' requires a known caller pane")
            target = "mine"
        else:
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
                if mine_only:
                    rows = self.db.execute(
                        "SELECT * FROM tasks WHERE scope IN (?, ?) AND "
                        "status IN ('open', 'in_progress') "
                        "ORDER BY created_at, id",
                        (GLOBAL, pane_id),
                    ).fetchall()
                    candidates = [self._decorate(row) for row in rows
                                  if pane_id in _decode(row["assignees"])]
                else:
                    rows = self.db.execute(
                        "SELECT * FROM tasks WHERE scope = ? AND "
                        "status IN ('open', 'in_progress') ORDER BY created_at, id",
                        (target,),
                    ).fetchall()
                    candidates = [self._decorate(row) for row in rows]
                candidates.sort(key=self._order_key)

                now = _now()
                if not allow_busy:
                    busy = self._busy_task_ids(pane_id, now)
                    if busy:
                        self.db.commit()
                        refused = {"reason": "busy", "task_ids": busy}
                        self._log_refusal(pane_id, target, refused)
                        return {"task": None, "scope": target,
                                "pane_id": pane_id, "refused": refused}

                chosen = None
                wanted = set(kinds) if kinds else None
                for task in candidates:
                    if task["blocked_by"]:
                        continue
                    # Role filters (FLAT_FLEET.md 5): a reviewer pulls only reviews
                    # it did not author; a worker never pulls a review.
                    if wanted is not None and task.get("kind", DEFAULT_KIND) not in wanted:
                        continue
                    if exclude_author and task.get("author") == exclude_author:
                        continue
                    if not self._claimable(task, pane_id):
                        continue
                    chosen = task
                    break

                if chosen is None:
                    refused = self._refusal(candidates, pane_id)
                    self.db.commit()
                    self._log_refusal(pane_id, target, refused)
                    return {"task": None, "scope": target,
                            "pane_id": pane_id, "refused": refused}

                self._claim_locked(chosen, pane_id, lease_s, caller, now)
                self.db.commit()
            except Exception:
                self.db.rollback()
                raise
        return {"task": self.get(chosen["id"]), "scope": target,
                "pane_id": pane_id, "lease_seconds": lease_s,
                "expires_at": now + lease_s,
                "caller": {"pid": caller_pid, "start": caller_start,
                           "name": caller_name}}

    def _claim_locked(self, chosen, pane_id, lease_s, caller, now):
        """Shared atomic lease write; caller owns BEGIN IMMEDIATE."""
        target = chosen["scope"]
        caller_pid, caller_start, caller_name = (caller.get(k) for k in ("pid", "start", "name"))
        assignees = list(chosen["assignees"])
        added_assignee = pane_id not in assignees
        if added_assignee:
            assignees.append(pane_id)
        expires = now + lease_s
        self.db.execute(
            "INSERT OR REPLACE INTO task_leases "
            "(task_id, pane_id, claimed_at, heartbeat_at, expires_at, "
            "caller_pid, caller_start, caller_name, assignee_added) "
            "VALUES (?,?,?,?,?,?,?,?,?)",
            (chosen["id"], pane_id, now, now, expires,
             caller_pid, caller_start, caller_name, 1 if added_assignee else 0),
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
        """Give a claim back without completing it.

        A claim that ADDED its pane to the assignee list takes it away again on
        release. `_after_lease_change` deliberately keeps assignees (a lease is
        activity, not an assignment), but the claim itself writes an assignee, and
        leaving that behind made a released task unclaimable by everyone: open +
        assignees + no multi_assignee means only the named pane may pull it, and
        that pane just said no (t595, live on t568). An assignment the owner made
        before the claim is untouched, because the lease records which one this is.
        """
        with self._lock:
            if self.db.in_transaction:
                self.db.commit()
            self.db.execute("BEGIN IMMEDIATE")
            try:
                row = self._require(task_id)
                task_id = row["id"]
                now = _now()
                lease = self.db.execute(
                    "SELECT assignee_added FROM task_leases "
                    "WHERE task_id = ? AND pane_id = ?",
                    (task_id, str(pane_id)),
                ).fetchone()
                added = bool(lease["assignee_added"]) if lease is not None else False
                self.db.execute(
                    "DELETE FROM task_leases WHERE task_id = ? AND pane_id = ?",
                    (task_id, str(pane_id)),
                )
                assignees = _decode(row["assignees"])
                if added:
                    assignees = [item for item in assignees if item != str(pane_id)]
                resulting = self._after_lease_change(
                    task_id, _encode(assignees), now, pane_id)
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
                        # The claimant is an owner too (claim appends it), and
                        # it is the common case: its lease ended and its pane is
                        # gone. Filtering it out hid exactly that (t145/i32091).
                        owners = _decode(row["assignees"])
                        if (alive_panes is not None and owners
                                and not any(owner in alive_panes for owner in owners)):
                            # Advisory only: ownership is durable, so a dead
                            # owner needs an explicit reassignment, not a lease
                            # side effect. Nothing is mutated here.
                            log.info("task %s reopened while owned by a pane that "
                                     "is not live: owners=%s",
                                     lease["task_id"], ",".join(owners))
                self.db.commit()
            except Exception:
                self.db.rollback()
                raise
        return requeued

    def _after_lease_change(self, task_id, raw_assignees, now, pane_id, row=None):
        """Reopen a task when its last lease goes away; keep the OWNER.

        A lease is activity: expiry, release and a reaped claim all end the
        claim, not the assignment. Owner2317 found ten global tasks whose
        assignees were deleted 30 minutes after a claim expired, which let the
        next ``task next`` take work nobody had released. The assignee list is
        therefore preserved here; only ``pane_id``'s lease is gone (the caller
        has already deleted the lease row), and the task reopens for its owner
        to reclaim. Re-owning work is an explicit ``assign``/``complete``
        decision, never a side effect of a lease ending (t145).
        """
        assignees = _decode(raw_assignees)
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
        leader_held = []
        for task in tasks:
            by_status[task["status"]] = by_status.get(task["status"], 0) + 1
            by_priority[task["priority"]] = by_priority.get(task["priority"], 0) + 1
            by_scope["global" if task["is_global"] else "pane"] += 1
            if task["status"] in ("open", "in_progress", "blocked"):
                if task["status"] == "blocked" and str(task.get("block_reason") or
                                                     "").startswith("needs-owner:"):
                    # FLAT_FLEET.md 5: the only work a person is holding is a
                    # question a lane asked the owner. (The key keeps its old
                    # name, `leader_held`, for the board; the leader is the owner.)
                    leader_held.append(task)
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
                "leader_held": {
                    "count": len(leader_held),
                    "oldest_created_at": (min(task["created_at"] for task in leader_held)
                                          if leader_held else None),
                    "oldest_age_s": (round(until - min(task["created_at"]
                                                       for task in leader_held), 1)
                                     if leader_held else None),
                    "definition": ("global tasks with no assignee (owner #1708: "
                                   "unowned means leader-owned), plus tasks "
                                   "explicitly assigned to the leader pane"),
                    "leader_pane": None,     # there is no leader; these are needs-owner questions
                },
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
