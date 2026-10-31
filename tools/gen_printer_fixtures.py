#!/usr/bin/env python3
"""Record the Python CLI's printer output for sample payloads (T43 differential).

For each (printer, payload) the Python `print_*` function runs with stdout captured; the
pair goes to rust/crates/ds-cli/tests/printers_fleet.json, and the Rust `render_*` must
produce the same bytes. Payloads are hand-written to cover every branch of the printer.

    python3 tools/gen_printer_fixtures.py --write | --check
"""
import argparse
import contextlib
import importlib.machinery
import importlib.util
import io
import json
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
OUT = ROOT / "rust/crates/ds-cli/tests/printers_fleet.json"

SAMPLES = {
    "print_poolrad_sync": [
        {},
        {"base": "abc123", "skipped": {"queue_rows": 10, "credited": 3, "already_filed": 2},
         "filed": [{"task": "t9", "id": "OVL_01", "rank": 1.5}, {"id": "OVL_02", "rank": 2}],
         "errors": ["boom", "bang"]},
        {"base": None, "skipped": {}, "filed": [], "errors": []},
    ],
    "print_poolrad_progress": [
        {"credited": 5, "identities": 2, "ceiling": 815, "ceiling_note": "upper bound",
         "composable_bodies": 7, "queue_candidates": 9, "main": "deadbeef", "replaced": 4},
        {"credited": 0, "ceiling": None, "ceiling_note": "n", "replaced": None,
         "replaced_note": "no baseline"},
        {"credited": 1.0, "identities": True, "main": [1, "a'b"], "replaced": 0},
    ],
    "print_review_sync": [
        {},
        {"repos": [{"repo": "a/b", "open": 2}],
         "filed": [{"task": "t1", "repo": "a/b", "pr": 7, "head": "0123456789abcdef0123",
                    "author": "%1"}, {"repo": "a/b", "pr": 8, "head": "abc"}],
         "cancelled": ["t2", "t3"],
         "statuses": [{"repo": "a/b", "head": "abc", "state": "success"}],
         "errors": ["e1"]},
        {"repos": [{"repo": "x/y", "open": 0}], "filed": [], "cancelled": [], "errors": []},
        {"statuses": [{"repo": "x/y", "head": "h", "state": "pending"}]},
    ],
    "print_review_list": [
        {}, {"reviews": []},
        {"reviews": [
            {"task": "t12", "status": "open", "repo": "a/b", "pr": 3, "head": "abc",
             "author": "pi", "reviewer": ["%5", "%6"], "age_s": 3700},
            {"task": "t1234567", "status": "in_progress", "repo": "a/b", "pr": 33,
             "head": "abcdef012345", "reviewer": [], "age_s": 59},
            {"task": "t2", "status": "x", "repo": "r", "pr": 1, "head": "h",
             "author": "averyverylongauthor", "reviewer": ["a-long-reviewer-name"], "age_s": 7200.5},
        ]},
    ],
    "print_whoami": [
        {"role": "worker"},
        {"pane_id": "%3", "role": "owner", "via_token": True, "identity_note": "from tty",
         "binding_note": "pid-bound", "registered": {"role": "worker", "pane_pid": 42,
                                                     "provider": "codex", "by": "owner"}},
        {"pane_id": None, "role": None, "registered": {"role": "web"}},
        {"role": "x", "registered": {}},
    ],
    "print_superuser": [
        {"path": "/p/roles.json", "superuser": False, "role": "worker"},
        {"path": "/p", "superuser": True, "pane_id": "%1", "role": "owner",
         "lanes": {"%2": {"role": "worker", "pane_pid": 9, "provider": "pi", "label": "w1"},
                   "%10": {"role": "reviewer"}, "%1": {"role": "owner", "pane_pid": None}},
         "tokens": [{"label": "web", "role": "web"}, {"label": "t", "role": "timer"}],
         "added": "%2", "removed": "%9", "entry": {"role": "worker"},
         "recovered": True, "name": "vp"},
        {"path": "/p", "lanes": {}, "removed": 0},
    ],
    "print_message": [
        {"message_id": "m3", "target_label": "w1", "pane_id": "%4", "queued": True},
        {"target_label": "w1", "pane_id": "%4", "ok": True},
        {"target_label": "w1", "pane_id": "%4", "ok": False, "message": "no such pane"},
        {"target_label": "x", "pane_id": "%1"},
    ],
    "print_capability": [
        {"pane_id": "%2", "mailbox": "pi-worker", "declared": True, "agent": "pi", "drains": True,
         "send": "push queue", "drain_evidence": "turn events", "expect": "mail arrives next turn"},
        {"pane_id": "%3", "mailbox": "it's", "declared": False, "agent": None, "drains": False,
         "send": "guarded", "expect": "typed", "desk_credential": True, "desk_can_type": False},
        {"pane_id": "%3", "mailbox": None, "declared": 0, "agent": "", "drains": 0,
         "send": "s", "expect": "e", "desk_credential": False, "desk_can_type": True},
    ],
    "print_lanes": [
        {}, {"lanes": []},
        {"lanes": [{"pane_id": "%1", "mailbox": "owner", "declared": True, "alive": True},
                   {"pane_id": "%22", "mailbox": "m" * 50, "alive": False},
                   {"pane_id": "%3", "declared": 0, "alive": None}]},
    ],
    "print_lane": [{"pane_id": "%1", "mailbox": "vp"}, {"pane_id": "%2", "mailbox": "a'b"}],
    "print_inbox_stats": [
        {"mailboxes": []},
        {"mailboxes": [{"destination": "pi-worker-9", "unread": 3, "archived": 10,
                        "undeliverable": 1, "total": 50, "oldest_unread_s": 120},
                       {"destination": "d" * 40, "unread": 0, "total": 2, "oldest_unread_s": 0},
                       {"destination": None, "unread": 1000, "total": 12345678}]},
    ],
    "print_owner_push": [{"id": "o7", "pane_id": "%1"}],
    "print_report_run": [
        {"path": "/r/1.md", "generated_at": "2026-10-31T00:00:00Z",
         "posted": {"message_id": "m9", "transport": "telegram"}, "prose_rejected": ["too long", "bad"]},
        {"path": "p", "generated_at": 1, "posted": {"message_id": "m1"}},
        {"path": "p", "posted": {"error": "no mailbox"}},
        {"path": "p"},
    ],
    "print_owner_reply": [
        {"question": "m1", "reply": {"message_id": "m2"}, "transport": "web", "acked": "m1"},
        {"question": "m1", "transport": None, "acked": 1},
    ],
    "print_clear": [
        {"target_label": "w1", "pane_id": "%4", "flushed": True, "before": {"state": "draft"},
         "after": {"state": "empty"}, "reason": "stuck"},
        {"pane_id": "%4", "flushed": True, "before": {"state": "draft"}, "after": {"state": "empty"}},
        {"target_label": "w1", "pane_id": "%4", "reason": "box already empty"},
        {"pane_id": "%9", "flushed": False},
    ],
    "print_messages": [
        {}, {"queued": [], "recent": []},
        {"queued": [{"message_id": "m1", "waiting_s": 12, "target_label": "worker-one", "source": "owner",
                     "text": "hello\nworld " * 8},
                    {"message_id": "m22", "waiting_s": 3, "target_label": "t" * 40, "source": "s" * 14,
                     "text": None, "blocked_by": "t5"},
                    {"message_id": "m3", "waiting_s": 0, "target_label": "x", "source": "y", "text": "z",
                     "unblock_needed": True}],
         "recent": [{"message_id": "m0", "status": "sent", "target_label": "a", "source": "b", "text": "c"}]},
        {"recent": [{"message_id": "m0", "status": "failed", "target_label": "a", "source": "b",
                     "text": "multi\nline"}]},
    ],
    "print_workspaces": [
        {}, {"workspaces": []},
        {"workspaces": [{"workspace_id": "w1", "label": "main", "tab_count": 2, "agent_status": "idle",
                         "tabs": [{"tab_id": "w1:t1", "label": "work", "pane_count": 3, "agent_status": "busy"},
                                  {"tab_id": "w1:t2", "label": "L" * 40}]},
                        {"workspace_id": "w2", "label": None, "tabs": [{"tab_id": "w2:t1"}]}],
         "self": {"workspace_id": "w1", "tab_id": "w1:t2"}},
        {"workspaces": [{"workspace_id": "w9", "label": "q", "tabs": []}]},
    ],
    "print_created": [
        {"workspace": {"workspace_id": "w3", "label": "ops"}, "tab": {"tab_id": "w3:t1", "label": "t"},
         "pane": {"pane_id": "%12"}, "ran": "claude"},
        {"pane": {"pane_id": "%13"}},
        {"tab": {"tab_id": "w3:t2", "label": "it's"}, "pane": {"pane_id": "%14"}, "ran": ""},
    ],
    "print_poll": [
        {},
        {"status": {"self_pane_id": "%3", "version": "2.71.4", "running": 3, "dunkers": 6},
         "inbox_count": {"unread": 2, "total": 40}, "error": "stale",
         "dunks": {"messages": [{"message_id": "m1", "sender": "owner", "subject": "hi"},
                                {"message_id": "m2", "sender": None}] + [{"message_id": "m%d" % i, "sender": "s"}
                                                                          for i in range(3, 9)],
                   "dunks": [{"id": "d1", "running": True, "interval_minutes": 15.0, "target_label": "steward"},
                             {"id": "d2", "interval_minutes": 5, "target_pane_id": "%9"}]}},
    ],
    "print_turn_end": [
        {"pane_id": "%1", "ended": True},
        {"pane_id": "%1", "ended": False, "dispatch": {"action": "nudge", "task_id": "t4", "delivered": True}},
        {"pane_id": "%1", "ended": True, "dispatch": {"action": "nudge", "task_id": "t4", "outcome": "typing"}},
        {"pane_id": "%1", "ended": True, "dispatch": {"action": "none", "reason": "no work"}},
    ],
    "print_inbox_status": [
        {"message_id": "m1", "state": "queued", "destination": "w1", "pane_id": "%3", "sender": "owner",
         "waiting_s": 12, "body_bytes": 100, "attachments": 1, "attachment_bytes": 2048,
         "created_at": 1790000000, "notify": {"state": "ok", "reason": "typed"}},
        {"message_id": "m2", "state": "read", "reason": "", "sender": None, "created_at": 1782000000.7,
         "read_at": 1782000100, "acked_at": 1782000200, "cancelled_at": 0, "waiting_s": 0,
         "notify": {"state": "failed"}},
        {"message_id": "m3", "notify": {}},
    ],
    "print_retired": [
        {"retired": []},
        {"retired": [{"pane_id": "%1", "retired_at": 1790000000, "reason": "done"},
                     {"pane_id": "%12", "retired_at": 1782000000, "reason": None}, {"pane_id": "%3"}]},
        {"pane_id": "%1", "reason": "idle", "unread_kept": 3},
        {"pane_id": "%1", "reason": "idle", "unread_kept": None},
        {"pane_id": "%1"},
    ],
    "print_report_list": [
        {},
        {"count": 2, "dir": "/r", "reports": [{"at": 1790000000, "size": 1234, "path": "/r/a.md"},
                                              {"at": 1782000000.9, "size": 5, "path": "/r/b.md"}]},
    ],
    "print_owner_conversation": [
        {},
        {"mailbox": "owner", "count": 4, "waiting": [1, 2],
         "messages": [{"id": "m1", "created_at": 1790000000, "transport": "telegram", "sender": "owner",
                       "body": "first line\nsecond", "state": "read"},
                      {"id": "m22", "created_at": 1782000000, "state": "queued", "body": "x" * 120},
                      {"id": "m3", "reply_to": "m1", "transport": "web-desk-long", "sender": "vp-of-agentry",
                       "created_at": 1790000000, "body": "answer\r\nmore"},
                      {"id": "m4", "created_at": None, "body": None, "sender": "s"}][:3]},
    ],
    "print_owner_take": [
        {}, {"messages": []},
        {"messages": [{"id": "o1", "created_at": 1790000000, "subject": "hi", "body": "text"},
                      {"id": "o2", "created_at": 1782000000.5, "body": "b\nc"}]},
    ],
}

