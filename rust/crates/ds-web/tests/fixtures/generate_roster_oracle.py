#!/usr/bin/env python3
"""Regenerate roster_oracle.json from web/roster.py using synthetic inputs only."""

from __future__ import annotations

import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[5]
sys.path.insert(0, str(ROOT))

from web.roster import build_roster, discovered_id, is_lane, match_pane, presence  # noqa: E402

DESK = {
    "pane_id": "%133", "agent": "", "agent_status": "working", "tab_label": "sn-desk",
    "workspace_label": "bootstrap", "cwd": "/home/hunter/workspace/desk",
}
FRESH = {
    "pane_id": "%135", "agent": "", "agent_status": None, "tab_label": "sn-reviewer-4",
    "workspace_label": "bootstrap", "cwd": "/home/hunter/workspace",
}
SHELL = {
    "pane_id": "%3", "agent": "", "agent_status": None, "tab_label": "bash",
    "workspace_label": "par-a", "cwd": "/home/hunter",
}
COS = {"id": "cos", "name": "Front Desk", "match": "desk", "kind": "active"}
LANE = {"id": "desklane", "name": "Desk lane", "match": "desk", "kind": "active"}


def build_case(name, configured, panes, *, include_terminals=False, authority_responses=None):
    calls = []
    if authority_responses is None:
        roster = build_roster(configured, panes, include_terminals=include_terminals)
    else:
        remaining = iter(authority_responses)

        def authority(pane):
            calls.append(None if pane is None else pane.get("pane_id"))
            try:
                return next(remaining)
            except StopIteration as exc:
                raise AssertionError(f"{name}: missing synthetic authority response") from exc

        roster = build_roster(configured, panes, include_terminals=include_terminals,
                              can_message=authority)
        try:
            next(remaining)
        except StopIteration:
            pass
        else:
            raise AssertionError(f"{name}: unused synthetic authority response")
    return {
        "name": name,
        "configured": configured,
        "panes": panes,
        "include_terminals": include_terminals,
        "authority_responses": authority_responses,
        "expected_calls": calls,
        "expected": roster,
    }


def main():
    quiet = dict(DESK, agent_status=None)
    blocked = dict(DESK, agent_status="blocked")
    configured_order = [
        {"id": "first", "name": "First", "match": "desk", "kind": "active"},
        {"id": "second", "name": "Second", "match": "desk", "kind": "active"},
    ]
    panes_order = [
        dict(DESK),
        {"pane_id": "%140", "agent": "pi", "agent_status": "idle",
         "tab_label": "pi-worker", "workspace_label": "parallel", "cwd": "/tmp/pi"},
        dict(SHELL),
    ]
    fallback_agent = {
        "pane_id": "%150", "agent": "pi", "agent_status": "idle", "tab_label": "",
        "workspace_label": "parallel", "cwd": "/tmp/pi",
    }
    fallback_pane_id = {
        "pane_id": "%151", "agent": "", "agent_status": "blocked", "tab_label": "",
        "workspace_label": "parallel", "cwd": "/tmp/worker",
    }
    configured_rows = [
        {"id": "offline", "name": "Offline", "match": "never-here", "kind": "active"},
        dict(COS),
        dict(LANE, id="other-channel", name="Other channel"),
    ]

    builds = [
        build_case("the_desk_channel_can_message_the_live_desk_pane", [COS], [SHELL, DESK]),
        build_case("a_missing_desk_stays_offline_and_unmessageable", [COS], [SHELL]),
        build_case("a_quiet_desk_is_messageable_when_the_daemon_has_process_evidence",
                   [LANE], [SHELL, quiet], authority_responses=[True, True]),
        build_case("the_authority_can_refuse_a_pane_the_heuristic_would_allow",
                   [LANE], [DESK], authority_responses=[False, False]),
        build_case("the_desk_channel_is_messageable_while_quiet_because_it_goes_through_the_owner_mailbox",
                   [COS], [quiet], authority_responses=[False]),
        build_case("the_owner_mailbox_rule_does_not_make_an_absent_desk_messageable",
                   [COS], [SHELL]),
        build_case("other_channels_still_need_an_agent",
                   [{"id": "poolrad", "name": "Poolrad PM", "match": "bash", "kind": "active"}],
                   [SHELL]),
        build_case("configured_duplicates_keep_order_and_only_suppress_the_used_pane",
                   configured_order, panes_order),
        build_case("include_terminals_keeps_shell_and_offline_status_distinct",
                   [{"id": "missing", "name": "Missing", "match": "absent", "kind": "active"}],
                   [SHELL, FRESH, fallback_agent, fallback_pane_id], include_terminals=True),
        build_case("activity_authority_call_order_and_short_circuit",
                   configured_rows, [DESK], authority_responses=[False, False, True, False]),
        build_case("discovered_eligibility_uses_lane_heuristic_before_authority",
                   [], [DESK, SHELL], authority_responses=[False, True]),
        build_case("blocked_pane_uses_waiting_activity_group", [LANE], [blocked]),
    ]

    match_cases = [
        {"name": "pinned_pane_wins_over_match", "config": {"pane": "%3", "match": "desk"},
         "panes": [DESK, SHELL]},
        {"name": "missing_pinned_pane_has_no_fallback", "config": {"pane": "%99", "match": "desk"},
         "panes": [DESK, SHELL]},
        {"name": "exact_cwd_precedes_earlier_substring_match", "config": {"match": "desk"},
         "panes": [dict(DESK, tab_label="desk-helper", cwd="/tmp/elsewhere"),
                   dict(FRESH, cwd="/home/team/desk/")]},
        {"name": "first_exact_match_wins", "config": {"match": "desk"},
         "panes": [dict(DESK, tab_label="DESK"), dict(FRESH, tab_label="desk")]},
        {"name": "casefold_and_trailing_slashes_match_cwd", "config": {"match": "TEAM"},
         "panes": [dict(FRESH, cwd="/home/Team///")]},
    ]
    for case in match_cases:
        pane = match_pane(case["config"], case["panes"])
        case["expected"] = pane
        case["expected_pane_id"] = pane and pane["pane_id"]

    function_cases = {
        "discovered_id": [
            {"input": "%Straße/Kelvin", "expected": discovered_id("%Straße/Kelvin")},
            {"input": "///Élan___Worker///", "expected": discovered_id("///Élan___Worker///")},
            {"input": "---", "expected": discovered_id("---")},
        ],
        "is_lane": [
            {"input": DESK, "expected": is_lane(DESK)},
            {"input": {"agent": "claude", "agent_status": None},
             "expected": is_lane({"agent": "claude", "agent_status": None})},
            {"input": SHELL, "expected": is_lane(SHELL)},
            {"input": None, "expected": is_lane(None)},
            {"input": {"agent": "", "agent_status": "blocked"},
             "expected": is_lane({"agent": "", "agent_status": "blocked"})},
            {"input": {"agent": "", "agent_status": "unknown"},
             "expected": is_lane({"agent": "", "agent_status": "unknown"})},
        ],
        "presence": [
            {"input": "working", "expected": presence("working")},
            {"input": "idle", "expected": presence("idle")},
            {"input": "blocked", "expected": presence("blocked")},
            {"input": None, "expected": presence(None)},
            {"input": "offline", "expected": presence("offline")},
        ],
    }
    output = {"build_cases": builds, "match_pane_cases": match_cases,
              "functions": function_cases}
    target = Path(__file__).with_name("roster_oracle.json")
    target.write_text(json.dumps(output, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(f"wrote {target.name}: {len(builds)} full rosters, {len(match_cases)} matches")


if __name__ == "__main__":
    main()
