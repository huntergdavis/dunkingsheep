"""Exception watchdog (no model): rules fire at and stay silent below their
boundary, config is refused rather than defaulted, alerts are capped, deduped
and cooled down across restarts, self errors never page the owner or recurse,
the dead-man check is independent, and nothing model-related is imported."""

import ast
import copy
import json
import os
import sqlite3
import subprocess
import sys
import tempfile
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
sys.path.insert(0, HERE)
sys.path.insert(0, ROOT)

import dunk_watchdog as wd  # noqa: E402
import dunk_watchdog_deadman as deadman  # noqa: E402
from dunk_core import DunkError, Flock  # noqa: E402
from dunk_inbox import Inbox  # noqa: E402
from fakes import FakeHerdr  # noqa: E402

NOW = 1_800_000_000.0
MIN = 60.0
HOUR = 3600.0

with open(os.path.join(ROOT, "packaging", "watchdog.example.json"), encoding="utf-8") as _handle:
    EXAMPLE = json.load(_handle)

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

def read(path):
    with open(path, encoding="utf-8") as handle:
        return handle.read()


HOST_OK = {"load1": 1.0, "cpus": 8, "tmp_used_fraction": 0.1, "tmp_free_bytes": 50 * 2**30,
           "tmp_total_bytes": 60 * 2**30, "tmp_inode_used_fraction": 0.1}


class Fleet:
    """A throwaway config dir with the real store schemas, filled per test."""

    def __init__(self, tmp):
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
        self.host = dict(HOST_OK)
        self.write_json()
        self.msg_seq = 0

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

    def task(self, task_id, status, updated_at, assignees=()):
        self.sql("tasks.db", "INSERT INTO tasks (id, scope, title, status, assignees, created_at, updated_at) "
                 "VALUES (?, 'global', 't', ?, ?, ?, ?)", (task_id, status, json.dumps(list(assignees)),
                                                          updated_at, updated_at))

    def task_event(self, task_id, event, at, actor=None, meta=None):
        self.sql("tasks.db", "INSERT INTO task_events (task_id, event, at, actor, meta) VALUES (?,?,?,?,?)",
                 (task_id, event, at, actor, json.dumps(meta) if meta else None))

    def lease(self, task_id, pane, claimed, heartbeat, expires):
        self.sql("tasks.db", "INSERT INTO task_leases VALUES (?,?,?,?,?)", (task_id, pane, claimed, heartbeat,
                                                                           expires))

    def message(self, mailbox, state, created_at, pane_id=None, sender=None, reason=None, read_at=None):
        self.msg_seq += 1
        message_id = f"i{self.msg_seq}"
        self.sql("inbox.db", "INSERT INTO messages (id, mailbox, pane_id, sender, body, state, created_at, "
                 "reason, read_at) VALUES (?,?,?,?,?,?,?,?,?)",
                 (message_id, mailbox, pane_id, sender, "BODY-NEVER-REPORTED", state, created_at, reason,
                  read_at))
        return message_id

    def metric(self, kind, source, at):
        self.sql("metrics.db", "INSERT INTO events (at, kind, source) VALUES (?,?,?)", (at, kind, source))

    def config(self, tmp, **rules):
        config = copy.deepcopy(EXAMPLE)
        config["stores"]["config_dir"] = self.dir
        config["state_path"] = os.path.join(tmp, "state.json")
        config["heartbeat_path"] = os.path.join(tmp, "heartbeat")
        for rule_id in config["rules"]:
            config["rules"][rule_id]["enabled"] = False
        for rule_id, overrides in rules.items():
            config["rules"][rule_id.replace("__", ".")].update({"enabled": True, **overrides})
        return wd.validate_config(config)

    def evaluate(self, config, now=NOW):
        return wd.evaluate(config, now, panes_provider=lambda: list(self.panes),
                           host_sampler=lambda: dict(self.host))["findings"]


