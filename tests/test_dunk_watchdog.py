"""Exception watchdog (no model), tested against docs/WATCHDOG_ESCALATION_SPEC.md
and the VP's tiers (i19817, i19821):

* every rule fires at its boundary and stays silent below it;
* config is refused (named) rather than defaulted, including zero thresholds,
  zero rate limits and owner classes on rules the spec does not allow;
* fail toward not paging: config errors, missing or corrupt state and rule
  errors leave the OWNER queue unchanged and tell VP once;
* an owner page needs its hold, respects the 3-per-hour cap, is DELIVERED only
  when the forwarder confirms it, and a failed post is one VP self error;
* VP-only classes never reach OWNER; recovery lines; dry-run line contract;
  replay; dead-man (independent, carries the config error, falls back when the
  inbox fails); system sender; and no model anywhere."""

import ast
import contextlib
import copy
import inspect
import io
import json
import os
import sqlite3
import subprocess
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
sys.path.insert(0, HERE)
sys.path.insert(0, ROOT)

import _bootstrap  # noqa: F401  (isolate live state)
import dunk_watchdog as wd  # noqa: E402
import dunk_watchdog_deadman as deadman  # noqa: E402
from dunk_core import DunkError, Flock  # noqa: E402
from dunk_inbox import Inbox  # noqa: E402
from fakes import FakeHerdr  # noqa: E402

NOW = 1_800_000_000.0
MIN = 60.0
HOUR = 3600.0


def read(path):
    with open(path, encoding="utf-8") as handle:
        return handle.read()


EXAMPLE = json.loads(read(os.path.join(ROOT, "packaging", "watchdog.example.json")))
FAKE_TOKEN = "ghp_" + "A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8"  # synthetic: matches the pattern, is no credential

SCHEMA = {
    "tasks.db": [
        """CREATE TABLE tasks (id TEXT PRIMARY KEY, scope TEXT NOT NULL, title TEXT NOT NULL,
           body TEXT NOT NULL DEFAULT '', status TEXT NOT NULL DEFAULT 'open',
           priority TEXT NOT NULL DEFAULT 'medium', parent_id TEXT,
           multi_assignee INTEGER NOT NULL DEFAULT 0, attempts INTEGER NOT NULL DEFAULT 0,
           assignees TEXT NOT NULL DEFAULT '[]', depends_on TEXT NOT NULL DEFAULT '[]',
           created_at REAL NOT NULL, updated_at REAL NOT NULL, created_by TEXT,
           completed_by TEXT, completed_at REAL, cancelled_at REAL, block_reason TEXT)""",
        """CREATE TABLE task_leases (task_id TEXT NOT NULL, pane_id TEXT NOT NULL,
           claimed_at REAL NOT NULL, heartbeat_at REAL NOT NULL, expires_at REAL NOT NULL,
           PRIMARY KEY (task_id, pane_id))""",
        """CREATE TABLE task_events (id INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT NOT NULL,
           event TEXT NOT NULL, at REAL NOT NULL, actor TEXT, scope TEXT, priority TEXT,
           status_from TEXT, status_to TEXT, duration_ms INTEGER, meta TEXT)""",
    ],
    "inbox.db": [
        """CREATE TABLE messages (id TEXT PRIMARY KEY, mailbox TEXT NOT NULL, mailbox_label TEXT,
           pane_id TEXT, sender TEXT, subject TEXT, body TEXT NOT NULL, state TEXT NOT NULL,
           created_at REAL NOT NULL, read_at REAL, acked_at REAL, cancelled_at REAL, reason TEXT,
           notify_state TEXT, notify_reason TEXT, archived_at REAL)""",
    ],
    "metrics.db": [
        """CREATE TABLE events (id INTEGER PRIMARY KEY AUTOINCREMENT, at REAL NOT NULL,
           kind TEXT NOT NULL, pane_id TEXT, mailbox TEXT, sender TEXT, source TEXT, outcome TEXT,
           reason TEXT, duration_ms INTEGER, attempts INTEGER, bytes INTEGER)""",
    ],
}

HOST_OK = {"load1": 1.0, "load5": 1.0, "cpus": 8, "used_fraction": 0.1, "free_bytes": 50 * 2**30, "inode_used_fraction": 0.1}


class FakeOwner:
    """The OWNER queue: records every post; confirms through `through`."""

    def __init__(self, through=None, fail=None):
        self.posts = []
        self.through = through
        self.fail = fail

    def post(self, body):
        if self.fail:
            raise self.fail
        self.posts.append(body)
        return 100 + len(self.posts)

    def confirmed_through(self):
        return self.through


class Fleet:
    """A throwaway config dir with the real store schemas, filled per test."""

    def __init__(self, tmp):
        self.tmp = tmp
        self.dir = os.path.join(tmp, "store")
        os.makedirs(self.dir)
        for name, statements in SCHEMA.items():
            conn = sqlite3.connect(os.path.join(self.dir, name))
            for sql in statements:
                conn.execute(sql)
            conn.commit()
            conn.close()
        self.dunks = []
        self.health = {"checked_at": NOW, "ok": True, "problems": []}
        self.panes = []
        self.hosts = {}
        self.write_json()
        self.msg_seq = 0

    def host(self, path):
        return dict(HOST_OK, **self.hosts.get(path, {}))

    def sql(self, db, statement, params=()):
        conn = sqlite3.connect(os.path.join(self.dir, db))
        conn.execute(statement, params)
        conn.commit()
        conn.close()

    def write_json(self):
        with open(os.path.join(self.dir, "dunks.json"), "w") as handle:
            json.dump({"version": 1, "next_id": 1, "dunkers": self.dunks}, handle)
        with open(os.path.join(self.dir, "health.json"), "w") as handle:
            json.dump(self.health, handle)

    def task(self, task_id, status, updated_at, assignees=(), title="t", body="", block_reason=None):
        self.sql("tasks.db", "INSERT INTO tasks (id, scope, title, body, status, assignees, created_at, "
                 "updated_at, block_reason) VALUES (?, 'global', ?, ?, ?, ?, ?, ?, ?)",
                 (task_id, title, body, status, json.dumps(list(assignees)), updated_at,
                  updated_at, block_reason))

    def task_event(self, task_id, event, at, actor=None, meta=None):
        self.sql("tasks.db", "INSERT INTO task_events (task_id, event, at, actor, meta) VALUES (?,?,?,?,?)",
                 (task_id, event, at, actor, json.dumps(meta) if meta else None))

    def lease(self, task_id, pane, claimed, heartbeat, expires):
        self.sql("tasks.db", "INSERT INTO task_leases VALUES (?,?,?,?,?)", (task_id, pane, claimed, heartbeat,
                                                                           expires))

    def message(self, mailbox, state, created_at, pane_id=None, sender=None, reason=None, read_at=None,
                body="BODY-NEVER-REPORTED"):
        self.msg_seq += 1
        message_id = f"i{self.msg_seq}"
        self.sql("inbox.db", "INSERT INTO messages (id, mailbox, pane_id, sender, body, state, created_at, "
                 "reason, read_at) VALUES (?,?,?,?,?,?,?,?,?)",
                 (message_id, mailbox, pane_id, sender, body, state, created_at, reason, read_at))
        return message_id

    def metric(self, kind, source, at):
        self.sql("metrics.db", "INSERT INTO events (at, kind, source) VALUES (?,?,?)", (at, kind, source))

    def config(self, **rules):
        config = copy.deepcopy(EXAMPLE)
        config["stores"]["config_dir"] = self.dir
        config["state_path"] = os.path.join(self.tmp, "state.json")
        config["heartbeat_path"] = os.path.join(self.tmp, "heartbeat")
        for rule_id in config["rules"]:
            config["rules"][rule_id]["enabled"] = False
        for rule_id, overrides in rules.items():
            config["rules"][rule_id.replace("__", ".")].update({"enabled": True, **overrides})
        return wd.validate_config(config)

    def evaluate(self, config, now=NOW):
        result = wd.evaluate(config, now, panes_provider=lambda: list(self.panes), host_sampler=self.host)
        assert not result["errors"], result["errors"]
        return result["findings"]

    def run(self, config, now=NOW, owner=None, dry=False, sent=None):
        sent = [] if sent is None else sent
        report = wd.run(config, now, dry_run_flag=dry, send=lambda *a: sent.append(a),
                        owner_channel=owner or FakeOwner(), panes_provider=lambda: list(self.panes),
                        host_sampler=self.host)
        return report, sent


