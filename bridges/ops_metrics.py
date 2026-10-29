"""Private, evidence-labelled operations metrics for the VP desk.

This collector stores observations, never terminal transcripts or message text.
Missing sources remain missing; a failed poll does not become a zero.
"""

from __future__ import annotations

import argparse
import base64
import binascii
import calendar
import hashlib
import json
import math
import os
import re
import sqlite3
import subprocess
from datetime import datetime, timedelta, timezone
from decimal import Decimal, InvalidOperation
from pathlib import Path
from typing import Any
from zoneinfo import ZoneInfo


ROOT = Path(__file__).resolve().parents[1]   # repo root: one repo (FLAT_FLEET.md 9)
DATA = Path(os.environ.get("VP_DATA_DIR", ROOT / "data"))
DB = Path(os.environ.get("VP_OPS_DB", DATA / "operations.sqlite3"))
MESSAGE_DB = DATA / "messages.sqlite3"
DUNK_EVENTS_DB = Path(os.environ.get("DUNK_EVENTS_DB", Path.home() / ".config/dunkingsheep/metrics.db"))
POOLRAD_REPO = Path(os.environ.get("POOLRAD_REPO", ROOT.parent / "poolrad-decomp"))
OPENCODE_DB = Path(os.environ.get("OPENCODE_DB", Path.home() / ".local/share/opencode/opencode.db"))
HOST_WATCH_STATE = Path(os.environ.get("VP_HOST_WATCH_STATE", DATA / "host-watch-state.json"))
PROJECT = "poolrad"
POLL_INTERVAL_S = 60
EXTERNAL_INTERVAL_S = 300
PUBLISHED_REPO = "huntergdavis/poolrad-decomp"

SCHEMA = """
CREATE TABLE IF NOT EXISTS collection_runs (
    id INTEGER PRIMARY KEY,
    observed_at TEXT NOT NULL,
    panes_state TEXT NOT NULL,
    github_state TEXT NOT NULL,
    vp_messages_state TEXT NOT NULL,
    dunk_metrics_state TEXT NOT NULL,
    accounting_state TEXT NOT NULL DEFAULT 'unavailable',
    agent_messages_state TEXT NOT NULL DEFAULT 'unavailable',
    errors_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS runs_observed_at ON collection_runs(observed_at);
CREATE TABLE IF NOT EXISTS lane_assignments (
    project TEXT NOT NULL,
    pane_id TEXT NOT NULL,
    role TEXT NOT NULL CHECK(role IN ('pm','coder','reviewer','other')),
    focus TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY(project,pane_id)
);
CREATE TABLE IF NOT EXISTS pane_samples (
    run_id INTEGER NOT NULL REFERENCES collection_runs(id) ON DELETE CASCADE,
    pane_id TEXT NOT NULL,
    project TEXT NOT NULL,
    agent_kind TEXT,
    agent_status TEXT,
    agent_name TEXT,
    lane_name TEXT,
    role TEXT NOT NULL,
    PRIMARY KEY(run_id,pane_id)
);
CREATE INDEX IF NOT EXISTS pane_samples_pane_run ON pane_samples(pane_id,run_id);
CREATE TABLE IF NOT EXISTS pr_latest (
    project TEXT NOT NULL,
    number INTEGER NOT NULL,
    state TEXT NOT NULL,
    head_oid TEXT,
    merged_at TEXT,
    first_seen_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    PRIMARY KEY(project,number)
);
CREATE TABLE IF NOT EXISTS pr_state_events (
    project TEXT NOT NULL,
    number INTEGER NOT NULL,
    state TEXT NOT NULL,
    head_oid TEXT NOT NULL DEFAULT '',
    observed_at TEXT NOT NULL,
    merged_at TEXT,
    PRIMARY KEY(project,number,state,head_oid)
);
CREATE TABLE IF NOT EXISTS vp_message_counts (
    run_id INTEGER NOT NULL REFERENCES collection_runs(id) ON DELETE CASCADE,
    channel TEXT NOT NULL,
    role TEXT NOT NULL,
    total INTEGER NOT NULL,
    PRIMARY KEY(run_id,channel,role)
);
CREATE TABLE IF NOT EXISTS quota_observations (
    id INTEGER PRIMARY KEY,
    project TEXT NOT NULL,
    provider TEXT NOT NULL,
    cohort TEXT NOT NULL,
    pane_id TEXT NOT NULL,
    observed_at TEXT NOT NULL,
    precision TEXT NOT NULL CHECK(precision IN ('second','minute','reported')),
    weekly_percent_left REAL NOT NULL CHECK(weekly_percent_left BETWEEN 0 AND 100),
    reset_at TEXT NOT NULL,
    evidence_ref TEXT NOT NULL,
    UNIQUE(project,provider,cohort,observed_at,evidence_ref)
);
CREATE INDEX IF NOT EXISTS quota_recent ON quota_observations(project,provider,cohort,observed_at);
CREATE TABLE IF NOT EXISTS subscription_costs (
    id INTEGER PRIMARY KEY,
    project TEXT NOT NULL,
    provider TEXT NOT NULL,
    monthly_usd_cents INTEGER NOT NULL CHECK(monthly_usd_cents >= 0),
    observed_at TEXT NOT NULL,
    evidence_ref TEXT NOT NULL,
    UNIQUE(project,provider,observed_at,evidence_ref)
);
CREATE INDEX IF NOT EXISTS subscription_latest ON subscription_costs(project,provider,id);
CREATE TABLE IF NOT EXISTS goal_observations (
    id INTEGER PRIMARY KEY,
    project TEXT NOT NULL,
    scope TEXT NOT NULL CHECK(scope IN ('local','overall')),
    goal_key TEXT NOT NULL,
    observed_at TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('planned','in_progress','blocked','verified','unknown')),
    value REAL,
    target REAL,
    unit TEXT,
    evidence_ref TEXT NOT NULL,
    note TEXT NOT NULL DEFAULT '',
    UNIQUE(project,goal_key,observed_at,evidence_ref)
);
CREATE INDEX IF NOT EXISTS goals_latest ON goal_observations(project,goal_key,id);
CREATE TABLE IF NOT EXISTS dunk_metrics_snapshots (
    id INTEGER PRIMARY KEY,
    observed_at TEXT NOT NULL,
    generated_at TEXT NOT NULL,
    interval_start TEXT NOT NULL,
    interval_end TEXT NOT NULL,
    schema_version INTEGER NOT NULL,
    payload_json TEXT NOT NULL,
    UNIQUE(generated_at,interval_start,interval_end)
);
CREATE TABLE IF NOT EXISTS accounting_snapshots (
    id INTEGER PRIMARY KEY,
    project TEXT NOT NULL,
    observed_at TEXT NOT NULL,
    commit_oid TEXT NOT NULL,
    strict_full_c INTEGER NOT NULL,
    verified_game_identities INTEGER NOT NULL,
    ceiling INTEGER NOT NULL,
    bound_kind TEXT NOT NULL CHECK(bound_kind IN ('exact','upper_bound')),
    source_signature TEXT NOT NULL,
    evidence_ref TEXT NOT NULL,
    UNIQUE(project,observed_at,commit_oid)
);
CREATE INDEX IF NOT EXISTS accounting_recent ON accounting_snapshots(project,observed_at);
CREATE TABLE IF NOT EXISTS agent_message_events (
    source_event_id INTEGER PRIMARY KEY,
    occurred_at TEXT NOT NULL,
    pane_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK(kind IN ('inbox.sent','message.sent')),
    delivery_source TEXT NOT NULL,
    sender_alias TEXT,
    mailbox_alias TEXT,
    message_bytes INTEGER,
    duration_ms INTEGER,
    attempts INTEGER,
    source_ref TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS agent_message_recent ON agent_message_events(occurred_at,pane_id);
CREATE TABLE IF NOT EXISTS agent_trials (
    trial_id TEXT PRIMARY KEY,
    project TEXT NOT NULL,
    pane_id TEXT NOT NULL,
    agent_type TEXT NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    reasoning_effort TEXT,
    hypothesis TEXT NOT NULL,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    input_tokens INTEGER CHECK(input_tokens IS NULL OR input_tokens >= 0),
    output_tokens INTEGER CHECK(output_tokens IS NULL OR output_tokens >= 0),
    metered_usd REAL CHECK(metered_usd IS NULL OR metered_usd >= 0),
    correction_minutes REAL CHECK(correction_minutes IS NULL OR correction_minutes >= 0),
    host_psi10 REAL CHECK(host_psi10 IS NULL OR host_psi10 >= 0),
    usage_source TEXT,
    evidence_ref TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS agent_trials_recent ON agent_trials(project,started_at);
CREATE TABLE IF NOT EXISTS function_body_events (
    id INTEGER PRIMARY KEY,
    project TEXT NOT NULL,
    function_id TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('authored','accepted','withdrawn')),
    pane_id TEXT NOT NULL,
    trial_id TEXT REFERENCES agent_trials(trial_id),
    source_head TEXT NOT NULL,
    evidence_ref TEXT NOT NULL,
    reviewed_by TEXT,
    occurred_at TEXT NOT NULL,
    UNIQUE(project,function_id,status,source_head)
);
CREATE INDEX IF NOT EXISTS body_events_recent ON function_body_events(project,occurred_at);
CREATE INDEX IF NOT EXISTS body_events_by_function_time ON function_body_events(project,function_id,occurred_at,id);
"""