STATUS_NOW = 1790003600.0
STATUS_SAMPLES = [
    {},
    {"version": "2.71.4", "code_hash": "abc123", "code_mtime": 1790000000, "started_at": 1789999000.5,
     "code_stale": False, "pid": 42, "dunkers": 6, "running": 3, "queued_messages": 0,
     "herdr_available": False, "herdr_hint": "", "state_file": "/s", "uptime_s": 12.5,
     "self_pane_id": "%3", "message": "ok"},
    {"version": "2.71.4", "code_stale": None, "code_identity_errors": ["no git", "no hash"]},
    {"version": "1", "code_stale": None},
    {"version": "1", "code_hash": "h", "code_stale": True, "code_changed_modules": ["dunk_api", "dunk_core"]},
    {"code_stale": True},
    {"code_stale": 0, "running": 0, "pid": None},
]


def load_cli():
    sys.path.insert(0, str(ROOT))
    loader = importlib.machinery.SourceFileLoader("ds_cli_src", str(ROOT / "dunkingsheep"))
    spec = importlib.util.spec_from_loader("ds_cli_src", loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


TZ = "EST5EDT,M3.2.0,M11.1.0"


def render():
    import os
    import time
    os.environ["TZ"] = TZ
    time.tzset()
    cli = load_cli()
    cases = []
    for name, payloads in SAMPLES.items():
        for payload in payloads:
            buffer = io.StringIO()
            with contextlib.redirect_stdout(buffer):
                getattr(cli, name)(payload)
            cases.append({"printer": name, "input": payload, "output": buffer.getvalue()})
    real_time = time.time
    time.time = lambda: STATUS_NOW
    try:
        for payload in STATUS_SAMPLES:
            buffer = io.StringIO()
            with contextlib.redirect_stdout(buffer):
                cli.print_status(payload)
            cases.append({"printer": "print_status", "input": payload, "output": buffer.getvalue(),
                          "now": STATUS_NOW})
    finally:
        time.time = real_time
    return json.dumps(cases, indent=1, sort_keys=True) + "\n"


def main():
    ap = argparse.ArgumentParser()
    mode = ap.add_mutually_exclusive_group(required=True)
    mode.add_argument("--write", action="store_true")
    mode.add_argument("--check", action="store_true")
    ns = ap.parse_args()
    text = render()
    if ns.write:
        OUT.write_text(text)
        print("wrote", OUT.relative_to(ROOT))
        return 0
    if not OUT.exists() or OUT.read_text() != text:
        print("printers_fleet.json is out of date")
        return 1
    print("printers_fleet.json matches the Python printers")
    return 0


if __name__ == "__main__":
    sys.exit(main())
