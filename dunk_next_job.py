"""t372 execution wiring. Tasks journal intentions; inbox owns delivery receipts.

No new store, no terminal typing bypass, and no polling of unassigned backlogs.
A transport attempt is reserved durably before I/O. Interrupted sends are uncertain
and are NOT blindly replayed. Only known holds may retry through the shared policy.
"""
import logging
import os
import threading
import time

from dunk_dispatch import delivery_retry_plan

log = logging.getLogger("dunkingsheep.next_job")


class NextJobs:
    def __init__(self, flock):
        self.flock = flock
        self._locks = {}
        self._lock = threading.Lock()

    def _pane_lock(self, pane):
        with self._lock:
            return self._locks.setdefault(pane, threading.RLock())

    def advance(self, pane, trigger, from_task=None, caller=None):
        f = self.flock
        with self._pane_lock(pane):
            try:
                unread = f.inbox_count(None, self_pane_id=pane).get("unread")
            except Exception:
                unread = None
            with f._lock:
                pending = any(m.status == "queued" for m in f.outbox.get(pane, ()))
            plan = f.tasks.advance_next_job(
                pane, trigger=trigger, from_task=from_task, caller=caller or f.current_caller(),
                unread=unread, pending_delivery=pending)
            f.metrics.record("next_job.decision", pane_id=pane, source=trigger,
                             outcome=plan["action"], reason=plan.get("reason"))
            log.info("next-job %s trigger=%s action=%s task=%s reason=%s", pane,
                     trigger, plan["action"], plan.get("task_id"), plan.get("reason"))
            # Claim and intent have committed before any delivery I/O (clause 1).
            if plan.get("handoff"):
                try:
                    self.materialise(plan["handoff"])
                except Exception:
                    log.exception("next-job inbox materialisation deferred")
            return plan

    def materialise(self, h):
        f = self.flock
        mailbox, _, pane = f.resolve_mailbox(h["pane_id"], None, strict=True)
        if pane != h["pane_id"]:
            raise ValueError("next-job pane changed")
        task = f.tasks.get(h["task_id"])
        body = (f"TASK {task['id']} [{task['priority']}] {task['title']}\n\n"
                f"{task['body']}\n\n"
                f"Already atomically claimed for {pane}; start this job now.\n"
                f"heartbeat: dunkingsheep task heartbeat {task['id']}\n"
                f"complete: dunkingsheep task complete {task['id']}\n"
                f"handoff: {h['message_id']} from {h['from_task'] or h['trigger']}")
        return f.inbox.ensure_next_job(h["message_id"], mailbox, pane, body,
                                       f"TASK {task['id']}: {task['title']}")

    def recover(self):
        """Startup uses the same decision; subsequent ticks service only intents."""
        f = self.flock
        rows = f.tasks.db.execute(
            "SELECT * FROM task_events WHERE event='next_job_decision' ORDER BY id").fetchall()
        import json
        latest = {r["actor"]: json.loads(r["meta"]) for r in rows if r["actor"]}
        for pane, plan in latest.items():
            # An explicit no-work is terminal. Restart is not a fleet backlog poll.
            if plan.get("reason") in ("unread_unknown", "unread_mail", "pending_delivery"):
                self.advance(pane, "restart", plan.get("from_task"), caller=plan.get("caller"))
        self.tick(restart=True)

    def tick(self, restart=False):
        for h in self.flock.tasks.next_job_handoffs():
            if h["state"] in ("started", "undeliverable", "cancelled"):
                continue
            try:
                self.deliver(h, restart=restart)
            except Exception:
                log.exception("next-job delivery failed for %s", h["message_id"])

    def deliver(self, h, restart=False):
        from dunk_core import Message, SUBMIT_ATTEMPTS
        from dunk_callers import process_start, process_alive
        from dunk_inbox import InboxError
        f = self.flock
        with self._pane_lock(h["pane_id"]):
            h = next(x for x in f.tasks.next_job_handoffs(h["pane_id"])
                     if x["message_id"] == h["message_id"])
            if h["state"] in ("started", "undeliverable", "cancelled"):
                return h
            task = f.tasks.get(h["task_id"])
            if task["status"] in ("done", "cancelled"):
                return f.tasks.update_next_job(h, state="cancelled", reason="task_finished")
            if h["state"] == "sending":
                if restart:
                    owner = h.get("transport_owner") or {}
                    if owner.get("pid") and process_alive(owner["pid"], owner.get("start")):
                        return h  # recovery racing a live sender cannot steal its attempt
                    try:
                        receipt = f.inbox.status(h["message_id"])["notify"]
                    except InboxError:
                        # Reservation preceded inbox materialisation. There can
                        # have been no typing, since transport requires that row.
                        self.materialise(h)
                        return f.tasks.update_next_job(h, state="pending",
                                                       reason="interrupted_before_inbox")
                    if receipt["attempt"] != h["attempt"]:
                        return f.tasks.update_next_job(h, state="pending",
                                                       reason="interrupted_before_transport")
                    if receipt["state"] == "sent" and receipt["attempt"] == h["attempt"]:
                        return f.tasks.update_next_job(h, state="sent", reason=None,
                                                       transport_id=receipt["transport_id"],
                                                       delivered_at=receipt["at"])
                    # Crash in the external-side-effect window: never manufacture
                    # a receipt or send the same execution request twice.
                    f.inbox.record_notify(h["message_id"], "undeliverable", "delivery_interrupted")
                    return f.tasks.update_next_job(h, state="undeliverable",
                                                   reason="delivery_interrupted")
                return h
            if h["state"] == "sent":
                return h  # delivery confirmed; waiting for an actual producer turn
            now = time.time()
            with f._lock:
                turn = f.turns.get(h["pane_id"]) or {}
                if not turn.get("ended_at") and turn.get("expires_at", 0) > now:
                    return h  # the completion caller is still in its current turn
            policy = delivery_retry_plan(attempt=h["attempt"], last_at=h.get("attempt_at"),
                                         now=now, max_attempts=SUBMIT_ATTEMPTS)
            if policy["action"] == "wait":
                return h
            if policy["action"] == "stop":
                f.inbox.record_notify(h["message_id"], "undeliverable", policy["reason"])
                return f.tasks.update_next_job(h, state="undeliverable", reason=policy["reason"])
            # Retain the pending assignment, but do not execute cancelled/reassigned
            # work or a lease a different agent now owns.
            if (h["pane_id"] not in task["assignees"] or task["blocked_by"]
                    or task.get("block_reason")
                    or h["pane_id"] not in task["claimed_by"]):
                f.inbox.record_notify(h["message_id"], "undeliverable", "claim_no_longer_valid")
                return f.tasks.update_next_job(h, state="undeliverable", reason="claim_no_longer_valid")
            h = f.tasks.update_next_job(h, expected={"state": h["state"], "attempt": h["attempt"]},
                                       state="sending", attempt=h["attempt"] + 1, attempt_at=now,
                                       transport_owner={"pid": os.getpid(), "start": process_start(os.getpid())})
            if h is None:
                return None  # another dispatcher reserved the same intent
            try:
                mail = self.materialise(h)
                text = (f"[inbox] TASK {h['task_id']} is already claimed for you. "
                        f"Read dunkingsheep inbox read and execute it now. "
                        f"Handoff {h['message_id']}.")
                # Reuse the full human-draft/dialog/turn safety gate, but NOT the
                # unbounded generic outbox worker. This journal owns retries.
                message = Message(f"{h['message_id']}:{h['attempt']}", h["pane_id"],
                                  mail["mailbox"], text, "next_job",
                                  lambda: f.submit_text(h["pane_id"], text))
                message.notify_for = [h["message_id"]]
                message.preserve_draft = True
                f.inbox.record_notify(h["message_id"], "queued", transport_id=message.id,
                                      attempt=h["attempt"])
                sent = f._deliver(message)
                if not sent:
                    state, reason = "pending", message.detail or "held"
                elif message.status == "sent":
                    state, reason = "sent", None
                else:
                    # The submit ladder could have reached the agent. Do not
                    # retry ambiguous sends and accidentally start a second turn.
                    state, reason = "undeliverable", message.detail or "submission_unconfirmed"
                f.inbox.record_notify(h["message_id"], "queued" if state == "pending" else state, reason)
                result = f.tasks.update_next_job(h, state=state, reason=reason,
                                                transport_id=message.id,
                                                delivered_at=message.finished_at if state == "sent" else None)
                if state == "sent":
                    self.bind_turn(h["pane_id"])
                return result
            except Exception as exc:
                # Before/during transport failure is deliberately conservative.
                reason = f"delivery_error: {exc}"
                f.inbox.record_notify(h["message_id"], "undeliverable", reason)
                return f.tasks.update_next_job(h, state="undeliverable", reason=reason)

    def bind_turn(self, pane):
        """Link a confirmed message to a new producer generation, never busy status."""
        f = self.flock
        lock = self._pane_lock(pane)
        if not lock.acquire(blocking=False):
            return  # the sender binds this generation after writing its receipt
        try:
            with f._lock:
                turn = dict(f.turns.get(pane) or {})
            if not turn.get("turn_id") or turn.get("ended_at"):
                return
            for h in f.tasks.next_job_handoffs(pane):
                if (h["state"] != "sent" or not h.get("delivered_at")
                        or turn.get("handoff_id") != h["message_id"]
                        or turn["started_at"] < h.get("attempt_at", float("inf"))):
                    continue
                receipt = f.inbox.status(h["message_id"])
                if receipt["notify"]["state"] != "sent":
                    continue
                f.tasks.update_next_job(h, state="started", turn_id=turn["turn_id"],
                                        turn_started_at=turn["started_at"],
                                        turn_source=turn.get("source"),
                                        turn_caller=turn.get("caller"))
        finally:
            lock.release()
