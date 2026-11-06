#!/usr/bin/env python3
"""The send scenario for the production socket replay (t1605 slice 3).

It sends the six commands the replay gate checks, in order, all aimed at one pane. Run it through
`diff_harness.py record --listen <proxy>` so the replies are captured, scrubbed and written as
traffic. The pane is a shell in the sandbox, so the scenario types only what does nothing: the text
`true` and the key `Enter`. It talks only to the socket it is given, and never to the live daemon.

    tools/send_scenario.py --socket <proxy socket> --pane %2
"""

import argparse
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import diff_harness as harness  # noqa: E402

DEFAULT_PANE = "%2"


def scenario(pane):
    """The ordered (command, args) pairs. Every request targets `pane`."""
    return [
        ("delivery", {"target": pane}),
        ("delivery_capability", {"target": pane}),
        ("send_text", {"target": pane, "text": "true", "press_enter": True, "wait_s": 1}),
        ("send_keys", {"target": pane, "keys": ["Enter"], "wait_s": 1}),
        ("flush_box", {"target": pane}),
        ("clear_box", {"target": pane, "reason": "t1605 send scenario: the box is empty"}),
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
                        help="the pane every request targets and claims as its caller")
    args = parser.parse_args(argv)
    harness.refuse_live_socket(args.socket)
    total = len(scenario(args.pane))
    answered = run(args.socket, args.pane)
    print(f"scenario: {answered}/{total} requests answered")
    return 0 if answered == total else 1


if __name__ == "__main__":
    sys.exit(main())
