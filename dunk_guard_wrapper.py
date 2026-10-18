#!/usr/bin/env python3
# dunkingsheep-herdr-guard
"""herdr guard - installed by `dunkingsheep guard install`; do not edit.

Routes pane-typing and destructive subcommands through the Dunking Sheep daemon
so they wait for the human and obey the superuser allowlist. Everything else
execs the real herdr unchanged.
"""
import json
import os
import shlex
import subprocess
import sys

DUNKINGSHEEP = __DUNKINGSHEEP__
GUARD_DIR = os.path.dirname(os.path.realpath(__file__))
NEWLINE = chr(10)


def is_guard(path):
    try:
        with open(path, "rb") as handle:
            return b"dunkingsheep-herdr-guard" in handle.read(400)
    except OSError:
        return False


def real_herdr():
    """The actual herdr binary, never this wrapper. When the guard shadows
    herdr in its own directory the real one sits beside it as `herdr-real`."""
    override = os.environ.get("DUNKINGSHEEP_HERDR_BIN")
    if override and os.path.exists(override) and not is_guard(override):
        return override
    beside = os.path.join(os.path.dirname(os.path.realpath(__file__)), "herdr-real")
    if os.path.isfile(beside) and os.access(beside, os.X_OK):
        return beside
    for directory in os.environ.get("PATH", "").split(os.pathsep):
        if not directory or os.path.realpath(directory) == GUARD_DIR:
            continue
        candidate = os.path.join(directory, "herdr")
        if os.path.isfile(candidate) and os.access(candidate, os.X_OK):
            if is_guard(candidate):
                sibling = os.path.join(directory, "herdr-real")
                if os.path.isfile(sibling) and os.access(sibling, os.X_OK):
                    return sibling
                continue
            return candidate
    return os.path.expanduser("~/.local/bin/herdr-real")


def passthrough(argv):
    binary = real_herdr()
    try:
        os.execv(binary, [binary, *argv])
    except OSError as error:
        print("herdr guard: cannot run %s: %s" % (binary, error), file=sys.stderr)
        return 127


def daemon_json(args, timeout=120):
    cmd = [sys.executable, DUNKINGSHEEP, "--json", *args]
    env = dict(os.environ, DUNKINGSHEEP_GUARD="1")
    return subprocess.run(cmd, env=env, capture_output=True, text=True,
                          timeout=timeout)


def report_failure(text):
    message = (text or "herdr guard: refused").strip()
    if message:
        print(message, file=sys.stderr)


def route_close(kind, target):
    # Never fall back to the real herdr: a refused close must stay refused.
    try:
        proc = daemon_json(["close", kind, target])
    except Exception as error:  # noqa: BLE001 - fail closed
        report_failure("herdr guard: daemon unreachable (%s); refusing close"
                       % error)
        return 1
    if proc.returncode != 0:
        report_failure(proc.stderr)
        return proc.returncode or 1
    sys.stdout.write(proc.stdout)
    return 0


def route_herdr(argv):
    # Layout/control mutations go through the daemon's superuser-gated herdr
    # passthrough. No direct fallback.
    try:
        proc = daemon_json(["herdr", shlex.join(argv)])
    except Exception as error:  # noqa: BLE001 - fail closed
        report_failure("herdr guard: daemon unreachable (%s); refusing" % error)
        return 1
    if proc.returncode != 0:
        report_failure(proc.stderr)
        return proc.returncode or 1
    try:
        payload = json.loads(proc.stdout)
    except ValueError:
        sys.stdout.write(proc.stdout)
        return 0
    if payload.get("error"):
        report_failure(str(payload["error"]))
        return 1
    if payload.get("text"):
        text = payload["text"]
        sys.stdout.write(text)
        if not text.endswith(NEWLINE):
            sys.stdout.write(NEWLINE)
    elif payload.get("result") is not None:
        print(json.dumps(payload["result"], indent=2))
    return 0


def route(kind, target, payload, argv):
    """Hand a typing command to the daemon. A refused or unreachable daemon is
    an error: falling back would bypass the superuser/self-only checks."""
    if kind == "text":
        cmd = [sys.executable, DUNKINGSHEEP, "--json", "send", target, payload]
    elif kind == "text-no-enter":
        cmd = [sys.executable, DUNKINGSHEEP, "--json", "send", target, payload,
               "--no-enter"]
    else:
        cmd = [sys.executable, DUNKINGSHEEP, "--json", "send-keys", target,
               payload]
    env = dict(os.environ, DUNKINGSHEEP_GUARD="1")
    try:
        proc = subprocess.run(cmd, env=env, capture_output=True, text=True,
                              timeout=120)
    except Exception as error:  # noqa: BLE001 - fail closed
        report_failure("herdr guard: daemon unreachable (%s); refusing "
                       "(set HERDR_GUARD_OFF=1 to bypass)" % error)
        return 1
    if proc.returncode != 0:
        report_failure(proc.stderr)
        return proc.returncode
    sys.stdout.write(proc.stdout)
    return 0


MUTATING = (
    ("pane", "split"), ("pane", "rename"), ("pane", "focus"),
    ("tab", "create"), ("tab", "rename"), ("tab", "focus"),
    ("workspace", "create"), ("workspace", "rename"), ("workspace", "focus"),
    ("agent", "start"),
)
CLOSING = (
    ("pane", "close", "pane"), ("pane", "kill", "pane"),
    ("tab", "close", "tab"), ("workspace", "close", "workspace"),
)


def main(argv):
    # Already inside the guard (the daemon calling herdr), or explicitly
    # bypassed by a human: go straight to the real binary.
    if os.environ.get("DUNKINGSHEEP_GUARD") or os.environ.get("HERDR_GUARD_OFF"):
        return passthrough(argv)
    head = tuple(argv[:2])
    try:
        if head == ("pane", "run") and len(argv) >= 4:
            return route("text", argv[2], argv[3], argv)
        if head == ("pane", "send-text") and len(argv) >= 4:
            return route("text-no-enter", argv[2], argv[3], argv)
        if head == ("agent", "send") and len(argv) >= 4:
            return route("text", argv[2], argv[3], argv)
        if head == ("pane", "send-keys") and len(argv) >= 4:
            return route("keys", argv[2], " ".join(argv[3:]), argv)
        for group, verb, kind in CLOSING:
            if head == (group, verb) and len(argv) >= 3:
                return route_close(kind, argv[2])
        if head in MUTATING:
            return route_herdr(argv)
    except Exception as error:  # noqa: BLE001 - a broken guard must not bypass
        report_failure("herdr guard: %s; refusing" % error)
        return 1
    return passthrough(argv)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
