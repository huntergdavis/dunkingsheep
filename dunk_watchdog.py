"""Deterministic exception watchdog for the Dunking Sheep fleet.

No model is in this loop, and none may be added: every rule is a fixed
predicate over read-only SQL and JSON reads, and every threshold, window,
severity, recipient and cooldown comes from one validated config file. The
same stores, config, state and `now` always produce the same findings in the
same order. `tests/test_dunk_watchdog.py` fails the build if this module (or
the dead-man check) imports or names anything model-related.

This alerts; it does not visualise. Output is text or JSON on stdout, at most
one digest message per recipient per run through the existing inbox API, and a
derived count. There is no UI, HTTP server or dashboard: viewing state is the
metrics collector's job.

Inputs (all opened read-only; their schemas are never changed):
  tasks.db (tasks, task_leases, task_events), inbox.db (messages),
  metrics.db (events), dunks.json, health.json, a host sample (os.statvfs,
  os.getloadavg), and optionally a herdr pane snapshot (`herdr pane list`, a
  local read used only by the lane rules and live/dead mailbox checks).

Outputs: findings, an alert plan, and (live runs only) one digest per
recipient, a state file written atomically, and a heartbeat file touched on
every successful live run. A dry run sends nothing and writes nothing.

Escalation: the VP inbox first. The owner is paged only when
`escalation.owner.enabled` is true (which also requires `approved_by`), only
for the configured categories (security/outage/money) and severity, and never
for the watchdog's own errors. A failure of the watchdog itself is one
`watchdog.self_error` finding per run (source `watchdog-self`), VP at most.
"""

import json
import os
import re
import sqlite3
import time

SCHEMA_ID = "dunkingsheep-watchdog/1"
SEVERITIES = ("info", "warning", "critical")
SEVERITY_RANK = {name: rank for rank, name in enumerate(SEVERITIES)}
CATEGORIES = ("work", "delivery", "scheduling", "audit", "security", "outage", "money")
OWNER_CATEGORIES = ("security", "outage", "money")
SOURCE_RULE = "watchdog-rule"
SOURCE_SELF = "watchdog-self"
SELF_ERROR_RULE = "watchdog.self_error"
SYSTEM_SENDER = "watchdog"


class ConfigError(Exception):
    """A named config problem: the dotted key and the reason. Refuse to start."""

    def __init__(self, key, reason):
        super().__init__(f"{key}: {reason}")
        self.key = key
        self.reason = reason


# -- config schema -----------------------------------------------------------
#
# Every key is required and unknown keys are refused, so nothing runs on a
# default nobody chose. A spec is (type, check, description); `check` returns
# an error string or None.

def _num(minimum=None, maximum=None, integer=False):
    def check(value):
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            return "must be a number"
        if integer and not isinstance(value, int):
            return "must be an integer"
        if minimum is not None and value < minimum:
            return f"must be >= {minimum}, got {value}"
        if maximum is not None and value > maximum:
            return f"must be <= {maximum}, got {value}"
        return None
    return check


def _bool(value):
    return None if isinstance(value, bool) else "must be true or false"


def _str(value):
    return None if isinstance(value, str) and value.strip() else "must be a non-empty string"


def _opt_str(value):
    return None if value is None or (isinstance(value, str) and value.strip()) else \
        "must be null or a non-empty string"


def _enum(*choices):
    def check(value):
        return None if value in choices else f"must be one of {list(choices)}, got {value!r}"
    return check


def _enum_list(*choices):
    def check(value):
        if not isinstance(value, list) or not all(v in choices for v in value):
            return f"must be a list drawn from {list(choices)}"
        if len(set(value)) != len(value):
            return "must not repeat entries"
        return None
    return check


def _regex(value):
    if not isinstance(value, str) or not value:
        return "must be a non-empty regular expression"
    try:
        re.compile(value)
    except re.error as error:
        return f"is not a valid regular expression ({error})"
    return None


RULE_COMMON = {
    "enabled": _bool,
    "severity": _enum(*SEVERITIES),
    "category": _enum(*CATEGORIES),
    "cooldown_minutes": _num(0),
}

