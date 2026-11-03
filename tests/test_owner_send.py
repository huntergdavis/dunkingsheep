"""t1151: the web composer's owner-mailbox path, through the real routes with a fake daemon."""
import importlib.util
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.dirname(__file__))
import _bootstrap  # noqa: E402,F401

# web.app fixes its data dir at first import, so every test in the process shares ONE.
os.environ.setdefault("VP_DATA_DIR", tempfile.mkdtemp(prefix="desk-test-"))

DESK = {"pane_id": "%133", "workspace_label": "bootstrap", "tab_label": "sn-desk",
        "agent": "", "agent_status": "working", "cwd": "/home/hunter/workspace/desk"}


class Fake:
    def __init__(self, panes=(DESK,), fail_nudge=False, conversation=None, fail_direct=False,
                 direct_outcome="ok"):
        self.calls, self.panes, self.fail_nudge = [], list(panes), fail_nudge
        self.fail_direct, self.direct_outcome = fail_direct, direct_outcome
        self.conversation = conversation or []

    def call(self, verb, **args):
        self.calls.append((verb, args))
        if verb == "list_panes":
            return {"panes": self.panes}
        if verb == "owner_ingest":
            return {"message": {"message_id": "i7"}, "duplicate": False, "transport": "web"}
        if verb == "delivery_capability":
            return {"alive": True, "send": "guarded", "pane_id": args.get("target")}
        if verb == "send_text":
            if self.fail_direct:
                raise RuntimeError("terminal busy")
            return {"ok": True, "outcome": self.direct_outcome, "message_id": "m9",
                    "message": f"typing guard says {self.direct_outcome}"}
        if verb == "inbox_send":
            if self.fail_nudge:
                raise RuntimeError("inbox down")
            return {"message_id": "i8", "state": "queued"}
        if verb == "owner_conversation":
            return {"messages": self.conversation, "waiting": []}
        return {}


class OwnerIngestWireOracle(unittest.TestCase):
    """Pin the raw values and error ordering used by the Rust dispatcher adapter."""

    def setUp(self):
        from dunk_core import Flock
        from dunk_inbox import Inbox
        from dunk_roles import Roles
        from dunk_tasks import Tasks
        from fakes import FakeHerdr

        self.tmp = tempfile.TemporaryDirectory()
        root = self.tmp.name
        self.inbox = Inbox(path=os.path.join(root, "inbox.db"),
                           blob_dir=os.path.join(root, "blobs"))
        self.flock = Flock(
            herdr=FakeHerdr(), state_file=os.path.join(root, "dunks.json"),
            persist=False, restore=False, inbox=self.inbox,
            tasks=Tasks(path=os.path.join(root, "tasks.db")),
            roles=Roles(path=os.path.join(root, "roles.json"),
                        tokens_path=os.path.join(root, "tokens.json")),
        )

    def tearDown(self):
        self.flock.close()
        self.inbox.close()
        self.flock.tasks.close()
        self.tmp.cleanup()

    def test_scalar_subjects_use_sqlite_text_affinity(self):
        cases = [("integer", 42, "42"), ("true", True, "1"),
                 ("false", False, "0"), ("string", " spaced ", " spaced "),
                 ("null", None, None)]
        for label, subject, expected in cases:
            message = self.flock.owner_ingest(
                "body", "web", external_id=f"scalar-{label}", subject=subject
            )["message"]
            self.assertEqual(expected, message["subject"])
        omitted = self.flock.owner_ingest(
            "body", "web", external_id="scalar-omitted"
        )["message"]
        self.assertIsNone(omitted["subject"])

    def test_duplicate_returns_before_malformed_attachments_and_subject(self):
        first = self.flock.owner_ingest(
            "first", "web", external_id="duplicate-wire"
        )["message"]
        duplicate = self.flock.owner_ingest(
            "retry", "web", external_id="duplicate-wire", attachments=17,
            subject=["unsupported"],
        )
        self.assertTrue(duplicate["duplicate"])
        self.assertEqual(first["message_id"], duplicate["message"]["message_id"])
        with self.assertRaisesRegex(Exception, "transport must be one of"):
            self.flock.owner_ingest(
                "retry", "invalid", external_id="duplicate-wire", attachments=17,
                subject={"unsupported": True},
            )

    def test_fresh_errors_follow_body_attachment_then_subject_order(self):
        from dunk_core import DunkError

        with self.assertRaisesRegex(Exception, "a message needs a body or an attachment"):
            self.flock.owner_ingest("  ", "web", external_id="empty-body", attachments=[])
        with self.assertRaisesRegex(TypeError, "not iterable"):
            self.flock.owner_ingest("body", "web", external_id="bad-attach", attachments=17)

        missing = os.path.join(self.tmp.name, "missing.txt")
        with self.assertRaisesRegex(DunkError, "no such file to attach"):
            self.flock.owner_ingest("body", "web", external_id="missing-before-subject",
                                    attachments=[missing], subject=[])

        attachment = os.path.join(self.tmp.name, "existing.txt")
        with open(attachment, "wb") as handle:
            handle.write(b"staged before subject bind")
        for subject, kind in [([], "list"), ({"unsupported": True}, "dict")]:
            with self.subTest(subject=subject), self.assertRaises(Exception) as caught:
                self.flock.owner_ingest("body", "web", external_id=f"bad-subject-{type(subject).__name__}",
                                        attachments=[attachment], subject=subject)
            self.assertEqual(
                f"Error binding parameter 6: type '{kind}' is not supported",
                str(caught.exception),
            )
        self.assertTrue(os.listdir(self.inbox.blob_dir), "attachments stage before subject binding")

        rows = self.inbox.conversation("owner")
        ids = {row["external_id"] for row in rows}
        for failed in ("empty-body", "bad-attach", "missing-before-subject",
                       "bad-subject-list", "bad-subject-dict"):
            self.assertNotIn(failed, ids)

    def test_large_integer_subject_and_outbox_limit_raise_sqlite_overflow(self):
        with self.assertRaises(OverflowError) as subject_error:
            self.flock.owner_ingest(
                "body", "web", external_id="oversized-subject",
                subject=18446744073709551616,
            )
        self.assertEqual(
            "Python int too large to convert to SQLite INTEGER",
            str(subject_error.exception),
        )
        self.assertIsNone(self.inbox.find_by_external_id("owner", "oversized-subject"))

        for limit in (9223372036854775808, "9223372036854775808"):
            with self.subTest(limit=limit), self.assertRaises(OverflowError) as limit_error:
                self.flock.owner_outbox("web", limit=limit)
            self.assertEqual(
                "Python int too large to convert to SQLite INTEGER",
                str(limit_error.exception),
            )


