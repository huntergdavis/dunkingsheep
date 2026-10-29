"""t269: a refused delivery backs off and then gives up, instead of holding for ever.

CoS measured 121,892 `message.held` rows on 46 panes: a dead or permanently blocked target
was re-attempted every MESSAGE_RECHECK_S and every attempt recorded a row. These tests pin
the two missing behaviours - exponential backoff and a TERMINAL undeliverable state - and
the guarantees that must survive them: a transient hold still delivers, the attempt budget
survives a restart, and the human/draft protections are unchanged (they are the reason the
hold happens at all).
"""
import json
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import _bootstrap  # noqa: E402,F401

import dunk_core  # noqa: E402
from dunk_inbox import Inbox  # noqa: E402
from dunk_tasks import Tasks  # noqa: E402
from fakes import FakeHerdr  # noqa: E402

PANE = "w1:p2"


class _Metrics:
    def __init__(self):
        self.events = []

    def record(self, event, **fields):
        self.events.append(dict(fields, event=event))

    def prune(self, *_a, **_k):
        return 0

    def close(self):
        return None


class BackoffCurveTests(unittest.TestCase):
    def test_the_interval_grows_and_is_capped(self):
        values = [dunk_core._hold_backoff_s(n) for n in range(1, 9)]
        self.assertEqual(sorted(values), values, "the interval must not shrink")
        self.assertEqual(dunk_core.HOLD_BACKOFF_BASE_S, values[0])
        self.assertEqual(values[1], values[0] * 2)
        self.assertTrue(all(v <= dunk_core.HOLD_BACKOFF_CAP_S for v in values),
                        f"capped at {dunk_core.HOLD_BACKOFF_CAP_S}: {values}")

    def test_the_limit_is_bounded_and_small(self):
        self.assertGreaterEqual(dunk_core.HOLD_ATTEMPT_LIMIT, 3)
        self.assertLessEqual(dunk_core.HOLD_ATTEMPT_LIMIT, 50)


