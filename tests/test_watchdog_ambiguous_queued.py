"""t409 OPTION 1 (VP i32480): a message held on an AMBIGUOUS composer is REPORTED, never acted on.

The class is real and measured live: m9180 sat on pane %79 with reason
"ambiguous composer/transcript: prompt row 18 is separated from footer row 23 by 2 blank
rows" for 13 refused attempts over 40 minutes, one row per 137 s, and never terminated.
Terminalising it was rejected - an unchanged frame is not proof of a dead agent, permanent
failure or human intent, and a 'gone' signature is missing telemetry rather than stability.
So this rule only says so, through the watchdog's existing digest/dedupe path.

What these tests pin: the class surfaces from real inputs; identical repeated observations are
suppressed by the EXISTING cooldown/streak policy; a changed or cleared condition is reported
honestly (no stale alert); and the read changes nothing - no payload in the alert, no key, no
retry, no clear, no re-target, no drop, no terminal state, no task ownership change.
"""
import hashlib
import json
import os
import sqlite3
import sys
import tempfile
import time
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import dunk_core  # noqa: E402
import dunk_watchdog as W  # noqa: E402

PANE = "%79"
MESSAGE = "m9180"
SECRET_BODY = "the private message body that must never appear in an alert"
AMBIGUOUS = ("ambiguous composer/transcript: prompt row 18 is separated from footer row 23 "
             "by 2 blank rows; visible prompt content may be pending input or transcript, so "
             "no keys are authorised")
TURN_WAIT = ("codex does not queue text typed during a turn (measured: notices are lost), so "
             "this waits for the turn to end")


def _rule(**overrides):
    rule = {"enabled": True, "severity": "warning", "class": "ops",
            "cooldown_minutes": 240, "must_hold_runs": 1,
            "min_holds": 10, "min_age_minutes": 20}
    rule.update(overrides)
    return rule


class AmbiguousQueuedTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = self.tmp.name
        self.now = 5_000_000.0
        self.write_state(holds=13, age_minutes=40)
        self.write_metrics(AMBIGUOUS)

    def tearDown(self):
        self.tmp.cleanup()

    # -- fixtures ---------------------------------------------------------
    def write_state(self, holds=13, age_minutes=40, reason_pane=PANE, queued=True):
        rows = []
        if queued:
            rows.append({"id": MESSAGE, "pane_id": reason_pane, "target_label": "t372-wiring",
                         "text": SECRET_BODY, "source": "send_text", "kind": "text",
                         "hold_while_typing": True, "is_notice": False, "notify_for": [],
                         "holds": holds, "next_attempt_at": self.now + 300,
                         "hold_signature": ["gone", ""],
                         "created_at": self.now - age_minutes * 60})
        payload = {"outbox": rows, "dunkers": [], "decided": {}}
        with open(os.path.join(self.dir, "dunks.json"), "w", encoding="utf-8") as handle:
            json.dump(payload, handle)

    def write_metrics(self, reason, attempts=13, at=None, extra_rows=()):
        path = os.path.join(self.dir, "metrics.db")
        if os.path.exists(path):
            os.unlink(path)
        db = sqlite3.connect(path)
        db.execute("CREATE TABLE events (id INTEGER PRIMARY KEY, at REAL, kind TEXT, "
                   "pane_id TEXT, mailbox TEXT, sender TEXT, source TEXT, outcome TEXT, "
                   "reason TEXT, duration_ms INTEGER, attempts INTEGER, bytes INTEGER)")
        when = self.now - 60 if at is None else float(at)
        db.execute("INSERT INTO events (at, kind, pane_id, outcome, reason, attempts) "
                   "VALUES (?, 'message.held', ?, 'held', ?, ?)",
                   (when, PANE, reason, attempts))
        for row_reason, row_at in extra_rows:
            db.execute("INSERT INTO events (at, kind, pane_id, outcome, reason, attempts) "
                       "VALUES (?, 'message.held', ?, 'held', ?, ?)",
                       (float(row_at), PANE, row_reason, attempts))
        db.commit()
        db.close()

    def inputs(self):
        return W.Inputs({"stores": {"config_dir": self.dir}}, self.now)

    def findings(self, **overrides):
        return W.rule_delivery_ambiguous_queued(self.inputs(), _rule(**overrides))

    def snapshot(self):
        out = {}
        for name in ("dunks.json", "metrics.db"):
            path = os.path.join(self.dir, name)
            with open(path, "rb") as handle:
                out[name] = hashlib.sha256(handle.read()).hexdigest()
        return out

    # -- the class surfaces ----------------------------------------------
    def test_the_held_message_surfaces_with_structure_not_content(self):
        found = self.findings()
        self.assertEqual(1, len(found), found)
        item = found[0]
        self.assertEqual("delivery.ambiguous_queued", item["rule"])
        self.assertEqual(MESSAGE, item["key"])
        self.assertEqual(13, item["evidence"]["holds"])
        self.assertEqual(40, item["evidence"]["age_minutes"])
        self.assertEqual(PANE, item["evidence"]["pane"])
        self.assertEqual("ambiguous_reader", item["evidence"]["reason_class"])
        self.assertEqual({"prompt_row": 18, "footer_row": 23, "blank_rows": 2},
                         item["evidence"]["shape"],
                         "the structural integers travel; the reason text does not")
        self.assertIn("last_refusal_age_s", item["evidence"])
        self.assertNotIn("reason", [k for k in item["evidence"] if k != "reason_class"])
        # NO payload and no death claim
        blob = json.dumps(item)
        self.assertNotIn(SECRET_BODY, blob)
        self.assertNotIn("t372-wiring", blob)
        # it may say "not assumed dead" (a negation); it must never CLAIM one
        for claim in ('"is dead"', "dead session", "recipient is dead", "agent is dead"):
            self.assertNotIn(claim, blob.lower(), f"the alert must not claim {claim}")
        self.assertIn("not terminal", item["summary"])
        self.assertIn("not assumed dead", item["summary"])

    def test_a_young_or_lightly_retried_message_is_not_reported(self):
        self.write_state(holds=13, age_minutes=5)          # too young
        self.assertEqual([], self.findings())
        self.write_state(holds=3, age_minutes=40)          # too few attempts
        self.assertEqual([], self.findings())

    def test_a_turn_wait_or_box_hold_is_not_this_class(self):
        self.write_metrics(TURN_WAIT)
        self.assertEqual([], self.findings(),
                         "a turn-wait holds for a different reason and drains by itself")

    def test_a_cleared_or_decided_message_is_no_longer_reported(self):
        self.write_state(queued=False)                     # consumed/decided
        self.assertEqual([], self.findings(),
                         "a condition that cleared must not keep alerting")

    # -- dedupe through the EXISTING policy ------------------------------
    def test_repeated_identical_observations_are_suppressed_by_the_existing_policy(self):
        """Driven LIVE through run(), because the dedupe state is only written when live."""
        config = json.load(open(os.path.join(os.path.dirname(os.path.dirname(
            os.path.abspath(__file__))), "packaging", "watchdog.example.json")))
        config["stores"]["config_dir"] = self.dir
        config["state_path"] = os.path.join(self.dir, "watchdog-state.json")
        config["heartbeat_path"] = os.path.join(self.dir, "heartbeat.json")
        config["enabled"] = True
        config["dry_run"] = False
        config["escalation"]["owner"]["enabled"] = False
        config["escalation"]["vp"]["min_severity"] = "warning"
        config["rules"]["delivery.ambiguous_queued"].update({"enabled": True,
                                                            "cooldown_minutes": 240})
        for rule_id in list(config["rules"]):
            if rule_id != "delivery.ambiguous_queued":
                config["rules"][rule_id]["enabled"] = False
        W.validate_config(config)

        sent = []

        def send(to, subject, body):        # the real callable shape: inbox_sender
            sent.append({"to": to, "subject": subject, "body": body})
            return True

        first = W.run(config, self.now, send=send)
        self.assertEqual(1, first["counts"]["findings"], first["counts"])
        self.assertEqual(1, first["counts"]["vp"],
                         f"the first observation must reach the VP channel: {first['counts']}")
        self.assertTrue(sent, "and something is actually delivered")
        state = json.load(open(config["state_path"]))
        self.assertTrue(state["alerts"], "the existing policy must record the alert")
        # A second identical observation inside the cooldown: the existing dedupe policy
        # must keep it out of the alert stream entirely.
        before = len(sent)
        second = W.run(config, self.now + 60, send=send)
        self.assertEqual(0, second["counts"]["vp"],
                         f"an unchanged condition must not re-page: {second['counts']}")
        self.assertEqual(before, len(sent), "and nothing new is delivered")

    # -- the alert changes nothing ---------------------------------------
    def test_the_read_is_read_only(self):
        before = self.snapshot()
        found = self.findings()
        self.assertEqual(1, len(found))
        self.assertEqual(before, self.snapshot(),
                         "the rule must not write to the state or metrics stores")

    def test_queue_payload_decided_next_attempt_and_attempts_are_unchanged(self):
        before = json.load(open(os.path.join(self.dir, "dunks.json")))
        self.findings()
        after = json.load(open(os.path.join(self.dir, "dunks.json")))
        self.assertEqual(before, after)
        row = after["outbox"][0]
        self.assertEqual(SECRET_BODY, row["text"], "the message body is untouched")
        self.assertEqual(13, row["holds"])
        self.assertEqual(300, round(row["next_attempt_at"] - self.now),
                         "the scheduled attempt is untouched")
        self.assertEqual({}, after["decided"], "nothing became terminal")

    def test_the_marker_matches_the_reader_constant(self):
        self.assertTrue(dunk_core.AMBIGUOUS_COMPOSER.startswith(W.AMBIGUOUS_READER_MARKER),
                        "the watchdog marker must track dunk_core.AMBIGUOUS_COMPOSER")


