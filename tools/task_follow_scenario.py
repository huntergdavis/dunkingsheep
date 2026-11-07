#!/usr/bin/env python3
"""The task-follow scenario for the production socket replay (t1605 slice 4).

Five requests, four commands, all made as one pane, in this order:

    task_add        seeds a task: no assignee, created_by the pane, global, open, low
    task_follow     subscribes the pane to task pushes, scope global
    task_followers  lists that one follower
    task_unfollow   drops the subscription
    task_followers  lists nobody

Run it through `diff_harness.py record --listen <proxy>` so the replies are captured, scrubbed and
written as traffic. It talks only to the socket it is given, and never to the live daemon.

The identity each command needs, from dunk_roles.ALLOW and the dunk_core handlers: task_add,
task_follow and task_unfollow are lane verbs (worker, reviewer) that the owner may also call, and
task_follow and task_unfollow refuse a caller with no pane ("requires a known caller pane");
task_followers is read-only, open to every role. The sandbox owner pane satisfies all four, so every
request claims that pane as its caller and none names a target.

    tools/task_follow_scenario.py --socket <proxy socket> --pane %2
"""

import argparse
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import diff_harness as harness  # noqa: E402

DEFAULT_PANE = "%2"
SEED_TITLE = "t1605 task follow scenario: seed task"
SEED_BODY = ("Seeded by tools/task_follow_scenario.py so the follow has a queue to follow. "
             "Sandbox only: the replay recreates it on every run.")


def scenario(pane):
    """The ordered (command, args) pairs. Every request runs as `pane`; none names a target."""
    del pane  # the caller is claimed per request in run(); no argument names a pane
    return [
        ("task_add", {"title": SEED_TITLE, "body": SEED_BODY, "priority": "low"}),
        ("task_follow", {"scope": "global"}),
        ("task_followers", {}),
        ("task_unfollow", {}),
        ("task_followers", {}),
    ]


def run(socket_path, pane):
    """Send each request and print one line per command. Return how many were answered."""
    answered = 0
    for number, (cmd, args) in enumerate(scenario(pane), start=1):
        reply, _latency = harness.call(socket_path, {"id": number, "cmd": cmd, "args": args,
                                                     "self_pane_id": pane})
        if reply is None:
            print(f"{cmd}: no reply from {socket_path}")
            continue
        answered += 1
        if reply.get("ok"):
            print(f"{cmd}: ok")
        else:
            print(f"{cmd}: refused: {reply.get('error')}")
    return answered


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--socket", required=True,
                        help="the proxy socket that `record --listen` serves")
    parser.add_argument("--pane", default=DEFAULT_PANE,
                        help="the pane every request claims as its caller")
    args = parser.parse_args(argv)
    harness.refuse_live_socket(args.socket)
    total = len(scenario(args.pane))
    answered = run(args.socket, args.pane)
    print(f"scenario: {answered}/{total} requests answered")
    return 0 if answered == total else 1


if __name__ == "__main__":
    sys.exit(main())
