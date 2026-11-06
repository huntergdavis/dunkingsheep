"""Per-command gate for the production socket replay (t1605 slice 3).

diff_harness.replay exits 0 even when no request matches, so this module counts the requests
itself. A command passes only when it has recorded traffic, at least one matched request (sent
and answered), and no diff against the Python recording. Any diff is reported verbatim by the
harness; this gate only decides pass or fail.
"""

import collections

SEND_COMMANDS = (
    "send_text",
    "send_keys",
    "flush_box",
    "clear_box",
    "delivery",
    "delivery_capability",
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