class MigrationTests(unittest.TestCase):
    def test_an_old_config_gains_the_rule_disabled(self):
        example = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                               "packaging", "watchdog.example.json")
        config = json.load(open(example))
        config["rules"].pop("delivery.ambiguous_queued")
        W.validate_config(config)                      # must not raise: it is a migration
        block = config["rules"]["delivery.ambiguous_queued"]
        self.assertFalse(block["enabled"],
                         "an old config keeps its behaviour: the new rule arrives disabled")
        self.assertEqual(W.HOLD_ATTEMPT_LIMIT_DEFAULT, block["min_holds"])

    def test_the_budget_default_tracks_the_delivery_budget(self):
        self.assertEqual(dunk_core.HOLD_ATTEMPT_LIMIT, W.HOLD_ATTEMPT_LIMIT_DEFAULT,
                         "the watchdog's default attempt budget must not drift from t269's")


class EvidenceContractTests(unittest.TestCase):
    """VP i32622: the two source defects - a future metric speaking for now, and an
    arbitrary reason suffix being echoed into the finding."""

    SENTINEL = "PRIVATE_SENTINEL_CONTENT_9f2b"

    def setUp(self):
        self.now = 6_000_000.0
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = self.tmp.name
        self.rule = {"enabled": True, "severity": "warning", "class": "ops",
                     "cooldown_minutes": 240, "must_hold_runs": 1,
                     "min_holds": 10, "min_age_minutes": 20}

    def tearDown(self):
        self.tmp.cleanup()

    def write(self, rows, holds=13, age_minutes=40):
        with open(os.path.join(self.dir, "dunks.json"), "w", encoding="utf-8") as handle:
            json.dump({"dunkers": [], "decided": {}, "outbox": [
                {"id": MESSAGE, "pane_id": PANE, "target_label": "t372-wiring",
                 "text": "body", "source": "send_text", "kind": "text",
                 "hold_while_typing": True, "is_notice": False, "notify_for": [],
                 "holds": holds, "next_attempt_at": self.now + 60,
                 "hold_signature": ["box", ""],
                 "created_at": self.now - age_minutes * 60}]}, handle)
        db = sqlite3.connect(os.path.join(self.dir, "metrics.db"))
        db.execute("CREATE TABLE events (id INTEGER PRIMARY KEY, at REAL, kind TEXT, "
                   "pane_id TEXT, mailbox TEXT, sender TEXT, source TEXT, outcome TEXT, "
                   "reason TEXT, duration_ms INTEGER, attempts INTEGER, bytes INTEGER)")
        for reason, at in rows:
            db.execute("INSERT INTO events (at, kind, pane_id, outcome, reason, attempts) "
                       "VALUES (?, 'message.held', ?, 'held', ?, 13)", (at, PANE, reason))
        db.commit()
        db.close()

    def findings(self):
        inputs = W.Inputs({"stores": {"config_dir": self.dir}}, self.now)
        return W.rule_delivery_ambiguous_queued(inputs, self.rule)

    # -- temporal ---------------------------------------------------------
    def test_a_current_turn_wait_is_not_overridden_by_a_future_ambiguous_row(self):
        self.write([(TURN_WAIT, self.now - 60), (AMBIGUOUS, self.now + 3600)])
        self.assertEqual([], self.findings(),
                         "a row dated in the future must not speak for the present")

    def test_a_future_only_ambiguous_row_is_not_reported_at_all(self):
        self.write([(AMBIGUOUS, self.now + 3600)])
        self.assertEqual([], self.findings())

    def test_a_current_ambiguous_row_is_reported_with_its_freshness(self):
        self.write([(AMBIGUOUS, self.now - 30)])
        found = self.findings()
        self.assertEqual(1, len(found), found)
        self.assertEqual(30, found[0]["evidence"]["last_refusal_age_s"],
                         "the finding says how old the observation is")

    # -- privacy ----------------------------------------------------------
    def test_an_arbitrary_reason_suffix_never_reaches_the_finding(self):
        self.write([(AMBIGUOUS + " " + self.SENTINEL, self.now - 60)])
        found = self.findings()
        self.assertEqual(1, len(found))
        self.assertNotIn(self.SENTINEL, json.dumps(found),
                         "the reason field can carry anything; the finding must not echo it")

    def test_an_unrecognised_structural_sentence_reports_the_class_only(self):
        self.write([("ambiguous composer/transcript: " + self.SENTINEL, self.now - 60)])
        found = self.findings()
        self.assertEqual(1, len(found))
        self.assertEqual({}, found[0]["evidence"]["shape"],
                         "no structural integers are invented from unmatched text")
        self.assertNotIn(self.SENTINEL, json.dumps(found))


if __name__ == "__main__":
    unittest.main()