def utc_now() -> datetime:
    return datetime.now(timezone.utc)


def iso(value: datetime) -> str:
    if value.tzinfo is None:
        raise ValueError("timestamp requires a timezone")
    return value.astimezone(timezone.utc).isoformat(timespec="seconds").replace("+00:00", "Z")


def parse_time(value: str) -> datetime:
    parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    if parsed.tzinfo is None:
        raise ValueError("timestamp requires a timezone offset")
    return parsed.astimezone(timezone.utc)


def connect(path: Path = DB) -> sqlite3.Connection:
    path.parent.mkdir(parents=True, exist_ok=True)
    old_umask = os.umask(0o077)
    try:
        conn = sqlite3.connect(path, timeout=10)
    finally:
        os.umask(old_umask)
    os.chmod(path, 0o600)
    conn.row_factory = sqlite3.Row
    conn.execute("PRAGMA foreign_keys=ON")
    conn.execute("PRAGMA journal_mode=WAL")
    conn.executescript(SCHEMA)
    if "accounting_state" not in {row[1] for row in conn.execute("PRAGMA table_info(collection_runs)")}:
        conn.execute("ALTER TABLE collection_runs ADD COLUMN accounting_state TEXT NOT NULL DEFAULT 'unavailable'")
    if "source_signature" not in {row[1] for row in conn.execute("PRAGMA table_info(accounting_snapshots)")}:
        conn.execute("ALTER TABLE accounting_snapshots ADD COLUMN source_signature TEXT NOT NULL DEFAULT ''")
    if "agent_messages_state" not in {row[1] for row in conn.execute("PRAGMA table_info(collection_runs)")}:
        conn.execute("ALTER TABLE collection_runs ADD COLUMN agent_messages_state TEXT NOT NULL DEFAULT 'unavailable'")
    pane_columns = {row[1] for row in conn.execute("PRAGMA table_info(pane_samples)")}
    for name in ("agent_name", "lane_name"):
        if name not in pane_columns:
            conn.execute(f"ALTER TABLE pane_samples ADD COLUMN {name} TEXT")
    message_columns = {row[1] for row in conn.execute("PRAGMA table_info(agent_message_events)")}
    for name, sql_type in (("sender_alias", "TEXT"), ("mailbox_alias", "TEXT"),
                           ("message_bytes", "INTEGER"), ("duration_ms", "INTEGER"),
                           ("attempts", "INTEGER")):
        if name not in message_columns:
            conn.execute(f"ALTER TABLE agent_message_events ADD COLUMN {name} {sql_type}")
    return conn


def command_json(argv: list[str], *, cwd: Path | None = None) -> Any:
    result = subprocess.run(argv, cwd=cwd, capture_output=True, text=True, timeout=25, check=False)
    if result.returncode:
        raise RuntimeError(f"{argv[0]} {argv[-1]} exited {result.returncode}")
    return json.loads(result.stdout)


def _source_due(conn: sqlite3.Connection, column: str, now: datetime) -> bool:
    if column not in {"github_state", "dunk_metrics_state", "accounting_state"}:
        raise ValueError("unknown source")
    row = conn.execute(
        f"SELECT observed_at FROM collection_runs WHERE {column}='ok' ORDER BY id DESC LIMIT 1"
    ).fetchone()
    return row is None or (now - parse_time(row["observed_at"])).total_seconds() >= EXTERNAL_INTERVAL_S


def _published_json(path: str, commit: str) -> dict:
    payload = command_json(["gh", "api", f"repos/{PUBLISHED_REPO}/contents/{path}?ref={commit}"])
    if not isinstance(payload, dict) or payload.get("encoding") != "base64":
        raise ValueError("published accounting file encoding is unsupported")
    try:
        decoded = base64.b64decode(payload["content"], validate=False)
        result = json.loads(decoded)
    except (KeyError, ValueError, TypeError, binascii.Error) as exc:
        raise ValueError("published accounting file is invalid") from exc
    if not isinstance(result, dict):
        raise ValueError("published accounting file must be an object")
    return result


def published_accounting() -> dict:
    """Read both PM-reviewed files at one published main commit."""
    reference = command_json(["gh", "api", f"repos/{PUBLISHED_REPO}/git/ref/heads/main"])
    target = reference.get("object") if isinstance(reference, dict) else None
    commit = target.get("sha") if isinstance(target, dict) else None
    if not isinstance(commit, str) or not re.fullmatch(r"[0-9a-f]{40}", commit):
        raise ValueError("published main commit is invalid")
    checkpoint = _published_json("docs/re/function_accounting_checkpoint.json", commit)
    census = _published_json("docs/re/function_ceiling_census.json", commit)
    strict = checkpoint.get("strict_full_c_originals")
    identities = checkpoint.get("verified_game_identities")
    ceiling = census.get("count")
    if (checkpoint.get("schema_version") != 1 or
            not all(type(n) is int and n >= 0 for n in (strict, identities)) or
            census.get("status") != "reviewed" or census.get("reviewed_by") != "PM" or
            census.get("bound_kind") not in {"exact", "upper_bound"} or
            census.get("scope") != "original-game-functions-overlay-and-resident" or
            type(ceiling) is not int or ceiling <= 0 or
            not (strict <= identities <= ceiling) or
            not isinstance(checkpoint.get("inputs"), dict) or not checkpoint["inputs"] or
            census.get("source_digests") != checkpoint["inputs"] or
            not isinstance(census.get("evidence_doc"), str) or
            not census["evidence_doc"].startswith("docs/re/")):
        raise ValueError("published accounting and census are not jointly reviewed")
    signature = hashlib.sha256(json.dumps(checkpoint["inputs"], sort_keys=True).encode()).hexdigest()
    return {"commit_oid": commit, "strict_full_c": strict,
            "verified_game_identities": identities, "ceiling": ceiling,
            "bound_kind": census["bound_kind"], "source_signature": signature,
            "evidence_ref": census["evidence_doc"]}