RULE_PARAMS = {
    "task.blocked_stale": {"threshold_minutes": _num(1)},
    "task.lease_stale": {"never_renewed_minutes": _num(1), "lookback_minutes": _num(1)},
    "dunk.skip_loop": {"consecutive_skips": _num(1, integer=True), "lookback_hours": _num(1)},
    "dunk.overdue": {"missed_intervals": _num(1)},
    "inbox.undeliverable": {"min_messages": _num(1, integer=True)},
    "inbox.unread_live": {"age_minutes": _num(1), "min_messages": _num(1, integer=True)},
    "inbox.unread_dead": {"min_messages": _num(1, integer=True)},
    "inbox.ambiguous_sender": {"lookback_hours": _num(1), "sample_ids": _num(1, 20, integer=True),
                               "require_live_pane": _bool},
    "lane.working_without_task": {"quiet_minutes": _num(1)},
    "lane.idle_holding_task": {"quiet_minutes": _num(1), "idle_statuses": _enum_list(
        "idle", "done", "blocked", "unknown"), "holding_statuses": _enum_list(
        "open", "in_progress", "blocked")},
    "host.load": {"load1_per_cpu": _num(0)},
    "host.tmp": {"max_used_fraction": _num(0, 1), "min_free_bytes": _num(0, integer=True),
                 "max_inode_fraction": _num(0, 1), "path": _str},
    "health.failed": {"stale_minutes": _num(1)},
}

TOP = {
    "schema": _enum(SCHEMA_ID),
    "enabled": _bool,
    "dry_run": _bool,
    "cadence_minutes": _num(1),
    "max_alerts_per_run": _num(1, integer=True),
    "state_path": _str,
    "heartbeat_path": _str,
    "state_retention_days": _num(1),
    "stores": {"config_dir": _str},
    "panes": {"source": _enum("herdr", "none"), "pane_id_pattern": _regex},
    "escalation": {
        "vp": {"mailbox": _str, "min_severity": _enum(*SEVERITIES)},
        "owner": {"enabled": _bool, "mailbox": _opt_str, "categories": _enum_list(*OWNER_CATEGORIES),
                  "min_severity": _enum(*SEVERITIES), "approved_by": _opt_str},
    },
    "rules": {rule: dict(RULE_COMMON, **params) for rule, params in RULE_PARAMS.items()},
}


def _validate(spec, value, prefix):
    if not isinstance(value, dict):
        raise ConfigError(prefix or "<root>", "must be an object")
    for key in sorted(value):
        if key not in spec:
            raise ConfigError(f"{prefix}{key}", "unknown key")
    for key in spec:  # schema order, so the first error named is stable
        if key not in value:
            raise ConfigError(f"{prefix}{key}", "missing (no defaults: every key must be set)")
        sub = spec[key]
        if isinstance(sub, dict):
            _validate(sub, value[key], f"{prefix}{key}.")
        else:
            problem = sub(value[key])
            if problem:
                raise ConfigError(f"{prefix}{key}", problem)


def validate_config(config):
    """Raise ConfigError naming the first bad key; return the config unchanged."""
    _validate(TOP, config, "")
    owner = config["escalation"]["owner"]
    if owner["enabled"]:
        for key in ("mailbox", "approved_by"):
            if not owner[key]:
                raise ConfigError(f"escalation.owner.{key}",
                                  "required when owner paging is enabled")
        if not owner["categories"]:
            raise ConfigError("escalation.owner.categories",
                              "must name at least one category when owner paging is enabled")
    return config


def load_config(path):
    if not path or not os.path.exists(path):
        raise ConfigError("<file>", f"config file not found: {path}")
    try:
        with open(path, encoding="utf-8") as handle:
            config = json.load(handle)
    except (OSError, ValueError) as error:
        raise ConfigError("<file>", f"unreadable or not JSON: {error}") from None
    return validate_config(config)


def _expand(path):
    return os.path.expanduser(path)


# -- inputs -------------------------------------------------------------------

def _ro(path):
    if not os.path.exists(path):
        raise FileNotFoundError(path)
    return sqlite3.connect(f"file:{path}?mode=ro", uri=True)


def _load_json(path):
    with open(path, encoding="utf-8") as handle:
        return json.load(handle)


