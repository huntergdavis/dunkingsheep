"""Deterministic exception watchdog for the Dunking Sheep fleet.

No model is in this loop, and none may be added: every rule is a fixed
predicate over read-only SQL and JSON reads, and every threshold, window,
severity, recipient, hold, cap and cooldown comes from one validated config
file. The same stores, config, state and `now` always produce the same findings
and the same plan, in the same order. `tests/test_dunk_watchdog.py` fails the
build if this module (or the dead-man check) imports or names anything
model-related.

This alerts; it does not visualise. Output is spec-format lines (or JSON) on
stdout, at most one digest per channel per run, and a derived count. There is
no UI, HTTP server or dashboard: viewing state is the metrics collector's job.

The escalation policy is docs/WATCHDOG_ESCALATION_SPEC.md. In short:

* Channels. VP is the dunkingsheep inbox. OWNER is anything that reaches the
  owner's phone; today that is a VP desk brief, which the Telegram bridge
  forwards. OWNER is one channel however it is delivered.
* Only the classes security, outage and money may page OWNER. ops and project
  are VP-only, however long they persist. The watchdog's own errors (class
  self) are VP-only.
* An owner page needs its rule's hold (`must_hold_runs` consecutive runs, each
  at least `min_run_spacing_seconds` after the previous counted run). It is
  limited by `max_pages_per_hour` across all rules and by the per-rule
  cooldown. A page counts as DELIVERED only when the forwarding path confirms
  it. Until then it is pending, no cooldown starts, and an unconfirmed or
  failed page is a watchdog.self_error to VP. Every check happens before
  anything is posted, because posting is the irrevocable "marked for
  forwarding" step.
* Fail toward not paging. A missing or corrupt state file, any rule error and
  any owner-transport error suppress owner pages for the run and are reported
  to VP. A bad config refuses to start (see the CLI).

Inputs (read-only; their schemas are never changed): tasks.db, inbox.db and
metrics.db (sqlite mode=ro), dunks.json, health.json, a host sample, and
optionally a herdr pane snapshot. Outputs: the watchdog's own state file and
heartbeat, and the digests.
"""

import hashlib
import json
import os
import re
import sqlite3
import time

SCHEMA_ID = "dunkingsheep-watchdog/2"
SEVERITIES = ("info", "warning", "critical")
SEVERITY_RANK = {name: rank for rank, name in enumerate(SEVERITIES)}
CLASSES = ("security", "outage", "money", "ops", "project")
OWNER_CLASSES = ("security", "outage", "money")
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


class OwnerChannelUnavailable(Exception):
    """The owner transport is not configured; fail toward not paging."""


# -- config schema -----------------------------------------------------------
#
# Every key is required and unknown keys are refused, so nothing runs on a
# default nobody chose. Thresholds, windows, holds, caps and cooldowns must be
# strictly positive: a zero threshold fires on every run and a zero rate limit
# is no limit, so both are config bugs that the validator refuses.

def _num(minimum=None, maximum=None, integer=False, positive=False):
    def check(value):
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            return "must be a number"
        if integer and not isinstance(value, int):
            return "must be an integer"
        if positive and value <= 0:
            return f"must be > 0, got {value}"
        if minimum is not None and value < minimum:
            return f"must be >= {minimum}, got {value}"
        if maximum is not None and value > maximum:
            return f"must be <= {maximum}, got {value}"
        return None
    return check


POSITIVE = _num(positive=True)
POSITIVE_INT = _num(integer=True, positive=True)
FRACTION = _num(maximum=1, positive=True)


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


def _enum_list(*choices, allow_empty=True):
    def check(value):
        if not isinstance(value, list) or not all(v in choices for v in value):
            return f"must be a list drawn from {list(choices)}"
        if len(set(value)) != len(value):
            return "must not repeat entries"
        if not allow_empty and not value:
            return "must not be empty"
        return None
    return check


def _str_list(value):
    if not isinstance(value, list) or not value or not all(isinstance(v, str) and v for v in value):
        return "must be a non-empty list of non-empty strings"
    return None


def _hex_list(value):
    if not isinstance(value, list) or not all(isinstance(v, str) and re.match(r"[0-9a-f]{64}\Z", v)
                                              for v in value):
        return "must be a list of lowercase sha256 hex digests (may be empty)"
    return None


def _regex_list(value):
    if not isinstance(value, list) or not value:
        return "must be a non-empty list of regular expressions"
    for item in value:
        problem = _regex(item)
        if problem:
            return f"entry {item!r} {problem}"
    return None


def _opt_positive(value):
    return None if value is None else POSITIVE(value)


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
    "class": _enum(*CLASSES),
    "cooldown_minutes": POSITIVE,
    "must_hold_runs": POSITIVE_INT,
}

RULE_PARAMS = {
    "task.blocked_stale": {"threshold_minutes": POSITIVE},
    "task.lease_stale": {"never_renewed_minutes": POSITIVE, "lookback_minutes": POSITIVE,
                         "fresh_minutes": POSITIVE},
    "dunk.skip_loop": {"consecutive_skips": POSITIVE_INT, "lookback_hours": POSITIVE},
    "dunk.overdue": {"missed_intervals": POSITIVE},
    # Above the 24h auto-archive: a page must mean "the archive did not catch
    # this", not "the archive is about to catch this".
    "inbox.undeliverable": {"min_messages": POSITIVE_INT, "min_age_hours": POSITIVE},
    "inbox.unread_live": {"age_minutes": POSITIVE, "min_messages": POSITIVE_INT},
    "inbox.unread_dead": {"min_messages": POSITIVE_INT},
    "inbox.ambiguous_sender": {"lookback_hours": POSITIVE, "sample_ids": _num(1, 50, integer=True),
                               "require_live_pane": _bool},
    "lane.working_without_task": {"quiet_minutes": POSITIVE},
    "lane.idle_holding_task": {"quiet_minutes": POSITIVE, "idle_statuses": _enum_list(
        "idle", "done", "blocked", "unknown", allow_empty=False), "holding_statuses": _enum_list(
        "open", "in_progress", "blocked", allow_empty=False)},
    # load1_* names are kept for config compatibility; the rule judges load5/cpu.
    "host.load": {"load1_per_cpu": POSITIVE, "critical_load1_per_cpu": POSITIVE,
                  "critical_hold_runs": POSITIVE_INT},
    "host.tmp": {"max_used_fraction": FRACTION, "critical_used_fraction": FRACTION,
                 "min_free_bytes": POSITIVE_INT, "max_inode_fraction": FRACTION, "path": _str},
    "host.heavy_slot_stale": {"directory": _str, "prefix": _str, "max_age_minutes": POSITIVE},
    "security.secret_text": {"patterns": _regex_list, "allowlist_sha256": _hex_list, "lookback_hours": POSITIVE},
    "money.spend_projection": {"source": _opt_str, "cap": _opt_positive},
    "host.disk_full": {"max_used_fraction": FRACTION, "paths": _str_list},
    "health.failed": {"stale_minutes": POSITIVE},
    # A would-kill is a THRESHOLD-wrong signal, not an incident: our own lane
    # tripped a limit while doing the work it was given.
    "resource.would_kill": {"policies_path": _str, "state_path": _str,
                            "require_task": _bool},
}

