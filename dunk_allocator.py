"""Registered, deterministic allocation. Task DB is the ownership authority.

Registration and offers are explicit contracts, never inferred from task prose.
The daemon journals the existing next-job intent in the same transaction as the
assignment/lease; existing next-job transport owns delivery and recovery.
"""
import json
import logging
import re
import time
import uuid

from dunk_tasks import DEFAULT_LEASE_S, TaskError

log = logging.getLogger("dunkingsheep.allocator")
SCHEMA = """
CREATE TABLE IF NOT EXISTS allocation_workers (
 pane_id TEXT PRIMARY KEY, pools TEXT NOT NULL, roles TEXT NOT NULL,
 skills TEXT NOT NULL, enabled INTEGER NOT NULL, registered_by TEXT NOT NULL,
 updated_at REAL NOT NULL, last_assigned REAL NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS allocation_offers (
 task_id TEXT PRIMARY KEY, pool TEXT NOT NULL, role TEXT NOT NULL,
 requires TEXT NOT NULL, excluded TEXT NOT NULL, owners TEXT NOT NULL,
 offered_by TEXT NOT NULL, state TEXT NOT NULL DEFAULT 'ready');
CREATE TABLE IF NOT EXISTS allocation_reviews (
 source_id TEXT PRIMARY KEY, task_id TEXT NOT NULL UNIQUE,
 revision TEXT NOT NULL, author TEXT NOT NULL);
"""


def tags(values, *, required=False):
    if not isinstance(values, list) or any(
            not isinstance(v, str) or not re.fullmatch(r"[a-zA-Z0-9_.:-]{1,80}", v)
            for v in values):
        raise TaskError("tags must be a list of nonempty simple names")
    result = sorted(set(values))
    if required and not result:
        raise TaskError("at least one tag is required")
    return result