class TerminalUndeliverableTests(unittest.TestCase):
    """A target that never accepts must stop producing hold rows."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.herdr = FakeHerdr()
        self.metrics = _Metrics()
        self.inbox = Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                           blob_dir=os.path.join(self.tmp.name, "blobs"))
        self.flock = dunk_core.Flock(
            herdr=self.herdr, persist=False, restore=False, inbox=self.inbox,
            tasks=Tasks(path=os.path.join(self.tmp.name, "tasks.db")),
            metrics=self.metrics)
        # a pane that shows an open dialog: the send is refused by the guard every time
        self.herdr.screens[PANE] = "│ Choose a model\n│ ❯ 1. fast\n│ Enter to select · esc to cancel\n"

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def held_rows(self):
        return [e for e in self.metrics.events if e["event"] == "message.held"]

    def terminal_rows(self):
        return [e for e in self.metrics.events if e["event"] == "message.undeliverable"]

    def message(self, text="a nudge that cannot land"):
        return self.flock._new_message(PANE, "Claude Proj", text, "send_text",
                                       lambda: self.flock.submit_text(PANE, text))

    def test_a_refused_delivery_backs_off_then_becomes_terminal(self):
        message = self.message()
        attempts = 0
        while message.status == "queued" and attempts < 40:
            attempts += 1
            self.flock._deliver(message)
        self.assertEqual("undeliverable", message.status,
                         f"must give up, took {attempts} attempts")
        self.assertEqual(dunk_core.HOLD_ATTEMPT_LIMIT, attempts)
        self.assertEqual(dunk_core.HOLD_ATTEMPT_LIMIT, len(self.held_rows()))
        self.assertEqual(1, len(self.terminal_rows()),
                         "exactly ONE terminal row, then silence")
        self.assertIn("undeliverable", message.detail)
        # the message leaves the queue: nothing can retry it
        self.assertEqual([], [m for m in (self.flock.outbox.get(PANE) or [])
                              if m.id == message.id])
        # A decided message is DONE, not "needs retrying": `_deliver` returns True for it
        # and does nothing (VP review: it must not emit a second terminal row).
        self.assertTrue(self.flock._deliver(message))
        self.assertEqual("undeliverable", message.status)

    def test_the_backoff_is_recorded_on_the_message(self):
        message = self.message()
        self.flock._deliver(message)
        self.assertEqual(1, message.holds)
        self.assertIsNotNone(message.next_attempt_at, "the next attempt must be scheduled")
        first_delay = message.next_attempt_at - message.created_at
        self.assertGreaterEqual(first_delay, dunk_core.HOLD_BACKOFF_BASE_S)
        self.flock._deliver(message)
        second_delay = message.next_attempt_at - message.created_at
        self.assertGreater(second_delay, first_delay, "the interval must grow")

    def test_a_transient_hold_still_delivers(self):
        message = self.message()
        self.flock._deliver(message)                     # refused: dialog open
        self.assertEqual(1, message.holds)
        # the dialog is dismissed; the same message must still go out
        self.herdr.screens[PANE] = f"│ ✻ idle\n❯\u00a0\n│ idle · pi\n"
        self.herdr.box[PANE] = ""
        message.next_attempt_at = None                   # due now
        self.assertTrue(self.flock._deliver(message))
        self.assertEqual("sent", message.status)
        self.assertEqual(1, len(self.held_rows()), "only the one refusal is recorded")


class BackoffPersistenceTests(unittest.TestCase):
    """The attempt budget must survive a restart, or the storm resumes after every deploy."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.state = os.path.join(self.tmp.name, "dunks.json")
        self.herdr = FakeHerdr()
        self.metrics = _Metrics()
        self.inbox = Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                           blob_dir=os.path.join(self.tmp.name, "blobs"))
        self.herdr.screens[PANE] = "│ Choose a model\n│ ❯ 1. fast\n│ Enter to select · esc to cancel\n"

    def tearDown(self):
        self.inbox.close()
        self.tmp.cleanup()

    def flock(self):
        return dunk_core.Flock(
            herdr=self.herdr, state_file=self.state, persist=True, restore=True,
            inbox=self.inbox, tasks=Tasks(path=os.path.join(self.tmp.name, "tasks.db")),
            metrics=self.metrics)

    def test_attempts_and_next_attempt_survive_a_restart(self):
        first = self.flock()
        message = None
        try:
            message = first._new_message(PANE, "Claude Proj", "held text", "send_text",
                                         lambda: first.submit_text(PANE, "held text"))
            first._enqueue(message)
            first._deliver(message)
            first._deliver(message)
            # The worker thread may also have attempted (it is live once enqueued), so the
            # exact count is whatever it reached - what matters is that it is > 0 and that
            # the restored message carries the SAME budget, not a reset one.
            counts_at_close = message.holds
            self.assertGreaterEqual(counts_at_close, 2)
            scheduled = message.next_attempt_at
            self.assertIsNotNone(scheduled)
        finally:
            first.close()
        again = self.flock()
        try:
            restored = [m for m in (again.outbox.get(PANE) or []) if m.id == message.id]
            self.assertTrue(restored, "the held message must be restored")
            # NOT a reset check by equality: the restoring process reads the box again and
            # may legitimately attempt once more (its signature differs from the stored
            # one), so the count can be higher. What must never happen is 0.
            self.assertGreaterEqual(restored[0].holds, counts_at_close,
                                    "the attempt budget must not reset on restart")
            self.assertGreater(restored[0].holds, 0)
            # The restoring process may make one more attempt and re-schedule, so the
            # invariant is that the wait is NEVER reset to "now": the next attempt must not
            # be earlier than the one the old process had scheduled.
            self.assertIsNotNone(restored[0].next_attempt_at,
                                 "a held message must still be scheduled after a restart")
            self.assertGreaterEqual(restored[0].next_attempt_at, scheduled - 1.0,
                                    "the schedule must not be reset by the restart")
        finally:
            again.close()