class Inputs:
    """Lazily loaded, read-only views of the stores. Each loader raises on an
    unreadable store; the evaluator turns that into a self error for the rules
    that needed it rather than a silent pass."""

    def __init__(self, config, now, panes_provider=None, host_sampler=None):
        self.config = config
        self.now = now
        self.dir = _expand(config["stores"]["config_dir"])
        self._panes_provider = panes_provider
        self._host_sampler = host_sampler
        self._cache = {}

    def _get(self, name, loader):
        if name not in self._cache:
            self._cache[name] = loader()
        return self._cache[name]

    def path(self, name):
        return os.path.join(self.dir, name)

    def tasks(self):
        def load():
            conn = _ro(self.path("tasks.db"))
            conn.row_factory = sqlite3.Row
            try:
                tasks = [dict(r) for r in conn.execute("SELECT * FROM tasks ORDER BY id")]
                leases = [dict(r) for r in conn.execute(
                    "SELECT * FROM task_leases ORDER BY task_id, pane_id")]
                events = [dict(r) for r in conn.execute(
                    "SELECT * FROM task_events ORDER BY at, id")]
            finally:
                conn.close()
            for task in tasks:
                task["assignees"] = json.loads(task.get("assignees") or "[]")
            return {"tasks": tasks, "leases": leases, "events": events}
        return self._get("tasks", load)

    def inbox(self, sql, params=()):
        conn = _ro(self.path("inbox.db"))
        try:
            return conn.execute(sql, params).fetchall()
        finally:
            conn.close()

    def metrics(self, sql, params=()):
        conn = _ro(self.path("metrics.db"))
        try:
            return conn.execute(sql, params).fetchall()
        finally:
            conn.close()

    def dunks(self):
        return self._get("dunks", lambda: _load_json(self.path("dunks.json")).get("dunkers", []))

    def health(self):
        return self._get("health", lambda: _load_json(self.path("health.json")))

    def panes(self):
        """{pane_id: agent_status} or None when the pane source is disabled."""
        if self.config["panes"]["source"] == "none":
            return None

        def load():
            if self._panes_provider is not None:
                panes = self._panes_provider()
            else:
                from herdr_client import HerdrClient
                panes = HerdrClient().list_panes()
            return {p["pane_id"]: (p.get("agent_status") or "unknown")
                    for p in panes if p.get("pane_id")}
        return self._get("panes", load)

    def host(self):
        def load():
            if self._host_sampler is not None:
                return self._host_sampler()
            return sample_host(self.config["rules"]["host.tmp"]["path"])
        return self._get("host", load)

    def name_to_pane(self):
        """Mailbox name -> pane id, from the inbox's own routing history (the
        latest message wins, ties broken by id). Read-only, no heuristics."""
        def load():
            mapping = {}
            for mailbox, pane in self.inbox(
                    "SELECT mailbox, pane_id FROM messages WHERE pane_id IS NOT NULL "
                    "ORDER BY created_at, id"):
                mapping[mailbox] = pane
            return mapping
        return self._get("name_to_pane", load)


def sample_host(path):
    stat = os.statvfs(path)
    total = stat.f_blocks * stat.f_frsize
    load1, _, _ = os.getloadavg()
    return {
        "load1": load1,
        "cpus": os.cpu_count() or 1,
        "tmp_used_fraction": ((stat.f_blocks - stat.f_bfree) / stat.f_blocks) if stat.f_blocks else 1.0,
        "tmp_free_bytes": stat.f_bavail * stat.f_frsize,
        "tmp_total_bytes": total,
        "tmp_inode_used_fraction": ((stat.f_files - stat.f_ffree) / stat.f_files) if stat.f_files else 0.0,
    }


# -- findings -----------------------------------------------------------------

def finding(rule_id, rule, key, summary, evidence, source=SOURCE_RULE):
    return {
        "rule": rule_id, "key": str(key), "severity": rule["severity"],
        "category": rule["category"], "source": source, "summary": summary,
        "evidence": evidence,
    }


def _minutes(seconds):
    return round(seconds / 60.0, 1)


def rule_task_blocked_stale(inputs, rule):
    data = inputs.tasks()
    last_event = {}
    for event in data["events"]:
        last_event[event["task_id"]] = max(last_event.get(event["task_id"], 0), event["at"])
    out = []
    for task in data["tasks"]:
        if task["status"] != "blocked":
            continue
        last = max(task["updated_at"], last_event.get(task["id"], 0))
        idle = inputs.now - last
        if idle >= rule["threshold_minutes"] * 60:
            out.append(finding("task.blocked_stale", rule, task["id"],
                               f"task {task['id']} blocked with no state change or event for "
                               f"{_minutes(idle)} min",
                               {"task": task["id"], "minutes_since_activity": _minutes(idle),
                                "assignees": task["assignees"],
                                "block_reason_set": task["block_reason"] is not None}))
    return out


