#!/usr/bin/env python3
"""t374: prepare and verify the real automatic A-to-B acceptance run.

The acceptance is not a unit test.  A dedicated, real pi agent and a dedicated,
real Codex agent each:

  1. claim their assigned job A themselves (the agent's own atomic claim);
  2. do A, write a private observable artifact, and complete A;
  3. receive the next job B from the automatic handoff, without a human nudge;
  4. start B, write its artifact, and complete B;
  5. go genuinely quiet afterwards (explicit no-assigned-work, no invented job).

This tool PREPARES the jobs and the launch commands, COLLECTS the live store
records after a run, and VALIDATES the receipt against the t372 clauses.  It
never presses a key, never calls send_text/send_keys, and never synthesises a
turn.  Execution against production code is gated on an approved release file.

Commands
--------
plan      write a run manifest, task definitions, launch commands, and the
          exact store queries.  No live changes.
prepare   create the pane-private A/B tasks for the dedicated panes.  Requires
          --execute and an approval file matching the running daemon.
collect   read the live tasks/metrics/message stores for a run and write
          receipt.json.  Read-only.
validate  check receipt.json against the t372 chain.  Exit 1 on any failure.
cleanup   print (and, with --execute, run) the guarded task-cancel commands.

Example
-------
    python3 tools/t374_acceptance.py plan --out /tmp/t374-run
    # after the CoS/VP has launched the dedicated lanes with the printed commands:
    python3 tools/t374_acceptance.py prepare --run /tmp/t374-run --execute \
        --approval /tmp/t374-approval.json --pi-pane %x --codex-pane %y
    python3 tools/t374_acceptance.py collect --run /tmp/t374-run
    python3 tools/t374_acceptance.py validate --receipt /tmp/t374-run/receipt.json

The approval file is JSON: {"version": "...", "code_hash": "...", "repo_head": "..."}.
`prepare` refuses unless `dunkingsheep status --json` matches version/code and
the checkout named by DUNKINGSHEEP_REPO (default /home/hunter/workspace/dunkingsheep)
matches repo_head.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shlex
import sqlite3
import subprocess
import sys
import time
from pathlib import Path

DUNK_BIN = os.environ.get("DUNKINGSHEEP_BIN") or "dunkingsheep"
CONFIG_DIR = Path(os.environ.get("DUNKINGSHEEP_DIR")
                  or os.path.expanduser("~/.config/dunkingsheep"))
TASKS_DB = CONFIG_DIR / "tasks.db"
METRICS_DB = CONFIG_DIR / "metrics.db"
DISPATCH_STATE = CONFIG_DIR / "dispatch-state.json"
REPO = Path(os.environ.get("DUNKINGSHEEP_REPO")
            or "/home/hunter/workspace/dunkingsheep")
LANES = ("pi", "codex")


def _now():
    return time.time()


def _run(argv, timeout=30):
    return subprocess.run([str(a) for a in argv], capture_output=True,
                          text=True, timeout=timeout)


def _cli(*args, timeout=30):
    return _run([DUNK_BIN, *args], timeout=timeout)


def _json(text):
    text = (text or "").strip()
    if not text:
        return None
    try:
        return json.loads(text)
    except ValueError:
        return None


def _db(path):
    if not path.is_file():
        return None
    return sqlite3.connect(f"file:{path}?mode=ro", uri=True, timeout=5)


def _rows(db, sql, params=()):
    if db is None:
        return []
    db.row_factory = sqlite3.Row
    return [dict(row) for row in db.execute(sql, params).fetchall()]


def _task_specs(run_id, run_dir, lane):
    """The two bounded, harmless jobs.  The agent writes a private artifact."""
    base = Path(run_dir) / lane
    a_path = base / "A.artifact"
    b_path = base / "B.artifact"
    common = (f"T374 acceptance run {run_id}, lane {lane}.\n"
              f"Private artifact directory: {base}\n"
              "This is a harmless acceptance job: write the artifact and complete "
              "the task. Do not touch any other task, file, service, or pane.\n")
    a_body = (common +
              f"Step A: run\n  mkdir -p {shlex.quote(str(base))}\n"
              f"  printf 't374 A {run_id} {lane} %s\\n' \"$(date -Is)\" "
              f">> {shlex.quote(str(a_path))}\n"
              "Then complete THIS task with the id shown in its header:\n"
              "  dunkingsheep task complete <this-task-id>\n"
              "Do NOT claim or start B yourself; the system must hand it to you.\n")
    b_body = (common +
              "This is the NEXT job, handed to you automatically after A. You did "
              "not claim it yourself; if you had to run `task next` to get it, the "
              "acceptance is invalid and you must say so.\n"
              f"Step B: run\n  mkdir -p {shlex.quote(str(base))}\n"
              f"  printf 't374 B {run_id} {lane} %s\\n' \"$(date -Is)\" "
              f">> {shlex.quote(str(b_path))}\n"
              "Then complete THIS task with the id shown in its header:\n"
              "  dunkingsheep task complete <this-task-id>\n"
              "Do not invent or claim a C.\n")
    return {
        "A": {"title": f"T374 A {run_id} {lane}: write A artifact and complete",
              "body": a_body, "artifact": str(a_path),
              "expect_prefix": f"t374 A {run_id} {lane}"},
        "B": {"title": f"T374 B {run_id} {lane}: write B artifact and complete",
              "body": b_body, "artifact": str(b_path),
              "expect_prefix": f"t374 B {run_id} {lane}"},
    }


def _launch_prompt(run_id, lane, wait_seconds=20):
    return (
        f"You are the t374 acceptance lane for {lane} (run {run_id}). "
        f"First run the shell command `sleep {int(wait_seconds)}` once (a one-time "
        "wait for your tasks to be created), then "
        "run exactly once: dunkingsheep task next \"$TMUX_PANE\" . "
        "Read the claimed task with: dunkingsheep task read <id> . "
        "Do exactly what its body says, then complete it with: "
        "dunkingsheep task complete <id> . "
        "Do NOT run task next again; the system must hand you the next job "
        "automatically. Do not ask a human and do not press a key for another "
        "lane. When a [turn-end] task arrives, read it with task read before "
        "acting."
    )


def plan(out_dir, run_id=None, pi_model="", codex_model="", codex_home="", codex_state="", wait_seconds=20, pi_extension=""):
    run_id = run_id or time.strftime("%Y%m%dT%H%M%S")
    out = Path(out_dir).expanduser().resolve()
    out.mkdir(parents=True, exist_ok=True)
    tool = {"path": str(Path(__file__).resolve())}
    try:
        tool["head"] = _run(["git", "-C", str(Path(__file__).resolve().parents[1]),
                             "rev-parse", "HEAD"]).stdout.strip()
    except OSError:
        tool["head"] = None
    try:
        tool["sha256"] = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
    except OSError:
        tool["sha256"] = None
    manifest = {
        "run_id": run_id,
        "created_at": _now(),
        "status": "planned",
        "tool": tool,
        "release": {"version": None, "code_hash": None, "repo_head": None,
                    "approval_file": None},
        "lanes": {},
        "acceptance": [
            "A claimed by the lane's own agent (lease caller_name pi/codex)",
            "A artifact exists and starts with the run/lane token",
            "A completed",
            "next_job_handoff event on B: from_task=A, message_id non-empty",
            "B claimed only after A completed and after the handoff event",
            "B delivery message source=turn_end status=sent (or metrics message.sent)",
            "B turn start after delivery, B artifact exists, B completed",
            "exactly one B claim/handoff/delivery (no duplicate under repeated completion)",
            "zero send_text/send_keys messages to the lane during the run",
            "after B: explicit no_assigned_work / no invented C",
        ],
        "stores": {
            "tasks": str(TASKS_DB),
            "metrics": str(METRICS_DB),
            "messages": "dunkingsheep messages --json (live daemon)",
        },
    }
    for lane in LANES:
        model = pi_model if lane == "pi" else codex_model
        prompt = _launch_prompt(run_id, lane, wait_seconds)
        launch = {
            "pi": ("pi" + (f" --no-extensions -e {shlex.quote(pi_extension)}" if pi_extension else "")
                   + (f" --model {shlex.quote(model)}" if model else "")
                   + f" {shlex.quote(prompt)}"),
            "codex": (f"env CODEX_HOME={shlex.quote(codex_home) if codex_home else '<approved-dedicated-codex-home>'} "
                      f"DUNKINGSHEEP_CODEX_STATE_DIR={shlex.quote(codex_state) if codex_state else '<run>/<lane>/codex-state'} "
                      f"codex --no-daemon --dangerously-bypass-hook-trust "
                      f"{'--model ' + shlex.quote(model) if model else ''} "
                      f"-C /home/hunter/workspace {shlex.quote(prompt)}"),
        }[lane]
        manifest["lanes"][lane] = {
            "pane": None,
            "harness": lane,
            "model": model or None,
            "launch_command": launch,
            "prompt": prompt,
            "wait_seconds": int(wait_seconds),
            "pi_extension": pi_extension or None,
            "tasks": _task_specs(run_id, out, lane),
        }
    (out / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    (out / "approval.example.json").write_text(json.dumps(
        {"version": "FILL-ME", "code_hash": "FILL-ME",
         "repo_head": "FILL-ME", "approved_by": "CoS2/reviewer19"},
        indent=2) + "\n")
    lines = [f"# t374 acceptance run {run_id}", "",
             "## Sequence (unambiguous)",
             f"1. Prepare the dedicated producer bundle; link the Codex auth at runtime "
             f"(never copy it into the artifact).",
             f"2. Launch BOTH dedicated lanes with the commands below; each sleeps "
             f"{int(wait_seconds)}s.",
             "3. Within that window run `t374_acceptance.py prepare --run <this-run> "
             "--execute --approval <approval.json> --pi-pane <pane> --codex-pane <pane>` "
             "to create the pane-private A/B tasks.",
             "4. Do not send any further prompt or key. A is the only initial prompt; "
             "B must arrive from the automatic handoff.", ""]
    for lane in LANES:
        info = manifest["lanes"][lane]
        lines += [f"## {lane}", "",
                  f"Launch this shared-pane command first; it waits {info['wait_seconds']}s. "
                  "Run `prepare` within that window to create the tasks, then do not "
                  "send any further prompt or key.",
                  "```sh", info["launch_command"], "```", ""]
    (out / "LAUNCH.md").write_text("\n".join(lines) + "\n")
    print(json.dumps({"run_id": run_id, "out": str(out),
                      "manifest": str(out / "manifest.json"),
                      "launch": str(out / "LAUNCH.md")}, indent=2))
    return manifest


def _approval(approval_path):
    try:
        approval = json.loads(Path(approval_path).read_text())
    except (OSError, ValueError) as error:
        raise SystemExit(f"approval file unreadable: {error}")
    status = _json(_cli("--json", "--no-start-daemon", "status").stdout) or {}
    if not status.get("version"):
        raise SystemExit("dunkingsheep status did not return a version")
    approved_code = approval.get("code_hash") or approval.get("code")
    running_code = status.get("code_hash") or status.get("code")
    if str(approval.get("version") or "") != str(status.get("version") or ""):
        raise SystemExit(f"refusing: approval version={approval.get('version')!r} "
                         f"does not match running daemon {status.get('version')!r}")
    if str(approved_code or "") != str(running_code or ""):
        raise SystemExit(f"refusing: approval code_hash={approved_code!r} "
                         f"does not match running daemon {running_code!r}")
    try:
        head = _run(["git", "-C", str(REPO), "rev-parse", "HEAD"]).stdout.strip()
    except OSError as error:
        raise SystemExit(f"cannot read repo head: {error}")
    if approval.get("repo_head") and approval["repo_head"] != head:
        raise SystemExit(f"refusing: repo head {head!r} != approved "
                         f"{approval['repo_head']!r}")
    return approval, status


def _create_task(pane, spec):
    result = _cli("--json", "--self-pane", pane, "task", "add",
                  spec["title"], "--scope", pane, "--assign", pane,
                  "-p", "urgent", "-b", spec["body"])
    data = _json(result.stdout)
    if result.returncode != 0 or not isinstance(data, dict) or not data.get("id"):
        raise SystemExit(f"task add failed for {pane}: {result.stderr or result.stdout}")
    return data["id"]


def prepare(run_dir, execute=False, approval=None, pi_pane=None, codex_pane=None):
    manifest = json.loads((Path(run_dir) / "manifest.json").read_text())
    panes = {"pi": pi_pane, "codex": codex_pane}
    if not execute:
        raise SystemExit("prepare is write-capable; pass --execute after the "
                         "approved release and dedicated lanes exist")
    approval_data, _status = _approval(approval)
    manifest["release"] = {"version": approval_data["version"],
                           "code_hash": approval_data.get("code_hash") or approval_data.get("code"),
                           "repo_head": approval_data.get("repo_head"),
                           "approval_file": str(approval)}
    tasks = {}
    for lane in LANES:
        pane = panes.get(lane)
        if not pane:
            raise SystemExit(f"--{lane}-pane is required")
        spec = manifest["lanes"][lane]["tasks"]
        a_id = _create_task(pane, spec["A"])
        b_id = _create_task(pane, spec["B"])
        tasks[lane] = {"pane": pane, "A": a_id, "B": b_id}
        manifest["lanes"][lane]["pane"] = pane
        print(f"{lane}: pane {pane} A={a_id} B={b_id}")
    manifest["status"] = "prepared"
    manifest["prepared_at"] = _now()
    (Path(run_dir) / "tasks.json").write_text(json.dumps(tasks, indent=2) + "\n")
    (Path(run_dir) / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print("tasks created. Launch each dedicated lane with the command in LAUNCH.md; "
          "do not send any further prompt or key.")
    return tasks


def _task_rows(task_id):
    db = _db(TASKS_DB)
    try:
        tasks = _rows(db, "SELECT * FROM tasks WHERE id = ?", (task_id,))
        events = _rows(db, "SELECT id,event,at,actor,status_from,status_to,meta "
                           "FROM task_events WHERE task_id = ? ORDER BY id", (task_id,))
        leases = _rows(db, "SELECT task_id,pane_id,claimed_at,heartbeat_at,expires_at,"
                           "caller_pid,caller_name FROM task_leases WHERE task_id = ?",
                       (task_id,))
    finally:
        if db:
            db.close()
    return {"task": tasks[0] if tasks else None, "events": events, "leases": leases}


def _metrics_for(pane, start, end):
    db = _db(METRICS_DB)
    try:
        return _rows(db, "SELECT id,at,kind,pane_id,source,outcome,reason,duration_ms "
                         "FROM events WHERE pane_id = ? AND at BETWEEN ? AND ? "
                         "ORDER BY id", (pane, start, end))
    finally:
        if db:
            db.close()


def _messages():
    data = _json(_cli("messages", "--json").stdout)
    if not isinstance(data, dict):
        return None
    out = []
    for section in ("queued", "recent", "sent", "messages"):
        out.extend(data.get(section) or [])
    return out


def _artifact(path, prefix):
    p = Path(path)
    if not p.is_file():
        return {"path": str(p), "exists": False}
    raw = p.read_bytes()
    text = raw.decode("utf-8", "replace")
    return {"path": str(p), "exists": True, "bytes": len(raw),
            "sha256": hashlib.sha256(raw).hexdigest(),
            "first_line": text.splitlines()[0] if text else "",
            "expected_prefix": prefix,
            "prefix_ok": text.startswith(prefix)}


def _first(events, name):
    for event in events:
        if event.get("event") == name:
            return event
    return None


def _inbox_row(message_id):
    db = _db(CONFIG_DIR / "inbox.db")
    try:
        rows = _rows(db, "SELECT * FROM messages WHERE id = ?", (message_id,))
    finally:
        if db:
            db.close()
    return rows[0] if rows else None


def _meta(event):
    try:
        value = json.loads((event or {}).get("meta") or "{}")
    except (TypeError, ValueError):
        return {}
    return value if isinstance(value, dict) else {}


def _last(events, name):
    found = [e for e in events if e.get("event") == name]
    return found[-1] if found else None


def _claims(events):
    return [e for e in events if e.get("event") == "claimed"]


def _handoff_history(events):
    """First and latest durable record per nj- message id.

    PR72 appends a next_job_handoff row for every state transition, so the
    number of ROWS is not the number of handoffs.  The message id is.
    """
    history = {}
    for event in events:
        if event.get("event") != "next_job_handoff":
            continue
        meta = _meta(event)
        message_id = meta.get("message_id")
        if not message_id:
            continue
        record = history.setdefault(str(message_id), {
            "message_id": str(message_id), "first_at": event.get("at"),
            "latest": None, "rows": 0})
        record["rows"] += 1
        record["first_at"] = min(float(record["first_at"] or event.get("at") or 0),
                                 float(event.get("at") or 0))
        record["latest"] = dict(meta, at=event.get("at"),
                                event_id=event.get("id"), actor=event.get("actor"))
    return history


def _decisions(events):
    out = []
    for event in events:
        if event.get("event") != "next_job_decision":
            continue
        meta = _meta(event)
        out.append(dict(meta, at=event.get("at"), actor=event.get("actor")))
    return out


def collect(run_dir):
    run = Path(run_dir)
    manifest = json.loads((run / "manifest.json").read_text())
    tasks = json.loads((run / "tasks.json").read_text())
    start = float(manifest.get("prepared_at") or manifest.get("created_at") or 0)
    end = _now() + 5
    messages = _messages()
    receipt = {"run_id": manifest["run_id"], "collected_at": _now(),
               "release": manifest.get("release"), "tool": manifest.get("tool"),
               "lanes": {}}
    for lane in LANES:
        ids = tasks[lane]
        pane = ids["pane"]
        a, b = _task_rows(ids["A"]), _task_rows(ids["B"])
        events_a, events_b = a["events"], b["events"]
        history = _handoff_history(events_b)
        handoff_ids = list(history)
        handoff = (history[handoff_ids[0]]["latest"] if len(handoff_ids) == 1 else None)
        receipt_row = _inbox_row(handoff["message_id"]) if handoff else None
        lane_messages = [m for m in messages if str(m.get("pane_id")) == str(pane)]
        human = None if messages is None else [
            m for m in lane_messages
            if str(m.get("source") or "") in ("send_text", "send_keys", "send_key")
            and float(m.get("created_at") or 0) >= start]
        metrics = _metrics_for(pane, start, end)
        decisions = _decisions(events_a + events_b)
        receipt["lanes"][lane] = {
            "pane": pane, "A": ids["A"], "B": ids["B"],
            "A_state": a, "B_state": b,
            "A_task": a["task"], "B_task": b["task"],
            "A_claims": _claims(events_a), "B_claims": _claims(events_b),
            "A_completed": _last(events_a, "completed"),
            "B_completed": _last(events_b, "completed"),
            "A_completeds": [e for e in events_a if e.get("event") == "completed"],
            "B_completeds": [e for e in events_b if e.get("event") == "completed"],
            "handoffs": [history[mid] for mid in handoff_ids],
            "handoff": handoff,
            "handoff_first_at": history[handoff_ids[0]]["first_at"] if len(handoff_ids) == 1 else None,
            "B_receipt": receipt_row,
            "next_job_decisions": decisions,
            "A_artifact": _artifact(manifest["lanes"][lane]["tasks"]["A"]["artifact"],
                                    manifest["lanes"][lane]["tasks"]["A"]["expect_prefix"]),
            "B_artifact": _artifact(manifest["lanes"][lane]["tasks"]["B"]["artifact"],
                                    manifest["lanes"][lane]["tasks"]["B"]["expect_prefix"]),
            "metrics": metrics,
            "turn_starts": [e for e in metrics if e.get("kind") == "turn.start"],
            "human_sends": human,
        }
    (run / "receipt.json").write_text(json.dumps(receipt, indent=2) + "\n")
    print(f"wrote {run / 'receipt.json'}")
    return receipt


def validate(receipt):
    failures = []
    for lane in LANES:
        row = (receipt.get("lanes") or {}).get(lane)
        if not row:
            failures.append(f"{lane}: missing lane receipt")
            continue
        pane = str(row.get("pane") or "")
        a_id, b_id = str(row.get("A") or ""), str(row.get("B") or "")
        a_claims = row.get("A_claims") or []
        b_claims = row.get("B_claims") or []
        a_completed = row.get("A_completed") or {}
        b_completed = row.get("B_completed") or {}
        a_task = row.get("A_task") or {}
        b_task = row.get("B_task") or {}
        handoffs = row.get("handoffs") or []
        handoff = row.get("handoff") or {}
        receipt_row = row.get("B_receipt") or {}
        a_at = a_completed.get("at")
        b_at = b_completed.get("at")

        if not a_id or not b_id or a_id == b_id:
            failures.append(f"{lane}: missing or duplicate A/B task ids")
        if not a_claims:
            failures.append(f"{lane}: A was not claimed")
        for claim in a_claims:
            if _meta(claim).get("caller_name") != lane:
                failures.append(f"{lane}: A claim caller is "
                                f"{_meta(claim).get('caller_name')!r}, not {lane!r}")
            if claim.get("actor") != pane:
                failures.append(f"{lane}: A claim actor is not the lane pane")
        if len(row.get("A_completeds") or [a_completed]) != 1:
            failures.append(f"{lane}: expected exactly 1 A completion event")
        if not a_completed or not a_at:
            failures.append(f"{lane}: A was not completed")
        else:
            if a_completed.get("actor") != pane:
                failures.append(f"{lane}: A completion actor is not the lane pane")
            if a_task.get("completed_by") not in (None, pane):
                failures.append(f"{lane}: A task completed_by is "
                                f"{a_task.get('completed_by')!r}, not {pane!r}")
        if not row.get("A_artifact", {}).get("prefix_ok"):
            failures.append(f"{lane}: A artifact missing or wrong prefix")

        if len(handoffs) != 1:
            failures.append(f"{lane}: expected exactly 1 handoff message id, got "
                            f"{len(handoffs)}")
        if not handoff:
            failures.append(f"{lane}: no next_job_handoff record")
        else:
            message_id = str(handoff.get("message_id") or "")
            if not message_id.startswith("nj-"):
                failures.append(f"{lane}: handoff message_id is not an nj- id")
            if handoff.get("from_task") != a_id:
                failures.append(f"{lane}: handoff from_task != A")
            caller = handoff.get("caller") or {}
            if not isinstance(caller, dict) or caller.get("name") != lane:
                failures.append(f"{lane}: handoff completion caller is "
                                f"{caller.get('name') if isinstance(caller, dict) else caller!r}, "
                                f"not {lane!r}")
            completion = handoff.get("completion_at")
            if a_at is None or completion is None or abs(float(completion) - float(a_at)) > 0.5:
                failures.append(f"{lane}: handoff completion_at does not match A completion")
            if handoff.get("state") != "started":
                failures.append(f"{lane}: handoff state is {handoff.get('state')!r}, not 'started'")
            if int(handoff.get("attempt") or 0) < 1:
                failures.append(f"{lane}: handoff has no transport attempt")
            if not handoff.get("transport_id"):
                failures.append(f"{lane}: handoff has no transport_id")
            if handoff.get("delivered_at") is None:
                failures.append(f"{lane}: handoff has no delivered_at")
            turn_started = handoff.get("turn_started_at")
            if not handoff.get("turn_id") or turn_started is None:
                failures.append(f"{lane}: handoff has no producer turn binding")
            else:
                if handoff.get("delivered_at") is not None and \
                        float(turn_started) <= float(handoff["delivered_at"]):
                    failures.append(f"{lane}: B turn started before its delivery")
                if b_at is not None and float(turn_started) > float(b_at):
                    failures.append(f"{lane}: only turn start is AFTER B completion")
            turn_caller = handoff.get("turn_caller") or {}
            if not isinstance(turn_caller, dict) or turn_caller.get("name") != lane:
                failures.append(f"{lane}: B turn caller is not {lane!r}")

        if len(b_claims) != 1:
            failures.append(f"{lane}: expected exactly 1 B claim, got {len(b_claims)}")
        else:
            b_claim = b_claims[0]
            b_meta = _meta(b_claim)
            if b_meta.get("caller_name") != lane:
                failures.append(f"{lane}: B claim caller is "
                                f"{b_meta.get('caller_name')!r}, not {lane!r}")
            if b_claim.get("actor") != pane:
                failures.append(f"{lane}: B claim actor is not the lane pane")
            if a_at is not None and float(b_claim.get("at") or 0) < float(a_at) - 0.5:
                failures.append(f"{lane}: B was claimed before A completed")
            if float(b_claim.get("at") or 0) > float(a_at or 0) + 1.5:
                failures.append(f"{lane}: B claim is not in A's completion transaction")

        if not receipt_row:
            failures.append(f"{lane}: no inbox receipt row for the handoff")
        else:
            if str(receipt_row.get("id")) != str(handoff.get("message_id")):
                failures.append(f"{lane}: receipt id is not the handoff message id")
            if str(receipt_row.get("pane_id")) != pane:
                failures.append(f"{lane}: receipt pane is not the lane pane")
            if str(receipt_row.get("sender")) != "next-job":
                failures.append(f"{lane}: receipt sender is not 'next-job'")
            if receipt_row.get("notify_state") != "sent":
                failures.append(f"{lane}: receipt notify_state is "
                                f"{receipt_row.get('notify_state')!r}, not 'sent'")
            if receipt_row.get("notify_at") is None:
                failures.append(f"{lane}: receipt has no notify_at")
            if not receipt_row.get("notify_transport_id"):
                failures.append(f"{lane}: receipt has no notify_transport_id")
            if int(receipt_row.get("notify_attempt") or 0) < 1:
                failures.append(f"{lane}: receipt has no notify_attempt")
            if str(receipt_row.get("notify_transport_id")) != str(handoff.get("transport_id")):
                failures.append(f"{lane}: receipt transport_id != handoff transport_id")
            if int(receipt_row.get("notify_attempt") or 0) != int(handoff.get("attempt") or 0):
                failures.append(f"{lane}: receipt attempt != handoff attempt")
            if handoff.get("delivered_at") is not None and receipt_row.get("notify_at") is not None:
                if abs(float(receipt_row["notify_at"]) - float(handoff["delivered_at"])) > 2.0:
                    failures.append(f"{lane}: receipt notify_at is not the handoff delivery time")
            if not receipt_row.get("read_at") and not receipt_row.get("acked_at"):
                failures.append(f"{lane}: receipt was never read/acked by the lane")
            body = str(receipt_row.get("body") or "")
            if b_id and b_id not in body:
                failures.append(f"{lane}: receipt body does not name B")
            if handoff.get("message_id") and str(handoff["message_id"]) not in body:
                failures.append(f"{lane}: receipt body does not echo the handoff id")

        if not row.get("B_artifact", {}).get("prefix_ok"):
            failures.append(f"{lane}: B artifact missing or wrong prefix")
        if len(row.get("B_completeds") or [b_completed]) != 1:
            failures.append(f"{lane}: expected exactly 1 B completion event")
        if not b_completed or not b_at:
            failures.append(f"{lane}: B was not completed")
        else:
            if b_completed.get("actor") != pane:
                failures.append(f"{lane}: B completion actor is not the lane pane")
            if b_task.get("completed_by") not in (None, pane):
                failures.append(f"{lane}: B task completed_by is "
                                f"{b_task.get('completed_by')!r}, not {pane!r}")
            if handoff.get("turn_started_at") is not None and \
                    float(b_at) <= float(handoff["turn_started_at"]):
                failures.append(f"{lane}: B completed before its own turn started")
            if receipt_row.get("notify_at") is not None and \
                    float(b_at) < float(receipt_row["notify_at"]):
                failures.append(f"{lane}: B completed before delivery")
        if not any(abs(float(e.get("at") or 0) - float(handoff.get("turn_started_at") or -999))
                   <= 2.0 for e in row.get("turn_starts") or []):
            failures.append(f"{lane}: no matching turn.start metric for the bound B turn")

        if row.get("human_sends") is None:
            failures.append(f"{lane}: human-send evidence unavailable (messages --json failed)")
        elif row.get("human_sends"):
            failures.append(f"{lane}: {len(row['human_sends'])} send_text/send_keys "
                            "message(s) to the lane; the acceptance requires no human nudge")

        decisions = row.get("next_job_decisions") or []
        no_work = [d for d in decisions
                   if d.get("reason") == "no_assigned_work"
                   and (b_at is None or float(d.get("at") or 0) >= float(b_at) - 0.1)]
        if not no_work:
            failures.append(f"{lane}: no explicit no_assigned_work after B")
    return failures


def cleanup(run_dir, execute=False):
    run = Path(run_dir)
    tasks = json.loads((run / "tasks.json").read_text())
    commands = []
    for lane in LANES:
        row = tasks[lane]
        for key in ("A", "B"):
            commands.append(["dunkingsheep", "--self-pane", row["pane"],
                             "task", "cancel", row[key], "--reason",
                             f"t374 acceptance cleanup {run.name}"])
    for cmd in commands:
        print(shlex.join(cmd))
    if execute:
        for cmd in commands:
            result = _run(cmd)
            print(result.stdout.strip() or result.stderr.strip())
    return commands


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("plan")
    p.add_argument("--out", required=True)
    p.add_argument("--run-id")
    p.add_argument("--pi-model", default="")
    p.add_argument("--codex-model", default="")
    p.add_argument("--codex-home", default="")
    p.add_argument("--codex-state", default="")
    p.add_argument("--wait-seconds", type=int, default=20)
    p.add_argument("--pi-extension", default="")
    p = sub.add_parser("prepare")
    p.add_argument("--run", required=True)
    p.add_argument("--execute", action="store_true")
    p.add_argument("--approval", required=True)
    p.add_argument("--pi-pane", required=True)
    p.add_argument("--codex-pane", required=True)
    p = sub.add_parser("collect")
    p.add_argument("--run", required=True)
    p = sub.add_parser("validate")
    p.add_argument("--receipt", required=True)
    p = sub.add_parser("cleanup")
    p.add_argument("--run", required=True)
    p.add_argument("--execute", action="store_true")
    args = parser.parse_args(argv)
    if args.command == "plan":
        plan(args.out, args.run_id, args.pi_model, args.codex_model,
             args.codex_home, args.codex_state, args.wait_seconds,
             args.pi_extension)
    elif args.command == "prepare":
        prepare(args.run, args.execute, args.approval, args.pi_pane, args.codex_pane)
    elif args.command == "collect":
        collect(args.run)
    elif args.command == "validate":
        receipt = json.loads(Path(args.receipt).read_text())
        failures = validate(receipt)
        if failures:
            print("FAIL")
            for failure in failures:
                print(" -", failure)
            return 1
        print("PASS: real A->B chain satisfies t372 clauses")
    elif args.command == "cleanup":
        cleanup(args.run, args.execute)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