def _vp_counts(path: Path = MESSAGE_DB) -> list[tuple[str, str, int]]:
    if not path.exists():
        raise FileNotFoundError("VP message database is absent")
    with sqlite3.connect(f"file:{path}?mode=ro", uri=True) as conn:
        return [(str(channel), str(role), int(total)) for channel, role, total in conn.execute(
            "SELECT channel,role,COUNT(*) FROM messages GROUP BY channel,role"
        )]


def _new_agent_message_events(conn: sqlite3.Connection, path: Path | None = None) -> list[tuple]:
    """Read delivery metadata only; never copy message text or account details."""
    path = path or DUNK_EVENTS_DB
    if not path.exists():
        raise FileNotFoundError("Dunking Sheep event database is absent")
    last_id = conn.execute("SELECT COALESCE(MAX(source_event_id),0) FROM agent_message_events").fetchone()[0]
    def safe_alias(value: Any) -> str | None:
        if not isinstance(value, str) or not value or "@" in value:
            return None
        return value[:120]
    with sqlite3.connect(f"file:{path}?mode=ro", uri=True) as source:
        return [
            (int(event_id), iso(datetime.fromtimestamp(float(at), timezone.utc)), str(pane_id),
             str(kind), str(delivery_source or "unknown"), safe_alias(sender),
             safe_alias(mailbox), message_bytes, duration_ms, attempts,
             f"dunkingsheep:metrics.db:events:{event_id}")
            for event_id, at, kind, pane_id, delivery_source, sender, mailbox,
                message_bytes, duration_ms, attempts in source.execute(
                "SELECT id,at,kind,pane_id,source,sender,mailbox,bytes,duration_ms,attempts "
                "FROM events WHERE id>? "
                "AND kind IN ('inbox.sent','message.sent') AND pane_id IS NOT NULL ORDER BY id",
                (last_id,),
            )
        ]


def _dunk_timestamp(item: Any, label: str) -> str:
    if isinstance(item, str):
        return iso(parse_time(item))
    if isinstance(item, (int, float)) and not isinstance(item, bool) and math.isfinite(item):
        try:
            return iso(datetime.fromtimestamp(item, timezone.utc))
        except (OverflowError, OSError, ValueError) as exc:
            raise ValueError(f"Dunking Sheep {label} invalid") from exc
    raise ValueError(f"Dunking Sheep {label} required")


def _valid_dunk_metrics(value: Any) -> dict:
    if not isinstance(value, dict) or value.get("schema_version") != 1:
        raise ValueError("Dunking Sheep metrics schema_version 1 required")
    interval = value.get("interval")
    if not isinstance(interval, dict):
        raise ValueError("Dunking Sheep metrics interval required")
    for key in ("start", "end"):
        _dunk_timestamp(interval.get(key), f"interval.{key}")
    _dunk_timestamp(value.get("generated_at"), "generated_at")
    # The upstream contract is aggregate-only. Reject known content keys before persistence.
    forbidden = {"body", "text", "subject", "filename", "email", "token", "secret"}
    def check(item: Any) -> None:
        if isinstance(item, dict):
            if forbidden.intersection(k.lower() for k in item):
                raise ValueError("Dunking Sheep metrics contains content-bearing keys")
            for child in item.values():
                check(child)
        elif isinstance(item, list):
            for child in item:
                check(child)
    check(value)
    return value


def collect(conn: sqlite3.Connection, *, now: datetime | None = None,
            force_external: bool = False) -> dict:
    now = now or utc_now()
    observed_at = iso(now)
    states = {"panes": "skipped", "github": "skipped", "vp_messages": "skipped",
              "agent_messages": "skipped",
              "dunk_metrics": "skipped", "accounting": "skipped"}
    errors: dict[str, str] = {}
    panes: list[dict] = []
    prs: list[dict] = []
    counts: list[tuple[str, str, int]] = []
    agent_message_events: list[tuple] = []
    metrics: dict | None = None
    accounting: dict | None = None
    try:
        result = command_json(["dunkingsheep", "--json", "panes"])
        panes = result["panes"]
        if not isinstance(panes, list):
            raise ValueError("panes must be a list")
        states["panes"] = "ok"
    except (OSError, RuntimeError, ValueError, KeyError, subprocess.TimeoutExpired) as exc:
        states["panes"] = "unavailable"
        errors["panes"] = type(exc).__name__
    try:
        counts = _vp_counts()
        states["vp_messages"] = "ok"
    except (OSError, sqlite3.Error) as exc:
        states["vp_messages"] = "unavailable"
        errors["vp_messages"] = type(exc).__name__
    try:
        agent_message_events = _new_agent_message_events(conn)
        states["agent_messages"] = "ok"
    except (OSError, sqlite3.Error, ValueError, OverflowError) as exc:
        states["agent_messages"] = "unavailable"
        errors["agent_messages"] = type(exc).__name__
    if force_external or _source_due(conn, "github_state", now):
        try:
            prs = command_json(
                ["gh", "pr", "list", "--state", "all", "--limit", "100", "--json",
                 "number,state,headRefOid,mergedAt"],
                cwd=POOLRAD_REPO,
            )
            if not isinstance(prs, list):
                raise ValueError("PR list must be a list")
            states["github"] = "ok"
        except (OSError, RuntimeError, ValueError, subprocess.TimeoutExpired) as exc:
            states["github"] = "unavailable"
            errors["github"] = type(exc).__name__
    else:
        states["github"] = "cached"
    if force_external or _source_due(conn, "dunk_metrics_state", now):
        try:
            metrics = _valid_dunk_metrics(command_json(["dunkingsheep", "--json", "metrics", "--window", "3600"]))
            states["dunk_metrics"] = "ok"
        except (OSError, RuntimeError, ValueError, subprocess.TimeoutExpired) as exc:
            states["dunk_metrics"] = "unavailable"
            errors["dunk_metrics"] = type(exc).__name__
    else:
        states["dunk_metrics"] = "cached"
    if force_external or _source_due(conn, "accounting_state", now):
        try:
            accounting = published_accounting()
            states["accounting"] = "ok"
        except (OSError, RuntimeError, ValueError, KeyError, subprocess.TimeoutExpired) as exc:
            states["accounting"] = "unavailable"
            errors["accounting"] = type(exc).__name__
    else:
        states["accounting"] = "cached"

    with conn:
        run_id = conn.execute(
            "INSERT INTO collection_runs(observed_at,panes_state,github_state,vp_messages_state,dunk_metrics_state,accounting_state,agent_messages_state,errors_json) "
            "VALUES(?,?,?,?,?,?,?,?)",
            (observed_at, states["panes"], states["github"], states["vp_messages"],
             states["dunk_metrics"], states["accounting"], states["agent_messages"],
             json.dumps(errors, sort_keys=True)),
        ).lastrowid
        assignments = {row["pane_id"]: row["role"] for row in conn.execute(
            "SELECT pane_id,role FROM lane_assignments WHERE project=?", (PROJECT,)
        )}
        for pane in panes:
            if pane.get("workspace_label") != "poolrad-decomp":
                continue
            pane_id = str(pane.get("pane_id", ""))
            if not pane_id:
                continue
            role = assignments.get(pane_id, "other")
            agent_name = str(pane.get("tab_label") or "").strip() or None
            lane_name = Path(str(pane.get("cwd") or "")).name or None
            conn.execute(
                "INSERT INTO pane_samples(run_id,pane_id,project,agent_kind,agent_status,agent_name,lane_name,role) VALUES(?,?,?,?,?,?,?,?)",
                (run_id, pane_id, PROJECT, pane.get("agent_type") or pane.get("agent"),
                 pane.get("agent_status"), agent_name, lane_name, role),
            )
        for channel, role, total in counts:
            conn.execute(
                "INSERT INTO vp_message_counts(run_id,channel,role,total) VALUES(?,?,?,?)",
                (run_id, channel, role, total),
            )
        conn.executemany(
            "INSERT OR IGNORE INTO agent_message_events"
            "(source_event_id,occurred_at,pane_id,kind,delivery_source,sender_alias,"
            "mailbox_alias,message_bytes,duration_ms,attempts,source_ref) "
            "VALUES(?,?,?,?,?,?,?,?,?,?,?)", agent_message_events,
        )
        for pr in prs:
            number = int(pr["number"])
            state = str(pr["state"])
            head = pr.get("headRefOid") or ""
            merged_at = pr.get("mergedAt")
            conn.execute(
                "INSERT INTO pr_latest(project,number,state,head_oid,merged_at,first_seen_at,last_seen_at) "
                "VALUES(?,?,?,?,?,?,?) ON CONFLICT(project,number) DO UPDATE SET "
                "state=excluded.state,head_oid=excluded.head_oid,merged_at=excluded.merged_at,last_seen_at=excluded.last_seen_at",
                (PROJECT, number, state, head, merged_at, observed_at, observed_at),
            )
            conn.execute(
                "INSERT OR IGNORE INTO pr_state_events(project,number,state,head_oid,observed_at,merged_at) "
                "VALUES(?,?,?,?,?,?)",
                (PROJECT, number, state, head, observed_at, merged_at),
            )
        if metrics is not None:
            conn.execute(
                "INSERT OR IGNORE INTO dunk_metrics_snapshots"
                "(observed_at,generated_at,interval_start,interval_end,schema_version,payload_json) "
                "VALUES(?,?,?,?,?,?)",
                (observed_at, _dunk_timestamp(metrics["generated_at"], "generated_at"),
                 _dunk_timestamp(metrics["interval"]["start"], "interval.start"),
                 _dunk_timestamp(metrics["interval"]["end"], "interval.end"), 1,
                 json.dumps(metrics, separators=(",", ":"), sort_keys=True)),
            )
        if accounting is not None:
            record_accounting(conn, accounting, observed_at=observed_at)
    return summary(conn, now=now)