@unittest.skipUnless(importlib.util.find_spec("fastapi"), "web.app needs fastapi")
class OwnerSend(unittest.TestCase):
    def run_with(self, fake, work):
        from fastapi.testclient import TestClient
        from web import app as desk
        saved, index = desk._flock, dict(desk.CHANNEL_INDEX)
        desk._flock = lambda: fake
        # the live registry's row: channel `cos`, match `desk`
        desk.CHANNEL_INDEX["cos"] = {"id": "cos", "name": "Front Desk", "match": "desk", "kind": "active"}
        desk.app.dependency_overrides[desk.require_auth] = lambda: "owner"
        try:
            return work(TestClient(desk.app))
        finally:
            desk._flock = saved
            desk.CHANNEL_INDEX.clear()
            desk.CHANNEL_INDEX.update(index)
            desk.app.dependency_overrides.clear()

    def read_thread_with(self, fake):
        """Call the route function directly; the owner-thread regression needs no ASGI client."""
        from web import app as desk
        saved = desk._flock
        desk._flock = lambda: fake
        try:
            return desk.api_owner_thread("owner", limit=50)
        finally:
            desk._flock = saved

    def test_a_web_message_lands_in_the_owner_mailbox_and_is_typed_straight_to_the_desk(self):
        fake = Fake()
        reply = self.run_with(fake, lambda c: c.post("/api/owner-send", json={"text": "hello desk"}))
        self.assertEqual(200, reply.status_code, reply.text)
        data = reply.json()
        self.assertEqual(("stored", "i7"), (data["outcome"], data["message_id"]))
        self.assertEqual("sent", data["notify"]["state"], "DIRECT, not an inbox nudge")
        self.assertEqual("guarded", data["notify"]["route"])
        verbs = [v for v, _ in fake.calls]
        self.assertNotIn("inbox_send", verbs, "no inbox line when the direct send worked")
        ingest = next(a for v, a in fake.calls if v == "owner_ingest")
        self.assertEqual(("hello desk", "web"), (ingest["body"], ingest["transport"]))
        typed = next(a for v, a in fake.calls if v == "send_text")
        self.assertEqual("%133", typed["target"])
        self.assertIn("hello desk", typed["text"])
        self.assertIn("owner reply i7", typed["text"], "the desk is told how to answer")
        self.assertLess(verbs.index("owner_ingest"), verbs.index("send_text"))

    def test_the_outcome_never_claims_delivery(self):
        data = self.run_with(Fake(), lambda c: c.post("/api/owner-send", json={"text": "x"})).json()
        self.assertEqual("stored", data["outcome"])
        self.assertNotIn("delivered", str(data["outcome"]).lower())

    def test_no_live_desk_still_stores_and_says_so(self):
        fake = Fake(panes=[])
        data = self.run_with(fake, lambda c: c.post("/api/owner-send", json={"text": "hi"})).json()
        self.assertEqual("stored", data["outcome"])
        self.assertEqual("desk_offline", data["notify"]["state"])
        self.assertNotIn("inbox_send", [v for v, _ in fake.calls])

    def test_a_quiet_desk_is_still_nudged(self):
        """Measured live: the desk had no declared turn (agent_status None) and read as offline."""
        quiet = dict(DESK, agent_status=None)
        fake = Fake(panes=[quiet])
        data = self.run_with(fake, lambda c: c.post("/api/owner-send", json={"text": "hi"})).json()
        self.assertEqual("sent", data["notify"]["state"])
        self.assertEqual("%133", next(a for v, a in fake.calls if v == "send_text")["target"])

    def test_a_failed_direct_send_falls_back_to_the_inbox_nudge(self):
        fake = Fake(fail_direct=True)
        data = self.run_with(fake, lambda c: c.post("/api/owner-send", json={"text": "hi"})).json()
        self.assertEqual("stored", data["outcome"])
        self.assertEqual("nudged", data["notify"]["state"])
        self.assertIn("terminal busy", data["notify"]["direct_failed"])
        self.assertEqual("%133", next(a for v, a in fake.calls if v == "inbox_send")["to"])

    def test_a_refused_direct_send_falls_back_to_the_nudge(self):
        """t1166: 'refused' returns ok False without raising; the desk must still be told."""
        fake = Fake(direct_outcome="refused")
        data = self.run_with(fake, lambda c: c.post("/api/owner-send", json={"text": "hi"})).json()
        self.assertEqual("nudged", data["notify"]["state"], data)
        self.assertIn("refused", data["notify"]["direct_failed"])
        self.assertIn("inbox_send", [v for v, _ in fake.calls])

    def test_a_queued_or_unconfirmed_direct_send_is_reported_as_such_and_never_doubled(self):
        for outcome in ("queued", "unconfirmed"):
            with self.subTest(outcome=outcome):
                fake = Fake(direct_outcome=outcome)
                data = self.run_with(fake, lambda c: c.post("/api/owner-send", json={"text": "hi"})).json()
                self.assertEqual(outcome, data["notify"]["state"], "never overclaimed as 'sent'")
                self.assertNotIn("inbox_send", [v for v, _ in fake.calls],
                                 "a held or maybe-delivered message must not be sent twice")

    def test_both_routes_failing_is_reported_not_hidden(self):
        data = self.run_with(Fake(fail_direct=True, fail_nudge=True),
                             lambda c: c.post("/api/owner-send", json={"text": "hi"})).json()
        self.assertEqual("stored", data["outcome"])
        self.assertEqual("nudge_failed", data["notify"]["state"])
        self.assertIn("direct_failed", data["notify"])

    def test_empty_oversize_and_attachments_are_refused_before_the_daemon(self):
        fake = Fake()
        post = lambda body: self.run_with(fake, lambda c: c.post("/api/owner-send", json=body)).status_code
        self.assertEqual(400, post({"text": "  "}))
        self.assertEqual(400, post({"text": "x" * 16001}))
        self.assertEqual(422, post({"text": "x", "attachment_ids": [1]}))
        self.assertEqual([], [v for v, _ in fake.calls if v == "owner_ingest"])

    def test_the_thread_shows_questions_and_the_desks_replies_in_order(self):
        rows = [
            # A human-origin question is ingested without a sender pane.
            {"id": "i5", "body": "q1", "created_at": 1000.0, "transport": "web", "sender": None, "reply_to": None, "answered": True},
            # A desk-authored answer has both a sender pane and a reply edge.
            {"id": "i6", "body": "a1", "created_at": 1010.0, "transport": "web", "sender": "%133", "reply_to": "i5"},
            # Agent mail to the desk is a top-level message, not a human question.
            {"id": "i9", "body": "agent report", "created_at": 1020.0, "transport": None, "sender": "%21", "reply_to": None, "answered": False},
            # A human-origin reply still has no sender pane; reply_to does not make it agent-authored.
            {"id": "i10", "body": "human follow-up", "created_at": 1030.0, "transport": "web", "sender": None, "reply_to": "i9"},
            # A legacy row with neither author nor transport provenance stays unattributed.
            {"id": "i11", "body": "legacy row", "created_at": 1040.0, "transport": None, "sender": None, "reply_to": None},
            # A report transport is not assumed to be human-origin without proof.
            {"id": "i12", "body": "generated report", "created_at": 1050.0, "transport": "report", "sender": None, "reply_to": None},
        ]
        data = self.read_thread_with(Fake(conversation=rows))
        items = data["items"]
        self.assertEqual([5, 6, 9, 10, 11, 12], [i["id"] for i in items])
        self.assertEqual(["owner", "agent", "agent", "owner", "unknown", "unknown"], [i["role"] for i in items])
        self.assertEqual([None, "%133", "%21", None, None, None], [i["sender"] for i in items],
                         "the API keeps the source sender as explicit attribution evidence")
        self.assertEqual("answered", items[0]["outcome"])
        self.assertNotIn("outcome", items[2], "agent-authored mail has no owner delivery outcome")
        self.assertNotIn("delivery", items[1], "a desk reply carries no delivery state")
        self.assertNotIn("delivery", items[2], "agent-authored mail is not an owner delivery")
        self.assertIn("waiting for the desk", items[3]["outcome"])
        self.assertIn("delivery", items[3], "a human-origin reply retains owner-side delivery state")
        self.assertNotIn("delivery", items[4], "ambiguous legacy attribution is not guessed as owner mail")
        self.assertNotIn("delivery", items[5], "report transport is not assumed to be a human sender")


if __name__ == "__main__":
    unittest.main()