class AllocationStore:
    def __init__(self, tasks):
        self.tasks = tasks
        with tasks._lock:
            tasks.db.executescript(SCHEMA)
            tasks.db.commit()

    def register(self, pane, pools, roles, skills, actor, enabled=True):
        if not isinstance(enabled, bool):
            raise TaskError("enabled must be boolean")
        pools, roles, skills = tags(pools, required=True), tags(roles, required=True), tags(skills)
        if not set(roles) <= {"worker", "reviewer"}:
            raise TaskError("roles must be worker or reviewer")
        if not pane or not actor:
            raise TaskError("registration requires pane and operator")
        t = self.tasks
        with t._lock, t.db:
            t.db.execute("INSERT INTO allocation_workers "
                         "(pane_id,pools,roles,skills,enabled,registered_by,updated_at) "
                         "VALUES (?,?,?,?,?,?,?) ON CONFLICT(pane_id) DO UPDATE SET "
                         "pools=excluded.pools,roles=excluded.roles,skills=excluded.skills,"
                         "enabled=excluded.enabled,registered_by=excluded.registered_by,"
                         "updated_at=excluded.updated_at",
                         (pane, json.dumps(pools), json.dumps(roles), json.dumps(skills),
                          int(enabled), actor, time.time()))
            t._record("", "allocation_registered", actor=actor,
                      meta={"pane_id": pane, "enabled": bool(enabled), "pools": pools,
                            "roles": roles, "skills": skills})
        return self.status()

    def status(self):
        t = self.tasks
        return {"workers": [dict(r) for r in t.db.execute("SELECT * FROM allocation_workers ORDER BY pane_id")],
                "offers": [dict(r) for r in t.db.execute("SELECT * FROM allocation_offers ORDER BY task_id")]}

    def _offer(self, task, pool, role, requires, excluded, actor):
        if task["scope"] != "global" or task["status"] not in ("open", "blocked"):
            raise TaskError("only open/blocked global backlog can be offered")
        if not task["body"].strip() or not task["assignees"] or task["leases"] or task["multi_assignee"]:
            raise TaskError("offer requires body, owner, exclusive task and no lease")
        if role not in ("worker", "reviewer"):
            raise TaskError("invalid allocation role")
        tags([pool], required=True)
        requires = tags(requires)
        t = self.tasks
        if any(h["task_id"] == task["id"] and h["state"] != "cancelled"
               for h in t.next_job_handoffs()):
            raise TaskError("task already has a delivery intent")
        t.db.execute("INSERT INTO allocation_offers VALUES (?,?,?,?,?,?,?,'ready') "
                     "ON CONFLICT(task_id) DO UPDATE SET pool=excluded.pool,role=excluded.role,"
                     "requires=excluded.requires,excluded=excluded.excluded,owners=excluded.owners,"
                     "offered_by=excluded.offered_by,state='ready'",
                     (task["id"], pool, role, json.dumps(requires), json.dumps(sorted(set(excluded))),
                      json.dumps(sorted(task["assignees"])), actor))
        t._record(task["id"], "allocation_offered", actor=actor,
                  meta={"pool": pool, "role": role, "requires": requires})

    def offer(self, task_id, pool, requires, actor):
        t = self.tasks
        with t._lock:
            t.db.execute("BEGIN IMMEDIATE")
            try:
                self._offer(t.get(task_id), pool, "worker", requires, [], actor)
                t.db.commit()
            except Exception:
                t.db.rollback()
                raise
        return t.get(task_id)

    def review(self, source_id, revision, body, pool, requires, author):
        """One canonical review per source, immutable author exclusion.

        Same revision is idempotent. A new revision reopens the same completed
        review, but cannot replace a revision being reviewed or in delivery.
        """
        if not re.fullmatch(r"[0-9a-f]{40,64}", revision or "") or not (body or "").strip():
            raise TaskError("review requires full revision hash and evidence/acceptance body")
        body = f"Frozen revision: {revision}\nSource task: {source_id}\n\n{body}"
        t = self.tasks
        with t._lock:
            t.db.execute("BEGIN IMMEDIATE")
            try:
                source = t.get(source_id)
                if source["scope"] != "global" or author not in source["assignees"]:
                    raise TaskError("only source owner may submit global work for review")
                old = t.db.execute("SELECT * FROM allocation_reviews WHERE source_id=?", (source_id,)).fetchone()
                if old and old["author"] != author:
                    raise TaskError("review author changed; operator reconciliation required")
                if old and old["revision"] == revision:
                    t.db.commit()
                    return t.get(old["task_id"])
                if old:
                    previous = t.get(old["task_id"])
                    if previous["status"] != "done":
                        raise TaskError("prior review is not finished")
                    task_id = old["task_id"]
                    # Completion is durable; retained terminal leases are not active
                    # review work and must not delay a corrected revision for 30m.
                    t.db.execute("DELETE FROM task_leases WHERE task_id=?", (task_id,))
                    t.db.execute("UPDATE tasks SET status='open',body=?,assignees=?,updated_at=?,"
                                 "completed_at=NULL,completed_by=NULL WHERE id=?",
                                 (body, json.dumps([author]), time.time(), task_id))
                else:
                    task_id, now = t._next_id(), time.time()
                    t.db.execute("INSERT INTO tasks (id,scope,title,body,status,priority,parent_id,"
                                 "assignees,created_at,updated_at,created_by) VALUES "
                                 "(?,'global',?,?,'open',?,?,?,?,?,?)",
                                 (task_id, "Review: " + source["title"], body, source["priority"],
                                  source_id, json.dumps([author]), now, now, author))
                    t._record(task_id, "created", actor=author, scope="global",
                              priority=source["priority"], status_to="open")
                # Cancel only terminal review's historical intents before reopening.
                for h in t.next_job_handoffs():
                    if old and h["task_id"] == task_id and h["state"] != "cancelled":
                        details = dict(h, state="cancelled", reason="review_revision_superseded")
                        t.record_next_job_handoff(task_id, pane_id=h["pane_id"],
                                                 from_task=h.get("from_task"), details=details)
                self._offer(t.get(task_id), pool, "reviewer", requires,
                            source["assignees"] + [p for p in (source.get("created_by"), source.get("completed_by")) if p], author)
                t.db.execute("INSERT INTO allocation_reviews VALUES (?,?,?,?) "
                             "ON CONFLICT(source_id) DO UPDATE SET revision=excluded.revision",
                             (source_id, task_id, revision, author))
                t._record(source_id, "review_submitted", actor=author,
                          meta={"review_task": task_id, "revision": revision})
                t.db.commit()
            except Exception:
                t.db.rollback()
                raise
        return t.get(task_id)

    def allocate(self, idle_callers):
        """Fresh process-verified idle snapshot supplied by the guarded daemon.

        SQLite serializes registration, reassignments and claims with this pass.
        Capacity is one; manual assigned work and existing intents take precedence.
        """
        t = self.tasks
        with t._lock:
            t.db.execute("BEGIN IMMEDIATE")
            try:
                now, results = time.time(), []
                workers = [dict(r) for r in t.db.execute(
                    "SELECT * FROM allocation_workers WHERE enabled=1 ORDER BY last_assigned,pane_id")]
                # Include private queues for capacity only; they are never offers
                # and their contents are not exposed by allocation status.
                tasks = [t._decorate(r) for r in t.db.execute(
                    "SELECT * FROM tasks WHERE status NOT IN ('done','cancelled')").fetchall()]
                offers = {r["task_id"]: dict(r) for r in t.db.execute(
                    "SELECT * FROM allocation_offers WHERE state='ready'")}
                candidates = sorted((x for x in tasks if x["id"] in offers), key=t._order_key)
                used = set()
                for task in candidates:
                    offer = offers[task["id"]]
                    if (task["status"] != "open" or task["blocked_by"] or task["block_reason"]
                            or task["leases"] or task["scope"] != "global"
                            or sorted(task["assignees"]) != json.loads(offer["owners"])):
                        continue
                    for worker in workers:
                        pane = worker["pane_id"]
                        if pane not in idle_callers or pane in used or pane in json.loads(offer["excluded"]):
                            continue
                        if (offer["pool"] not in json.loads(worker["pools"])
                                or offer["role"] not in json.loads(worker["roles"])
                                or not set(json.loads(offer["requires"])) <= set(json.loads(worker["skills"]))):
                            continue
                        if t._busy_task_ids(pane, now):
                            continue
                        owned = [x for x in tasks if pane in x["assignees"] and x["id"] not in offers
                                 and (x["status"] == "in_progress" or
                                      (x["status"] == "open" and not x["blocked_by"] and not x["block_reason"]))]
                        if owned or t.active_handoffs(pane, {x["id"] for x in tasks}):
                            continue
                        # An offered item is released by its current owners for allocation.
                        t.db.execute("UPDATE tasks SET assignees=? WHERE id=?", (json.dumps([pane]), task["id"]))
                        task["assignees"] = [pane]
                        t._claim_locked(task, pane, DEFAULT_LEASE_S, idle_callers[pane], now)
                        handoff = dict(from_task=None, completion_at=None, message_id="nj-"+uuid.uuid4().hex,
                                       attempt=0, reason=None, state="pending", trigger="allocation",
                                       caller=idle_callers[pane])
                        t.record_next_job_handoff(task["id"], pane_id=pane, from_task=None, details=handoff)
                        t.db.execute("UPDATE allocation_offers SET state='assigned' WHERE task_id=?", (task["id"],))
                        t.db.execute("UPDATE allocation_workers SET last_assigned=? WHERE pane_id=?", (now, pane))
                        t._record(task["id"], "allocated", actor=pane,
                                  meta={"pool": offer["pool"], "role": offer["role"],
                                        "previous_owners": json.loads(offer["owners"]), "message_id": handoff["message_id"]})
                        results.append(dict(handoff, task_id=task["id"], pane_id=pane))
                        used.add(pane)
                        break
                t.db.commit()
            except Exception:
                t.db.rollback()
                raise
        return results


