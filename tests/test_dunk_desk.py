"""The desk credential: one permission (send to a listed pane), nothing else."""

import json
import os
import sys
import tempfile
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

import _bootstrap  # noqa: F401
import dunk_api  # noqa: E402
import dunk_desk  # noqa: E402
from dunk_core import DunkError, Flock  # noqa: E402
from fakes import FakeHerdr  # noqa: E402


class DeskCredentialTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.path = os.path.join(self.tmp.name, "desk.json")
        self.herdr = FakeHerdr()
        self.flock = Flock(herdr=self.herdr, persist=False, restore=False)
        self.env = mock.patch.dict(os.environ, {"DUNKINGSHEEP_DESK_FILE": self.path})
        self.env.start()
        self.desk = dunk_desk.DeskCredential(path=self.path)
        self.token = self.desk.issue(["%1", "%2"])

    def tearDown(self):
        self.env.stop()
        self.flock.close()
        self.tmp.cleanup()

    def send(self, target, token, command="send_text", text="hello"):
        args = {"target": target, "desk_token": token}
        if command == "send_text":
            args.update({"text": text, "hold_while_typing": False, "wait_s": 0.1})
        else:
            args.update({"keys": "Enter", "hold_while_typing": False, "wait_s": 0.1})
        return dunk_api.dispatch(self.flock, command, args, self_pane_id="%desk")

    def test_a_listed_pane_gets_the_send(self):
        self.desk.issue(["w2:p1"])
        token = json.load(open(self.path))["token"]      # rotating issues a new one
        result = self.send("w2:p1", token)
        self.assertTrue(result["ok"], result)
        self.assertIn(("w2:p1", "hello"), self.herdr.sends)

    def test_a_pane_that_is_not_listed_is_refused_by_name(self):
        """A VALID token for a pane that is not on the list: the token is fine, the
        pane is not - and the refusal has to say so, or the operator debugs the
        wrong half."""
        with self.assertRaisesRegex(DunkError, "may not type into"):
            self.send("w2:p1", self.token)

    def test_the_right_token_for_an_unlisted_pane_is_still_refused(self):
        # The token is valid; the pane is not on the list. One rule per boundary.
        desk = dunk_desk.DeskCredential(path=self.path)
        token = desk.issue(["%9"])                        # w2:p1 not included
        with self.assertRaisesRegex(DunkError, "may not type into"):
            self.send("w2:p1", token)

    def test_a_wrong_token_is_refused(self):
        with self.assertRaisesRegex(DunkError, "not valid"):
            self.send("w2:p1", "not-the-token")

    def test_the_credential_cannot_close_a_pane(self):
        with self.assertRaises(DunkError):
            dunk_api.dispatch(self.flock, "close",
                              {"kind": "pane", "target": "w2:p1",
                               "desk_token": self.token}, self_pane_id="%desk")

    def test_the_credential_cannot_start_an_agent(self):
        with self.assertRaises(DunkError):
            dunk_api.dispatch(self.flock, "start_agent",
                              {"name": "x", "command": "claude",
                               "desk_token": self.token}, self_pane_id="%desk")

    def test_a_desk_send_does_not_leak_into_the_next_request(self):
        """The permission is per request, not per process: after a desk send, a
        plain send from a non-superuser must be refused again."""
        self.desk.issue(["w2:p1"])
        token = json.load(open(self.path))["token"]
        self.send("w2:p1", token)
        with self.assertRaisesRegex(DunkError, "requires a superuser"):
            dunk_api.dispatch(self.flock, "send_text",
                              {"target": "w2:p1", "text": "leak?",
                               "hold_while_typing": False, "wait_s": 0.1},
                              self_pane_id="%desk")

    def test_the_pane_set_is_read_every_time(self):
        desk = dunk_desk.DeskCredential(path=self.path)
        token = desk.issue(["%1"])                        # rotate, narrower list
        with self.assertRaisesRegex(DunkError, "may not type into"):
            self.send("w2:p1", token)


if __name__ == "__main__":
    unittest.main()