def rule_task_lease_stale(inputs, rule):
    data = inputs.tasks()
    out = []
    for lease in data["leases"]:
        key = f"{lease['task_id']}@{lease['pane_id']}"
        if lease["expires_at"] <= inputs.now:
            out.append(finding("task.lease_stale", rule, f"expired:{key}",
                               f"lease {key} expired {_minutes(inputs.now - lease['expires_at'])} min ago "
                               "and is not yet reaped",
                               {"task": lease["task_id"], "pane": lease["pane_id"],
                                "expired_minutes_ago": _minutes(inputs.now - lease["expires_at"])}))
        elif (lease["heartbeat_at"] <= lease["claimed_at"]
              and inputs.now - lease["claimed_at"] >= rule["never_renewed_minutes"] * 60):
            out.append(finding("task.lease_stale", rule, f"unrenewed:{key}",
                               f"lease {key} never renewed in {_minutes(inputs.now - lease['claimed_at'])} min",
                               {"task": lease["task_id"], "pane": lease["pane_id"],
                                "claimed_minutes_ago": _minutes(inputs.now - lease["claimed_at"])}))
    since = inputs.now - rule["lookback_minutes"] * 60
    for event in data["events"]:
        if event["event"] not in ("requeued", "released") or event["at"] < since:
            continue
        meta = json.loads(event["meta"] or "{}")
        if meta.get("reason") not in ("expired", "pane_gone"):
            continue
        out.append(finding("task.lease_stale", rule, f"reaped:{event['id']}",
                           f"task {event['task_id']} lease reaped ({meta['reason']}) and "
                           f"{event['event']}",
                           {"task": event["task_id"], "event_id": event["id"],
                            "reason": meta["reason"], "minutes_ago": _minutes(inputs.now - event["at"])}))
    return out


def rule_dunk_skip_loop(inputs, rule):
    since = inputs.now - rule["lookback_hours"] * 3600
    out = []
    for dunk in sorted(inputs.dunks(), key=lambda d: d["id"]):
        rows = inputs.metrics(
            "SELECT id, at, kind FROM events WHERE source=? AND kind IN "
            "('dunk.sent','dunk.skipped') AND at>=? ORDER BY at, id", (dunk["id"], since))
        run = best = 0
        run_start = best_start = best_end = None
        for event_id, at, kind in rows:
            if kind == "dunk.skipped":
                run += 1
                run_start = run_start if run > 1 else at
                if run > best:
                    best, best_start, best_end = run, run_start, (event_id, at)
            else:
                run, run_start = 0, None
        if best >= rule["consecutive_skips"]:
            out.append(finding("dunk.skip_loop", rule, f"{dunk['id']}:{best_end[0]}",
                               f"dunk {dunk['id']} skipped {best} times in a row "
                               f"(window {rule['lookback_hours']} h)",
                               {"dunk": dunk["id"], "target_pane": dunk.get("target_pane_id"),
                                "longest_consecutive_skips": best, "current_streak": run,
                                "run_start_hours_ago": round((inputs.now - best_start) / 3600, 1),
                                "run_end_hours_ago": round((inputs.now - best_end[1]) / 3600, 1),
                                "cumulative_skip_count": dunk.get("skip_count"),
                                "skip_if_unconsumed": dunk.get("skip_if_unconsumed")}))
    return out


def rule_dunk_overdue(inputs, rule):
    out = []
    for dunk in sorted(inputs.dunks(), key=lambda d: d["id"]):
        if not dunk.get("running"):
            continue
        interval = float(dunk.get("interval_minutes") or 0) * 60
        due = dunk.get("next_send_at")
        if due is None:
            out.append(finding("dunk.overdue", rule, dunk["id"],
                               f"dunk {dunk['id']} is running with no scheduled send",
                               {"dunk": dunk["id"], "next_send_at": None}))
        elif interval > 0 and inputs.now - due >= rule["missed_intervals"] * interval:
            out.append(finding("dunk.overdue", rule, dunk["id"],
                               f"dunk {dunk['id']} has not fired for {_minutes(inputs.now - due)} min "
                               "past its scheduled send",
                               {"dunk": dunk["id"], "overdue_minutes": _minutes(inputs.now - due),
                                "interval_minutes": dunk.get("interval_minutes")}))
    return out


def _mailbox_groups(rows):
    groups = {}
    for message_id, mailbox, created_at, reason in rows:
        group = groups.setdefault(mailbox, {"count": 0, "ids": [], "reasons": {}, "oldest": created_at,
                                            "newest": created_at})
        group["count"] += 1
        group["ids"].append(message_id)
        group["reasons"][reason or "-"] = group["reasons"].get(reason or "-", 0) + 1
        group["oldest"] = min(group["oldest"], created_at)
        group["newest"] = max(group["newest"], created_at)
    return groups


def _group_evidence(inputs, mailbox, group, samples=3):
    return {"mailbox": mailbox, "count": group["count"], "sample_ids": group["ids"][:samples],
            "reasons": dict(sorted(group["reasons"].items())),
            "oldest_hours_ago": round((inputs.now - group["oldest"]) / 3600, 1),
            "newest_hours_ago": round((inputs.now - group["newest"]) / 3600, 1)}