class WatchdogCase(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = self._tmp.name
        self.fleet = Fleet(self.tmp)

    def tearDown(self):
        self._tmp.cleanup()

    def keys(self, findings, rule=None):
        return [(f["rule"], f["key"]) for f in findings if rule is None or f["rule"] == rule]

    def live(self, config, **owner):
        config.update(enabled=True, dry_run=False)
        if owner:
            config["escalation"]["owner"].update(enabled=True, approved_by="VP i19821", **owner)
        return config

    def seed_state(self, config, state=None):
        wd.save_state(config["state_path"], state or wd.empty_state())


class RuleBoundaryTests(WatchdogCase):
    """Each rule: one case that fires exactly at its threshold, one just below."""

    def test_a_task_held_for_the_owner_does_not_page_but_is_counted(self):
        cfg = self.fleet.config(task__blocked_stale={"threshold_minutes": 60})
        self.fleet.task("held", "blocked", NOW - 5 * 24 * HOUR,
                        block_reason="HELD FOR OWNER DECISION: A2 needs 3-7 days of funding")
        self.fleet.task("stale", "blocked", NOW - 2 * HOUR)
        findings = self.fleet.evaluate(cfg)
        paged = [f for f in findings if f["key"] != "held-owner"]
        self.assertEqual(["stale"], [f["key"] for f in paged])
        digest = [f for f in findings if f["key"] == "held-owner"]
        self.assertEqual(1, len(digest))
        self.assertIn("1 task(s) held on owner decisions", digest[0]["summary"])

    def test_the_hold_marker_is_declared_not_prose(self):
        cfg = self.fleet.config(task__blocked_stale={"threshold_minutes": 60})
        self.fleet.task("prose", "blocked", NOW - 2 * HOUR,
                        block_reason="waiting for the owner to decide")  # no declared marker
        self.assertEqual([("task.blocked_stale", "prose")],
                         [(f["rule"], f["key"]) for f in self.fleet.evaluate(cfg)])

    def test_task_blocked_stale(self):
        cfg = self.fleet.config(task__blocked_stale={"threshold_minutes": 60})
        self.fleet.task("t1", "blocked", NOW - 60 * MIN)
        self.fleet.task("t2", "blocked", NOW - 60 * MIN + 1)
        self.fleet.task("t3", "blocked", NOW - 120 * MIN)
        self.fleet.task_event("t3", "status", NOW - 10 * MIN)
        self.assertEqual([("task.blocked_stale", "t1")], self.keys(self.fleet.evaluate(cfg)))

    def test_task_lease_expired_unrenewed_and_reaped(self):
        cfg = self.fleet.config(task__lease_stale={"never_renewed_minutes": 30, "lookback_minutes": 60, "fresh_minutes": 60})
        self.fleet.lease("a", "w1:p1", NOW - 99 * MIN, NOW - 50 * MIN, NOW)
        self.fleet.lease("b", "w1:p1", NOW - 30 * MIN, NOW - 30 * MIN, NOW + HOUR)
        self.fleet.lease("c", "w1:p1", NOW - 29 * MIN, NOW - 29 * MIN, NOW + HOUR)
        self.fleet.lease("d", "w1:p1", NOW - 99 * MIN, NOW - 1 * MIN, NOW + HOUR)
        self.fleet.task_event("e", "requeued", NOW - 60 * MIN, meta={"reason": "expired"})
        self.fleet.task_event("f", "requeued", NOW - 61 * MIN, meta={"reason": "expired"})
        self.fleet.task_event("g", "released", NOW - 5 * MIN, meta={"reason": "manual"})
        self.assertEqual([("task.lease_stale", "expired:a@w1:p1"), ("task.lease_stale", "reaped:1"),
                          ("task.lease_stale", "unrenewed:b@w1:p1")], self.keys(self.fleet.evaluate(cfg)))

    def test_old_reap_ages_out_of_the_fresh_window(self):
        cfg = self.fleet.config(task__lease_stale={
            "never_renewed_minutes": 30, "lookback_minutes": 1440,
            "fresh_minutes": 60})
        self.fleet.task_event("e", "requeued", NOW - 90 * MIN,
                              meta={"reason": "expired"})
        self.assertEqual([], self.fleet.evaluate(cfg),
                         "a 90-minute-old reap is history, not news")

    def test_a_reaped_event_is_reported_once_ever_not_again_after_cooldown(self):
        cfg = self.fleet.config(task__lease_stale={
            "never_renewed_minutes": 30, "lookback_minutes": 1440,
            "fresh_minutes": 1440})
        self.fleet.task_event("e", "requeued", NOW - 1 * MIN,
                              meta={"reason": "expired"})
        findings = self.fleet.evaluate(cfg)
        self.assertEqual(1, len(findings))
        self.assertTrue(findings[0].get("once"))
        state = wd.empty_state()
        first = wd.plan(cfg, findings,
                        state, wd.advance_streaks(cfg, state, findings, NOW), NOW)
        self.assertEqual("no", first[0]["vp"])
        # `run` records this after a send; simulate four hours later, past the
        # 240-minute cooldown, with the event still inside the fresh window.
        state["alerts"][wd.dedupe_key(findings[0])] = {"last_alerted": NOW}
        later = NOW + 4 * HOUR
        later_findings = [dict(findings[0], measured="241min old")]
        again = wd.plan(cfg, later_findings, state,
                        wd.advance_streaks(cfg, state, later_findings, later), later)
        self.assertEqual("dedupe", again[0]["vp"])

    def test_dunk_skip_loop_uses_lookback_not_recency_and_a_stable_key(self):
        cfg = self.fleet.config(dunk__skip_loop={"consecutive_skips": 3, "lookback_hours": 168})
        self.fleet.dunks = [{"id": "d1", "running": True}, {"id": "d2", "running": True}]
        self.fleet.write_json()
        for n in range(3):
            self.fleet.metric("dunk.skipped", "d1", NOW - 48 * HOUR + n)
        first = self.fleet.evaluate(cfg)
        self.fleet.metric("dunk.skipped", "d1", NOW - 47.5 * HOUR)  # the run grows
        grown = self.fleet.evaluate(cfg)
        self.assertEqual(self.keys(first), self.keys(grown), "a growing loop keeps its key (one incident)")
        self.fleet.metric("dunk.sent", "d1", NOW - 47 * HOUR)
        for n in range(2):
            self.fleet.metric("dunk.skipped", "d2", NOW - HOUR + n)
        self.assertEqual(["d1"], [f["evidence"]["dunk"] for f in self.fleet.evaluate(cfg)])
        self.assertEqual("4 consecutive skips", self.fleet.evaluate(cfg)[0]["measured"])

    def test_events_after_now_are_never_seen(self):
        cfg = self.fleet.config(dunk__skip_loop={"consecutive_skips": 3, "lookback_hours": 168})
        self.fleet.dunks = [{"id": "d1", "running": True}]
        self.fleet.write_json()
        for n in range(5):
            self.fleet.metric("dunk.skipped", "d1", NOW + 1 + n)
        self.assertEqual([], self.fleet.evaluate(cfg))

    def test_dunk_overdue(self):
        cfg = self.fleet.config(dunk__overdue={"missed_intervals": 3})
        self.fleet.dunks = [
            {"id": "d1", "running": True, "interval_minutes": 10, "next_send_at": NOW - 30 * MIN},
            {"id": "d2", "running": True, "interval_minutes": 10, "next_send_at": NOW - 30 * MIN + 1},
            {"id": "d3", "running": False, "interval_minutes": 10, "next_send_at": NOW - 99 * MIN},
            {"id": "d4", "running": True, "interval_minutes": 10, "next_send_at": None},
        ]
        self.fleet.write_json()
        self.assertEqual([("dunk.overdue", "d1"), ("dunk.overdue", "d4")], self.keys(self.fleet.evaluate(cfg)))

    def test_inbox_undeliverable_backlog_has_no_age_limit(self):
        cfg = self.fleet.config(inbox__undeliverable={"min_messages": 2, "min_age_hours": 0.01})
        for _ in range(2):
            self.fleet.message("w9:p8", "undeliverable", NOW - 30 * 24 * HOUR, reason="target pane no longer exists")
        self.fleet.message("w9:p7", "undeliverable", NOW - 30 * HOUR)
        found = self.fleet.evaluate(cfg)
        self.assertEqual([("inbox.undeliverable", "w9:p8")], self.keys(found))
        self.assertNotIn("BODY-NEVER-REPORTED", json.dumps(found))

    def test_inbox_undeliverable_pages_only_above_the_auto_archive_age(self):
        # The auto-archive clears undeliverable mail at 24h, so a page inside that
        # window describes mail that is about to disappear on its own. The floor
        # sits above it: a page must mean the archive did not catch this.
        cfg = self.fleet.config(inbox__undeliverable={"min_messages": 2, "min_age_hours": 26})
        for _ in range(3):
            self.fleet.message("young", "undeliverable", NOW - 3 * HOUR,
                               reason="target pane no longer exists")
        self.assertEqual([], self.keys(self.fleet.evaluate(cfg)))
        for _ in range(3):
            self.fleet.message("stuck", "undeliverable", NOW - 30 * HOUR,
                               reason="target pane no longer exists")
        self.assertEqual([("inbox.undeliverable", "stuck")], self.keys(self.fleet.evaluate(cfg)))

    def test_inbox_unread_live_and_dead(self):
        cfg = self.fleet.config(inbox__unread_live={"age_minutes": 60, "min_messages": 1},
                                inbox__unread_dead={"min_messages": 1},
                                inbox__undeliverable={"min_messages": 99, "min_age_hours": 0.01})
        self.fleet.panes = [{"pane_id": "w1:p1", "agent_status": "idle"}]
        self.fleet.message("alive", "queued", NOW - 60 * MIN, pane_id="w1:p1")
        self.fleet.message("alive2", "queued", NOW - 60 * MIN + 1, pane_id="w1:p1")
        self.fleet.message("gone", "queued", NOW - 1, pane_id="w9:p9")
        self.assertEqual([("inbox.unread_dead", "gone"), ("inbox.unread_live", "alive")],
                         self.keys(self.fleet.evaluate(cfg)))

    def test_inbox_ambiguous_sender_names_only_pane_resolving_labels(self):
        cfg = self.fleet.config(inbox__ambiguous_sender={"lookback_hours": 24, "sample_ids": 2,
                                                         "require_live_pane": True})
        self.fleet.panes = [{"pane_id": "wR:p4", "agent_status": "idle"}]
        self.fleet.message("codex poolrad", "read", NOW - 48 * HOUR, pane_id="wR:p4")
        self.fleet.message("dead lane", "read", NOW - 48 * HOUR, pane_id="w9:p9")
        ids = [self.fleet.message("x", "read", NOW - HOUR, sender="codex poolrad") for _ in range(3)]
        self.fleet.message("x", "read", NOW - HOUR, sender="dead lane")
        self.fleet.message("x", "read", NOW - HOUR, sender="legacy-name")
        self.fleet.message("x", "read", NOW - HOUR, sender="wR:p4")
        found = self.fleet.evaluate(cfg)
        self.assertEqual([("inbox.ambiguous_sender", "codex poolrad")], self.keys(found))
        self.assertEqual(ids[:2], found[0]["evidence"]["sample_ids"])

    def test_lane_rules(self):
        cfg = self.fleet.config(lane__working_without_task={"quiet_minutes": 60},
                                lane__idle_holding_task={"quiet_minutes": 60, "idle_statuses": ["idle"],
                                                         "holding_statuses": ["in_progress"]})
        self.fleet.panes = [{"pane_id": "w1:p1", "agent_status": "working"},
                            {"pane_id": "w1:p2", "agent_status": "working"},
                            {"pane_id": "w1:p3", "agent_status": "idle"},
                            {"pane_id": "w1:p4", "agent_status": "idle"}]
        self.fleet.task_event("tx", "status", NOW - 59 * MIN, actor="w1:p2")
        self.fleet.task("t3", "in_progress", NOW - 99 * MIN, assignees=["w1:p3"])
        self.fleet.task("t4", "in_progress", NOW - 99 * MIN, assignees=["w1:p4"])
        self.fleet.message("x", "read", NOW - 59 * MIN, sender="w1:p4")
        self.assertEqual([("lane.idle_holding_task", "t3@w1:p3"), ("lane.working_without_task", "w1:p1")],
                         sorted(self.keys(self.fleet.evaluate(cfg))))

    def test_host_load_and_tmp_bands(self):
        cfg = self.fleet.config(host__load={"load1_per_cpu": 2.0, "critical_load1_per_cpu": 2.6, "interim": True, "critical_hold_runs": 3, "must_hold_runs": 1},
                                host__tmp={"max_used_fraction": 0.7, "critical_used_fraction": 0.9,
                                           "min_free_bytes": 100, "max_inode_fraction": 0.9, "path": "/tmp"})
        # INTERIM fixed: warning 2.0/cpu, critical 2.6/cpu. 16.0/8=2.0 -> warning.
        self.fleet.panes = [{"pane_id": f"w1:p{i}", "agent_status": "working"}
                            for i in range(10)]
        self.fleet.hosts = {"/": {"load1": 16.0, "load5": 16.0, "cpus": 8}, "/tmp": {"used_fraction": 0.7}}
        found = self.fleet.evaluate(cfg)
        self.assertEqual({("host.load", "load5:warning"): "warning", ("host.tmp", "/tmp:used_fraction"): "warning"},
                         {(f["rule"], f["key"]): f["severity"] for f in found})
        self.fleet.hosts["/tmp"] = {"used_fraction": 0.95}
        self.assertEqual("critical", self.keys and [f for f in self.fleet.evaluate(cfg)
                                                    if f["rule"] == "host.tmp"][0]["severity"])
        self.fleet.hosts = {"/": {"load1": 15.99, "load5": 15.99, "cpus": 8}, "/tmp": {"used_fraction": 0.69}}
        self.assertEqual([], self.fleet.evaluate(cfg))

    def test_host_disk_full(self):
        cfg = self.fleet.config(host__disk_full={"max_used_fraction": 0.98, "paths": ["/tmp", "/"]})
        self.fleet.hosts = {"/": {"used_fraction": 0.98}, "/tmp": {"used_fraction": 0.979}}
        found = self.fleet.evaluate(cfg)
        self.assertEqual([("host.disk_full", "/:0.98")], self.keys(found))
        self.assertEqual("outage", found[0]["class"])

    def test_heavy_slot_stale_uses_holder_liveness_not_age(self):
        slots = os.path.join(self.tmp, "slots")
        os.makedirs(os.path.join(slots, "poolrad-heavy.1"))
        os.makedirs(os.path.join(slots, "poolrad-heavy.2"))
        os.makedirs(os.path.join(slots, "poolrad-heavy.3"))
        os.makedirs(os.path.join(slots, "other"))
        # 1: old but the holder is alive -> a legitimate long job, no finding.
        with open(os.path.join(slots, "poolrad-heavy.1", "pid"), "w") as h:
            h.write(str(os.getpid()))
        os.utime(os.path.join(slots, "poolrad-heavy.1"), (NOW - 60 * MIN, NOW - 60 * MIN))
        # 2: old and the holder is gone -> stale.
        with open(os.path.join(slots, "poolrad-heavy.2", "pid"), "w") as h:
            h.write("99999999")
        os.utime(os.path.join(slots, "poolrad-heavy.2"), (NOW - 60 * MIN, NOW - 60 * MIN))
        # 3: legacy slot with no pid, old -> stale.
        os.utime(os.path.join(slots, "poolrad-heavy.3"), (NOW - 60 * MIN, NOW - 60 * MIN))
        os.utime(os.path.join(slots, "other"), (NOW - 99 * MIN, NOW - 99 * MIN))
        cfg = self.fleet.config(host__heavy_slot_stale={"directory": slots, "prefix": "poolrad-heavy.",
                                                        "max_age_minutes": 30})
        self.assertEqual([("host.heavy_slot_stale", "poolrad-heavy.2"),
                          ("host.heavy_slot_stale", "poolrad-heavy.3")],
                         self.keys(self.fleet.evaluate(cfg)))

    def test_health_failed(self):
        cfg = self.fleet.config(health__failed={"stale_minutes": 30})
        self.fleet.health = {"checked_at": NOW - 30 * MIN + 1, "ok": True, "problems": []}
        self.fleet.write_json()
        self.assertEqual([], self.fleet.evaluate(cfg))
        self.fleet.health = {"checked_at": NOW - 30 * MIN, "ok": False, "problems": ["wal"]}
        self.fleet.write_json()
        self.assertEqual([("health.failed", "problems"), ("health.failed", "stale")],
                         self.keys(self.fleet.evaluate(cfg)))

    def test_secret_scan_reports_hashes_only_and_honours_the_allowlist(self):
        cfg = self.fleet.config(security__secret_text={"lookback_hours": 24})
        self.fleet.message("x", "read", NOW - HOUR, body=f"here it is\n{FAKE_TOKEN}\nthanks")
        self.fleet.message("x", "read", NOW - 25 * HOUR, body=FAKE_TOKEN)  # outside lookback
        self.fleet.task("t1", "open", NOW, body=f"token: {FAKE_TOKEN}")
        found = self.fleet.evaluate(cfg)
        self.assertEqual(["inbox", "task"], [f["evidence"]["kind"] for f in found])
        self.assertNotIn(FAKE_TOKEN, json.dumps(found), "evidence is (kind, id, sha256), never the match")
        self.assertEqual("security", found[0]["class"])
        allow = sorted({f["evidence"]["sha256"] for f in found})
        cfg["rules"]["security.secret_text"]["allowlist_sha256"] = allow
        self.assertEqual([], self.fleet.evaluate(cfg))

    def test_money_is_skipped_without_a_metering_source(self):
        cfg = self.fleet.config()
        result = wd.evaluate(cfg, NOW)
        self.assertIn({"rule": "money.spend_projection", "error": "no metering source"}, result["skipped"])

    def test_pane_rules_skip_visibly_without_a_pane_source(self):
        cfg = self.fleet.config(lane__working_without_task={"quiet_minutes": 60})
        cfg["panes"]["source"] = "none"
        self.assertIn("lane.working_without_task", [s["rule"] for s in wd.evaluate(cfg, NOW)["skipped"]])

    def test_output_is_deterministic(self):
        cfg = self.fleet.config(inbox__undeliverable={"min_messages": 1, "min_age_hours": 0.01}, task__blocked_stale={"threshold_minutes": 1})
        for mailbox in ("b", "a", "c"):
            self.fleet.message(mailbox, "undeliverable", NOW - 30 * HOUR)
        self.fleet.task("t9", "blocked", NOW - HOUR)
        self.assertEqual(self.fleet.evaluate(cfg), self.fleet.evaluate(cfg))
        self.assertEqual(["a", "b", "c", "t9"], [f["key"] for f in self.fleet.evaluate(cfg)])


class ConfigTests(WatchdogCase):
    def assertNamed(self, mutate, key):
        config = copy.deepcopy(EXAMPLE)
        mutate(config)
        with self.assertRaises(wd.ConfigError) as caught:
            wd.validate_config(config)
        self.assertEqual(key, caught.exception.key)

    def test_example_config_is_valid_and_quiet(self):
        config = wd.validate_config(copy.deepcopy(EXAMPLE))
        self.assertFalse(config["enabled"])
        self.assertTrue(config["dry_run"])
        self.assertFalse(config["escalation"]["owner"]["enabled"])

    def test_missing_enabled_or_dry_run_is_an_error_not_a_default(self):
        self.assertNamed(lambda c: c.pop("enabled"), "enabled")
        self.assertNamed(lambda c: c.pop("dry_run"), "dry_run")

    def test_zero_and_negative_values_are_refused(self):
        self.assertNamed(lambda c: c["rules"]["host.load"].update(load1_per_cpu=0), "rules.host.load.load1_per_cpu")
        self.assertNamed(lambda c: c["rules"]["dunk.overdue"].update(cooldown_minutes=0),
                         "rules.dunk.overdue.cooldown_minutes")
        self.assertNamed(lambda c: c["escalation"]["owner"].update(max_pages_per_hour=0),
                         "escalation.owner.max_pages_per_hour")
        self.assertNamed(lambda c: c["rules"]["task.blocked_stale"].update(threshold_minutes=-5),
                         "rules.task.blocked_stale.threshold_minutes")
        self.assertNamed(lambda c: c["rules"]["host.disk_full"].update(must_hold_runs=0),
                         "rules.host.disk_full.must_hold_runs")

    def test_named_errors(self):
        self.assertNamed(lambda c: c.update(pagethreshold=1), "pagethreshold")
        self.assertNamed(lambda c: c["rules"]["host.tmp"].update(max_used_fraction="high"),
                         "rules.host.tmp.max_used_fraction")
        self.assertNamed(lambda c: c["rules"].pop("host.load"), "rules.host.load")
        self.assertNamed(lambda c: c["panes"].update(pane_id_pattern="("), "panes.pane_id_pattern")
        self.assertNamed(lambda c: c["rules"]["security.secret_text"].update(allowlist_sha256=["xyz"]),
                         "rules.security.secret_text.allowlist_sha256")

    def test_ops_rules_cannot_be_given_an_owner_class(self):
        self.assertNamed(lambda c: c["rules"]["host.tmp"].update({"class": "outage"}), "rules.host.tmp.class")
        self.assertNamed(lambda c: c["rules"]["host.load"].update({"class": "money"}), "rules.host.load.class")

    def test_money_cannot_be_enabled_without_source_and_cap(self):
        self.assertNamed(lambda c: c["rules"]["money.spend_projection"].update(enabled=True),
                         "rules.money.spend_projection.enabled")

    def test_owner_paging_requires_an_approver(self):
        self.assertNamed(lambda c: c["escalation"]["owner"].update(enabled=True), "escalation.owner.approved_by")

    def test_cli_refuses_bad_config_names_it_and_leaves_the_error_for_the_deadman(self):
        cases = {"truncated.json": '{"schema": "dunkingsheep-watchdog/2", "enabled": tr',
                 "unknown.json": json.dumps(dict(copy.deepcopy(EXAMPLE), pagethreshold=1)),
                 "zero.json": json.dumps(EXAMPLE).replace('"load1_per_cpu": 2.0', '"load1_per_cpu": 0')}
        for name, text in cases.items():
            path, error_file = os.path.join(self.tmp, name), os.path.join(self.tmp, name + ".err")
            with open(path, "w") as handle:
                handle.write(text)
            proc = subprocess.run([sys.executable, os.path.join(ROOT, "dunkingsheep"), "watchdog", "--config", path,
                                   "--error-file", error_file], capture_output=True, text=True, timeout=60)
            self.assertEqual(2, proc.returncode, proc.stderr)
            self.assertIn("refusing to start: CONFIG:", proc.stderr)
            self.assertEqual("", proc.stdout, "nothing evaluated, nothing sent")
            self.assertIn("CONFIG:", read(error_file))


class LiveNowRefusalTests(WatchdogCase):
    def test_now_is_refused_on_a_live_run_and_allowed_on_a_dry_run(self):
        cfg = self.fleet.config()
        cfg.update(enabled=True, dry_run=False)
        path = os.path.join(self.tmp, "live.json")
        with open(path, "w") as handle:
            json.dump(cfg, handle)
        base = [sys.executable, os.path.join(ROOT, "dunkingsheep"), "watchdog", "--config", path,
                "--error-file", os.path.join(self.tmp, "err"), "--now", str(NOW)]
        proc = subprocess.run(base, capture_output=True, text=True, timeout=60)
        self.assertEqual(2, proc.returncode)
        self.assertIn("refusing --now on a live run", proc.stderr)
        self.assertFalse(os.path.exists(cfg["heartbeat_path"]))
        proc = subprocess.run(base + ["--dry-run"], capture_output=True, text=True, timeout=60)
        self.assertEqual(0, proc.returncode, proc.stderr)


class FailTowardNotPagingTests(WatchdogCase):
    """Spec section 3: none of these may touch the OWNER queue."""

    def disk_config(self, **owner):
        cfg = self.fleet.config(host__disk_full={"max_used_fraction": 0.98, "paths": ["/"], "must_hold_runs": 1})
        self.fleet.hosts = {"/": {"used_fraction": 0.99}}
        return self.live(cfg, **owner) if owner is not None else cfg

    def test_missing_state_suppresses_owner_pages_and_tells_vp_naming_the_state_path(self):
        cfg = self.disk_config(classes=["outage"])
        owner = FakeOwner()
        report, sent = self.fleet.run(cfg, owner=owner)
        self.assertEqual([], owner.posts)
        self.assertEqual("state-loss", report["decisions"][0]["owner"])
        self.assertIn(cfg["state_path"], sent[0][2])
        self.assertIn("missing", sent[0][2])

    def test_corrupt_state_suppresses_owner_pages(self):
        cfg = self.disk_config(classes=["outage"])
        with open(cfg["state_path"], "w") as handle:
            handle.write("{ truncated")
        owner = FakeOwner()
        report, sent = self.fleet.run(cfg, owner=owner)
        self.assertEqual([], owner.posts)
        self.assertIn("corrupt", sent[0][2])
        self.assertNotIn("config", sent[0][2].split("corrupt")[0].lower().split("state")[0])

    def test_a_rule_error_suppresses_owner_pages_for_the_run(self):
        cfg = self.disk_config(classes=["outage"])
        cfg["rules"]["host.load"]["enabled"] = True
        self.seed_state(cfg)
        original = dict(wd.RULES)
        wd.RULES["host.load"] = lambda inputs, rule: 1 / 0
        try:
            owner = FakeOwner()
            report, sent = self.fleet.run(cfg, owner=owner)
        finally:
            wd.RULES.clear()
            wd.RULES.update(original)
        self.assertEqual([], owner.posts)
        self.assertEqual(1, sent[0][2].count("rule=" + wd.SELF_ERROR_RULE), "exactly one self error")
        self.assertIn("host.load SKIPPED: ZeroDivisionError", sent[0][2])

    def test_dry_run_never_posts_writes_state_or_beats(self):
        cfg = self.disk_config(classes=["outage"])
        self.seed_state(cfg)
        before = read(cfg["state_path"])
        owner = FakeOwner()
        report, sent = self.fleet.run(cfg, owner=owner, dry=True)
        self.assertEqual(([], []), (owner.posts, sent))
        self.assertEqual(before, read(cfg["state_path"]))
        self.assertFalse(os.path.exists(cfg["heartbeat_path"]))
        self.assertTrue(any(line.startswith("WOULD-PAGE OWNER rule=host.disk_full") for line in report["lines"]))


class OwnerPagingTests(WatchdogCase):
    def disk_config(self, hold=2, paths=("/",)):
        cfg = self.fleet.config(host__disk_full={"max_used_fraction": 0.98, "paths": list(paths),
                                                 "must_hold_runs": hold})
        self.fleet.hosts = {path: {"used_fraction": 0.99} for path in paths}
        cfg = self.live(cfg, classes=["outage"])
        self.seed_state(cfg)
        return cfg

    def test_hold_needs_consecutive_spaced_runs(self):
        cfg = self.disk_config(hold=2)
        owner = FakeOwner()
        report, _ = self.fleet.run(cfg, owner=owner)
        self.assertEqual("hold", report["decisions"][0]["owner"])
        report, _ = self.fleet.run(cfg, now=NOW + 59, owner=owner)  # too soon: does not count
        self.assertEqual("hold", report["decisions"][0]["owner"])
        report, _ = self.fleet.run(cfg, now=NOW + 60, owner=owner)
        self.assertEqual("no", report["decisions"][0]["owner"])
        self.assertEqual(1, len(owner.posts))

    def test_streak_resets_when_the_condition_clears(self):
        cfg = self.disk_config(hold=2)
        owner = FakeOwner()
        self.fleet.run(cfg, owner=owner)
        self.fleet.hosts = {"/": {"used_fraction": 0.5}}
        self.fleet.run(cfg, now=NOW + 60, owner=owner)
        self.fleet.hosts = {"/": {"used_fraction": 0.99}}
        report, _ = self.fleet.run(cfg, now=NOW + 120, owner=owner)
        self.assertEqual("hold", report["decisions"][0]["owner"])

    def test_owner_cap_is_three_per_rolling_hour(self):
        cfg = self.disk_config(hold=1, paths=("/a", "/b", "/c", "/d", "/e"))
        owner = FakeOwner()
        report, sent = self.fleet.run(cfg, owner=owner)
        self.assertEqual(3, len(owner.posts))
        self.assertEqual(2, sum(d["owner"] == "global-cap" for d in report["decisions"]))
        report, _ = self.fleet.run(cfg, now=NOW + 30 * MIN, owner=owner)
        self.assertEqual(3, len(owner.posts), "still inside the rolling hour")

    def test_delivered_only_when_confirmed(self):
        cfg = self.disk_config(hold=1)
        owner = FakeOwner(through=None)
        self.fleet.run(cfg, owner=owner)
        state, _ = wd.load_state(cfg["state_path"])
        page = state["owner_pages"]["host.disk_full|/:0.98"]
        self.assertIsNone(page["confirmed_at"], "posted is not delivered")
        self.fleet.run(cfg, now=NOW + 60, owner=owner)
        self.assertEqual(1, len(owner.posts), "a pending page is never re-posted")
        owner.through = page["brief_id"]
        self.fleet.run(cfg, now=NOW + 120, owner=owner)
        state, _ = wd.load_state(cfg["state_path"])
        self.assertEqual(NOW + 120, state["owner_pages"]["host.disk_full|/:0.98"]["confirmed_at"])

    def test_unconfirmed_page_becomes_a_vp_self_error_and_stays_undelivered(self):
        cfg = self.disk_config(hold=1)
        owner = FakeOwner(through=None)
        self.fleet.run(cfg, owner=owner)
        limit = cfg["escalation"]["owner"]["confirm_within_minutes"] * MIN
        _, sent = self.fleet.run(cfg, now=NOW + limit + 1, owner=owner)
        self.assertIn("UNDELIVERED", sent[0][2])
        state, _ = wd.load_state(cfg["state_path"])
        self.assertIsNone(state["owner_pages"]["host.disk_full|/:0.98"]["confirmed_at"])

    def test_failed_post_is_one_vp_self_error_and_nothing_is_recorded(self):
        cfg = self.disk_config(hold=1)
        owner = FakeOwner(fail=ConnectionError("desk down"))
        report, sent = self.fleet.run(cfg, owner=owner)
        self.assertEqual(1, sent[0][2].count("rule=" + wd.SELF_ERROR_RULE))
        self.assertIn("desk down", sent[0][2])
        state, _ = wd.load_state(cfg["state_path"])
        self.assertEqual({}, state["owner_pages"])
        self.assertEqual([], state["owner_page_times"])

    def test_the_default_owner_channel_fails_toward_not_paging(self):
        cfg = self.disk_config(hold=1)
        sent = []
        wd.run(cfg, NOW, send=lambda *a: sent.append(a), host_sampler=self.fleet.host,
               panes_provider=lambda: [])
        self.assertIn("desk brief post not wired", sent[0][2])

    def test_vp_only_classes_never_page_even_with_owner_enabled(self):
        cfg = self.fleet.config(host__tmp={"max_used_fraction": 0.5, "critical_used_fraction": 0.5,
                                           "min_free_bytes": 1, "max_inode_fraction": 0.9, "path": "/tmp"},
                                inbox__undeliverable={"min_messages": 1, "min_age_hours": 0.01})
        cfg = self.live(cfg, classes=["security", "outage", "money"], min_severity="info")
        self.seed_state(cfg)
        self.fleet.hosts = {"/tmp": {"used_fraction": 0.97}}
        self.fleet.message("dead", "undeliverable", NOW - 30 * HOUR)
        owner = FakeOwner()
        report, _ = self.fleet.run(cfg, owner=owner)
        self.assertEqual([], owner.posts)
        self.assertTrue(all(d["owner"] is None for d in report["decisions"]))

    def test_resolved_owner_line_only_for_a_confirmed_page(self):
        cfg = self.disk_config(hold=1)
        owner = FakeOwner(through=None)
        self.fleet.run(cfg, owner=owner)
        owner.through = 999
        self.fleet.run(cfg, now=NOW + 60, owner=owner)
        self.fleet.hosts = {"/": {"used_fraction": 0.5}}
        _, sent = self.fleet.run(cfg, now=NOW + 120, owner=owner)
        self.assertEqual(2, len(owner.posts))
        self.assertIn("RESOLVED host.disk_full|/:0.98", owner.posts[1])
        self.assertIn("RESOLVED VP dedupe=host.disk_full|/:0.98", sent[-1][2])


class DeskBriefChannelTests(WatchdogCase):
    """The agreed bridge status contract: {"last_brief_id": int, "updated_at": float} (VP i19825)."""

    def channel(self, text):
        path = os.path.join(self.tmp, "telegram-status.json")
        if text is not None:
            with open(path, "w") as handle:
                handle.write(text)
        return wd.DeskBriefChannel(path)

    def test_reads_last_brief_id_and_treats_null_as_unconfirmed(self):
        self.assertEqual(1065, self.channel('{"last_brief_id": 1065, "updated_at": 1.5}').confirmed_through())
        self.assertIsNone(self.channel('{"last_brief_id": null, "updated_at": 1.5}').confirmed_through())

    def test_unparseable_or_missing_status_is_an_error_never_delivered(self):
        for text in ("{ trunc", '{"last_brief_id": "1065"}', '{"last_brief_id": true}', None):
            with self.assertRaises((ValueError, OSError)):
                self.channel(text).confirmed_through()

    def test_post_fails_toward_not_paging(self):
        with self.assertRaises(wd.OwnerChannelUnavailable):
            self.channel(None).post("page")


class VpDeliveryTests(WatchdogCase):
    def vp_config(self, **extra):
        cfg = self.fleet.config(inbox__undeliverable={"min_messages": 1, "cooldown_minutes": 60})
        cfg.update(enabled=True, dry_run=False, **extra)
        self.seed_state(cfg)
        return cfg

    def test_enabled_false_or_dry_run_true_sends_nothing(self):
        self.fleet.message("dead", "undeliverable", NOW - 30 * HOUR)
        for overrides in ({"enabled": False}, {"dry_run": True}):
            cfg = self.vp_config()
            cfg.update(overrides)
            report, sent = self.fleet.run(cfg)
            self.assertEqual("dry-run", report["mode"])
            self.assertEqual([], sent)

    def test_dry_run_lines_carry_threshold_measured_and_window(self):
        self.fleet.message("dead", "undeliverable", NOW - 30 * HOUR)
        report, _ = self.fleet.run(self.vp_config(), dry=True)
        vp = [line for line in report["lines"] if line.startswith("WOULD-NOTIFY VP")]
        self.assertEqual(1, len(vp))
        for part in ("rule=inbox.undeliverable", "class=ops", "threshold=1 msgs", "measured=1 msgs",
                     "window=current backlog", "dedupe=inbox.undeliverable|dead", "suppressed=no", "evidence="):
            self.assertIn(part, vp[0])
        self.assertIn("SKIPPED rule=money.spend_projection error=no metering source", report["lines"])

    def test_live_watchdog_run_records_its_cadence_in_the_heartbeat(self):
        self.fleet.message("dead", "undeliverable", NOW - 30 * HOUR)
        cfg = self.vp_config(cadence_minutes=15)
        self.assertEqual(["vp"], self.fleet.run(cfg)[0]["sent"])
        self.assertEqual({"schema": 1, "cadence_minutes": 15, "at": NOW},
                         json.loads(read(cfg["heartbeat_path"])))
        self.assertEqual(NOW, os.stat(cfg["heartbeat_path"]).st_mtime)
        self.assertEqual((15, None), deadman.recorded_cadence(cfg["heartbeat_path"]))

    def test_live_run_sends_one_digest_and_touches_heartbeat(self):
        self.fleet.message("dead", "undeliverable", NOW - 30 * HOUR)
        cfg = self.vp_config()
        report, sent = self.fleet.run(cfg)
        self.assertEqual(["vp"], report["sent"])
        self.assertEqual(1, len(sent))
        self.assertNotIn("BODY-NEVER-REPORTED", sent[0][2])
        self.assertEqual(NOW, os.stat(cfg["heartbeat_path"]).st_mtime)

    def test_cap_sends_exactly_max_and_says_how_many_were_suppressed(self):
        for n in range(5):
            self.fleet.message(f"dead{n}", "undeliverable", NOW - 30 * HOUR)
        report, sent = self.fleet.run(self.vp_config(max_alerts_per_run=3))
        self.assertEqual(3, report["counts"]["vp"])
        self.assertIn("2 further finding(s) suppressed", sent[0][2])

    def test_cooldown_dedupes_and_persists_across_restart(self):
        self.fleet.message("dead", "undeliverable", NOW - 30 * HOUR)
        cfg = self.vp_config()
        self.assertEqual(1, len(self.fleet.run(cfg)[1]))
        self.assertEqual([], self.fleet.run(cfg, now=NOW + 59 * MIN)[1])
        self.assertEqual(1, len(self.fleet.run(cfg, now=NOW + 60 * MIN)[1]))

    def test_vp_send_failure_is_reported_never_recursed(self):
        self.fleet.message("dead", "undeliverable", NOW - 30 * HOUR)
        cfg = self.vp_config()
        calls = []

        def failing(*args):
            calls.append(args)
            raise ConnectionError("daemon down")
        report = wd.run(cfg, NOW, send=failing, owner_channel=FakeOwner(), panes_provider=lambda: [],
                        host_sampler=self.fleet.host)
        self.assertEqual(1, len(calls))
        self.assertIn("daemon down", report["delivery_error"])
        self.assertFalse(os.path.exists(cfg["heartbeat_path"]))
        self.assertIn("DELIVERY FAILED (source=watchdog-self)", wd.render_text(report))

    def test_recovery_line_goes_to_vp(self):
        self.fleet.message("dead", "undeliverable", NOW - 30 * HOUR)
        cfg = self.vp_config()
        self.fleet.run(cfg)
        self.fleet.sql("inbox.db", "UPDATE messages SET state='archived'")
        _, sent = self.fleet.run(cfg, now=NOW + 60)
        self.assertIn("RESOLVED VP dedupe=inbox.undeliverable|dead", sent[0][2])


class ReplayTests(WatchdogCase):
    def test_replay_counts_history_backed_rules_and_names_the_rest(self):
        cfg = self.fleet.config(dunk__skip_loop={"consecutive_skips": 3, "lookback_hours": 24, "cooldown_minutes": 600},
                                inbox__undeliverable={"min_messages": 1, "min_age_hours": 0.01})
        self.fleet.dunks = [{"id": "d1", "running": True}]
        self.fleet.write_json()
        for n in range(4):
            self.fleet.metric("dunk.skipped", "d1", NOW - 10 * HOUR + n * MIN)
        result = wd.replay(cfg, NOW - 12 * HOUR, NOW, 60)
        self.assertEqual({"dunk.skip_loop": {"vp": 1, "owner": 0}}, result["counts"],
                         "one incident under its cooldown, not one per step")
        self.assertIn("inbox.undeliverable", result["not_replayable"])
        self.assertFalse(os.path.exists(cfg["state_path"]))


class DeadManTests(WatchdogCase):
    def check(self, now, **extra):
        sent = []
        args = dict(heartbeat=os.path.join(self.tmp, "heartbeat"), cadence_minutes=10, multiplier=3,
                    to="vp of agentry", state_path=os.path.join(self.tmp, "deadman.json"), cooldown_minutes=60,
                    now=now, send=lambda *a: sent.append(a),
                    daemon_path=os.path.join(self.tmp, "dunkingsheep.sock"), expected_dir=self.tmp)
        args.update(extra)
        return deadman.check(**args), sent

    def beat(self, at):
        path = os.path.join(self.tmp, "heartbeat")
        with open(path, "w"):
            pass
        os.utime(path, (at, at))

    def test_fresh_heartbeat_is_silent_and_aged_one_alerts_once(self):
        self.beat(NOW - 30 * MIN)
        self.assertEqual([], self.check(NOW)[1])
        report, sent = self.check(NOW + 1)
        self.assertTrue(report["alerted"])
        self.assertIn(deadman.RULE_ID, sent[0][2])
        self.assertEqual([], self.check(NOW + 2)[1], "cooldown: one alert, not one per check")

    def test_it_carries_the_watchdogs_raw_config_error(self):
        error_file = os.path.join(self.tmp, "watchdog.config-error")
        with open(error_file, "w") as handle:
            handle.write('CONFIG: pagethreshold: unknown key\n')
        _, sent = self.check(NOW, error_file=error_file)
        self.assertIn("pagethreshold: unknown key", sent[0][2])

    # The six fallback cases (w8:p8 i19841, VP i19837): socket first; a direct
    # inbox row ONLY when the daemon is provably absent; never around a slow or
    # live-but-failing daemon; a status file when nothing could be delivered.

    def raising(self, error):
        def send(*args):
            raise error
        return send

    def inbox_rows(self, path):
        if not os.path.exists(path):
            return []
        conn = sqlite3.connect(path)
        try:
            return conn.execute("SELECT id, mailbox, sender, body FROM messages").fetchall()
        finally:
            conn.close()

    def test_1_daemon_down_writes_one_verified_direct_row(self):
        inbox = os.path.join(self.tmp, "inbox.db")
        for error in (ConnectionRefusedError("refused"), FileNotFoundError("no socket")):
            report, _ = self.check(NOW, send=self.raising(error), inbox_path=inbox,
                                   state_path=os.path.join(self.tmp, f"s-{type(error).__name__}.json"))
            self.assertTrue(report["alerted"])
            self.assertEqual("direct", report["delivered_via"])
        rows = self.inbox_rows(inbox)
        self.assertEqual(2, len(rows), "one row per independent state (two separate dead-men here)")
        self.assertEqual({("vp of agentry", "watchdog")}, {(r[1], r[2]) for r in rows})
        self.assertIn(deadman.RULE_ID, rows[0][3])

    def test_2_daemon_down_and_store_unwritable_records_status_and_no_row(self):
        blocker = os.path.join(self.tmp, "not-a-dir")
        with open(blocker, "w") as handle:
            handle.write("x")
        status = os.path.join(self.tmp, "deadman-status.json")
        report, _ = self.check(NOW, send=self.raising(ConnectionRefusedError("refused")),
                               inbox_path=os.path.join(blocker, "inbox.db"), status_path=status)
        self.assertFalse(report["alerted"])
        recorded = json.loads(read(status))
        self.assertEqual("direct", recorded["attempted"])
        self.assertIn("direct write failed", recorded["error"])
        self.assertFalse(os.path.exists(os.path.join(self.tmp, "deadman.json")), "no cooldown started")

    def test_3_daemon_up_but_failing_never_writes_directly(self):
        inbox, status = os.path.join(self.tmp, "inbox.db"), os.path.join(self.tmp, "deadman-status.json")
        report, _ = self.check(NOW, send=self.raising(DunkError("unknown mailbox")), inbox_path=inbox,
                               status_path=status)
        self.assertFalse(report["alerted"])
        self.assertEqual([], self.inbox_rows(inbox))
        self.assertEqual("daemon", json.loads(read(status))["attempted"])

    def test_4_slow_daemon_never_writes_directly_and_retries_next_run(self):
        inbox, status = os.path.join(self.tmp, "inbox.db"), os.path.join(self.tmp, "deadman-status.json")
        for error in (TimeoutError("timed out"), deadman_unavailable()):
            report, _ = self.check(NOW, send=self.raising(error), inbox_path=inbox, status_path=status)
            self.assertFalse(report["alerted"])
        self.assertEqual([], self.inbox_rows(inbox))
        self.assertFalse(os.path.exists(os.path.join(self.tmp, "deadman.json")), "no cooldown: next run retries")
        report, sent = self.check(NOW + 60, inbox_path=inbox)
        self.assertEqual(("daemon", 1), (report["delivered_via"], len(sent)))

    def test_5_retry_after_a_failed_direct_write_makes_exactly_one_row(self):
        blocker = os.path.join(self.tmp, "late-dir")
        with open(blocker, "w") as handle:
            handle.write("x")
        inbox = os.path.join(blocker, "inbox.db")
        down = self.raising(ConnectionRefusedError("refused"))
        self.assertFalse(self.check(NOW, send=down, inbox_path=inbox)[0]["alerted"])
        os.remove(blocker)  # the store becomes writable
        self.assertTrue(self.check(NOW + 60, send=down, inbox_path=inbox)[0]["alerted"])
        state = json.loads(read(os.path.join(self.tmp, "deadman.json")))
        self.assertEqual(NOW + 60, state["last_alerted"], "cooldown starts only after a confirmed row")
        self.check(NOW + 60 + 3601, send=down, inbox_path=inbox)  # past cooldown, same incident
        self.assertEqual(1, len(self.inbox_rows(inbox)), "never a second row for the same incident")

    # FileNotFoundError is ambiguous (VP i19851, w8:p8 i19854): it is daemon_down
    # only when the daemon path is configured and sits in the expected, existing
    # directory. Otherwise config_bad: no send, no direct row, no cooldown.

    def assert_refused(self, cause, **extra):
        inbox, status = os.path.join(self.tmp, "inbox.db"), os.path.join(self.tmp, "deadman-status.json")
        report, _ = self.check(NOW, inbox_path=inbox, status_path=status, **extra)
        self.assertFalse(report["alerted"])
        self.assertEqual(cause, report["cause"])
        self.assertEqual(cause, json.loads(read(status))["cause"])
        self.assertEqual([], self.inbox_rows(inbox), "never a direct row")
        self.assertFalse(os.path.exists(os.path.join(self.tmp, "deadman.json")), "no cooldown: next run retries")
        return report

    def test_missing_socket_in_the_expected_directory_is_daemon_down(self):
        inbox, status = os.path.join(self.tmp, "inbox.db"), os.path.join(self.tmp, "deadman-status.json")
        sock = os.path.join(self.tmp, "dunkingsheep.sock")
        error = FileNotFoundError(2, "No such file or directory", sock)
        report, _ = self.check(NOW, send=self.raising(error), inbox_path=inbox, status_path=status)
        self.assertEqual(("direct", 1), (report["delivered_via"], len(self.inbox_rows(inbox))))

    def test_empty_daemon_path_is_config_bad_and_sends_nothing(self):
        sent = []
        for empty in ("", "  "):
            self.assert_refused("config_bad", daemon_path=empty, send=lambda *a: sent.append(a))
        self.assertEqual([], sent, "a CONFIG refusal is decided before any send")

    def test_missing_parent_is_config_bad(self):
        report = self.assert_refused("config_bad", daemon_path=os.path.join(self.tmp, "gone", "x.sock"),
                                     send=self.raising(FileNotFoundError("no socket")))
        self.assertIn("does not exist", report["delivery_error"])

    def test_unexpected_parent_is_config_bad(self):
        other = os.path.join(self.tmp, "elsewhere")
        os.mkdir(other)
        report = self.assert_refused("config_bad", daemon_path=os.path.join(other, "dunkingsheep.sock"),
                                     send=self.raising(FileNotFoundError("no socket")))
        self.assertIn("not the expected directory", report["delivery_error"])

    def test_file_not_found_for_a_different_file_is_config_bad(self):
        error = FileNotFoundError(2, "No such file or directory", os.path.join(self.tmp, "other.sock"))
        self.assert_refused("config_bad", send=self.raising(error))

    def test_not_a_directory_is_config_bad(self):
        self.assert_refused("config_bad", send=self.raising(NotADirectoryError("a component is a file")))

    def test_timeout_and_reply_less_close_are_other(self):
        for error in (TimeoutError("timed out"), deadman_unavailable(), IsADirectoryError("dir")):
            self.assert_refused("other", send=self.raising(error))

    def test_cli_exit_codes_by_cause(self):
        import argparse
        base = dict(heartbeat=os.path.join(self.tmp, "heartbeat"), cadence_minutes=10, multiplier=3,
                    to="vp of agentry", state=os.path.join(self.tmp, "deadman.json"), cooldown_minutes=60,
                    now=NOW, dry_run=False, error_file=os.path.join(self.tmp, "err"),
                    status=os.path.join(self.tmp, "st.json"), inbox=os.path.join(self.tmp, "inbox.db"),
                    expected_dir=self.tmp, json=True)
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(2, deadman.main(argparse.Namespace(daemon_path="", **base)))
        self.assertEqual("config_bad", json.loads(read(base["status"]))["cause"])

    # The heartbeat records the watchdog's cadence (VP i19870, i19872, i19874):
    # limit = multiplier x max(unit, recorded), recorded clamped to 6 x unit.

    def beat_with(self, at, body):
        path = os.path.join(self.tmp, "heartbeat")
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(body)
        os.utime(path, (at, at))

    def quiet_check(self, now, **extra):
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            report, sent = self.check(now, **extra)
        return report, sent, err.getvalue()

    def test_fresh_legacy_empty_heartbeat_uses_the_unit_value_and_stays_silent(self):
        self.beat_with(NOW - 29 * MIN, "")
        report, sent, err = self.quiet_check(NOW)
        self.assertEqual((False, [], "", []), (report["stale"], sent, err, report["notes"]))
        self.assertEqual((10, None, 30.0), (report["effective_cadence_minutes"], report["recorded_cadence_minutes"],
                                            report["limit_minutes"]))

    def test_legacy_empty_heartbeat_alerts_with_no_problem_line(self):
        self.beat_with(NOW - 31 * MIN, "")
        report, sent, _ = self.quiet_check(NOW)
        self.assertTrue(report["alerted"])
        self.assertNotIn("heartbeat " + os.path.join(self.tmp, "heartbeat") + ":", sent[0][2])

    def test_a_longer_recorded_cadence_widens_the_limit(self):
        self.beat_with(NOW - 31 * MIN, json.dumps({"schema": 1, "cadence_minutes": 15, "at": NOW - 31 * MIN}))
        report, sent, _ = self.quiet_check(NOW)
        self.assertEqual((False, [], 45.0), (report["stale"], sent, report["limit_minutes"]),
                         "a 15-min watchdog against a 10-min unit is not dead at 31 min")
        self.beat_with(NOW - 46 * MIN, json.dumps({"cadence_minutes": 15}))
        self.assertTrue(self.quiet_check(NOW)[0]["alerted"])

    def test_a_shorter_recorded_cadence_never_narrows_the_limit(self):
        self.beat_with(NOW - 29 * MIN, json.dumps({"cadence_minutes": 5}))
        report, sent, _ = self.quiet_check(NOW)
        self.assertEqual((False, [], 30.0), (report["stale"], sent, report["limit_minutes"]))

    def test_an_unparseable_heartbeat_uses_the_unit_value_and_names_the_problem(self):
        for index, body in enumerate(("garbage", "[1]", '{"cadence_minutes": 0}', '{"cadence_minutes": true}',
                                      '{"cadence_minutes": "15"}')):
            self.beat_with(NOW - 31 * MIN, body)
            report, sent, _ = self.quiet_check(NOW, state_path=os.path.join(self.tmp, f"s-{index}.json"))
            self.assertEqual(30.0, report["limit_minutes"], body)
            self.assertTrue(report["alerted"], body)
            self.assertEqual(1, sum(line.startswith("heartbeat ") for line in sent[0][2].splitlines()), body)
        self.beat_with(NOW - 5 * MIN, "garbage")
        report, sent, err = self.quiet_check(NOW)
        self.assertEqual(([], 1), (sent, err.count("not JSON")), "fresh: journal only, never the inbox")

    def test_a_huge_recorded_cadence_is_clamped_to_six_times_the_unit(self):
        huge = json.dumps({"cadence_minutes": 1440})
        self.beat_with(NOW - 100 * MIN, huge)
        report, sent, err = self.quiet_check(NOW)
        self.assertEqual((False, [], True, 1440, 60, 180.0),
                         (report["stale"], sent, report["clamped"], report["recorded_cadence_minutes"],
                          report["effective_cadence_minutes"], report["limit_minutes"]))
        self.assertIn("clamped to 60 min", err, "fresh and clamped: a journal line, no inbox message")
        self.beat_with(NOW - 181 * MIN, huge)
        report, sent, _ = self.quiet_check(NOW)
        self.assertTrue(report["alerted"], "never more than 6 x late")
        self.assertIn("clamped to 60 min", sent[0][2])

    def test_cli_fresh_and_clamped_exits_zero_with_json_fields(self):
        import argparse
        self.beat_with(NOW - 100 * MIN, json.dumps({"cadence_minutes": 1440}))
        args = argparse.Namespace(
            heartbeat=os.path.join(self.tmp, "heartbeat"), cadence_minutes=10, multiplier=3, to="vp of agentry",
            state=os.path.join(self.tmp, "deadman.json"), cooldown_minutes=60, now=NOW, dry_run=False,
            error_file=os.path.join(self.tmp, "err"), status=os.path.join(self.tmp, "st.json"),
            inbox=os.path.join(self.tmp, "inbox.db"), daemon_path=os.path.join(self.tmp, "dunkingsheep.sock"),
            expected_dir=self.tmp, json=True)
        out = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(0, deadman.main(args))
        report = json.loads(out.getvalue())
        self.assertEqual((True, 1440, False), (report["clamped"], report["recorded_cadence_minutes"],
                                               report["alerted"]))
        self.assertFalse(os.path.exists(args.inbox))

    def test_the_deadman_has_exactly_one_recipient(self):
        """'Cannot page the owner at any severity' holds because every path,
        daemon or direct, delivers only to --to."""
        self.beat_with(NOW - 300 * MIN, json.dumps({"cadence_minutes": 1440}))
        _, sent, _ = self.quiet_check(NOW, to="vp of agentry")
        self.assertEqual(["vp of agentry"], [target for target, _, _ in sent])
        inbox = os.path.join(self.tmp, "inbox.db")
        self.quiet_check(NOW, to="vp of agentry", send=self.raising(ConnectionRefusedError("refused")),
                         inbox_path=inbox, state_path=os.path.join(self.tmp, "s2.json"))
        self.assertEqual({"vp of agentry"}, {row[1] for row in self.inbox_rows(inbox)})
        params = list(inspect.signature(deadman.check).parameters)
        self.assertEqual(["to"], [name for name in params if name in ("to", "owner", "cc", "recipients", "page")])

    def test_watchdog_dry_run_never_touches_the_heartbeat(self):
        self.beat(NOW - HOUR)
        cfg = self.fleet.config()
        cfg["heartbeat_path"] = os.path.join(self.tmp, "heartbeat")
        self.fleet.run(cfg, dry=True)
        self.assertEqual(NOW - HOUR, os.stat(cfg["heartbeat_path"]).st_mtime)
        self.assertTrue(self.check(NOW)[0]["alerted"], "a dry run is not coverage")

    def test_deadman_does_not_import_the_watchdog(self):
        tree = ast.parse(read(os.path.join(ROOT, "dunk_watchdog_deadman.py")))
        imported = {alias.name for node in ast.walk(tree) if isinstance(node, (ast.Import, ast.ImportFrom))
                    for alias in node.names} | {node.module for node in ast.walk(tree)
                                                if isinstance(node, ast.ImportFrom)}
        self.assertNotIn("dunk_watchdog", imported)


def deadman_unavailable():
    from dunk_client import DaemonUnavailable
    return DaemonUnavailable("daemon closed the connection without replying")


class SystemSenderTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.inbox = Inbox(path=os.path.join(self.tmp.name, "inbox.db"),
                           blob_dir=os.path.join(self.tmp.name, "blobs"))
        self.flock = Flock(herdr=FakeHerdr(), persist=False, restore=False, inbox=self.inbox)

    def tearDown(self):
        self.flock.close()
        self.inbox.close()
        self.tmp.cleanup()

    def test_watchdog_sender_only_from_a_paneless_caller(self):
        sent = self.flock.inbox_send("codex", "alert", notify=False, system_sender="watchdog")
        conn = sqlite3.connect(os.path.join(self.tmp.name, "inbox.db"))
        try:
            sender = conn.execute("SELECT sender FROM messages WHERE id=?", (sent["message_id"],)).fetchone()[0]
        finally:
            conn.close()
        self.assertEqual("watchdog", sender)
        with self.assertRaises(DunkError):
            self.flock.inbox_send("codex", "spoof", notify=False, self_pane_id="w1:p2", system_sender="watchdog")
        with self.assertRaises(DunkError):
            self.flock.inbox_send("codex", "spoof", notify=False, system_sender="owner")


class NoModelTests(unittest.TestCase):
    """The owner's constraint: nothing at runtime may consult a model."""

    ALLOWED_IMPORTS = {"hashlib", "json", "os", "re", "sqlite3", "sys", "time", "herdr_client", "dunk_client",
                       "dunk_inbox"}
    FORBIDDEN = ("anthropic", "openai", "claude", "codex", "gemini", "llm", "gpt", "ollama", "mistral",
                 "completion", "subprocess", "urllib", "requests", "http", "socket")

    def violations(self, source):
        tree = ast.parse(source)
        imported = set()
        for node in ast.walk(tree):
            if isinstance(node, ast.Import):
                imported |= {alias.name.split(".")[0] for alias in node.names}
            elif isinstance(node, ast.ImportFrom):
                imported.add(node.module.split(".")[0])
        found = [f"import {name}" for name in sorted(imported - self.ALLOWED_IMPORTS)]
        docstrings = {id(node.body[0].value) for node in ast.walk(tree)
                      if isinstance(node, (ast.Module, ast.FunctionDef, ast.ClassDef)) and node.body
                      and isinstance(node.body[0], ast.Expr) and isinstance(node.body[0].value, ast.Constant)}
        for node in ast.walk(tree):
            texts = []
            if isinstance(node, ast.Name):
                texts.append(node.id)
            elif isinstance(node, ast.Attribute):
                texts.append(node.attr)
            elif isinstance(node, ast.Constant) and isinstance(node.value, str) and id(node) not in docstrings:
                texts.append(node.value)
            found += [f"{text!r} mentions {word!r}" for text in texts for word in self.FORBIDDEN
                      if word in text.lower()]
        return found

    def test_watchdog_modules_import_and_name_nothing_model_related(self):
        for name in ("dunk_watchdog.py", "dunk_watchdog_deadman.py"):
            self.assertEqual([], self.violations(read(os.path.join(ROOT, name))), name)

    def test_the_guard_catches_a_model_call(self):
        self.assertTrue(self.violations("import anthropic\n"))
        self.assertTrue(self.violations("from dunk_client import X\nX().call('ask', model='claude-x')\n"))
        self.assertTrue(self.violations("import subprocess\nsubprocess.run(['codex', 'exec'])\n"))


if __name__ == "__main__":
    unittest.main()