def record_accounting(conn: sqlite3.Connection, accounting: dict,
                      *, observed_at: str | None = None) -> None:
    """Store one PM-reviewed accounting snapshot (published main)."""
    when = iso(parse_time(observed_at)) if observed_at else iso(utc_now())
    with conn:
        conn.execute(
            "INSERT OR IGNORE INTO accounting_snapshots"
            "(project,observed_at,commit_oid,strict_full_c,verified_game_identities,ceiling,bound_kind,source_signature,evidence_ref) "
            "VALUES(?,?,?,?,?,?,?,?,?)",
            (PROJECT, when, accounting["commit_oid"], accounting["strict_full_c"],
             accounting["verified_game_identities"], accounting["ceiling"],
             accounting["bound_kind"], accounting["source_signature"], accounting["evidence_ref"]),
        )


def record_quota(
    conn: sqlite3.Connection, *, pane: str, percent_left: float, reset_at: str,
    observed_at: str, evidence: str, provider: str = "codex",
    cohort: str | None = None, precision: str = "second",
) -> None:
    if not 0 <= percent_left <= 100:
        raise ValueError("percent left must be between 0 and 100")
    if precision not in {"second", "minute", "reported"}:
        raise ValueError("invalid timestamp precision")
    provider = provider.strip().lower()
    if not provider:
        raise ValueError("provider is required")
    cohort = cohort or f"poolrad-{provider}"
    with conn:
        conn.execute(
            "INSERT OR IGNORE INTO quota_observations"
            "(project,provider,cohort,pane_id,observed_at,precision,weekly_percent_left,reset_at,evidence_ref) "
            "VALUES(?,?,?,?,?,?,?,?,?)",
            (PROJECT, provider, cohort, pane, iso(parse_time(observed_at)), precision,
             percent_left, iso(parse_time(reset_at)), evidence),
        )


def record_subscription(
    conn: sqlite3.Connection, *, provider: str, monthly_usd: str,
    evidence: str, observed_at: str | None = None,
) -> None:
    try:
        amount = Decimal(monthly_usd)
    except InvalidOperation as exc:
        raise ValueError("monthly USD must be a decimal number") from exc
    if not amount.is_finite() or amount < 0 or amount * 100 != (amount * 100).to_integral_value():
        raise ValueError("monthly USD must be nonnegative with at most two decimal places")
    when = iso(parse_time(observed_at)) if observed_at else iso(utc_now())
    with conn:
        conn.execute(
            "INSERT OR IGNORE INTO subscription_costs"
            "(project,provider,monthly_usd_cents,observed_at,evidence_ref) VALUES(?,?,?,?,?)",
            (PROJECT, provider.strip().lower(), int(amount * 100), when, evidence),
        )


def record_goal(
    conn: sqlite3.Connection, *, key: str, scope: str, status: str, evidence: str,
    value: float | None = None, target: float | None = None, unit: str | None = None,
    note: str = "", observed_at: str | None = None,
) -> None:
    if scope not in {"local", "overall"} or status not in {"planned", "in_progress", "blocked", "verified", "unknown"}:
        raise ValueError("invalid goal scope or status")
    when = iso(parse_time(observed_at)) if observed_at else iso(utc_now())
    with conn:
        conn.execute(
            "INSERT INTO goal_observations"
            "(project,scope,goal_key,observed_at,status,value,target,unit,evidence_ref,note) "
            "VALUES(?,?,?,?,?,?,?,?,?,?)",
            (PROJECT, scope, key, when, status, value, target, unit, evidence, note),
        )


def record_trial(
    conn: sqlite3.Connection, *, trial_id: str, pane: str, agent_type: str,
    provider: str, model: str, hypothesis: str, started_at: str, evidence: str,
    reasoning_effort: str | None = None, ended_at: str | None = None,
    input_tokens: int | None = None, output_tokens: int | None = None,
    metered_usd: float | None = None, correction_minutes: float | None = None,
    host_psi10: float | None = None, usage_source: str | None = None,
) -> None:
    if not all(value and value.strip() for value in
               (trial_id, pane, agent_type, provider, model, hypothesis, evidence)):
        raise ValueError("trial identity, model, hypothesis and evidence are required")
    start = iso(parse_time(started_at))
    end = iso(parse_time(ended_at)) if ended_at else None
    if end and end < start:
        raise ValueError("trial end precedes start")
    for value in (input_tokens, output_tokens, metered_usd, correction_minutes, host_psi10):
        if value is not None and (not math.isfinite(value) or value < 0):
            raise ValueError("trial measures must be finite and nonnegative")
    if (input_tokens is not None and type(input_tokens) is not int or
            output_tokens is not None and type(output_tokens) is not int):
        raise ValueError("token counts must be integers")
    if (input_tokens is not None or output_tokens is not None or metered_usd is not None) and not usage_source:
        raise ValueError("usage source is required for measured tokens or spend")
    identity = (PROJECT, pane, agent_type, provider, model, reasoning_effort,
                hypothesis, start)
    with conn:
        old = conn.execute(
            "SELECT project,pane_id,agent_type,provider,model,reasoning_effort,hypothesis,started_at "
            "FROM agent_trials WHERE trial_id=?", (trial_id,),
        ).fetchone()
        if old and tuple(old) != identity:
            raise ValueError("trial identity cannot change")
        conn.execute(
            "INSERT INTO agent_trials(trial_id,project,pane_id,agent_type,provider,model,reasoning_effort,"
            "hypothesis,started_at,ended_at,input_tokens,output_tokens,metered_usd,correction_minutes,"
            "host_psi10,usage_source,evidence_ref) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?) "
            "ON CONFLICT(trial_id) DO UPDATE SET ended_at=COALESCE(excluded.ended_at,agent_trials.ended_at),"
            "input_tokens=COALESCE(excluded.input_tokens,agent_trials.input_tokens),"
            "output_tokens=COALESCE(excluded.output_tokens,agent_trials.output_tokens),"
            "metered_usd=COALESCE(excluded.metered_usd,agent_trials.metered_usd),"
            "correction_minutes=COALESCE(excluded.correction_minutes,agent_trials.correction_minutes),"
            "host_psi10=COALESCE(excluded.host_psi10,agent_trials.host_psi10),"
            "usage_source=COALESCE(excluded.usage_source,agent_trials.usage_source),"
            "evidence_ref=excluded.evidence_ref",
            (trial_id, *identity, end, input_tokens, output_tokens, metered_usd,
             correction_minutes, host_psi10, usage_source, evidence),
        )