def rule_inbox_undeliverable(inputs, rule):
    rows = inputs.inbox("SELECT id, mailbox, created_at, reason FROM messages "
                        "WHERE state='undeliverable' ORDER BY created_at, id")
    out = []
    for mailbox, group in sorted(_mailbox_groups(rows).items()):
        if group["count"] >= rule["min_messages"]:
            out.append(finding("inbox.undeliverable", rule, mailbox,
                               f"{group['count']} undeliverable message(s) for {mailbox}",
                               _group_evidence(inputs, mailbox, group)))
    return out


def _queued(inputs, older_than=None):
    sql = "SELECT id, mailbox, created_at, pane_id FROM messages WHERE state='queued'"
    params = ()
    if older_than is not None:
        sql += " AND created_at<=?"
        params = (older_than,)
    return inputs.inbox(sql + " ORDER BY created_at, id", params)


def rule_inbox_unread_live(inputs, rule):
    panes = inputs.panes()
    rows = [(i, m, c, None) for i, m, c, pane in _queued(inputs, inputs.now - rule["age_minutes"] * 60)
            if panes is None or (pane in panes)]
    out = []
    for mailbox, group in sorted(_mailbox_groups(rows).items()):
        if group["count"] >= rule["min_messages"]:
            evidence = _group_evidence(inputs, mailbox, group)
            evidence["pane_liveness"] = "unknown (pane source disabled)" if panes is None else "live"
            out.append(finding("inbox.unread_live", rule, mailbox,
                               f"{group['count']} message(s) unread for over {rule['age_minutes']} min "
                               f"in {mailbox}", evidence))
    return out


def rule_inbox_unread_dead(inputs, rule):
    panes = inputs.panes()
    if panes is None:
        return []
    rows = [(i, m, c, None) for i, m, c, pane in _queued(inputs) if pane and pane not in panes]
    out = []
    for mailbox, group in sorted(_mailbox_groups(rows).items()):
        if group["count"] >= rule["min_messages"]:
            out.append(finding("inbox.unread_dead", rule, mailbox,
                               f"{group['count']} unread message(s) queued for dead pane mailbox {mailbox}",
                               _group_evidence(inputs, mailbox, group)))
    return out


def rule_inbox_ambiguous_sender(inputs, rule):
    pattern = re.compile(inputs.config["panes"]["pane_id_pattern"])
    names = {name: pane for name, pane in inputs.name_to_pane().items() if not pattern.match(name)}
    panes = inputs.panes() if rule["require_live_pane"] else None
    rows = inputs.inbox("SELECT id, sender, created_at FROM messages WHERE sender IS NOT NULL "
                        "AND created_at>=? ORDER BY created_at, id",
                        (inputs.now - rule["lookback_hours"] * 3600,))
    groups = {}
    for message_id, sender, created_at in rows:
        if pattern.match(sender) or sender not in names:
            continue
        if panes is not None and names[sender] not in panes:
            continue
        groups.setdefault(sender, []).append((message_id, created_at))
    out = []
    for sender, items in sorted(groups.items()):
        out.append(finding("inbox.ambiguous_sender", rule, sender,
                           f"{len(items)} message(s) with sender {sender!r}, a mailbox name that "
                           f"resolves to pane {names[sender]}, not a pane id",
                           {"sender": sender, "resolves_to_pane": names[sender], "count": len(items),
                            "sample_ids": [i for i, _ in items[:rule["sample_ids"]]],
                            "pane_liveness": "unchecked" if panes is None else "live"}))
    return out


def _pane_activity(inputs, since):
    """Pane id -> latest activity time from tasks and inbox (read-only)."""
    activity = {}
    for event in inputs.tasks()["events"]:
        if event["actor"] and event["at"] >= since:
            activity[event["actor"]] = max(activity.get(event["actor"], 0), event["at"])
    for sender, at in inputs.inbox("SELECT sender, MAX(created_at) FROM messages WHERE sender IS NOT NULL "
                                   "AND created_at>=? GROUP BY sender", (since,)):
        activity[sender] = max(activity.get(sender, 0), at)
    for pane, at in inputs.inbox("SELECT pane_id, MAX(read_at) FROM messages WHERE pane_id IS NOT NULL "
                                 "AND read_at>=? GROUP BY pane_id", (since,)):
        activity[pane] = max(activity.get(pane, 0), at)
    return activity


def _holders(inputs, statuses):
    """Task id -> sorted holder pane ids, from leases and pane-resolvable assignees."""
    data = inputs.tasks()
    names = inputs.name_to_pane()
    pattern = re.compile(inputs.config["panes"]["pane_id_pattern"])
    holders = {}
    for task in data["tasks"]:
        if task["status"] not in statuses:
            continue
        panes = {lease["pane_id"] for lease in data["leases"] if lease["task_id"] == task["id"]}
        for who in task["assignees"]:
            pane = who if pattern.match(who) else names.get(who)
            if pane:
                panes.add(pane)
        if panes:
            holders[task["id"]] = sorted(panes)
    return holders