class RealWorkerStepTests(unittest.TestCase):
    """CoS i31970: drive the REAL worker step on a fake clock.

    The earlier tests called `_deliver` directly, and the transient case cleared the
    deadline by hand, so neither exercised the worker's backoff/changed-box/terminal
    behaviour - the code that actually runs on a live daemon.
    """

    T0 = 2_000_000.0

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.herdr = FakeHerdr()
        self.metrics = _Metrics()
        self.inbox = Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                           blob_dir=os.path.join(self.tmp.name, "blobs"))
        self.flock = dunk_core.Flock(
            herdr=self.herdr, persist=False, restore=False, inbox=self.inbox,
            tasks=Tasks(path=os.path.join(self.tmp.name, "tasks.db")),
            metrics=self.metrics)
        self.herdr.screens[PANE] = "│ Choose a model\n│ \u276f 1. fast\n│ Enter to select \u00b7 esc to cancel\n"

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def rows(self, kind):
        return [e for e in self.metrics.events if e["event"] == kind]

    def enqueue(self, text="a nudge that cannot land", notify_for=()):
        message = self.flock._new_message(PANE, "Claude Proj", text, "send_text",
                                          lambda: self.flock.submit_text(PANE, text))
        message.notify_for = list(notify_for)
        # Queue it WITHOUT starting the live thread: this drive must be deterministic, and
        # `_outbox_step` is the worker's own body. (Calling `_enqueue` here let the thread
        # take an attempt of its own, which made the expected interval ambiguous.)
        with self.flock._lock:
            self.flock.outbox.setdefault(PANE, []).append(message)
        return message

    def test_the_worker_step_backs_off_at_the_documented_intervals(self):
        message = self.enqueue()
        self.assertEqual("held", self.flock._outbox_step(PANE, now=self.T0))
        first_due = message.next_attempt_at
        self.assertAlmostEqual(self.T0 + dunk_core.HOLD_BACKOFF_BASE_S, first_due, delta=1.0)
        # before it is due, the step reports "due" and does NOT add a row
        self.assertEqual("due", self.flock._outbox_step(PANE, now=self.T0 + 1))
        self.assertEqual(1, len(self.rows("message.held")))
        # when it is due, another attempt is made with a LONGER interval
        self.assertEqual("held",
                         self.flock._outbox_step(PANE, now=first_due + 0.1))
        self.assertEqual(2, len(self.rows("message.held")))
        self.assertGreater(message.next_attempt_at - (first_due + 0.1),
                           dunk_core.HOLD_BACKOFF_BASE_S / 2)

    def test_the_worker_step_reaches_terminal_and_terminates_the_lifecycle(self):
        inbox_id = self.inbox.send("pi-worker-1", "hello", sender="pm")["message_id"]
        message = self.enqueue(notify_for=[inbox_id])
        now = self.T0
        for _ in range(40):
            outcome = self.flock._outbox_step(PANE, now=now)
            if message.status == "undeliverable":
                break
            now = (message.next_attempt_at or now) + 0.1
        self.assertEqual("undeliverable", message.status)
        self.assertEqual(dunk_core.HOLD_ATTEMPT_LIMIT, len(self.rows("message.held")))
        self.assertEqual(1, len(self.rows("message.undeliverable")))
        # THE LIFECYCLE FIX: a waiter is released and the inbox notice is decided
        self.assertTrue(message.done.is_set(),
                        "a terminal message must release its waiter, not hang for ever")
        self.assertEqual([], [m for m in (self.flock.outbox.get(PANE) or [])
                              if m.id == message.id])
        notice = self.inbox.status(inbox_id)
        self.assertIn(notice["state"], ("failed", "queued"),
                      "the notice must be decided rather than falsely pending")
        # and no further rows, ever
        before = len(self.rows("message.held"))
        for _ in range(5):
            self.assertEqual("empty", self.flock._outbox_step(PANE, now=now + 600))
        self.assertEqual(before, len(self.rows("message.held")))

    def test_a_changed_box_clears_the_backoff_without_waiting(self):
        message = self.enqueue()
        self.assertEqual("held", self.flock._outbox_step(PANE, now=self.T0))
        due = message.next_attempt_at
        # the dialog is answered: the box now reads clear
        self.herdr.screens[PANE] = "\u2502 \u273b idle\n\u276f\u00a0\n\u2502 idle \u00b7 pi\n"
        self.herdr.box[PANE] = ""
        # well before the deadline, and with no manual clearing of next_attempt_at
        self.assertEqual("delivered", self.flock._outbox_step(PANE, now=self.T0 + 1))
        self.assertEqual("sent", message.status)
        self.assertLess(self.T0 + 1, due, "the point: delivered before the deadline")

    def test_a_dead_pane_is_bounded_and_terminal(self):
        # permanent blocker on the not-clear path: a session that reports dead
        self.herdr.screens[PANE] = ("\u2502 \u25a0 Server connection could not be restored\n"
                                    "\u2502 \u2022 Reconnect failed - check the endpoint, then relaunch (41h 22m)\n"
                                    "\u2502 \u203a [inbox] new message\n  ^c quit\n")
        message = self.enqueue()
        now = self.T0
        for _ in range(40):
            self.flock._outbox_step(PANE, now=now)
            if message.status == "undeliverable":
                break
            now = (message.next_attempt_at or now) + 0.1
        self.assertEqual("undeliverable", message.status,
                         "a dead target must not be retried for ever")
        self.assertEqual([], [m for m in (self.flock.outbox.get(PANE) or [])
                              if m.id == message.id])
        self.assertTrue(message.done.is_set())