def _unique_authored_trial(conn: sqlite3.Connection, function_id: str,
                           source_head: str) -> str | None:
    trials = [row[0] for row in conn.execute(
        "SELECT DISTINCT trial_id FROM function_body_events "
        "WHERE project=? AND function_id=? AND source_head=? "
        "AND status='authored' AND trial_id IS NOT NULL",
        (PROJECT, function_id, source_head),
    )]
    return trials[0] if len(trials) == 1 else None


def record_body(
    conn: sqlite3.Connection, *, function_id: str, status: str, pane: str,
    source_head: str, evidence: str, occurred_at: str | None = None,
    reviewed_by: str | None = None, trial_id: str | None = None,
) -> None:
    if status not in {"authored", "accepted", "withdrawn"}:
        raise ValueError("invalid C body status")
    if not re.fullmatch(r"[A-Za-z0-9_+.-]{3,64}", function_id):
        raise ValueError("invalid original function ID")
    if not re.fullmatch(r"[0-9a-f]{7,40}", source_head):
        raise ValueError("exact local or published source head required")
    if not pane or not evidence:
        raise ValueError("pane and evidence are required")
    if status == "accepted" and not reviewed_by:
        raise ValueError("accepted C body requires a named reviewer")
    when = iso(parse_time(occurred_at)) if occurred_at else iso(utc_now())
    with conn:
        if status == "accepted" and trial_id is None:
            trial_id = _unique_authored_trial(conn, function_id, source_head)
        conn.execute(
            "INSERT OR IGNORE INTO function_body_events"
            "(project,function_id,status,pane_id,trial_id,source_head,evidence_ref,reviewed_by,occurred_at) "
            "VALUES(?,?,?,?,?,?,?,?,?)",
            (PROJECT, function_id, status, pane, trial_id, source_head,
             evidence, reviewed_by, when),
        )


def experiment_summary(conn: sqlite3.Connection, now: datetime) -> dict:
    start = iso(now - timedelta(hours=24))
    messages = [dict(row) for row in conn.execute(
        "SELECT pane_id, SUM(kind='inbox.sent') AS inbox_deliveries,"
        "SUM(kind='message.sent') AS direct_sends "
        "FROM agent_message_events WHERE occurred_at>=? AND occurred_at<=? "
        "GROUP BY pane_id ORDER BY inbox_deliveries+direct_sends DESC,pane_id",
        (start, iso(now)),
    )]
    for message in messages:
        identity = conn.execute(
            "SELECT agent_name,lane_name FROM pane_samples "
            "WHERE pane_id=? AND agent_name IS NOT NULL ORDER BY run_id DESC LIMIT 1",
            (message["pane_id"],),
        ).fetchone()
        alias = None
        if identity is None:
            alias = conn.execute(
                "SELECT mailbox_alias FROM agent_message_events "
                "WHERE pane_id=? AND kind='inbox.sent' AND mailbox_alias IS NOT NULL "
                "AND mailbox_alias NOT LIKE '%@%' GROUP BY mailbox_alias "
                "ORDER BY COUNT(*) DESC,mailbox_alias LIMIT 1",
                (message["pane_id"],),
            ).fetchone()
        message["agent_name"] = identity["agent_name"] if identity else alias["mailbox_alias"] if alias else None
        message["lane_name"] = identity["lane_name"] if identity else None
    latest = [dict(row) for row in conn.execute(
        "SELECT e.function_id,e.status,e.pane_id,e.trial_id,e.source_head,e.evidence_ref,"
        "e.reviewed_by,e.occurred_at FROM function_body_events e "
        "WHERE e.project=? AND e.id=(SELECT x.id FROM function_body_events x "
        "WHERE x.project=e.project AND x.function_id=e.function_id "
        "ORDER BY x.occurred_at DESC,x.id DESC LIMIT 1) "
        "ORDER BY e.occurred_at DESC,e.function_id", (PROJECT,),
    )]
    trials = [dict(row) for row in conn.execute(
        "SELECT * FROM agent_trials WHERE project=? AND started_at>=? "
        "ORDER BY started_at DESC,trial_id", (PROJECT, iso(now - timedelta(hours=168))),
    )]
    by_trial = {row["trial_id"]: row for row in trials}
    counts: dict[str, dict] = {}
    authored_keys: set[tuple[str, str]] = set()
    for row in conn.execute(
        "SELECT DISTINCT trial_id,function_id FROM function_body_events "
        "WHERE project=? AND status='authored' AND trial_id IS NOT NULL",
        (PROJECT,),
    ):
        key = (row["trial_id"], row["function_id"])
        if key[0] not in by_trial:
            continue
        authored_keys.add(key)
        counts.setdefault(key[0], {"authored_bodies": 0, "accepted_bodies": 0})["authored_bodies"] += 1
    for body in latest:
        if body["status"] == "withdrawn":
            continue
        if body["status"] == "accepted" and body["trial_id"] is None:
            inferred = _unique_authored_trial(conn, body["function_id"], body["source_head"])
            if inferred is not None:
                body["trial_id"] = inferred
                body["trial_attribution"] = "matching_authored_head_backfill"
        trial = by_trial.get(body["trial_id"])
        if trial is None:
            continue
        item = counts.setdefault(trial["trial_id"], {"authored_bodies": 0, "accepted_bodies": 0})
        if body["status"] == "accepted":
            key = (trial["trial_id"], body["function_id"])
            if key not in authored_keys:
                item["authored_bodies"] += 1
                authored_keys.add(key)
            item["accepted_bodies"] += 1
    for trial in trials:
        trial.update(counts.get(trial["trial_id"], {"authored_bodies": 0, "accepted_bodies": 0}))
        end = parse_time(trial["ended_at"]) if trial["ended_at"] else now
        hours = max(0, (end - parse_time(trial["started_at"])).total_seconds() / 3600)
        trial["elapsed_hours"] = round(hours, 3)
        trial["accepted_per_hour"] = round(trial["accepted_bodies"] / hours, 3) if hours > 0 and trial["ended_at"] else None
        trial["accepted_per_usd"] = round(trial["accepted_bodies"] / trial["metered_usd"], 3) if trial["metered_usd"] else None
        trial_messages = conn.execute(
            "SELECT SUM(kind='inbox.sent') AS inbox_deliveries,"
            "SUM(kind='message.sent') AS direct_sends FROM agent_message_events "
            "WHERE pane_id=? AND occurred_at>=? AND occurred_at<=?",
            (trial["pane_id"], trial["started_at"], trial["ended_at"] or iso(now)),
        ).fetchone()
        trial["inbox_deliveries"] = trial_messages["inbox_deliveries"] or 0
        trial["direct_sends"] = trial_messages["direct_sends"] or 0
    groups: dict[tuple, dict] = {}
    for trial in trials:
        model_key = trial["model"].strip().casefold()
        key = (trial["agent_type"].strip().casefold(), model_key,
               (trial["reasoning_effort"] or "unspecified").strip().casefold())
        group = groups.setdefault(key, {
            "agent_type": key[0],
            "model": {"gpt-6-sol": "GPT-6-Sol"}.get(model_key, trial["model"].strip()),
            "reasoning_effort": key[2],
            "trials": 0, "completed_trials": 0, "authored_bodies": 0,
            "accepted_bodies": 0, "completed_accepted_bodies": 0,
            "completed_hours": 0.0,
            "inbox_deliveries": 0, "direct_sends": 0,
            "metered_usd": 0.0, "all_completed_costs_sourced": True,
        })
        group["trials"] += 1
        group["authored_bodies"] += trial["authored_bodies"]
        group["accepted_bodies"] += trial["accepted_bodies"]
        group["inbox_deliveries"] += trial["inbox_deliveries"]
        group["direct_sends"] += trial["direct_sends"]
        if trial["ended_at"]:
            group["completed_trials"] += 1
            group["completed_accepted_bodies"] += trial["accepted_bodies"]
            group["completed_hours"] += trial["elapsed_hours"]
            if trial["metered_usd"] is None:
                group["all_completed_costs_sourced"] = False
            else:
                group["metered_usd"] += trial["metered_usd"]
    efficiency = []
    for group in groups.values():
        hours = group["completed_hours"]
        accepted = group["completed_accepted_bodies"]
        group["completed_hours"] = round(hours, 3)
        group["accepted_per_hour"] = round(accepted / hours, 3) if hours > 0 else None
        group["usd_per_accepted"] = (
            round(group["metered_usd"] / accepted, 3)
            if accepted and group["all_completed_costs_sourced"] else None
        )
        if not group["all_completed_costs_sourced"]:
            group["metered_usd"] = None
        efficiency.append(group)
    efficiency.sort(key=lambda item: (item["agent_type"], item["model"], item["reasoning_effort"]))
    return {"window_start": start, "messages_by_pane": messages,
            "message_definition": "Dunking Sheep inbox.sent plus confirmed message.sent delivery events to each pane; these are prompts/notifications, not model turns or token usage. A direct notification and its inbox item can both appear.",
            "bodies": latest, "authored_bodies": sum(row["status"] in {"authored", "accepted"} for row in latest),
            "accepted_bodies": sum(row["status"] == "accepted" for row in latest),
            "trials": trials, "efficiency_by_type_model": efficiency}


