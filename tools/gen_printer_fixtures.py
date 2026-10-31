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
}


def load_cli():
    sys.path.insert(0, str(ROOT))
    loader = importlib.machinery.SourceFileLoader("ds_cli_src", str(ROOT / "dunkingsheep"))
    spec = importlib.util.spec_from_loader("ds_cli_src", loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


def render():
    cli = load_cli()
    cases = []
    for name, payloads in SAMPLES.items():
        for payload in payloads:
            buffer = io.StringIO()
            with contextlib.redirect_stdout(buffer):
                getattr(cli, name)(payload)
            cases.append({"printer": name, "input": payload, "output": buffer.getvalue()})
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
