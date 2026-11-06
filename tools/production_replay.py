"""Per-command gate for the production socket replay (t1605 slice 3).

diff_harness.replay exits 0 even when no request matches, so this module counts the requests
itself. A command passes only when it has recorded traffic, at least one matched request (sent
and answered), and no diff against the Python recording. Any diff is reported verbatim by the
harness; this gate only decides pass or fail.
"""

import argparse
import collections
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import diff_harness as harness  # noqa: E402

SEND_COMMANDS = (
    "send_text",
    "send_keys",
    "flush_box",
    "clear_box",
    "delivery",
    "delivery_capability",
)

# t1605 slice 4: the task-follow replay (tools/task_follow_scenario.py). task_add seeds a task
# first; task_followers is replayed twice, before and after the unfollow.
TASK_FOLLOW_COMMANDS = (
    "task_add",
    "task_follow",
    "task_followers",
    "task_unfollow",
)


def requested_counts(entries, commands):
    """Recorded requests per wanted command, zero when the traffic has none."""
    counts = collections.Counter(
        entry["cmd"] for entry in entries if entry["cmd"] in commands
    )
    return {cmd: counts.get(cmd, 0) for cmd in commands}


def gate(entries, result, commands):
    """Return (ok, lines) for the replay result that diff_harness.replay returned."""
    requested = requested_counts(entries, commands)
    # An error from replay starts with the command name: "<cmd>: no reply from <socket>".
    errors = collections.Counter(text.split(":", 1)[0] for text in result["errors"])
    diffs = collections.Counter(cmd for cmd, _text in result["diffs"])
    ok = True
    lines = []
    for cmd in commands:
        total = requested[cmd]
        unanswered = errors.get(cmd, 0)
        matched = max(total - unanswered, 0)
        differing = diffs.get(cmd, 0)
        problems = []
        if total == 0:
            problems.append("no recorded traffic")
        elif matched < 1:
            problems.append("zero matched requests")
        if unanswered:
            problems.append(f"{unanswered} unanswered request(s)")
        if differing:
            problems.append(f"{differing} diff(s)")
        ok = ok and not problems
        status = "FAIL" if problems else "ok"
        detail = f" ({'; '.join(problems)})" if problems else ""
        lines.append(
            f"{cmd}: {status} requested={total} matched={matched} diffs={differing}{detail}"
        )
    return ok, lines


def main(argv=None):
    """Replay the traffic against one socket, print the per-command gate, and return 0 or 1."""
    parser = argparse.ArgumentParser(
        description="Replay recorded traffic against a daemon socket and gate each command.")
    parser.add_argument("--traffic", required=True, help="the sanitized recording (JSONL)")
    parser.add_argument("--against", required=True, help="the daemon socket to replay against")
    parser.add_argument("--commands", default=",".join(SEND_COMMANDS),
                        help="comma separated command names to replay and gate")
    parser.add_argument("--volatile", default=None,
                        help="optional volatile-field rules for the harness")
    args = parser.parse_args(argv)
    harness.refuse_live_socket(args.against)
    commands = [cmd.strip() for cmd in args.commands.split(",") if cmd.strip()]
    entries = harness.load_traffic(args.traffic)
    result = harness.replay(entries, args.against, commands=commands,
                            volatile=args.volatile, verbose=True)
    ok, lines = gate(entries, result, commands)
    print("per-command gate:")
    for line in lines:
        print(f"  {line}")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