TOP = {
    "schema": _enum(SCHEMA_ID),
    "enabled": _bool,
    "dry_run": _bool,
    "cadence_minutes": POSITIVE,
    "min_run_spacing_seconds": POSITIVE,
    "max_alerts_per_run": POSITIVE_INT,
    "state_path": _str,
    "heartbeat_path": _str,
    "state_retention_days": POSITIVE,
    "stores": {"config_dir": _str},
    "panes": {"source": _enum("herdr", "none"), "pane_id_pattern": _regex},
    "escalation": {
        "vp": {"mailbox": _str, "min_severity": _enum(*SEVERITIES)},
        "owner": {"enabled": _bool, "channel": _enum("desk-brief"),
                  "classes": _enum_list(*OWNER_CLASSES), "min_severity": _enum(*SEVERITIES),
                  "approved_by": _opt_str, "max_pages_per_hour": POSITIVE_INT,
                  "confirm_within_minutes": POSITIVE, "confirm_path": _str},
    },
    "rules": {rule: dict(RULE_COMMON, **params) for rule, params in RULE_PARAMS.items()},
}


def _validate(spec, value, prefix):
    if not isinstance(value, dict):
        raise ConfigError(prefix.rstrip(".") or "<root>", "must be an object")
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
        if not owner["approved_by"]:
            raise ConfigError("escalation.owner.approved_by", "required when owner paging is enabled")
        if not owner["classes"]:
            raise ConfigError("escalation.owner.classes",
                              "must name at least one class when owner paging is enabled")
    tmp = config["rules"]["host.tmp"]
    if tmp["critical_used_fraction"] < tmp["max_used_fraction"]:
        raise ConfigError("rules.host.tmp.critical_used_fraction", "must be >= max_used_fraction")
    money = config["rules"]["money.spend_projection"]
    if money["enabled"] and (money["source"] is None or money["cap"] is None):
        raise ConfigError("rules.money.spend_projection.enabled",
                          "cannot be enabled without both source and cap (no metering source exists; VP i19821)")
    for rule_id, rule in config["rules"].items():
        if rule["class"] in OWNER_CLASSES and rule_id not in OWNER_ELIGIBLE_RULES:
            raise ConfigError(f"rules.{rule_id}.class",
                              f"{rule['class']!r} is owner-eligible, but {rule_id} is not an approved "
                              "owner rule (spec section 1); use ops or project")
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
    that needed it rather than a silent pass. Every time-windowed query is
    bounded above by `now`, so evaluating at a past `now` never sees the future."""

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
                    "SELECT * FROM task_events WHERE at<=? ORDER BY at, id", (self.now,))]
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

    def host(self, path):
        def load():
            if self._host_sampler is not None:
                return self._host_sampler(path)
            return sample_host(path)
        return self._get(f"host:{path}", load)

    def name_to_pane(self):
        """Mailbox name -> pane id, from the inbox's own routing history (the
        latest message wins, ties broken by id). Read-only, no heuristics."""
        def load():
            mapping = {}
            for mailbox, pane in self.inbox(
                    "SELECT mailbox, pane_id FROM messages WHERE pane_id IS NOT NULL AND created_at<=? "
                    "ORDER BY created_at, id", (self.now,)):
                mapping[mailbox] = pane
            return mapping
        return self._get("name_to_pane", load)


def sample_host(path):
    stat = os.statvfs(path)
    load1, load5, _ = os.getloadavg()
    return {
        "load1": load1,
        "load5": load5,
        "cpus": os.cpu_count() or 1,
        "used_fraction": ((stat.f_blocks - stat.f_bfree) / stat.f_blocks) if stat.f_blocks else 1.0,
        "free_bytes": stat.f_bavail * stat.f_frsize,
        "inode_used_fraction": ((stat.f_files - stat.f_ffree) / stat.f_files) if stat.f_files else 0.0,
    }


# -- findings -----------------------------------------------------------------

def finding(rule_id, rule, key, summary, threshold, measured, window, evidence,
            source=SOURCE_RULE, once=False):
    """One finding. `threshold`, `measured` and `window` are value+unit strings
    printed on every dry-run line (spec section 4).

    `once=True` marks an immutable event: it is reported once ever, not again
    after a cooldown. A cooldown is for a continuing condition; a historical
    reap is not news four hours later.
    """
    item = {
        "rule": rule_id, "key": str(key), "severity": rule["severity"], "class": rule["class"],
        "source": source, "summary": summary, "threshold": threshold, "measured": measured,
        "window": window, "evidence": evidence,
    }
    if once:
        item["once"] = True
    return item


def _minutes(seconds):
    return round(seconds / 60.0, 1)


# A DECLARED marker, not prose inference: a blocked task whose reason starts
# with this is parked on the owner, not stale. The digest still counts it.
HELD_OWNER_MARKER = "HELD FOR OWNER DECISION"


def _is_held(task):
    return (task.get("block_reason") or "").strip().upper().startswith(HELD_OWNER_MARKER)


def rule_task_blocked_stale(inputs, rule):
    data = inputs.tasks()
    last_event = {}
    for event in data["events"]:
        last_event[event["task_id"]] = max(last_event.get(event["task_id"], 0), event["at"])
    out = []
    held = []
    for task in data["tasks"]:
        if task["status"] != "blocked":
            continue
        last = max(task["updated_at"], last_event.get(task["id"], 0))
        idle = inputs.now - last
        if _is_held(task):
            held.append((task, idle))
            continue
        if idle >= rule["threshold_minutes"] * 60:
            out.append(finding("task.blocked_stale", rule, task["id"],
                               f"task {task['id']} blocked with no state change or event",
                               f"{rule['threshold_minutes']}min", f"{_minutes(idle)}min", "since last activity",
                               {"task": task["id"], "assignees": task["assignees"],
                                "block_reason_set": task["block_reason"] is not None}))
    if held:
        oldest = max(idle for _task, idle in held) / 86400
        out.append(finding("task.blocked_stale", rule, "held-owner",
                           f"{len(held)} task(s) held on owner decisions (oldest {oldest:.1f}d)",
                           f"{rule['threshold_minutes']}min", f"{oldest:.1f}d", "held by declared marker",
                           {"held": [task["id"] for task, _idle in held],
                            "oldest_days": round(oldest, 1)},
                           source="digest"))
        out[-1]["severity"] = "info"
    return out


def rule_task_lease_stale(inputs, rule):
    data = inputs.tasks()
    out = []
    for lease in data["leases"]:
        key = f"{lease['task_id']}@{lease['pane_id']}"
        if lease["expires_at"] <= inputs.now:
            out.append(finding("task.lease_stale", rule, f"expired:{key}",
                               f"lease {key} expired and is not yet reaped",
                               "expires_at<=now", f"expired {_minutes(inputs.now - lease['expires_at'])}min ago",
                               "current leases", {"task": lease["task_id"], "pane": lease["pane_id"]}))
        elif (lease["heartbeat_at"] <= lease["claimed_at"]
              and inputs.now - lease["claimed_at"] >= rule["never_renewed_minutes"] * 60):
            out.append(finding("task.lease_stale", rule, f"unrenewed:{key}",
                               f"lease {key} never renewed",
                               f"{rule['never_renewed_minutes']}min",
                               f"{_minutes(inputs.now - lease['claimed_at'])}min", "since claim",
                               {"task": lease["task_id"], "pane": lease["pane_id"]}))
    # A reaped event is history, not a current state: only fresh reaps are
    # news, and each event is reported once (finding once=True). The wider
    # lookback is for the current leases, not for resurrecting old events.
    fresh = rule.get("fresh_minutes", rule["lookback_minutes"]) * 60
    since = max(inputs.now - rule["lookback_minutes"] * 60,
                inputs.now - fresh)
    for event in data["events"]:
        if event["event"] not in ("requeued", "released") or event["at"] < since:
            continue
        meta = json.loads(event["meta"] or "{}")
        if meta.get("reason") not in ("expired", "pane_gone"):
            continue
        out.append(finding("task.lease_stale", rule, f"reaped:{event['id']}",
                           f"task {event['task_id']} lease reaped ({meta['reason']}) "
                           f"{_minutes(inputs.now - event['at'])}min ago and {event['event']}",
                           "any reap", f"{_minutes(inputs.now - event['at'])}min old",
                           f"{rule['fresh_minutes']}min fresh window",
                           {"task": event["task_id"], "event_id": event["id"], "reason": meta["reason"],
                            "minutes_ago": _minutes(inputs.now - event["at"])},
                           once=True))
    return out


def rule_dunk_skip_loop(inputs, rule):
    since = inputs.now - rule["lookback_hours"] * 3600
    out = []
    for dunk in sorted(inputs.dunks(), key=lambda d: d["id"]):
        rows = inputs.metrics(
            "SELECT id, at, kind FROM events WHERE source=? AND kind IN "
            "('dunk.sent','dunk.skipped') AND at>=? AND at<=? ORDER BY at, id", (dunk["id"], since, inputs.now))
        run = best = 0
        run_start = best_start = best_end = None
        for event_id, at, kind in rows:
            if kind == "dunk.skipped":
                run += 1
                run_start = run_start if run > 1 else (event_id, at)
                if run > best:
                    best, best_start, best_end = run, run_start, (event_id, at)
            else:
                run, run_start = 0, None
        if best >= rule["consecutive_skips"]:
            # Keyed by the run's FIRST skip, which stays fixed while the run grows,
            # so a continuing loop is one incident under its cooldown, not one per skip.
            out.append(finding("dunk.skip_loop", rule, f"{dunk['id']}:{best_start[0]}",
                               f"dunk {dunk['id']} skipped {best} times in a row",
                               f"{rule['consecutive_skips']} consecutive skips", f"{best} consecutive skips",
                               f"{rule['lookback_hours']}h lookback",
                               {"dunk": dunk["id"], "target_pane": dunk.get("target_pane_id"),
                                "current_streak": run,
                                "run_start_hours_ago": round((inputs.now - best_start[1]) / 3600, 1),
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
            out.append(finding("dunk.overdue", rule, dunk["id"], f"dunk {dunk['id']} is running with no scheduled send",
                               "a scheduled send", "none", "current", {"dunk": dunk["id"]}))
        elif interval > 0 and inputs.now - due >= rule["missed_intervals"] * interval:
            out.append(finding("dunk.overdue", rule, dunk["id"],
                               f"dunk {dunk['id']} has not fired past its scheduled send",
                               f"{rule['missed_intervals']} intervals", f"{round((inputs.now - due) / interval, 2)} intervals",
                               f"interval {dunk.get('interval_minutes')}min",
                               {"dunk": dunk["id"], "overdue_minutes": _minutes(inputs.now - due)}))
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
    return {"mailbox": mailbox, "sample_ids": group["ids"][:samples],
            "reasons": dict(sorted(group["reasons"].items())),
            "oldest_hours_ago": round((inputs.now - group["oldest"]) / 3600, 1),
            "newest_hours_ago": round((inputs.now - group["newest"]) / 3600, 1)}


def rule_inbox_undeliverable(inputs, rule):
    rows = inputs.inbox("SELECT id, mailbox, created_at, reason FROM messages "
                        "WHERE state='undeliverable' AND created_at<=? ORDER BY created_at, id", (inputs.now,))
    out = []
    for mailbox, group in sorted(_mailbox_groups(rows).items()):
        age_hours = (inputs.now - group["oldest"]) / 3600
        if group["count"] >= rule["min_messages"] and age_hours >= rule["min_age_hours"]:
            out.append(finding("inbox.undeliverable", rule, mailbox,
                               f"undeliverable message backlog for {mailbox}",
                               f"{rule['min_messages']} msgs older than {rule['min_age_hours']}h",
                               f"{group['count']} msgs, oldest {age_hours:.1f}h", "current backlog",
                               _group_evidence(inputs, mailbox, group)))
    return out


def _queued(inputs, older_than=None):
    limit = inputs.now if older_than is None else older_than
    return inputs.inbox("SELECT id, mailbox, created_at, pane_id FROM messages WHERE state='queued' "
                        "AND created_at<=? ORDER BY created_at, id", (limit,))


def rule_inbox_unread_live(inputs, rule):
    panes = inputs.panes()
    rows = [(i, m, c, None) for i, m, c, pane in _queued(inputs, inputs.now - rule["age_minutes"] * 60)
            if panes is None or (pane in panes)]
    out = []
    for mailbox, group in sorted(_mailbox_groups(rows).items()):
        if group["count"] >= rule["min_messages"]:
            evidence = _group_evidence(inputs, mailbox, group)
            evidence["pane_liveness"] = "unknown (pane source disabled)" if panes is None else "live"
            out.append(finding("inbox.unread_live", rule, mailbox, f"unread mail in {mailbox}",
                               f"{rule['min_messages']} msgs older than {rule['age_minutes']}min",
                               f"{group['count']} msgs", "current", evidence))
    return out


def rule_inbox_unread_dead(inputs, rule):
    panes = inputs.panes()
    rows = [(i, m, c, None) for i, m, c, pane in _queued(inputs) if pane and pane not in panes]
    out = []
    for mailbox, group in sorted(_mailbox_groups(rows).items()):
        if group["count"] >= rule["min_messages"]:
            out.append(finding("inbox.unread_dead", rule, mailbox, f"unread mail queued for dead pane mailbox {mailbox}",
                               f"{rule['min_messages']} msgs", f"{group['count']} msgs", "current",
                               _group_evidence(inputs, mailbox, group)))
    return out


def rule_inbox_ambiguous_sender(inputs, rule):
    pattern = re.compile(inputs.config["panes"]["pane_id_pattern"])
    names = {name: pane for name, pane in inputs.name_to_pane().items() if not pattern.match(name)}
    panes = inputs.panes() if rule["require_live_pane"] else None
    rows = inputs.inbox("SELECT id, sender, created_at FROM messages WHERE sender IS NOT NULL "
                        "AND created_at>=? AND created_at<=? ORDER BY created_at, id",
                        (inputs.now - rule["lookback_hours"] * 3600, inputs.now))
    groups = {}
    for message_id, sender, created_at in rows:
        if pattern.match(sender) or sender not in names:
            continue
        if panes is not None and names[sender] not in panes:
            continue
        groups.setdefault(sender, []).append(message_id)
    out = []
    for sender, ids in sorted(groups.items()):
        out.append(finding("inbox.ambiguous_sender", rule, sender,
                           f"sender {sender!r} is a mailbox name resolving to pane {names[sender]}, not a pane id",
                           "1 msg", f"{len(ids)} msgs", f"{rule['lookback_hours']}h lookback",
                           {"sender": sender, "resolves_to_pane": names[sender],
                            "sample_ids": ids[:rule["sample_ids"]],
                            "pane_liveness": "unchecked" if panes is None else "live"}))
    return out


def _pane_activity(inputs, since):
    """Pane id -> latest activity time from tasks and inbox (read-only)."""
    activity = {}
    for event in inputs.tasks()["events"]:
        if event["actor"] and event["at"] >= since:
            activity[event["actor"]] = max(activity.get(event["actor"], 0), event["at"])
    for sender, at in inputs.inbox("SELECT sender, MAX(created_at) FROM messages WHERE sender IS NOT NULL "
                                   "AND created_at>=? AND created_at<=? GROUP BY sender", (since, inputs.now)):
        activity[sender] = max(activity.get(sender, 0), at)
    for pane, at in inputs.inbox("SELECT pane_id, MAX(read_at) FROM messages WHERE pane_id IS NOT NULL "
                                 "AND read_at>=? AND read_at<=? GROUP BY pane_id", (since, inputs.now)):
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
    since = inputs.now - rule["quiet_minutes"] * 60
    task_actors = {e["actor"] for e in inputs.tasks()["events"] if e["actor"] and e["at"] >= since}
    holding = {p for holders in _holders(inputs, ("in_progress",)).values() for p in holders}
    out = []
    for pane, status in sorted(panes.items()):
        if status == "working" and pane not in holding and pane not in task_actors:
            out.append(finding("lane.working_without_task", rule, pane,
                               f"pane {pane} is working with no task held and no task event",
                               f"{rule['quiet_minutes']}min quiet", "no task activity", "current",
                               {"pane": pane, "agent_status": status}))
    return out


def rule_lane_idle_holding_task(inputs, rule):
    panes = inputs.panes()
    since = inputs.now - rule["quiet_minutes"] * 60
    activity = _pane_activity(inputs, since)
    out = []
    for task_id, holders in sorted(_holders(inputs, tuple(rule["holding_statuses"])).items()):
        for pane in holders:
            status = panes.get(pane)
            if status in rule["idle_statuses"] and pane not in activity:
                out.append(finding("lane.idle_holding_task", rule, f"{task_id}@{pane}",
                                   f"pane {pane} holds task {task_id} while {status} with no activity",
                                   f"{rule['quiet_minutes']}min quiet", f"{status}, no activity", "current",
                                   {"task": task_id, "pane": pane, "agent_status": status}))
    return out


def _heavy_jobs(inputs):
    """Heavy slot holders plus free-model slots currently running.

    Best effort: an unreadable slot directory reports 0 rather than breaking
    the rule. `live`/`busy` come from the same host sample once the recorder
    writes them; until then they are derived from the pane source.
    """
    count = 0
    try:
        for name in os.listdir("/tmp"):
            if name.startswith("poolrad-heavy."):
                holder = os.path.join("/tmp", name, "pid")
                try:
                    pid = int(open(holder, encoding="utf-8").read().strip())
                except (OSError, ValueError):
                    count += 1
                    continue
                try:
                    os.kill(pid, 0)
                    count += 1
                except ProcessLookupError:
                    pass
                except PermissionError:
                    count += 1
    except OSError:
        pass
    return count


def rule_host_load(inputs, rule):
    """A band fitted to our own recorded work (2026-10-22), not to a guess.

    Fit over 461 samples / 8.3 h of host-watch history, on load5/cpu:

        load5/cpu = 0.49 + 0.190*busy + 0.336*heavy     R2 0.52, RMS 0.33

    `live` was 27 in every sample (nobody closed a pane all night), so its
    coefficient is unidentifiable from this data - which is also why the earlier
    lane-reserve shape could never be estimated. The band is set from the
    observed envelope of legitimate work, not from the point prediction: the
    machine reached 2.79/cpu with four lanes and a heavy job, so a threshold of
    2.0/2.6 paged on our own productivity. warning 3.0/cpu, critical 3.5/cpu
    (2 sigma of the fit residual above the observed maximum).
    """
    host = inputs.host("/")
    cpus = max(1, host["cpus"])
    panes = inputs.panes() or {}
    live = len(panes)
    busy = sum(1 for status in panes.values()
               if str(status or "").lower() == "working")
    heavy = _heavy_jobs(inputs)
    warning = rule["load1_per_cpu"]
    critical = rule["critical_load1_per_cpu"]
    # Judge on load5, not load1: a CRITICAL fired at load1 30.5 with busy 1 /
    # heavy 0 and load1 was 18.9 a minute later. A one-minute spike is the wrong
    # measurement; load5 plus must_hold_runs (consecutive spaced runs) is a
    # persisted condition. The config keys still say load1_* for compatibility;
    # the value they hold is per-cpu load5.
    load5 = host.get("load5", host["load1"])
    per_cpu = load5 / cpus
    if per_cpu < warning:
        return []
    level = "critical" if per_cpu >= critical else "warning"
    item = finding("host.load", rule, f"load5:{level}",
                   f"load5 {load5:.1f} = {per_cpu:.2f}/cpu, "
                   f"band {warning:.1f}-{critical:.1f}/cpu (fitted 2026-10-22), "
                   f"{live} live / {busy} busy / {heavy} heavy",
                   f"{warning:.1f}/cpu (load5 {warning * cpus:.1f})",
                   f"{per_cpu:.2f}/cpu (load5 {load5:.1f})",
                   "load5 (5 min)",
                   {"load1": round(host["load1"], 2), "load5": round(load5, 2),
                    "cpus": cpus, "per_cpu": round(per_cpu, 2), "live": live,
                    "busy": busy, "heavy": heavy,
                    "warning_per_cpu": warning,
                    "critical_per_cpu": critical})
    if level == "critical":
        item["severity"] = "critical"
        item["threshold"] = (f"{critical:.1f}/cpu "
                             f"(sustained {rule['critical_hold_runs']} runs)")
    return [item]


def rule_host_tmp(inputs, rule):
    host = inputs.host(rule["path"])
    out = []
    checks = (
        ("used_fraction", host["used_fraction"] >= rule["max_used_fraction"],
         host["used_fraction"], rule["max_used_fraction"]),
        ("free_bytes", host["free_bytes"] <= rule["min_free_bytes"], host["free_bytes"], rule["min_free_bytes"]),
        ("inode_fraction", host["inode_used_fraction"] >= rule["max_inode_fraction"],
         host["inode_used_fraction"], rule["max_inode_fraction"]),
    )
    for name, fired, value, threshold in checks:
        if fired:
            unit = "B" if name == "free_bytes" else ""
            item = finding("host.tmp", rule, f"{rule['path']}:{name}", f"{rule['path']} {name} breaches its limit",
                           f"{threshold}{unit}", f"{round(value, 4)}{unit}", "current sample",
                           {"path": rule["path"], "metric": name})
            if name == "used_fraction" and value >= rule["critical_used_fraction"]:
                # The 90-98% band is VP critical (VP i19821); >=98% is host.disk_full's.
                item["severity"] = "critical"
                item["threshold"] = f"{rule['critical_used_fraction']} (critical)"
            out.append(item)
    return out


def _slot_holder_alive(slot):
    """True if the slot records a live holder pid; None for a legacy slot."""
    try:
        with open(os.path.join(slot, "pid"), encoding="utf-8") as handle:
            raw = handle.read().strip()
    except OSError:
        return None
    try:
        pid = int(raw)
    except ValueError:
        return False
    if pid <= 0:
        return False
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True  # exists, just not ours
    return True


def rule_resource_would_kill(inputs, rule):
    """Report a lane that would trip a resource policy while it is doing its job.

    This is the signal that a threshold is wrong rather than a lane: the limits in
    the policy file are guesses until a lane that is legitimately working crosses
    one. It never signals anything - the sampler is asked for `would_kill` and the
    policy is evaluated with the caller's own dry_run/kill flags recorded, so the
    finding says what WOULD happen, not what happened.

    The rule lives in the info band on purpose: a lane being expensive is a fact
    about our thresholds until proven otherwise, and the owner asked for it in the
    digest, not as a page.
    """
    path = _expand(rule["policies_path"])
    if not os.path.exists(path):
        return []            # no policies configured: there is nothing to cross
    try:
        with open(path, encoding="utf-8") as handle:
            policies = json.load(handle)
    except (OSError, ValueError) as exc:
        raise ConfigError(f"resource.would_kill: cannot read {path}: {exc}")
    tasks = inputs.tasks()
    leased = {}
    for lease in tasks["leases"]:
        task = next((t for t in tasks["tasks"] if t["id"] == lease["task_id"]), None)
        if task and task.get("status") == "in_progress":
            leased[str(lease.get("pane_id"))] = task
    callers, names = [], {}
    for lease in tasks["leases"]:
        if not lease.get("caller_pid"):
            continue
        names[str(lease["caller_pid"])] = lease.get("caller_name") or lease.get("pane_id")
        callers.append({"caller_pid": lease["caller_pid"], "caller_start": lease.get("caller_start"),
                        "caller_name": lease.get("caller_name") or lease.get("pane_id")})
    if not callers:
        return []
    import dunk_load
    result = dunk_load.collect(callers, now=inputs.now, policies=policies,
                               state_path=_expand(rule["state_path"]))
    out = []
    for item in result["would_kill"]:
        agent = item["agent"]
        task = leased.get(str(agent))
        if rule["require_task"] and task is None:
            continue        # an idle lane crossing a limit is not a threshold problem
        doing = (f"while working on {task['id']}: {task['title']}" if task
                 else "with no leased task")
        measured = ", ".join(f"{f['rule']} {f['measured']} > {f['threshold']} {f['window']}"
                             for f in item["findings"] if f.get("measured"))
        out.append(finding("resource.would_kill", rule, f"{agent}:{item['pid']}",
                           f"{agent} would be {('killed' if item['kill'] and not item['dry_run'] else 'flagged')} "
                           f"by resource policy {doing}",
                           "policy limits", measured or "no measured finding",
                           "sample window", {"agent": agent, "pid": item["pid"],
                                             "task": task["id"] if task else None,
                                             "kill": item["kill"], "dry_run": item["dry_run"],
                                             "findings": item["findings"]}))
    return out


def rule_host_heavy_slot_stale(inputs, rule):
    """A poolrad-heavy slot is stale only when its holder is gone.

    poolrad-heavy.sh records the holder pid in the slot. Age alone is not
    staleness: a legitimate >30min compile kept its slot and the watchdog woke
    the VP for a healthy job (t31). Legacy slots with no pid file fall back to
    the age threshold for one migration.
    """
    out = []
    directory = rule["directory"]
    for name in sorted(os.listdir(directory)):
        if not name.startswith(rule["prefix"]):
            continue
        slot = os.path.join(directory, name)
        age = inputs.now - os.stat(slot).st_mtime
        holder = _slot_holder_alive(slot)
        if holder is True:
            continue
        if holder is None and age < rule["max_age_minutes"] * 60:
            continue
        detail = ("holder gone" if holder is False else
                  f"legacy slot, age {_minutes(age)}min")
        out.append(finding("host.heavy_slot_stale", rule, name,
                           f"heavy-job slot {name} is stale",
                           f"holder alive or <{rule['max_age_minutes']}min",
                           detail, "holder liveness",
                           {"slot": slot, "holder": holder}))
    return out


class RuleUnavailable(Exception):
    """A rule that has no input source: reported SKIPPED, never a pass or a page."""


def _text_records(inputs, since):
    """(kind, id, text) for the content the secret scan covers: inbox bodies,
    task titles/bodies and dunk texts (VP i19821). This is a deliberate widening:
    the watchdog otherwise never reads message bodies, and it never emits them."""
    for message_id, body in inputs.inbox("SELECT id, body FROM messages WHERE created_at>=? AND created_at<=? "
                                         "ORDER BY created_at, id", (since, inputs.now)):
        yield "inbox", message_id, body or ""
    for task in inputs.tasks()["tasks"]:
        yield "task", task["id"], f"{task['title']}\n{task['body']}"
    for dunk in sorted(inputs.dunks(), key=lambda d: d["id"]):
        yield "dunk", dunk["id"], dunk.get("text") or ""


def _secret_hits(text, patterns, allow):
    hits = set()
    for text_line in text.splitlines():
        if any(p.search(text_line) for p in patterns):
            digest = hashlib.sha256(text_line.encode("utf-8", "surrogatepass")).hexdigest()
            if digest not in allow:
                hits.add(digest)
    return hits


def rule_security_secret_text(inputs, rule):
    patterns = [re.compile(p) for p in rule["patterns"]]
    allow = set(rule["allowlist_sha256"])
    since = inputs.now - rule["lookback_hours"] * 3600
    first = {(kind, ident, digest) for kind, ident, text in _text_records(inputs, since)
             for digest in _secret_hits(text, patterns, allow)}
    if not first:
        return []
    # Re-verify on a second, independent read before anything can page (spec section 2).
    inputs._cache.pop("tasks", None)
    inputs._cache.pop("dunks", None)
    second = {(kind, ident, digest) for kind, ident, text in _text_records(inputs, since)
              for digest in _secret_hits(text, patterns, allow)}
    out = []
    for kind, ident, digest in sorted(first & second):
        out.append(finding("security.secret_text", rule, f"{kind}:{ident}:{digest[:16]}",
                           f"a credential-shaped line in {kind} {ident}", "1 confirmed match",
                           "1 match (re-verified)", f"{rule['lookback_hours']}h lookback (inbox)",
                           {"kind": kind, "id": ident, "sha256": digest}))
    return out


def rule_money_spend_projection(inputs, rule):
    raise RuleUnavailable("no metering source" if rule["source"] is None or rule["cap"] is None
                          else f"no metering reader implemented for source {rule['source']!r}")


def rule_host_disk_full(inputs, rule):
    out = []
    for path in sorted(rule["paths"]):
        host = inputs.host(path)
        if host["used_fraction"] >= rule["max_used_fraction"]:
            out.append(finding("host.disk_full", rule, f"{path}:{rule['max_used_fraction']}",
                               f"{path} is {round(host['used_fraction'] * 100, 1)}% full",
                               f"{rule['max_used_fraction']}", f"{round(host['used_fraction'], 4)}", "current sample",
                               {"path": path}))
    return out


def rule_health_failed(inputs, rule):
    health = inputs.health()
    out = []
    age = inputs.now - float(health.get("checked_at") or 0)
    if age >= rule["stale_minutes"] * 60:
        out.append(finding("health.failed", rule, "stale", "health.json is not being refreshed",
                           f"{rule['stale_minutes']}min", f"{_minutes(age)}min", "since last check", {}))
    if not health.get("ok", False) or health.get("problems"):
        out.append(finding("health.failed", rule, "problems", "health check reports problems",
                           "ok=true and no problems", f"ok={health.get('ok')}, "
                           f"{len(health.get('problems') or [])} problem(s)", "last check",
                           {"problems": list(health.get("problems") or [])}))
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
    "host.disk_full": rule_host_disk_full,
    "host.heavy_slot_stale": rule_host_heavy_slot_stale,
    "resource.would_kill": rule_resource_would_kill,
    "security.secret_text": rule_security_secret_text,
    "money.spend_projection": rule_money_spend_projection,
    "health.failed": rule_health_failed,
}
assert set(RULES) == set(RULE_PARAMS), "every configurable rule needs exactly one implementation"

# Spec section 1: the only rules that may carry an owner class. Adding one is a
# VP-approved edit to the spec, then to this tuple.
OWNER_ELIGIBLE_RULES = ("host.disk_full", "security.secret_text", "money.spend_projection")
PANE_RULES = ("inbox.unread_dead", "lane.working_without_task", "lane.idle_holding_task")
# Rules whose inputs are append-only history, so they can be re-evaluated at a
# past `now`. Every other rule reads a current snapshot and is not replayable.
REPLAYABLE_RULES = ("dunk.skip_loop", "inbox.ambiguous_sender")


def _sort_key(item):
    return (-SEVERITY_RANK.get(item["severity"], -1), item["rule"], item["key"])


def evaluate(config, now, panes_provider=None, host_sampler=None, only=None):
    """Run every enabled rule (or only those named). A rule that raises is
    SKIPPED with its raw error; the errors feed one self error per run."""
    inputs = Inputs(config, now, panes_provider, host_sampler)
    findings, errors, skipped = [], [], []
    for rule_id in sorted(RULES):
        rule = config["rules"][rule_id]
        if only is not None and rule_id not in only:
            continue
        if rule_id == "money.spend_projection" and (rule["source"] is None or rule["cap"] is None):
            skipped.append({"rule": rule_id, "error": "no metering source"})  # shown even when disabled
            continue
        if not rule["enabled"]:
            continue
        if rule_id in PANE_RULES and config["panes"]["source"] == "none":
            skipped.append({"rule": rule_id, "error": "pane source disabled (panes.source=none)"})
            continue
        try:
            findings.extend(RULES[rule_id](inputs, rule))
        except RuleUnavailable as error:
            skipped.append({"rule": rule_id, "error": str(error)})
        except Exception as error:  # noqa: BLE001 - a failing rule is reported, never raised
            errors.append({"rule": rule_id, "error": f"{type(error).__name__}: {error}"})
    findings.sort(key=_sort_key)
    return {"findings": findings, "errors": errors, "skipped": skipped}


def self_error(problems):
    """Exactly one self finding per run, carrying every raw problem."""
    return {"rule": SELF_ERROR_RULE, "key": "run", "severity": "self", "class": "self", "source": SOURCE_SELF,
            "summary": f"watchdog could not do {len(problems)} thing(s); coverage or owner delivery is affected",
            "threshold": "-", "measured": f"{len(problems)} problem(s)", "window": "this run",
            "evidence": {"problems": problems}}


# -- state --------------------------------------------------------------------

def empty_state():
    return {"alerts": {}, "streaks": {}, "owner_pages": {}, "owner_page_times": []}


def load_state(path):
    """(state, problem). A missing or corrupt file gives a fresh state AND a
    problem, which suppresses owner pages for the run (spec section 3)."""
    if not os.path.exists(path):
        return empty_state(), f"state {path}: missing (first run, or the dedupe store was lost)"
    try:
        with open(path, encoding="utf-8") as handle:
            state = json.load(handle)
        if not isinstance(state, dict):
            raise ValueError("not an object")
    except (OSError, ValueError) as error:
        return empty_state(), f"state {path}: corrupt ({error})"
    for key, value in empty_state().items():
        state.setdefault(key, value)
    return state, None


def save_state(path, state):
    directory = os.path.dirname(path) or "."
    os.makedirs(directory, exist_ok=True)
    temp = f"{path}.tmp.{os.getpid()}"
    with open(temp, "w", encoding="utf-8") as handle:
        json.dump(state, handle, indent=2, sort_keys=True)
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(temp, path)


def dedupe_key(item):
    return f"{item['rule']}|{item['key']}"


def advance_streaks(config, state, findings, now):
    """Consecutive-run counts per finding, as they would be after this run. A
    run counts only if at least min_run_spacing_seconds after the previous
    counted run; an absent finding resets its streak. Pure: returns a new dict."""
    spacing = config["min_run_spacing_seconds"]
    old = state.get("streaks", {})
    new = {}
    for item in findings:
        key = dedupe_key(item)
        prior = old.get(key)
        if prior is None:
            new[key] = {"count": 1, "last_counted": now}
        elif now - prior["last_counted"] >= spacing:
            new[key] = {"count": prior["count"] + 1, "last_counted": now}
        else:
            new[key] = dict(prior)
    return new


# -- planning -----------------------------------------------------------------

def _cooling(config, state, item, now):
    if item["source"] == SOURCE_SELF:
        return False
    entry = state["alerts"].get(dedupe_key(item), {})
    last = entry.get("last_alerted")
    if last is None:
        return False
    if item.get("once"):
        # Immutable event: said once, never re-alerted after a cooldown.
        return True
    return now - last < config["rules"][item["rule"]]["cooldown_minutes"] * 60


def plan(config, findings, state, streaks, now, owner_blocked=None):
    """Decide, per finding, VP notify and owner page with the reason for any
    suppression. Pure: no I/O. `owner_blocked` names a run-wide reason (state
    loss, rule error, transport) that suppresses every owner page."""
    vp_rule = config["escalation"]["vp"]
    owner = config["escalation"]["owner"]
    hour_ago = now - 3600
    recent_owner = [t for t in state.get("owner_page_times", []) if t > hour_ago]
    owner_budget = owner["max_pages_per_hour"] - len(recent_owner)
    vp_count = 0
    decisions = []
    for item in findings:
        is_self = item["source"] == SOURCE_SELF
        owner_eligible = (not is_self and item["class"] in OWNER_CLASSES
                          and item["rule"] in OWNER_ELIGIBLE_RULES)
        decision = {"finding": item, "vp": None, "owner": None,
                    "streak": streaks.get(dedupe_key(item), {}).get("count", 1)}
        # VP
        if not is_self and SEVERITY_RANK[item["severity"]] < SEVERITY_RANK[vp_rule["min_severity"]]:
            decision["vp"] = "below-min-severity"
        elif _cooling(config, state, item, now):
            decision["vp"] = "dedupe"
        elif vp_count >= config["max_alerts_per_run"]:
            decision["vp"] = "rate"
        else:
            decision["vp"] = "no"
            vp_count += 1
        # OWNER
        if owner_eligible:
            rule = config["rules"][item["rule"]]
            pending = state["owner_pages"].get(dedupe_key(item))
            if not owner["enabled"] or item["class"] not in owner["classes"]:
                decision["owner"] = "disabled"
            elif SEVERITY_RANK[item["severity"]] < SEVERITY_RANK[owner["min_severity"]]:
                decision["owner"] = "below-min-severity"
            elif owner_blocked:
                decision["owner"] = owner_blocked
            elif decision["streak"] < rule["must_hold_runs"]:
                decision["owner"] = "hold"
            elif pending is not None and (pending.get("confirmed_at") is None or
                                          now - pending["confirmed_at"] < rule["cooldown_minutes"] * 60):
                # Owner dedupe is its own: a pending page is never re-posted, and a
                # confirmed one cools down from its confirmation, not from a VP alert.
                decision["owner"] = "dedupe"
            elif owner_budget <= 0:
                decision["owner"] = "global-cap"
            else:
                decision["owner"] = "no"
                owner_budget -= 1
        decisions.append(decision)
    return decisions


def recoveries(state, findings):
    """Keys alerted before and absent now: VP recovery lines, plus one owner
    'resolved' line only for pages the owner actually received."""
    current = {dedupe_key(item) for item in findings}
    vp = sorted(key for key, entry in state["alerts"].items()
                if entry.get("last_alerted") is not None and key not in current)
    owner = sorted(key for key, page in state["owner_pages"].items()
                   if page.get("confirmed_at") is not None and key not in current)
    return {"vp": vp, "owner": owner}


# -- output -------------------------------------------------------------------

def _evidence(item):
    return json.dumps(item["evidence"], sort_keys=True, separators=(",", ":"))


def line(prefix, item, suppressed, streak=None, hold=None):
    window = item["window"] if hold is None else f"{item['window']}; held {streak}/{hold} runs"
    return (f"{prefix} rule={item['rule']} class={item['class']} severity={item['severity']} "
            f"source={item['source']} threshold={item['threshold']} measured={item['measured']} "
            f"window={window} dedupe={dedupe_key(item)} suppressed={suppressed} evidence={_evidence(item)}")


def render_lines(config, decisions, skipped, recovered, mode):
    verb = {"vp": "NOTIFY VP" if mode == "live" else "WOULD-NOTIFY VP",
            "owner": "PAGE OWNER" if mode == "live" else "WOULD-PAGE OWNER"}
    out = []
    for decision in decisions:
        item = decision["finding"]
        if decision["owner"] is not None:
            hold = config["rules"][item["rule"]]["must_hold_runs"]
            out.append(line(verb["owner"], item, decision["owner"], decision["streak"], hold))
        out.append(line(verb["vp"], item, decision["vp"]))
    for entry in skipped:
        out.append(f"SKIPPED rule={entry['rule']} error={entry['error']}")
    for key in recovered["vp"]:
        out.append(f"RESOLVED VP dedupe={key}")
    for key in recovered["owner"]:
        out.append(f"RESOLVED OWNER dedupe={key}")
    return out


def digest(title, lines):
    return "\n".join([f"Watchdog {title}: deterministic rules; no model involved."] + lines)


# -- delivery -----------------------------------------------------------------

def inbox_sender(to, subject, body):
    """VP delivery through the daemon's inbox API as the `watchdog` system
    sender. Never starts the daemon: if it is down, the run fails and the
    dead-man check reports the missing heartbeat."""
    from dunk_client import FlockClient
    FlockClient(self_pane_id=None).call("inbox_send", to=to, subject=subject, body=body,
                                        system_sender=SYSTEM_SENDER)


class DeskBriefChannel:
    """OWNER = a VP desk brief, which the Telegram bridge forwards. Delivery is
    confirmed through the bridge's non-secret status file
    {"last_brief_id": int, "updated_at": float} (VP i19821/i19822); the watchdog
    never opens telegram.json, which holds the bot token.

    Posting needs the desk API token, and how the watchdog may hold it is not yet
    decided, so `post` fails toward not paging: nothing is posted, and the
    failure is a VP self error."""

    def __init__(self, confirm_path):
        self.confirm_path = confirm_path

    def post(self, body):
        raise OwnerChannelUnavailable("desk brief post not wired: desk API token handling awaits VP decision")

    def confirmed_through(self):
        with open(self.confirm_path, encoding="utf-8") as handle:
            status = json.load(handle)
        value = status.get("last_brief_id") if isinstance(status, dict) else None
        if value is not None and (isinstance(value, bool) or not isinstance(value, int)):
            raise ValueError(f"last_brief_id is not an integer: {value!r}")
        return value


def _confirm_pending(config, state, owner_channel, now, problems):
    """Mark pending owner pages confirmed when the forwarder has passed them;
    report pages that stay unconfirmed past confirm_within_minutes."""
    pending = {k: p for k, p in state["owner_pages"].items() if p.get("confirmed_at") is None}
    if not pending:
        return
    try:
        through = owner_channel.confirmed_through()
    except Exception as error:  # noqa: BLE001
        problems.append(f"owner confirmation unreadable: {type(error).__name__}: {error}")
        return
    limit = config["escalation"]["owner"]["confirm_within_minutes"] * 60
    for key, page in sorted(pending.items()):
        if through is not None and page["brief_id"] <= through:
            page["confirmed_at"] = now
        elif now - page["posted_at"] > limit:
            problems.append(f"owner page {key} (brief {page['brief_id']}) unconfirmed after "
                            f"{_minutes(now - page['posted_at'])}min; recorded as UNDELIVERED")


def run(config, now, dry_run_flag=False, send=None, owner_channel=None, panes_provider=None,
        host_sampler=None):
    """One watchdog run. Live only when config enabled=true AND dry_run=false AND
    no --dry-run flag. Never raises for rule, state or owner-transport errors."""
    live = config["enabled"] and not config["dry_run"] and not dry_run_flag
    mode = "live" if live else "dry-run"
    owner_channel = owner_channel or DeskBriefChannel(_expand(config["escalation"]["owner"]["confirm_path"]))
    state_path = _expand(config["state_path"])
    state, state_problem = load_state(state_path)
    result = evaluate(config, now, panes_provider, host_sampler)
    problems = [f"rule {e['rule']} SKIPPED: {e['error']}" for e in result["errors"]]
    if state_problem:
        problems.append(state_problem + " - owner pages suppressed this run")
    if live:
        _confirm_pending(config, state, owner_channel, now, problems)
    skipped = result["skipped"] + result["errors"]
    owner_blocked = "state-loss" if state_problem else ("self-error" if result["errors"] else None)
    streaks = advance_streaks(config, state, result["findings"], now)
    decisions = plan(config, result["findings"], state, streaks, now, owner_blocked)
    recovered = recoveries(state, result["findings"])
    report = {
        "mode": mode,
        "why_not_live": None if live else ("--dry-run flag" if dry_run_flag else
                                           "config enabled=false" if not config["enabled"] else "config dry_run=true"),
        "now": now, "findings": result["findings"], "skipped": skipped, "decisions": decisions,
        "recovered": recovered, "problems": problems, "sent": [], "delivery_error": None,
    }
    vp_send = [d for d in decisions if d["vp"] == "no"]
    owner_send = [d for d in decisions if d["owner"] == "no"]
    report["counts"] = {"findings": len(result["findings"]), "vp": len(vp_send), "owner": len(owner_send),
                        "needs_attention": len(vp_send) + sum(d["vp"] == "rate" for d in decisions)}
    if not live:
        report["lines"] = render_lines(config, decisions, skipped, recovered, mode)
        return report

    # Owner first, so a transport failure can ride the VP digest as a self error.
    page_lines = []
    for decision in owner_send:
        item = decision["finding"]
        body = digest("OWNER page", [line("PAGE OWNER", item, "no")])
        try:
            brief_id = owner_channel.post(body)
        except Exception as error:  # noqa: BLE001 - never recorded as sent
            problems.append(f"owner page {dedupe_key(item)} not posted: {type(error).__name__}: {error}; "
                            "recorded as UNDELIVERED")
            decision["owner"] = "transport-error"
            continue
        state["owner_pages"][dedupe_key(item)] = {"brief_id": brief_id, "posted_at": now, "confirmed_at": None}
        state["owner_page_times"].append(now)
        page_lines.append(line("PAGE OWNER (pending confirmation)", item, "no"))
    for key in recovered["owner"]:
        try:
            owner_channel.post(digest("OWNER resolved", [f"RESOLVED {key}"]))
        except Exception as error:  # noqa: BLE001
            problems.append(f"owner resolved line {key} not posted: {type(error).__name__}: {error}")
        state["owner_pages"].pop(key, None)

    vp_lines = []
    if problems:
        vp_lines.append(line("NOTIFY VP", self_error(problems), "no"))
    vp_lines += page_lines
    vp_lines += [line("NOTIFY VP", d["finding"], "no") for d in vp_send]
    suppressed = sum(d["vp"] == "rate" for d in decisions)
    if suppressed:
        vp_lines.append(f"{suppressed} further finding(s) suppressed by max_alerts_per_run; "
                        "run `dunkingsheep watchdog --dry-run` to see all.")
    vp_lines += [f"RESOLVED VP dedupe={key}" for key in recovered["vp"]]
    report["lines"] = vp_lines
    if vp_lines:
        try:
            (send or inbox_sender)(config["escalation"]["vp"]["mailbox"],
                                   f"watchdog: {len(vp_send)} alert(s)" + (" + self-error" if problems else ""),
                                   digest("VP digest", vp_lines))
            report["sent"].append("vp")
        except Exception as error:  # noqa: BLE001 - reported to stderr; never re-sent through the inbox
            report["delivery_error"] = f"{type(error).__name__}: {error}"
            return report

    for decision in vp_send:
        state["alerts"].setdefault(dedupe_key(decision["finding"]), {})["last_alerted"] = now
    for key in recovered["vp"]:
        state["alerts"].pop(key, None)
    for key, entry in state["alerts"].items():
        entry.setdefault("first_seen", now)
    state["streaks"] = streaks
    state["owner_page_times"] = [t for t in state["owner_page_times"] if t > now - 3600]
    retention = config["state_retention_days"] * 86400
    current = {dedupe_key(item) for item in result["findings"]}
    for key in sorted(state["alerts"]):
        entry = state["alerts"][key]
        if key in current:
            entry["last_seen"] = now
        elif now - entry.get("last_seen", now) > retention:
            del state["alerts"][key]
    state["last_run"] = now
    save_state(state_path, state)
    write_heartbeat(_expand(config["heartbeat_path"]), now, config["cadence_minutes"])
    return report


def write_heartbeat(path, now, cadence_minutes):
    """The heartbeat records the effective cadence, so the dead-man can widen
    (never narrow) its limit when the two units disagree (VP i19870). Written
    atomically: the dead-man never sees half a file. Its mtime is still the
    signal; `at` is informational."""
    directory = os.path.dirname(path) or "."
    os.makedirs(directory, exist_ok=True)
    temp = f"{path}.tmp.{os.getpid()}"
    with open(temp, "w", encoding="utf-8") as handle:
        json.dump({"schema": 1, "cadence_minutes": cadence_minutes, "at": now}, handle, sort_keys=True)
    os.utime(temp, (now, now))
    os.replace(temp, path)


def render_text(report):
    head = (f"watchdog {report['mode']}" + (f" ({report['why_not_live']})" if report["why_not_live"] else "")
            + f"  findings {report['counts']['findings']}  vp {report['counts']['vp']}  owner "
            f"{report['counts']['owner']}  needs_attention {report['counts']['needs_attention']}")
    lines = [head] + list(report["lines"])
    for problem in report["problems"]:
        if report["mode"] != "live":
            lines.append(f"SELF source={SOURCE_SELF} {problem}")
    if report["delivery_error"]:
        lines.append(f"DELIVERY FAILED (source={SOURCE_SELF}): {report['delivery_error']}; "
                     "heartbeat not touched, so the dead-man check will report it")
    return "\n".join(lines)


# -- replay -------------------------------------------------------------------

def replay(config, start, end, step_minutes, panes_provider=None, host_sampler=None):
    """Re-evaluate the history-backed rules at each step from `start` to `end`
    over the current stores, applying each rule's cooldown in memory, and count
    the VP notifications (and owner pages) each rule would have sent. Snapshot
    rules are listed as not replayable rather than faked. Reads only."""
    if step_minutes <= 0 or end < start:
        raise ValueError("replay needs step > 0 and end >= start")
    replayable = [r for r in REPLAYABLE_RULES if config["rules"][r]["enabled"]]
    state = empty_state()
    counts = {rule: {"vp": 0, "owner": 0} for rule in replayable}
    errors = []
    t = start
    while t <= end:
        result = evaluate(config, t, panes_provider, host_sampler, only=replayable)
        errors += [dict(e, at=t) for e in result["errors"]]
        streaks = advance_streaks(config, state, result["findings"], t)
        for decision in plan(config, result["findings"], state, streaks, t):
            item = decision["finding"]
            if decision["vp"] == "no":
                counts[item["rule"]]["vp"] += 1
                state["alerts"].setdefault(dedupe_key(item), {})["last_alerted"] = t
            if decision["owner"] == "no":
                counts[item["rule"]]["owner"] += 1
        state["streaks"] = streaks
        t += step_minutes * 60
    not_replayable = sorted(r for r in RULES if config["rules"][r]["enabled"] and r not in replayable)
    return {"from": start, "to": end, "step_minutes": step_minutes, "counts": counts,
            "not_replayable": not_replayable, "errors": errors}