def host_health(now: datetime, *, path: Path | None = None) -> dict | None:
    """Return the latest host-watch sample without exposing its private state file."""
    path = path or HOST_WATCH_STATE
    try:
        state = json.loads(path.read_text())
        sample = state["last_sample"]
        age = max(0, int(now.timestamp() - sample["at"]))
        return {
            "observed_at": iso(datetime.fromtimestamp(sample["at"], timezone.utc)),
            "age_s": age, "stale": age > 180,
            "levels": state.get("levels", {}),
            "tmp_used_fraction": sample["tmp_used_fraction"],
            "tmp_free_gib": round(sample["tmp_free_bytes"] / 1024**3, 2),
            "tmp_inode_used_fraction": sample["tmp_inode_used_fraction"],
            "load1": sample["load1"], "load5": sample["load5"],
            "cpus": sample["cpus"],
            "cpu_psi_some_avg60": sample["cpu_psi_some_avg60"],
            "process_count": sample.get("process_count"),
            "zombie_count": sample.get("zombie_count"),
        }
    except (OSError, ValueError, KeyError, TypeError):
        return None


def opencode_usage(now: datetime, *, path: Path | None = None) -> dict | None:
    """Read aggregate session estimates without copying OpenCode prompts or account data."""
    path = path or OPENCODE_DB
    if not path.is_file():
        return None
    local = now.astimezone(ZoneInfo("America/Los_Angeles"))
    start = local.replace(day=1, hour=0, minute=0, second=0, microsecond=0)
    end = (start.replace(year=start.year + 1, month=1) if start.month == 12
           else start.replace(month=start.month + 1))
    try:
        with sqlite3.connect(path.as_uri() + "?mode=ro", uri=True, timeout=2) as source:
            count, total, month, updated = source.execute(
                "SELECT COUNT(*), COALESCE(SUM(cost),0), "
                "COALESCE(SUM(CASE WHEN time_created>=? AND time_created<? THEN cost ELSE 0 END),0), "
                "MAX(time_updated) FROM session",
                (int(start.timestamp() * 1000), int(end.timestamp() * 1000)),
            ).fetchone()
    except (OSError, sqlite3.Error):
        return None
    return {
        "source": "OpenCode local session cost (same source as opencode stats)",
        "scope": "All local OpenCode sessions; model cost estimate, not billed overage",
        "observed_at": iso(now),
        "billing_month": start.strftime("%Y-%m"),
        "month_estimated_usd": round(month, 4),
        "all_time_estimated_usd": round(total, 4),
        "session_count": count,
        "last_session_at": iso(datetime.fromtimestamp(updated / 1000, timezone.utc)) if updated else None,
    }


