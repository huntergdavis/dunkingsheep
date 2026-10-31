"""t1151: the web composer's owner-mailbox path, through the real routes with a fake daemon."""
import importlib.util
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

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
            {"id": "i5", "body": "q1", "created_at": 1000.0, "transport": "web", "reply_to": None, "answered": True},
            {"id": "i6", "body": "a1", "created_at": 1010.0, "transport": "web", "reply_to": "i5"},
            {"id": "i9", "body": "q2", "created_at": 1020.0, "transport": "telegram", "reply_to": None, "answered": False},
        ]
        data = self.run_with(Fake(conversation=rows), lambda c: c.get("/api/owner-thread")).json()
        items = data["items"]
        self.assertEqual([5, 6, 9], [i["id"] for i in items])
        self.assertEqual(["owner", "agent", "owner"], [i["role"] for i in items])
        self.assertEqual("answered", items[0]["outcome"])
        self.assertIn("waiting for the desk", items[2]["outcome"])
        self.assertNotIn("delivery", items[1], "a desk reply carries no delivery state")


if __name__ == "__main__":
    unittest.main()