class WatchdogCase(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.tmp = self._tmp.name
        self.fleet = Fleet(self.tmp)

    def tearDown(self):
        self._tmp.cleanup()

    def keys(self, findings, rule=None):
        return [(f["rule"], f["key"]) for f in findings if rule is None or f["rule"] == rule]


class RuleBoundaryTests(WatchdogCase):
    """Each rule: one case that fires exactly at its threshold, one just below."""

    def test_task_blocked_stale(self):
        cfg = self.fleet.config(self.tmp, task__blocked_stale={"threshold_minutes": 60})
        self.fleet.task("t1", "blocked", NOW - 60 * MIN)
        self.fleet.task("t2", "blocked", NOW - 60 * MIN + 1)
        self.fleet.task("t3", "blocked", NOW - 120 * MIN)
        self.fleet.task_event("t3", "status", NOW - 10 * MIN)  # a state change resets the clock
        self.assertEqual([("task.blocked_stale", "t1")], self.keys(self.fleet.evaluate(cfg)))

    def test_task_lease_expired_unrenewed_and_reaped(self):
        cfg = self.fleet.config(self.tmp, task__lease_stale={"never_renewed_minutes": 30,
                                                             "lookback_minutes": 60})
        self.fleet.lease("a", "w1:p1", NOW - 99 * MIN, NOW - 50 * MIN, NOW)  # expires_at == now: expired
        self.fleet.lease("b", "w1:p1", NOW - 30 * MIN, NOW - 30 * MIN, NOW + HOUR)  # never renewed, 30 min
        self.fleet.lease("c", "w1:p1", NOW - 29 * MIN, NOW - 29 * MIN, NOW + HOUR)  # 29 min: silent
        self.fleet.lease("d", "w1:p1", NOW - 99 * MIN, NOW - 1 * MIN, NOW + HOUR)  # renewed: silent
        self.fleet.task_event("e", "requeued", NOW - 60 * MIN, meta={"reason": "expired"})
        self.fleet.task_event("f", "requeued", NOW - 61 * MIN, meta={"reason": "expired"})  # outside lookback
        self.fleet.task_event("g", "released", NOW - 5 * MIN, meta={"reason": "manual"})  # not a reap
        self.assertEqual([("task.lease_stale", "expired:a@w1:p1"), ("task.lease_stale", "reaped:1"),
                          ("task.lease_stale", "unrenewed:b@w1:p1")], self.keys(self.fleet.evaluate(cfg)))

    def test_dunk_skip_loop_uses_lookback_not_recency(self):
        cfg = self.fleet.config(self.tmp, dunk__skip_loop={"consecutive_skips": 3, "lookback_hours": 168})
        self.fleet.dunks = [{"id": "d1", "running": True}, {"id": "d2", "running": True}]
        self.fleet.write_json()
        for n in range(3):  # three in a row, two days ago, then a send: still reported
            self.fleet.metric("dunk.skipped", "d1", NOW - 48 * HOUR + n)
        self.fleet.metric("dunk.sent", "d1", NOW - 47 * HOUR)
        for n in range(2):  # only two in a row: silent
            self.fleet.metric("dunk.skipped", "d2", NOW - HOUR + n)
        self.fleet.metric("dunk.sent", "d2", NOW - 1)
        self.fleet.metric("dunk.skipped", "d2", NOW)
        found = self.fleet.evaluate(cfg)
        self.assertEqual(["dunk.skip_loop"], [f["rule"] for f in found])
        self.assertEqual(3, found[0]["evidence"]["longest_consecutive_skips"])
        self.assertEqual(0, found[0]["evidence"]["current_streak"])

    def test_dunk_skip_loop_outside_lookback_is_silent(self):
        cfg = self.fleet.config(self.tmp, dunk__skip_loop={"consecutive_skips": 3, "lookback_hours": 24})
        self.fleet.dunks = [{"id": "d1", "running": True}]
        self.fleet.write_json()
        for n in range(5):
            self.fleet.metric("dunk.skipped", "d1", NOW - 25 * HOUR + n)
        self.assertEqual([], self.fleet.evaluate(cfg))

    def test_dunk_overdue(self):
        cfg = self.fleet.config(self.tmp, dunk__overdue={"missed_intervals": 3})
        self.fleet.dunks = [
            {"id": "d1", "running": True, "interval_minutes": 10, "next_send_at": NOW - 30 * MIN},
            {"id": "d2", "running": True, "interval_minutes": 10, "next_send_at": NOW - 30 * MIN + 1},
            {"id": "d3", "running": False, "interval_minutes": 10, "next_send_at": NOW - 99 * MIN},
            {"id": "d4", "running": True, "interval_minutes": 10, "next_send_at": None},
        ]
        self.fleet.write_json()
        self.assertEqual([("dunk.overdue", "d1"), ("dunk.overdue", "d4")], self.keys(self.fleet.evaluate(cfg)))

    def test_inbox_undeliverable_backlog_has_no_age_limit(self):
        cfg = self.fleet.config(self.tmp, inbox__undeliverable={"min_messages": 2})
        for _ in range(2):
            self.fleet.message("w9:p8", "undeliverable", NOW - 30 * 24 * HOUR, reason="target pane no longer exists")
        self.fleet.message("w9:p7", "undeliverable", NOW - HOUR, reason="target pane no longer exists")
        found = self.fleet.evaluate(cfg)
        self.assertEqual([("inbox.undeliverable", "w9:p8")], self.keys(found))
        self.assertNotIn("BODY-NEVER-REPORTED", json.dumps(found))

    def test_inbox_unread_live_and_dead(self):
        cfg = self.fleet.config(self.tmp, inbox__unread_live={"age_minutes": 60, "min_messages": 1},
                                inbox__unread_dead={"min_messages": 1})
        self.fleet.panes = [{"pane_id": "w1:p1", "agent_status": "idle"}]
        self.fleet.message("alive", "queued", NOW - 60 * MIN, pane_id="w1:p1")       # live, at age: fires
        self.fleet.message("alive2", "queued", NOW - 60 * MIN + 1, pane_id="w1:p1")  # just under: silent
        self.fleet.message("gone", "queued", NOW - 1, pane_id="w9:p9")              # dead pane: fires
        self.assertEqual([("inbox.unread_dead", "gone"), ("inbox.unread_live", "alive")],
                         self.keys(self.fleet.evaluate(cfg)))

    def test_inbox_ambiguous_sender_names_only_pane_resolving_labels(self):
        cfg = self.fleet.config(self.tmp, inbox__ambiguous_sender={"lookback_hours": 24, "sample_ids": 2,
                                                                   "require_live_pane": True})
        self.fleet.panes = [{"pane_id": "wR:p4", "agent_status": "idle"}]
        self.fleet.message("codex poolrad", "read", NOW - 48 * HOUR, pane_id="wR:p4")  # routing history
        self.fleet.message("dead lane", "read", NOW - 48 * HOUR, pane_id="w9:p9")
        ids = [self.fleet.message("x", "read", NOW - HOUR, sender="codex poolrad") for _ in range(3)]
        self.fleet.message("x", "read", NOW - HOUR, sender="dead lane")      # pane not live: silent
        self.fleet.message("x", "read", NOW - HOUR, sender="legacy-name")    # not a pane label: silent
        self.fleet.message("x", "read", NOW - HOUR, sender="wR:p4")          # a pane id: silent
        found = self.fleet.evaluate(cfg)
        self.assertEqual([("inbox.ambiguous_sender", "codex poolrad")], self.keys(found))
        self.assertEqual(ids[:2], found[0]["evidence"]["sample_ids"])
        self.assertEqual("info", found[0]["severity"])

    def test_lane_rules(self):
        cfg = self.fleet.config(self.tmp, lane__working_without_task={"quiet_minutes": 60},
                                lane__idle_holding_task={"quiet_minutes": 60, "idle_statuses": ["idle"],
                                                         "holding_statuses": ["in_progress"]})
        self.fleet.panes = [{"pane_id": "w1:p1", "agent_status": "working"},
                            {"pane_id": "w1:p2", "agent_status": "working"},
                            {"pane_id": "w1:p3", "agent_status": "idle"},
                            {"pane_id": "w1:p4", "agent_status": "idle"}]
        self.fleet.task_event("tx", "status", NOW - 59 * MIN, actor="w1:p2")  # recent task activity: silent
        self.fleet.task("t3", "in_progress", NOW - 99 * MIN, assignees=["w1:p3"])  # idle holder, quiet: fires
        self.fleet.task("t4", "in_progress", NOW - 99 * MIN, assignees=["w1:p4"])
        self.fleet.message("x", "read", NOW - 59 * MIN, sender="w1:p4")  # recent activity: silent
        self.assertEqual([("lane.idle_holding_task", "t3@w1:p3"), ("lane.working_without_task", "w1:p1")],
                         sorted(self.keys(self.fleet.evaluate(cfg))))

    def test_host_rules(self):
        cfg = self.fleet.config(self.tmp, host__load={"load1_per_cpu": 2.0},
                                host__tmp={"max_used_fraction": 0.9, "min_free_bytes": 100,
                                           "max_inode_fraction": 0.9, "path": "/tmp"})
        self.fleet.host.update(load1=16.0, cpus=8, tmp_used_fraction=0.9, tmp_free_bytes=101,
                               tmp_inode_used_fraction=0.89)
        self.assertEqual([("host.tmp", "/tmp:used_fraction"), ("host.load", "load1")],
                         self.keys(self.fleet.evaluate(cfg)))
        self.fleet.host.update(load1=15.99, tmp_used_fraction=0.89)
        self.assertEqual([], self.fleet.evaluate(cfg))

    def test_health_failed(self):
        cfg = self.fleet.config(self.tmp, health__failed={"stale_minutes": 30})
        self.fleet.health = {"checked_at": NOW - 30 * MIN + 1, "ok": True, "problems": []}
        self.fleet.write_json()
        self.assertEqual([], self.fleet.evaluate(cfg))
        self.fleet.health = {"checked_at": NOW - 30 * MIN, "ok": False, "problems": ["wal"]}
        self.fleet.write_json()
        self.assertEqual([("health.failed", "problems"), ("health.failed", "stale")],
                         self.keys(self.fleet.evaluate(cfg)))

    def test_pane_rules_skip_visibly_without_a_pane_source(self):
        cfg = self.fleet.config(self.tmp, lane__working_without_task={"quiet_minutes": 60})
        cfg["panes"]["source"] = "none"
        result = wd.evaluate(cfg, NOW)
        self.assertEqual(["lane.working_without_task"], result["skipped_rules"])

    def test_output_is_deterministic(self):
        cfg = self.fleet.config(self.tmp, inbox__undeliverable={"min_messages": 1},
                                task__blocked_stale={"threshold_minutes": 1})
        for mailbox in ("b", "a", "c"):
            self.fleet.message(mailbox, "undeliverable", NOW - HOUR)
        self.fleet.task("t9", "blocked", NOW - HOUR)
        self.assertEqual(self.fleet.evaluate(cfg), self.fleet.evaluate(cfg))
        self.assertEqual(["a", "b", "c", "t9"], [f["key"] for f in self.fleet.evaluate(cfg)])


class ConfigTests(WatchdogCase):
    def test_example_config_is_valid_and_quiet(self):
        config = wd.validate_config(copy.deepcopy(EXAMPLE))
        self.assertFalse(config["enabled"])
        self.assertTrue(config["dry_run"])
        self.assertFalse(config["escalation"]["owner"]["enabled"])

    def assertNamed(self, mutate, key):
        config = copy.deepcopy(EXAMPLE)
        mutate(config)
        with self.assertRaises(wd.ConfigError) as caught:
            wd.validate_config(config)
        self.assertEqual(key, caught.exception.key)

    def test_missing_enabled_or_dry_run_is_an_error_not_a_default(self):
        self.assertNamed(lambda c: c.pop("enabled"), "enabled")
        self.assertNamed(lambda c: c.pop("dry_run"), "dry_run")

    def test_named_errors(self):
        self.assertNamed(lambda c: c.update(surprise=1), "surprise")
        self.assertNamed(lambda c: c["rules"]["dunk.skip_loop"].update(consecutive_skips=0),
                         "rules.dunk.skip_loop.consecutive_skips")
        self.assertNamed(lambda c: c["rules"]["host.tmp"].update(max_used_fraction="high"),
                         "rules.host.tmp.max_used_fraction")
        self.assertNamed(lambda c: c["rules"].pop("host.load"), "rules.host.load")
        self.assertNamed(lambda c: c["escalation"]["vp"].update(min_severity="loud"),
                         "escalation.vp.min_severity")
        self.assertNamed(lambda c: c["panes"].update(pane_id_pattern="("), "panes.pane_id_pattern")

    def test_owner_paging_requires_an_approver(self):
        def enable(config):
            config["escalation"]["owner"].update(enabled=True, mailbox="owner")
        self.assertNamed(enable, "escalation.owner.approved_by")

    def test_cli_refuses_missing_and_invalid_config_without_alerting(self):
        bad = os.path.join(self.tmp, "bad.json")
        with open(bad, "w") as handle:
            json.dump({"schema": wd.SCHEMA_ID}, handle)
        for path, needle in ((os.path.join(self.tmp, "none.json"), "not found"), (bad, "enabled: missing")):
            proc = subprocess.run([sys.executable, os.path.join(ROOT, "dunkingsheep"), "watchdog",
                                   "--config", path], capture_output=True, text=True, timeout=60)
            self.assertEqual(2, proc.returncode, proc.stderr)
            self.assertIn("refusing to start", proc.stderr)
            self.assertIn(needle, proc.stderr)
            self.assertEqual("", proc.stdout)


class QuietStartAndDeliveryTests(WatchdogCase):
    def live_config(self, **extra):
        cfg = self.fleet.config(self.tmp, inbox__undeliverable={"min_messages": 1, "cooldown_minutes": 60})
        cfg.update(enabled=True, dry_run=False, **extra)
        return cfg

    def run_once(self, cfg, now=NOW, dry=False):
        sent = []
        report = wd.run(cfg, now, dry_run_flag=dry, send=lambda *a: sent.append(a),
                        panes_provider=lambda: [], host_sampler=lambda: dict(HOST_OK))
        return report, sent

    def test_enabled_false_or_dry_run_true_sends_nothing(self):
        self.fleet.message("dead", "undeliverable", NOW - HOUR)
        for overrides in ({"enabled": False}, {"dry_run": True}):
            cfg = self.live_config()
            cfg.update(overrides)
            report, sent = self.run_once(cfg)
            self.assertEqual("dry-run", report["mode"])
            self.assertEqual([], sent)
            self.assertEqual(1, len(report["plan"]["vp"]["alerts"]), "the plan still shows what would fire")

    def test_dry_run_writes_no_state_and_no_heartbeat(self):
        self.fleet.message("dead", "undeliverable", NOW - HOUR)
        cfg = self.live_config()
        _, sent = self.run_once(cfg, dry=True)
        self.assertEqual([], sent)
        self.assertFalse(os.path.exists(cfg["state_path"]))
        self.assertFalse(os.path.exists(cfg["heartbeat_path"]))

    def test_live_run_sends_one_digest_and_touches_heartbeat(self):
        self.fleet.message("dead", "undeliverable", NOW - HOUR)
        cfg = self.live_config()
        report, sent = self.run_once(cfg)
        self.assertEqual(["vp"], report["sent"])
        self.assertEqual(1, len(sent))
        self.assertEqual("vp of agentry", sent[0][0])
        self.assertNotIn("BODY-NEVER-REPORTED", sent[0][2])
        self.assertEqual(NOW, os.stat(cfg["heartbeat_path"]).st_mtime)

    def test_cap_sends_exactly_max_and_says_how_many_were_suppressed(self):
        for n in range(5):
            self.fleet.message(f"dead{n}", "undeliverable", NOW - HOUR)
        cfg = self.live_config(max_alerts_per_run=3)
        report, sent = self.run_once(cfg)
        self.assertEqual(3, len(report["plan"]["vp"]["alerts"]))
        self.assertEqual(2, report["plan"]["suppressed"])
        self.assertIn("2 further finding(s) suppressed", sent[0][2])

    def test_cooldown_dedupes_and_persists_across_restart(self):
        self.fleet.message("dead", "undeliverable", NOW - HOUR)
        cfg = self.live_config()
        self.assertEqual(1, len(self.run_once(cfg)[1]))
        # A new run reloads state from disk: the same as a restarted process.
        self.assertEqual([], self.run_once(cfg, now=NOW + 59 * MIN)[1], "inside cooldown: no duplicate")
        self.assertEqual(1, len(self.run_once(cfg, now=NOW + 60 * MIN)[1]), "after cooldown: re-alert")

    def test_owner_paged_only_when_enabled_for_category(self):
        self.fleet.message("dead", "undeliverable", NOW - HOUR)  # category delivery
        self.fleet.host.update(tmp_used_fraction=0.99)
        cfg = self.fleet.config(self.tmp, inbox__undeliverable={"min_messages": 1},
                                host__tmp={"severity": "critical"})
        cfg.update(enabled=True, dry_run=False)
        cfg["escalation"]["owner"].update(enabled=True, mailbox="owner", approved_by="VP i19781")
        sent = []
        wd.run(cfg, NOW, send=lambda *a: sent.append(a), panes_provider=lambda: [],
               host_sampler=lambda: dict(self.fleet.host))
        by_target = {to: body for to, _, body in sent}
        self.assertIn("host.tmp", by_target["owner"])
        self.assertNotIn("inbox.undeliverable", by_target["owner"])
        self.assertIn("inbox.undeliverable", by_target["vp of agentry"])


class SelfErrorTests(WatchdogCase):
    def owner_live_config(self):
        cfg = self.fleet.config(self.tmp, inbox__undeliverable={"min_messages": 1},
                                task__blocked_stale={"threshold_minutes": 1})
        cfg.update(enabled=True, dry_run=False)
        cfg["escalation"]["owner"].update(enabled=True, mailbox="owner", approved_by="VP", min_severity="info",
                                          categories=["outage", "security", "money"])
        return cfg

    def test_rule_exception_is_one_self_error_to_vp_never_owner(self):
        self.fleet.message("dead", "undeliverable", NOW - HOUR)
        cfg = self.owner_live_config()
        cfg["rules"]["host.load"]["enabled"] = True
        broken = dict(wd.RULES)
        broken["host.load"] = lambda inputs, rule: 1 / 0
        broken["task.blocked_stale"] = lambda inputs, rule: [][1]
        original, wd.RULES = wd.RULES, broken
        try:
            sent = []
            report = wd.run(cfg, NOW, send=lambda *a: sent.append(a), panes_provider=lambda: [],
                            host_sampler=lambda: dict(HOST_OK))
        finally:
            wd.RULES = original
        selfs = [f for f in report["findings"] if f["rule"] == wd.SELF_ERROR_RULE]
        self.assertEqual(1, len(selfs), "one self error per run, however many rules failed")
        self.assertEqual(wd.SOURCE_SELF, selfs[0]["source"])
        self.assertEqual(["host.load", "task.blocked_stale"],
                         [e["rule"] for e in selfs[0]["evidence"]["failed_rules"]])
        self.assertIn("inbox.undeliverable", [f["rule"] for f in report["findings"]], "other rules still run")
        targets = [to for to, _, _ in sent]
        self.assertEqual(["vp of agentry"], targets, "never the owner, even with owner paging on")
        self.assertIn("(watchdog-self)", sent[0][2])

    def test_unreadable_store_becomes_a_self_error(self):
        with open(os.path.join(self.fleet.dir, "tasks.db"), "wb") as handle:
            handle.write(b"not a database")
        cfg = self.owner_live_config()
        found = wd.evaluate(cfg, NOW)["findings"]
        self.assertEqual(wd.SELF_ERROR_RULE, found[0]["rule"])
        self.assertEqual(["task.blocked_stale"], [e["rule"] for e in found[0]["evidence"]["failed_rules"]])

    def test_delivery_failure_does_not_recurse_or_beat(self):
        self.fleet.message("dead", "undeliverable", NOW - HOUR)
        cfg = self.owner_live_config()
        calls = []

        def failing(*args):
            calls.append(args)
            raise ConnectionError("daemon down")
        report = wd.run(cfg, NOW, send=failing, panes_provider=lambda: [], host_sampler=lambda: dict(HOST_OK))
        self.assertEqual(1, len(calls), "no retry through the failing channel")
        self.assertIn("daemon down", report["delivery_error"])
        self.assertFalse(os.path.exists(cfg["heartbeat_path"]), "no heartbeat: the dead-man will report it")
        self.assertFalse(os.path.exists(cfg["state_path"]))
        self.assertIn("DELIVERY FAILED (source=watchdog-self)", wd.render_text(report))


class DeadManTests(WatchdogCase):
    def check(self, now, **extra):
        sent = []
        args = dict(heartbeat=os.path.join(self.tmp, "heartbeat"), cadence_minutes=10, multiplier=3,
                    to="vp of agentry", state_path=os.path.join(self.tmp, "deadman.json"), cooldown_minutes=60,
                    now=now, send=lambda *a: sent.append(a))
        args.update(extra)
        return deadman.check(**args), sent

    def beat(self, at):
        path = os.path.join(self.tmp, "heartbeat")
        with open(path, "w"):
            pass
        os.utime(path, (at, at))

    def test_fresh_heartbeat_is_silent_and_aged_one_alerts_once(self):
        self.beat(NOW - 30 * MIN)
        report, sent = self.check(NOW)
        self.assertFalse(report["stale"])
        self.assertEqual([], sent)
        report, sent = self.check(NOW + 1)
        self.assertTrue(report["alerted"])
        self.assertEqual(1, len(sent))
        self.assertIn(deadman.RULE_ID, sent[0][2])
        self.assertEqual([], self.check(NOW + 2)[1], "cooldown: one alert, not one per check")

    def test_missing_heartbeat_alerts_even_with_watchdog_disabled_or_broken(self):
        with open(os.path.join(self.tmp, "watchdog.json"), "w") as handle:
            handle.write("{ not json")  # an invalid watchdog config is irrelevant to the dead-man
        report, sent = self.check(NOW)
        self.assertTrue(report["alerted"])
        self.assertIn("missing", sent[0][2])

    def test_watchdog_dry_run_never_touches_the_heartbeat(self):
        self.beat(NOW - HOUR)
        cfg = self.fleet.config(self.tmp)
        cfg["heartbeat_path"] = os.path.join(self.tmp, "heartbeat")
        wd.run(cfg, NOW, dry_run_flag=True, panes_provider=lambda: [], host_sampler=lambda: dict(HOST_OK))
        self.assertEqual(NOW - HOUR, os.stat(cfg["heartbeat_path"]).st_mtime)
        self.assertTrue(self.check(NOW)[0]["alerted"], "a dry run is not coverage")

    def test_deadman_does_not_import_the_watchdog(self):
        tree = ast.parse(read(os.path.join(ROOT, "dunk_watchdog_deadman.py")))
        imported = {alias.name for node in ast.walk(tree) if isinstance(node, (ast.Import, ast.ImportFrom))
                    for alias in node.names} | {node.module for node in ast.walk(tree)
                                                if isinstance(node, ast.ImportFrom)}
        self.assertNotIn("dunk_watchdog", imported)


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
            self.flock.inbox_send("codex", "spoof", notify=False, self_pane_id="w1:p2",
                                  system_sender="watchdog")
        with self.assertRaises(DunkError):
            self.flock.inbox_send("codex", "spoof", notify=False, system_sender="owner")


class NoModelTests(unittest.TestCase):
    """The owner's constraint: nothing at runtime may consult a model."""

    ALLOWED_IMPORTS = {"json", "os", "re", "sqlite3", "time", "herdr_client", "dunk_client"}
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
                      and isinstance(node.body[0], ast.Expr)
                      and isinstance(node.body[0].value, ast.Constant)}
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