def rule_lane_working_without_task(inputs, rule):
    panes = inputs.panes()
    if panes is None:
        return []
    since = inputs.now - rule["quiet_minutes"] * 60
    task_actors = {e["actor"] for e in inputs.tasks()["events"] if e["actor"] and e["at"] >= since}
    holding = {p for holders in _holders(inputs, ("in_progress",)).values() for p in holders}
    out = []
    for pane, status in sorted(panes.items()):
        if status == "working" and pane not in holding and pane not in task_actors:
            out.append(finding("lane.working_without_task", rule, pane,
                               f"pane {pane} is working with no task held and no task event in "
                               f"{rule['quiet_minutes']} min",
                               {"pane": pane, "agent_status": status, "quiet_minutes": rule["quiet_minutes"]}))
    return out


def rule_lane_idle_holding_task(inputs, rule):
    panes = inputs.panes()
    if panes is None:
        return []
    since = inputs.now - rule["quiet_minutes"] * 60
    activity = _pane_activity(inputs, since)
    out = []
    for task_id, holders in sorted(_holders(inputs, tuple(rule["holding_statuses"])).items()):
        for pane in holders:
            status = panes.get(pane)
            if status in rule["idle_statuses"] and pane not in activity:
                out.append(finding("lane.idle_holding_task", rule, f"{task_id}@{pane}",
                                   f"pane {pane} holds task {task_id} while {status} with no activity "
                                   f"in {rule['quiet_minutes']} min",
                                   {"task": task_id, "pane": pane, "agent_status": status}))
    return out


def rule_host_load(inputs, rule):
    host = inputs.host()
    per_cpu = host["load1"] / max(1, host["cpus"])
    if per_cpu >= rule["load1_per_cpu"]:
        return [finding("host.load", rule, "load1",
                        f"load1 {host['load1']:.2f} on {host['cpus']} CPUs ({per_cpu:.2f}/CPU)",
                        {"load1": round(host["load1"], 2), "cpus": host["cpus"],
                         "per_cpu": round(per_cpu, 2), "threshold": rule["load1_per_cpu"]})]
    return []


def rule_host_tmp(inputs, rule):
    host = inputs.host()
    out = []
    checks = (
        ("used_fraction", host["tmp_used_fraction"] >= rule["max_used_fraction"],
         host["tmp_used_fraction"], rule["max_used_fraction"]),
        ("free_bytes", host["tmp_free_bytes"] <= rule["min_free_bytes"],
         host["tmp_free_bytes"], rule["min_free_bytes"]),
        ("inode_fraction", host["tmp_inode_used_fraction"] >= rule["max_inode_fraction"],
         host["tmp_inode_used_fraction"], rule["max_inode_fraction"]),
    )
    for name, fired, value, threshold in checks:
        if fired:
            out.append(finding("host.tmp", rule, f"{rule['path']}:{name}",
                               f"{rule['path']} {name} {round(value, 4)} breaches {threshold}",
                               {"path": rule["path"], "metric": name, "value": round(value, 4),
                                "threshold": threshold}))
    return out


def rule_health_failed(inputs, rule):
    health = inputs.health()
    out = []
    age = inputs.now - float(health.get("checked_at") or 0)
    if age >= rule["stale_minutes"] * 60:
        out.append(finding("health.failed", rule, "stale",
                           f"health.json not refreshed for {_minutes(age)} min",
                           {"minutes_since_check": _minutes(age)}))
    if not health.get("ok", False) or health.get("problems"):
        out.append(finding("health.failed", rule, "problems",
                           f"health check reports ok={health.get('ok')} with "
                           f"{len(health.get('problems') or [])} problem(s)",
                           {"ok": health.get("ok"), "problems": list(health.get("problems") or [])}))
    return out


RULES = {
    "task.blocked_stale": rule_task_blocked_stale,
    "task.lease_stale": rule_task_lease_stale,
    "dunk.skip_loop": rule_dunk_skip_loop,
    "dunk.overdue": rule_dunk_overdue,
    "inbox.undeliverable": rule_inbox_undeliverable,
    "inbox.unread_live": rule_inbox_unread_live,
    "inbox.unread_dead": rule_inbox_unread_dead,
    "inbox.ambiguous_sender": rule_inbox_ambiguous_sender,
    "lane.working_without_task": rule_lane_working_without_task,
    "lane.idle_holding_task": rule_lane_idle_holding_task,
    "host.load": rule_host_load,
    "host.tmp": rule_host_tmp,
    "health.failed": rule_health_failed,
}
assert set(RULES) == set(RULE_PARAMS), "every configurable rule needs exactly one implementation"