class TerminalContractTests(unittest.TestCase):
    """VP review i32063: durable sender-visible terminal result, idempotence, and the
    refusal detail preserved verbatim (the next-job journal reports it as the reason)."""

    T0 = 3_000_000.0

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.herdr = FakeHerdr()
        self.metrics = _Metrics()
        self.inbox = Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                           blob_dir=os.path.join(self.tmp.name, "blobs"))
        self.flock = dunk_core.Flock(
            herdr=self.herdr, persist=False, restore=False, inbox=self.inbox,
            tasks=Tasks(path=os.path.join(self.tmp.name, "tasks.db")),
            metrics=self.metrics)
        self.herdr.screens[PANE] = "│ Choose a model\n│ \u276f 1. fast\n│ Enter to select \u00b7 esc to cancel\n"

    def tearDown(self):
        self.flock.close()
        self.flock.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def exhausted(self, notify_for=()):
        """Drive a message to the terminal state through the worker's own step."""
        message = self.flock._new_message(PANE, "Claude Proj", "text that cannot land",
                                          "send_text",
                                          lambda: self.flock.submit_text(PANE, "x"))
        message.notify_for = list(notify_for)
        with self.flock._lock:
            self.flock.outbox.setdefault(PANE, []).append(message)
        now = self.T0
        for _ in range(40):
            self.flock._outbox_step(PANE, now=now)
            if message.status == "undeliverable":
                break
            now = (message.next_attempt_at or now) + 0.1
        self.assertEqual("undeliverable", message.status)
        return message

    def terminal_rows(self):
        return [e for e in self.metrics.events if e["event"] == "message.undeliverable"]

    def test_the_terminal_message_is_visible_to_its_sender(self):
        message = self.exhausted()
        listed = self.flock.list_messages()
        ids = [m["message_id"] for m in listed["recent"]]
        self.assertIn(message.id, ids,
                      "a terminal message must appear in the sender's view, not vanish")
        fetched = self.flock.get_message(message.id)
        self.assertEqual("undeliverable", fetched["status"])
        self.assertTrue(fetched["message"])

    def test_terminal_is_idempotent(self):
        message = self.exhausted()
        before_held = len([e for e in self.metrics.events if e["event"] == "message.held"])
        self.assertTrue(self.flock._deliver(message))          # decided: no work
        self.assertTrue(self.flock._deliver(message))          # and still none
        self.assertEqual(1, len(self.terminal_rows()),
                         "re-entering _deliver must not emit a second terminal row")
        self.assertEqual(before_held,
                         len([e for e in self.metrics.events if e["event"] == "message.held"]))
        self.assertEqual("undeliverable", message.status)

    def test_an_exhausted_message_is_not_revived_by_clearing_the_box(self):
        message = self.exhausted()
        # the dialog is answered and the box is clear: this used to send the exhausted
        # message and flip it to "sent", a phantom recovery after we had given up.
        self.herdr.screens[PANE] = "\u2502 \u273b idle\n\u276f\u00a0\n\u2502 idle \u00b7 pi\n"
        self.herdr.box[PANE] = ""
        self.flock._deliver(message)
        self.flock._outbox_step(PANE, now=self.T0 + 10_000)
        self.assertEqual("undeliverable", message.status,
                         "a given-up message must stay given up")
        self.assertEqual([], self.herdr.sends)

    def test_the_refusal_detail_is_preserved_verbatim(self):
        # a policy refusal (permission) is not a box hold: its own words must survive,
        # because the next-job journal reports them as the reason
        message = self.flock._new_message(PANE, "Claude Proj", "held by policy",
                                          "next_job",
                                          lambda: self.flock.submit_text(PANE, "held by policy"))
        message.permission = lambda: "disabled"
        with self.flock._lock:
            self.flock.outbox.setdefault(PANE, []).append(message)
        self.flock._outbox_step(PANE, now=self.T0)
        self.assertEqual("disabled", message.detail,
                         "the refusal reason must not be replaced by a generic label")
        self.assertTrue(getattr(message, "retry_detail", None),
                        "the retry schedule is recorded separately")


