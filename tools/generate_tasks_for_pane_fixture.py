#!/usr/bin/env python3
"""Generate a Tasks.for_pane oracle using only isolated temporary SQLite stores.

Run from any directory. No daemon, project queue, credentials or production data.
Input row maps name real SQLite columns; JSON array columns remain SQLite TEXT.
"""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import sys
import tempfile
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
import dunk_allocator  # noqa: E402
import dunk_tasks  # noqa: E402

NOW = 2_000_000_000.0
PANE = "%4"


def task(number, *, scope="global", priority="low", status="open", **fields):
    return dict(id=f"t{number}", scope=scope, title=f"fixture task {number}",
                created_at=1000.0, updated_at=1000.0,
                priority=priority, status=status, **fields)


def lease(task_id, pane_id, *, live=True, **fields):
    return dict(task_id=task_id, pane_id=pane_id, claimed_at=1000.0,
                heartbeat_at=1001.0, expires_at=1_000_000_000_000.0 if live else 1.0,
                **fields)


def offer(task_id, owners, state="ready"):
    return dict(task_id=task_id, pool="fixture", role="worker", requires="[]",
                excluded="[]", owners=json.dumps(owners), offered_by="%1", state=state)


def cases():
    yield dict(name="empty_default_limit", pane_id=PANE, limit=None, tables={"tasks": []})
    combined = [
        task(1, priority="urgent"),
        task(2, scope=PANE, priority="high"),
        task(3, scope=PANE, priority="medium"),
        task(4),
        task(5, scope="%9", priority="urgent"),
        task(6, status="done"),
        task(7, scope=PANE, status="cancelled"),
    ]
    for name, limit in [("combined_limit_before_pane_filter", 2),
                        ("outer_zero_still_computes_next", 0),
                        ("outer_negative_python_slice", -1)]:
        yield dict(name=name, pane_id=PANE, limit=limit, tables={"tasks": combined})
    yield dict(name="priority_created_id_order_and_own_pick", pane_id=PANE, limit=20,
               tables={"tasks": [task(11, scope=PANE, priority="urgent"),
                                 task(10, priority="high"),
                                 task(12, scope=PANE, priority="urgent", body="later id"),
                                 task(9, scope=PANE, priority="urgent", body="lexical t9") ]})
    rows = [task(100, status="done"), task(101, status="done", landed=0),
            task(102, status="cancelled"), task(103, assignees='["%9"]'),
            task(20, priority="high", assignees='["%4"]',
                 body="Full metadata: Δ\nsecond line", created_by="%1", attempts=3),
            task(21, status="in_progress", assignees='["%4"]'),
            task(22, status="in_progress", assignees='["%4"]'),
            task(23, status="in_progress", assignees='["%4","%9"]', multi_assignee=1),
            task(24, assignees='["%4"]'),
            task(25, status="in_progress", assignees='["%9"]', multi_assignee=1),
            task(26, assignees='["%9"]'),
            task(27, depends_on='["t103"]'), task(28, depends_on='["t999"]'),
            task(29, depends_on='["t101"]'), task(30, depends_on='["t102"]'),
            task(31, status="blocked", depends_on='["t100"]', block_reason="await owner"),
            task(32, status="blocked", depends_on='["t102"]'),
            task(33, assignees='["%4"]'), task(34, assignees='["%4"]'),
            task(35, assignees='["%4"]'), task(36, priority="unknown"),
            task(37, kind="review", author="%4", repo="fixture/repo", pr=7,
                 head="fixture-head", verdict="needs-fix", accounting_pr=9,
                 landed=0, parent_id="t20", completed_by="%7", completed_at=900.0,
                 cancelled_at=800.0)]
    leases = [lease("t22", "%4", live=False, caller_pid=123, caller_start="old",
                    caller_name="expired fixture", assignee_added=1),
              lease("t23", "%4", caller_pid=234, caller_start="start:234",
                    caller_name="fixture codex Δ", assignee_added=1),
              lease("t24", "%9", caller_pid=345, caller_start="start:345",
                    caller_name="other fixture", assignee_added=0),
              lease("t25", "%9", caller_pid=456, caller_start="start:456",
                    caller_name="multi fixture", assignee_added=1)]
    yield dict(name="eligibility_dependencies_allocation_and_full_metadata", pane_id=PANE,
               limit=50, tables={"tasks": rows, "task_leases": leases,
                                 "allocation_offers": [offer("t33", [PANE]),
                                                       offer("t34", ["%9"]),
                                                       offer("t35", [PANE], "accepted")]})
    # next() uses list's DEFAULT 200 independently of for_pane's outer limit.
    # 200 urgent foreign-assigned globals hide both the 201st eligible global
    # and every local row from next's combined global+pane query.
    cutoff = [task(10000 + i, priority="urgent", assignees='["%9"]') for i in range(200)]
    cutoff += [task(10200, priority="high")]
    cutoff += [task(10201 + i, scope=PANE) for i in range(200)]
    assert len(cutoff) == 401
    yield dict(name="default200_next_cutoff_401_rows", pane_id=PANE, limit=None,
               tables={"tasks": cutoff})
    boundary = [task(20000 + i, priority="urgent", assignees='["%9"]') for i in range(199)]
    boundary += [task(20199, priority="high")]
    boundary += [task(20200 + i, scope=PANE) for i in range(200)]
    yield dict(name="default200_includes_200th_eligible_global", pane_id=PANE, limit=1,
               tables={"tasks": boundary})