PANE_RULES = ("inbox.unread_dead", "lane.working_without_task", "lane.idle_holding_task")


def _sort_key(item):
    return (-SEVERITY_RANK.get(item["severity"], -1), item["rule"], item["key"])


def evaluate(config, now, panes_provider=None, host_sampler=None):
    """Run every enabled rule. A rule that raises becomes part of exactly one
    `watchdog.self_error` finding for the run; the other rules still report."""
    inputs = Inputs(config, now, panes_provider, host_sampler)
    findings, errors, skipped = [], [], []
    for rule_id in sorted(RULES):
        rule = config["rules"][rule_id]
        if not rule["enabled"]:
            continue
        if rule_id in PANE_RULES and config["panes"]["source"] == "none":
            skipped.append(rule_id)
            continue
        try:
            findings.extend(RULES[rule_id](inputs, rule))
        except Exception as error:  # noqa: BLE001 - a failing rule is reported, never raised
            errors.append({"rule": rule_id, "error": f"{type(error).__name__}: {error}"})
    findings.sort(key=_sort_key)
    if errors:
        findings.insert(0, {
            "rule": SELF_ERROR_RULE, "key": "run", "severity": "self", "category": "outage",
            "source": SOURCE_SELF,
            "summary": f"watchdog could not evaluate {len(errors)} rule(s); their coverage is missing",
            "evidence": {"failed_rules": errors},
        })
    return {"findings": findings, "skipped_rules": skipped}


# -- planning, state and delivery --------------------------------------------

def load_state(path):
    if not os.path.exists(path):
        return {"alerts": {}}
    with open(path, encoding="utf-8") as handle:
        state = json.load(handle)
    state.setdefault("alerts", {})
    return state


def save_state(path, state):
    directory = os.path.dirname(path) or "."
    os.makedirs(directory, exist_ok=True)
    temp = f"{path}.tmp.{os.getpid()}"
    with open(temp, "w", encoding="utf-8") as handle:
        json.dump(state, handle, indent=2, sort_keys=True)
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temp, path)


def _cooldown_seconds(config, item):
    if item["source"] == SOURCE_SELF:
        return config["cadence_minutes"] * 60
    return config["rules"][item["rule"]]["cooldown_minutes"] * 60


def plan(config, findings, state, now):
    """Which findings would be sent, to whom, and how many were held back.
    Pure: no I/O. Dedupe key is rule+key; cooldown is per rule."""
    vp = config["escalation"]["vp"]
    owner = config["escalation"]["owner"]
    due, cooling = [], 0
    for item in findings:
        is_self = item["source"] == SOURCE_SELF
        if not is_self and SEVERITY_RANK[item["severity"]] < SEVERITY_RANK[vp["min_severity"]]:
            continue
        last = state["alerts"].get(f"{item['rule']}|{item['key']}", {}).get("last_alerted")
        if last is not None and now - last < _cooldown_seconds(config, item):
            cooling += 1
            continue
        due.append(item)
    cap = config["max_alerts_per_run"]
    vp_alerts, suppressed = due[:cap], max(0, len(due) - cap)
    owner_alerts = [item for item in vp_alerts
                    if owner["enabled"] and item["source"] == SOURCE_RULE
                    and item["category"] in owner["categories"]
                    and SEVERITY_RANK[item["severity"]] >= SEVERITY_RANK[owner["min_severity"]]]
    return {"vp": {"mailbox": vp["mailbox"], "alerts": vp_alerts},
            "owner": {"mailbox": owner["mailbox"] if owner["enabled"] else None, "alerts": owner_alerts},
            "suppressed": suppressed, "cooling_down": cooling}


def _line(item):
    evidence = json.dumps(item["evidence"], sort_keys=True, separators=(",", ":"))
    return f"[{item['severity']}] {item['rule']} {item['key']} ({item['source']}): {item['summary']} | {evidence}"


def digest(alerts, suppressed, audience):
    lines = [f"Watchdog ({audience}): {len(alerts)} alert(s). Deterministic rules; no model involved."]
    lines += [_line(item) for item in alerts]
    if suppressed:
        lines.append(f"{suppressed} further finding(s) suppressed by max_alerts_per_run; "
                     "run `dunkingsheep watchdog --dry-run` to see all.")
    return "\n".join(lines)