class TerminalPersistenceTests(unittest.TestCase):
    """VP i32107: the terminal verdict must be ON DISK before the save that follows it.

    Both terminal branches saved before `_remember`, so the file carried no tombstone: the
    verdict existed in memory, and a replayed stale queued row was re-enqueued by
    `_restore_outbox` because nothing on disk said that id was decided.
    """

    T0 = 4_000_000.0

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.state = os.path.join(self.tmp.name, "dunks.json")
        self.herdr = FakeHerdr()
        self.metrics = _Metrics()
        self.inbox = Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                           blob_dir=os.path.join(self.tmp.name, "blobs"))
        self.herdr.screens[PANE] = "│ Choose a model\n│ \u276f 1. fast\n│ Enter to select \u00b7 esc to cancel\n"

    def tearDown(self):
        self.inbox.close()
        self.tmp.cleanup()

    def flock(self):
        return dunk_core.Flock(
            herdr=self.herdr, state_file=self.state, persist=True, restore=True,
            inbox=self.inbox, tasks=Tasks(path=os.path.join(self.tmp.name, "tasks.db")),
            metrics=self.metrics)

    def exhaust(self, pane=PANE):
        flock = self.flock()
        message = flock._new_message(pane, "Claude Proj", "text that cannot land",
                                     "send_text", lambda: flock.submit_text(pane, "x"))
        with flock._lock:
            flock.outbox.setdefault(pane, []).append(message)
        now = self.T0
        for _ in range(40):
            flock._outbox_step(pane, now=now)
            if message.status == "undeliverable":
                break
            now = (message.next_attempt_at or now) + 0.1
        self.assertEqual("undeliverable", message.status)
        flock.close()
        return message

    def test_the_terminal_verdict_is_on_disk(self):
        message = self.exhaust()
        payload = json.load(open(self.state))
        decided = payload.get("decided") or {}
        self.assertIn(message.id, decided,
                      "the terminal verdict must be persisted, not only in memory")
        self.assertEqual("undeliverable",
                         (decided[message.id] or {}).get("status"))

    def test_a_replayed_stale_queued_row_is_not_requeued_after_a_restart(self):
        message = self.exhaust()
        # Replay the exact scenario VP used: put a stale QUEUED row for the decided
        # message back into the file, as a snapshot taken before the terminal state.
        payload = json.load(open(self.state))
        payload.setdefault("outbox", []).append({
            "id": message.id, "pane_id": PANE, "target_label": "Claude Proj",
            "text": "text that cannot land", "source": "send_text", "kind": "text",
            "hold_while_typing": True, "is_notice": False, "notify_for": [],
            "holds": 1, "next_attempt_at": None, "hold_signature": None,
            "created_at": message.created_at})
        with open(self.state, "w", encoding="utf-8") as handle:
            json.dump(payload, handle)
        again = self.flock()
        try:
            requeued = [m for m in (again.outbox.get(PANE) or []) if m.id == message.id]
            self.assertEqual([], requeued,
                             "a decided message must not be re-enqueued from a stale row")
            self.assertNotIn(message.id, [m.id for m in again.message_history],
                             "and it must not be delivered again")
        finally:
            again.close()

    def test_an_undecided_queued_row_is_still_restored(self):
        # control: the tombstone must not make the restore path deaf to real work
        message = self.exhaust()
        other = "m999999"
        payload = json.load(open(self.state))
        payload.setdefault("outbox", []).append({
            "id": other, "pane_id": PANE, "target_label": "Claude Proj",
            "text": "still pending", "source": "send_text", "kind": "text",
            "hold_while_typing": True, "is_notice": False, "notify_for": [],
            "holds": 1, "next_attempt_at": None, "hold_signature": None,
            "created_at": message.created_at + 5})
        with open(self.state, "w", encoding="utf-8") as handle:
            json.dump(payload, handle)
        again = self.flock()
        try:
            self.assertIn(other, [m.id for m in (again.outbox.get(PANE) or [])],
                          "an undecided row must still be restored")
        finally:
            again.close()


if __name__ == "__main__":
    unittest.main()