class Allocator:
    def __init__(self, flock):
        self.flock = flock
        self.store = AllocationStore(flock.tasks)
        self.last_tick = 0
        self.last_result = {}

    def tick(self):
        """Bounded periodic watchdog pass, with native idle evidence, no AI."""
        if time.monotonic() - self.last_tick < 30:
            return self.last_result
        self.last_tick = time.monotonic()
        workers = self.store.status()["workers"]
        if not workers:
            return {}
        from dunk_callers import caller_pane_auth_failure
        f = self.flock
        lanes = f.resolve_lanes()
        panes = {p["pane_id"]: p for p in lanes}
        idle, reasons, tokens = {}, {}, {}
        for worker in workers:
            pane = worker["pane_id"]
            info = panes.get(pane)
            reason = "disabled" if not worker["enabled"] else f.next_jobs.gate(pane, lanes=lanes)
            if not reason and info:
                with f._lock:
                    turn = dict(f.turns.get(pane) or {})
                    pending = any(m.status == "queued" for m in f.outbox.get(pane, ()))
                caller = turn.get("caller") or {}
                reason = ("not_idle" if not turn.get("ended_at") else
                          "pending_delivery" if pending else
                          caller_pane_auth_failure(caller, info.get("pane_pid")))
                if not reason and f.inbox_count(None, self_pane_id=pane).get("unread") != 0:
                    reason = "unread_or_unknown"
                if not reason and f.delivery_state(pane, status="idle", pane=info).get("state") != "ready":
                    reason = "delivery_not_ready"
                if not reason:
                    idle[pane] = caller
                    tokens[pane] = (turn.get("turn_id"), turn.get("ended_at"))
            reasons[pane] = reason or (None if info else "missing")
        # Serialize against a new native turn/outbox enqueue during selection.
        # No socket or terminal I/O occurs under this lock/DB transaction.
        with f._lock:
            still_idle = {}
            for pane, caller in idle.items():
                state = f.turns.get(pane) or {}
                if ((state.get("turn_id"), state.get("ended_at")) == tokens[pane]
                        and not any(m.status == "queued" for m in f.outbox.get(pane, ()))):
                    still_idle[pane] = caller
            handoffs = self.store.allocate(still_idle)
        for h in handoffs:
            try:
                f.next_jobs.materialise(h)
            except Exception:
                log.exception("allocation intent retained for next-job recovery")
        self.last_result = {"allocated": handoffs, "workers": reasons, "at": time.time()}
        return self.last_result