def deliver(alert_plan, send):
    """Send at most one digest per recipient. `send(to, subject, body)` raises
    on failure; the caller reports it to stderr and does NOT retry through the
    inbox, so a failing channel can never recurse."""
    sent = []
    for audience in ("vp", "owner"):
        target = alert_plan[audience]
        if not target["alerts"] or not target["mailbox"]:
            continue
        has_self = any(a["source"] == SOURCE_SELF for a in target["alerts"])
        subject = f"watchdog: {len(target['alerts'])} alert(s)" + (" incl. self-error" if has_self else "")
        suppressed = alert_plan["suppressed"] if audience == "vp" else 0
        send(target["mailbox"], subject, digest(target["alerts"], suppressed, audience))
        sent.append(audience)
    return sent


def record(state, findings, alert_plan, now, retention_seconds):
    alerted = {f"{a['rule']}|{a['key']}" for a in alert_plan["vp"]["alerts"]}
    for item in findings:
        key = f"{item['rule']}|{item['key']}"
        entry = state["alerts"].setdefault(key, {"first_seen": now})
        entry["last_seen"] = now
        if key in alerted:
            entry["last_alerted"] = now
    for key in sorted(state["alerts"]):
        if now - state["alerts"][key].get("last_seen", now) > retention_seconds:
            del state["alerts"][key]
    state["last_run"] = now
    return state


def touch(path, now):
    directory = os.path.dirname(path) or "."
    os.makedirs(directory, exist_ok=True)
    with open(path, "a", encoding="utf-8"):
        pass
    os.utime(path, (now, now))


def inbox_sender(to, subject, body):
    """Live delivery through the daemon's inbox API as the `watchdog` system
    sender. Never starts the daemon: if it is down, the run fails and the
    dead-man check reports the missing heartbeat."""
    from dunk_client import FlockClient
    client = FlockClient(self_pane_id=None)
    client.call("inbox_send", to=to, subject=subject, body=body, system_sender=SYSTEM_SENDER)


def run(config, now, dry_run_flag=False, send=None, panes_provider=None, host_sampler=None):
    """One watchdog run. Live only when config enabled=true AND dry_run=false
    AND no --dry-run flag. Returns a report dict; never raises for rule errors."""
    live = config["enabled"] and not config["dry_run"] and not dry_run_flag
    state_path = _expand(config["state_path"])
    result = evaluate(config, now, panes_provider, host_sampler)
    state = load_state(state_path)
    alert_plan = plan(config, result["findings"], state, now)
    report = {
        "mode": "live" if live else "dry-run",
        "why_not_live": None if live else (
            "--dry-run flag" if dry_run_flag else
            "config enabled=false" if not config["enabled"] else "config dry_run=true"),
        "now": now, "findings": result["findings"], "skipped_rules": result["skipped_rules"],
        "plan": alert_plan, "sent": [], "delivery_error": None,
        "counts": {"findings": len(result["findings"]),
                   "needs_attention": len(alert_plan["vp"]["alerts"]) + alert_plan["suppressed"]},
    }
    if not live:
        return report
    try:
        report["sent"] = deliver(alert_plan, send or inbox_sender)
    except Exception as error:  # noqa: BLE001 - reported, never re-sent through the inbox
        report["delivery_error"] = f"{type(error).__name__}: {error}"
        return report
    save_state(state_path, record(state, result["findings"], alert_plan, now,
                                  config["state_retention_days"] * 86400))
    touch(_expand(config["heartbeat_path"]), now)
    return report


def render_text(report):
    lines = [f"watchdog {report['mode']}" + (f" ({report['why_not_live']})" if report["why_not_live"] else "")
             + f"  findings {report['counts']['findings']}  needs_attention "
             f"{report['counts']['needs_attention']}"]
    for item in report["findings"]:
        lines.append("  " + _line(item))
    for rule_id in report["skipped_rules"]:
        lines.append(f"  [skipped] {rule_id}: pane source disabled (panes.source=none)")
    vp, owner = report["plan"]["vp"], report["plan"]["owner"]
    verb = "sent" if report["mode"] == "live" else "would send"
    lines.append(f"plan: {verb} {len(vp['alerts'])} alert(s) to VP {vp['mailbox']}; "
                 f"{len(owner['alerts'])} to owner {owner['mailbox'] or '(owner paging disabled)'}; "
                 f"{report['plan']['suppressed']} suppressed by max_alerts_per_run; "
                 f"{report['plan']['cooling_down']} in cooldown")
    if report["delivery_error"]:
        lines.append(f"DELIVERY FAILED (source={SOURCE_SELF}): {report['delivery_error']}; "
                     "heartbeat not touched, so the dead-man check will report it")
    return "\n".join(lines)