def insert_rows(db, table, rows):
    for row in rows:
        columns = list(row)
        sql = f'INSERT INTO "{table}" ({",".join(columns)}) VALUES ({",".join("?" for _ in columns)})'
        db.execute(sql, [row[column] for column in columns])


def snapshot(db):
    return "\n".join(db.iterdump())


def generate():
    result = dict(schema="tasks-for-pane-oracle/1", python_now=NOW,
                  python_source="dunk_tasks.py::Tasks.for_pane/list/next/_decorate/_claimable",
                  python_source_sha256=hashlib.sha256((ROOT / "dunk_tasks.py").read_bytes()).hexdigest(),
                  default_outer_limit=20, default_next_list_limit=200,
                  cases=[])
    with tempfile.TemporaryDirectory(prefix="tasks-for-pane-oracle-") as directory:
        for index, case in enumerate(cases()):
            store = dunk_tasks.Tasks(str(Path(directory) / f"case-{index}.db"))
            try:
                if "allocation_offers" in case["tables"]:
                    store.db.executescript(dunk_allocator.SCHEMA)
                for table, rows in case["tables"].items():
                    insert_rows(store.db, table, rows)
                store.db.commit()
                before = snapshot(store.db)
                with patch.object(dunk_tasks, "_now", return_value=NOW):
                    kwargs = {} if case["limit"] is None else {"limit": case["limit"]}
                    expected = store.for_pane(case["pane_id"], **kwargs)
                    repeated = store.for_pane(case["pane_id"], **kwargs)
                assert expected == repeated, case["name"]
                assert snapshot(store.db) == before, case["name"]
                case.update(expected=expected, unchanged=True)
                result["cases"].append(case)
            finally:
                store.close()
    cutoff = next(c for c in result["cases"] if c["name"] == "default200_next_cutoff_401_rows")
    assert cutoff["expected"]["next"]["pick"] is None
    boundary = result["cases"][-1]
    assert boundary["expected"]["next"]["pick"]["id"] == "t20199"
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "rust/fixtures/tasks_for_pane.json")
    args = parser.parse_args()
    result = generate()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    temporary = args.output.with_suffix(args.output.suffix + ".tmp")
    temporary.write_text(json.dumps(result, indent=2, ensure_ascii=False) + "\n")
    temporary.replace(args.output)
    print(f"wrote {len(result['cases'])} oracle cases; {args.output.stat().st_size} bytes")


if __name__ == "__main__":
    main()