def summary(conn: sqlite3.Connection, *, now: datetime | None = None) -> dict:
    now = now or utc_now()
    run = conn.execute("SELECT * FROM collection_runs ORDER BY id DESC LIMIT 1").fetchone()
    source_freshness = {}
    for source in ("panes", "github", "vp_messages", "agent_messages", "dunk_metrics", "accounting"):
        row = conn.execute(
            f"SELECT observed_at FROM collection_runs WHERE {source}_state='ok' ORDER BY id DESC LIMIT 1"
        ).fetchone()
        source_freshness[source] = {
            "last_ok_at": row["observed_at"] if row else None,
            "age_s": max(0, int((now - parse_time(row["observed_at"])).total_seconds())) if row else None,
        }
    staff = {"pm": {}, "coder": {}, "reviewer": {}, "other": {}}
    pane_roster = []
    if run and run["panes_state"] == "ok":
        for row in conn.execute(
            "SELECT role,COALESCE(agent_status,'unknown') status,COUNT(*) n "
            "FROM pane_samples WHERE run_id=? GROUP BY role,status", (run["id"],)
        ):
            staff[row["role"]][row["status"]] = row["n"]
        pane_roster = [dict(row) for row in conn.execute(
            "SELECT pane_id,role,agent_kind,agent_status,agent_name,lane_name FROM pane_samples "
            "WHERE run_id=? AND project=? ORDER BY pane_id", (run["id"], PROJECT),
        )]
    quotas = {"codex": None, "claude": None}
    for provider in quotas:
        latest = conn.execute(
            "SELECT * FROM quota_observations WHERE project=? AND provider=? "
            "ORDER BY observed_at DESC,id DESC LIMIT 1", (PROJECT, provider),
        ).fetchone()
        if latest is None:
            continue
        item = {
            "pane": latest["pane_id"], "provider": provider, "cohort": latest["cohort"],
            "observed_at": latest["observed_at"],
            "age_s": max(0, int((now - parse_time(latest["observed_at"])).total_seconds())),
            "precision": latest["precision"], "weekly_percent_left": latest["weekly_percent_left"],
            "reset_at": latest["reset_at"], "evidence_ref": latest["evidence_ref"],
            "burn_percentage_points_per_hour": None,
        }
        prior = conn.execute(
            "SELECT weekly_percent_left,observed_at FROM quota_observations "
            "WHERE project=? AND provider=? AND cohort=? AND reset_at=? AND observed_at<? "
            "ORDER BY observed_at DESC,id DESC LIMIT 1",
            (PROJECT, provider, latest["cohort"], latest["reset_at"], latest["observed_at"]),
        ).fetchone()
        if prior:
            hours = (parse_time(latest["observed_at"]) - parse_time(prior["observed_at"])).total_seconds() / 3600
            if hours >= 0.5:
                item["burn_percentage_points_per_hour"] = round(
                    (prior["weekly_percent_left"] - latest["weekly_percent_left"]) / hours, 2
                )
        quotas[provider] = item
    subscriptions = [dict(row) for row in conn.execute(
        "SELECT provider,monthly_usd_cents,observed_at,evidence_ref "
        "FROM subscription_costs s WHERE project=? AND id=("
        "SELECT MAX(id) FROM subscription_costs WHERE project=s.project AND provider=s.provider"
        ") ORDER BY provider", (PROJECT,)
    )]
    for item in subscriptions:
        item["monthly_usd"] = item.pop("monthly_usd_cents") / 100
    monthly_fixed_usd = sum(item["monthly_usd"] for item in subscriptions) if subscriptions else None
    calendar_hours = calendar.monthrange(now.year, now.month)[1] * 24
    fixed_hourly_usd = round(monthly_fixed_usd / calendar_hours, 4) if monthly_fixed_usd is not None else None
    quota = max((item for item in quotas.values() if item),
                key=lambda item: item["observed_at"], default=None)
    goals = [dict(row) for row in conn.execute(
        "SELECT scope,goal_key,observed_at,status,value,target,unit,evidence_ref,note "
        "FROM goal_observations g WHERE project=? AND id=("
        "SELECT MAX(id) FROM goal_observations WHERE project=g.project AND goal_key=g.goal_key"
        ") ORDER BY scope,goal_key", (PROJECT,)
    )]
    latest_accounting = conn.execute(
        "SELECT * FROM accounting_snapshots WHERE project=? ORDER BY observed_at DESC,id DESC LIMIT 1",
        (PROJECT,),
    ).fetchone()
    function_progress = None
    completion_rate = None
    if latest_accounting:
        item = dict(latest_accounting)
        ceiling = item["ceiling"]
        upper = item["bound_kind"] == "upper_bound"
        percent = lambda n: (n * 1000 // ceiling) / 10 if upper else round(100 * n / ceiling, 1)
        function_progress = {
            "strict_full_c": item["strict_full_c"],
            "verified_game_identities": item["verified_game_identities"],
            "ceiling": ceiling, "bound_kind": item["bound_kind"],
            "strict_percent": percent(item["strict_full_c"]),
            "identity_percent": percent(item["verified_game_identities"]),
            "observed_at": item["observed_at"],
            "age_s": max(0, int((now - parse_time(item["observed_at"])).total_seconds())),
            "commit_oid": item["commit_oid"], "evidence_ref": item["evidence_ref"],
        }
        end = parse_time(item["observed_at"])
        prior = conn.execute(
            "SELECT * FROM accounting_snapshots WHERE project=? AND observed_at<=? "
            "AND observed_at>=? AND ceiling=? AND bound_kind=? AND source_signature=? "
            "ORDER BY observed_at ASC,id ASC LIMIT 1",
            (PROJECT, iso(end - timedelta(hours=1)), iso(end - timedelta(hours=24)),
             ceiling, item["bound_kind"], item["source_signature"]),
        ).fetchone()
        if prior and item["strict_full_c"] >= prior["strict_full_c"]:
            elapsed = (end - parse_time(prior["observed_at"])).total_seconds() / 3600
            delta = item["strict_full_c"] - prior["strict_full_c"]
            completion_rate = {
                "strict_functions_per_hour": round(delta / elapsed, 4),
                "function_coverage_points_per_hour": round(100 * delta / ceiling / elapsed, 4),
                "delta_strict_functions": delta, "window_hours": round(elapsed, 2),
                "window_start": prior["observed_at"], "window_end": item["observed_at"],
                "ceiling": ceiling, "bound_kind": item["bound_kind"],
            }
    prs = [dict(row) for row in conn.execute(
        "SELECT number,state,merged_at,last_seen_at FROM pr_latest WHERE project=? "
        "ORDER BY number DESC LIMIT 12", (PROJECT,)
    )]
    telemetry = conn.execute(
        "SELECT generated_at,interval_start,interval_end,schema_version,payload_json "
        "FROM dunk_metrics_snapshots ORDER BY id DESC LIMIT 1"
    ).fetchone()
    vp_counts = []
    if run and run["vp_messages_state"] == "ok":
        vp_counts = [dict(row) for row in conn.execute(
            "SELECT channel,role,total FROM vp_message_counts WHERE run_id=? ORDER BY channel,role",
            (run["id"],),
        )]
    return {
        "generated_at": iso(now),
        "project": PROJECT,
        "latest_sample_at": run["observed_at"] if run else None,
        "latest_sample_age_s": max(0, int((now - parse_time(run["observed_at"])).total_seconds())) if run else None,
        "sources": {name: run[f"{name}_state"] for name in ("panes", "github", "vp_messages", "agent_messages", "dunk_metrics", "accounting")} if run else {},
        "source_freshness": source_freshness,
        "staff": staff,
        "pane_roster": pane_roster,
        "quota": quota,
        "quotas": quotas,
        "subscriptions": subscriptions,
        "monthly_fixed_usd": monthly_fixed_usd,
        "fixed_subscription_usd_per_calendar_hour": fixed_hourly_usd,
        "calendar_hours_in_month": calendar_hours,
        "billing_month": now.strftime("%Y-%m"),
        "usage_cost_usd": None,
        "opencode_usage": opencode_usage(now),
        "host_health": host_health(now),
        "goals": goals,
        "function_progress": function_progress,
        "completion_rate": completion_rate,
        "tracked_recent_prs": prs,
        "vp_message_counts": vp_counts,
        "dunk_metrics": {
            "generated_at": telemetry["generated_at"],
            "age_s": max(0, int((now - parse_time(telemetry["generated_at"])).total_seconds())),
            "interval": {"start": telemetry["interval_start"], "end": telemetry["interval_end"]},
            "schema_version": telemetry["schema_version"],
            "data": json.loads(telemetry["payload_json"]),
        } if telemetry else None,
        "experiment_lab": experiment_summary(conn, now),
        "limitations": [
            "Pane state is sampled presence, not verified productive work or token consumption.",
            "PR list tracks recent visible PRs, not a whole-game completion denominator.",
            "Quota burn is percentage points per hour only after two direct readings at least 30 minutes apart with one reset.",
            "Usage-based money and token cost are unknown without a billing source.",
            "Fixed subscription totals are owner-reported and exclude unknown taxes, add-ons, and usage charges.",
            "The hourly price divides fixed monthly subscriptions by calendar hours, not coder hours or metered usage.",
            "Completion speed is change in PM-reviewed strict C credit divided by the actual interval, using the same reviewed function bound; it is not whole-game completion speed.",
            "Agent delivery events count prompts and inbox items delivered to a pane; they do not count model turns, and notification sends can duplicate inbox items.",
            "Function body trial records are append-only local evidence and remain separate from the PM-reviewed published 815 denominator.",
        ],
    }


def history(conn: sqlite3.Connection, *, now: datetime | None = None, hours: int = 24) -> dict:
    """Return timestamped observations; never interpolate missing source samples."""
    if not 1 <= hours <= 168:
        raise ValueError("history window must be 1 to 168 hours")
    now = now or utc_now()
    start = iso(now - timedelta(hours=hours))
    goals = [dict(row) for row in conn.execute(
        "SELECT goal_key,observed_at,value,unit,evidence_ref FROM goal_observations "
        "WHERE project=? AND goal_key IN ('strict_fullc','verified_game_identities') "
        "AND status='verified' AND observed_at>=? ORDER BY observed_at,id",
        (PROJECT, start),
    )]
    coders = [dict(row) for row in conn.execute(
        "SELECT r.observed_at, COUNT(p.pane_id) AS tracked, "
        "SUM(CASE WHEN p.agent_status='working' THEN 1 ELSE 0 END) AS working "
        "FROM collection_runs r JOIN pane_samples p ON p.run_id=r.id "
        "WHERE r.observed_at>=? AND r.panes_state='ok' AND p.project=? AND p.role='coder' "
        "GROUP BY r.id ORDER BY r.observed_at",
        (start, PROJECT),
    )]
    merges = [dict(row) for row in conn.execute(
        "SELECT number,merged_at FROM pr_latest WHERE project=? AND merged_at>=? "
        "ORDER BY merged_at,number", (PROJECT, start),
    )]
    accounting = [dict(row) for row in conn.execute(
        "SELECT observed_at,strict_full_c,verified_game_identities,ceiling,bound_kind,commit_oid "
        "FROM accounting_snapshots WHERE project=? AND observed_at>=? ORDER BY observed_at,id",
        (PROJECT, start),
    )]
    return {
        "generated_at": iso(now), "window_start": start, "window_hours": hours,
        "goal_observations": goals, "accounting_samples": accounting,
        "coder_samples": coders, "pr_merges": merges,
        "limits": {
            "coder_samples": "Pane presence, not verified productive work; gaps mean no successful sample.",
            "pr_merges": "Recent visible PRs only; merges are not deployments or strict C credit.",
            "dora": "Deployment frequency, change failure rate and recovery time unavailable without deployment and incident records.",
        },
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--db", type=Path, default=DB, help="private SQLite database path")
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("init", help="create the private database")
    sub.add_parser("collect", help="sample known local APIs and update the database")
    sub.add_parser("summary", help="print a source-labelled JSON summary")
    quota = sub.add_parser("quota", help="record a directly observed weekly quota reading")
    quota.add_argument("--pane", required=True)
    quota.add_argument("--left", type=float, required=True)
    quota.add_argument("--reset", required=True, help="ISO timestamp with timezone")
    quota.add_argument("--observed-at", required=True, help="ISO timestamp with timezone")
    quota.add_argument("--precision", choices=["second", "minute", "reported"], default="second")
    quota.add_argument("--evidence", required=True)
    quota.add_argument("--provider", default="codex")
    quota.add_argument("--cohort", help="account cohort; defaults to poolrad-PROVIDER")
    subscription = sub.add_parser("subscription", help="record an owner-reported fixed monthly subscription")
    subscription.add_argument("--provider", required=True)
    subscription.add_argument("--monthly-usd", required=True)
    subscription.add_argument("--evidence", required=True)
    subscription.add_argument("--observed-at")
    goal = sub.add_parser("goal", help="record a sourced local or overall goal checkpoint")
    goal.add_argument("--key", required=True)
    goal.add_argument("--scope", choices=["local", "overall"], required=True)
    goal.add_argument("--status", choices=["planned", "in_progress", "blocked", "verified", "unknown"], required=True)
    goal.add_argument("--value", type=float)
    goal.add_argument("--target", type=float)
    goal.add_argument("--unit")
    goal.add_argument("--evidence", required=True)
    goal.add_argument("--note", default="")
    goal.add_argument("--observed-at")
    lane = sub.add_parser("lane", help="update a pane's project role and focus")
    lane.add_argument("--pane", required=True)
    lane.add_argument("--role", choices=["pm", "coder", "reviewer", "other"], required=True)
    lane.add_argument("--focus", required=True)
    trial = sub.add_parser("trial", help="record or finish one sourced model/reasoning trial")
    trial.add_argument("--id", required=True)
    trial.add_argument("--pane", required=True)
    trial.add_argument("--agent-type", required=True)
    trial.add_argument("--provider", required=True)
    trial.add_argument("--model", required=True)
    trial.add_argument("--reasoning-effort")
    trial.add_argument("--hypothesis", required=True)
    trial.add_argument("--started-at", required=True)
    trial.add_argument("--ended-at")
    trial.add_argument("--input-tokens", type=int)
    trial.add_argument("--output-tokens", type=int)
    trial.add_argument("--metered-usd", type=float)
    trial.add_argument("--correction-minutes", type=float)
    trial.add_argument("--host-psi10", type=float)
    trial.add_argument("--usage-source")
    trial.add_argument("--evidence", required=True)
    body = sub.add_parser("body", help="record an authored or reviewed substantive original C body")
    body.add_argument("--function-id", required=True)
    body.add_argument("--status", choices=["authored", "accepted", "withdrawn"], required=True)
    body.add_argument("--pane", required=True)
    body.add_argument("--source-head", required=True)
    body.add_argument("--trial-id")
    body.add_argument("--reviewed-by")
    body.add_argument("--occurred-at")
    body.add_argument("--evidence", required=True)
    args = parser.parse_args()
    conn = connect(args.db)
    try:
        if args.command == "init":
            print(json.dumps({"database": str(args.db), "initialized": True}))
        elif args.command == "collect":
            print(json.dumps(collect(conn), sort_keys=True))
        elif args.command == "summary":
            print(json.dumps(summary(conn), sort_keys=True))
        elif args.command == "quota":
            record_quota(conn, pane=args.pane, percent_left=args.left, reset_at=args.reset,
                         observed_at=args.observed_at, precision=args.precision,
                         evidence=args.evidence, provider=args.provider, cohort=args.cohort)
            print(json.dumps({"recorded": True}))
        elif args.command == "subscription":
            record_subscription(conn, provider=args.provider, monthly_usd=args.monthly_usd,
                                evidence=args.evidence, observed_at=args.observed_at)
            print(json.dumps({"recorded": True}))
        elif args.command == "goal":
            record_goal(conn, key=args.key, scope=args.scope, status=args.status,
                        value=args.value, target=args.target, unit=args.unit,
                        evidence=args.evidence, note=args.note, observed_at=args.observed_at)
            print(json.dumps({"recorded": True}))
        elif args.command == "lane":
            with conn:
                conn.execute(
                    "INSERT INTO lane_assignments(project,pane_id,role,focus,updated_at) VALUES(?,?,?,?,?) "
                    "ON CONFLICT(project,pane_id) DO UPDATE SET role=excluded.role,focus=excluded.focus,updated_at=excluded.updated_at",
                    (PROJECT, args.pane, args.role, args.focus, iso(utc_now())),
                )
            print(json.dumps({"recorded": True}))
        elif args.command == "trial":
            record_trial(conn, trial_id=args.id, pane=args.pane,
                         agent_type=args.agent_type, provider=args.provider,
                         model=args.model, reasoning_effort=args.reasoning_effort,
                         hypothesis=args.hypothesis, started_at=args.started_at,
                         ended_at=args.ended_at, input_tokens=args.input_tokens,
                         output_tokens=args.output_tokens, metered_usd=args.metered_usd,
                         correction_minutes=args.correction_minutes,
                         host_psi10=args.host_psi10, usage_source=args.usage_source,
                         evidence=args.evidence)
            print(json.dumps({"recorded": True}))
        elif args.command == "body":
            record_body(conn, function_id=args.function_id, status=args.status,
                        pane=args.pane, source_head=args.source_head,
                        trial_id=args.trial_id, reviewed_by=args.reviewed_by,
                        occurred_at=args.occurred_at, evidence=args.evidence)
            print(json.dumps({"recorded": True}))
    finally:
        conn.close()


if __name__ == "__main__":
    main()
