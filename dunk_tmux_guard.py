#!/usr/bin/env python3
# dunkingsheep-tmux-guard
"""tmux guard - installed by `dunkingsheep guard install-tmux`; do not edit.

The herdr guard's counterpart for the tmux backend. It shadows `tmux` *for the
agent panes Dunking Sheep starts* (their PATH gets the guard directory first);
the user's own shell keeps the real tmux.

Policy:
  * read-only commands (list-*, capture-pane, display-message, has-session,
    show-*) exec the real tmux unchanged;
  * kill-pane/kill-window/kill-session go through the daemon's `close`, which
    applies the superuser/self policy and the backend's "only panes we created"
    check;
  * kill-server and `-a` (kill all) are refused outright;
  * typing (send-keys, paste-buffer, ...) and layout mutations are refused
    with a pointer at the daemon commands, because bypassing them would skip
    the human-input hold and the close guard.

If the daemon is unreachable or refuses, the answer is a refusal. There is no
fallback to the real tmux for a destructive command.
"""
import json
import os
import subprocess
import sys

DUNKINGSHEEP = os.environ.get("DUNKINGSHEEP_CLI") or os.path.expanduser(
    "~/.local/bin/dunkingsheep")
GUARD_DIR = os.path.dirname(os.path.realpath(__file__))
NEWLINE = chr(10)
MARKER = b"dunkingsheep-tmux-guard"

READ_ONLY = {
    "list-panes", "list-windows", "list-sessions", "list-clients", "list-keys",
    "list-commands", "list-buffers", "capture-pane", "display-message",
    "display-panes", "show-options", "show-window-options", "show-messages",
    "show-environment", "show-hooks", "has-session", "server-info", "info",
}
DESTRUCTIVE = {
    "kill-pane": "pane", "kill-window": "tab", "kill-session": "workspace",
}
TYPING = {
    "send-keys", "send-text", "paste-buffer", "set-buffer", "load-buffer",
    "run-shell", "if-shell", "pipe-pane", "command-prompt", "confirm-before",
}
GLOBAL_WITH_ARG = {"-L", "-S", "-f", "-T"}
GLOBAL_FLAGS = {"-2", "-C", "-D", "-l", "-N", "-q", "-u", "-v", "-V"}


class Decision:
    """A pure close-policy decision: no tmux, no daemon, no side effects."""

    __slots__ = ("allowed", "reason")

    def __init__(self, allowed, reason):
        self.allowed = bool(allowed)
        self.reason = reason

    def __repr__(self):
        return f"Decision(allowed={self.allowed!r}, reason={self.reason!r})"


def config_dir():
    return (os.environ.get("DUNKINGSHEEP_DIR")
            or os.path.expanduser("~/.config/dunkingsheep"))


def load_protected(path=None):
    """Pane ids the daemon treats as superusers (the VP/owner)."""
    path = path or os.path.join(config_dir(), "superusers.json")
    try:
        with open(path, "r", encoding="utf-8") as handle:
            data = json.load(handle)
    except (OSError, ValueError):
        return set()
    return {str(pane) for pane in (data.get("panes") or [])}


def load_managed(path=None):
    """Pane ids the tmux backend recorded as created by Dunking Sheep."""
    path = path or os.environ.get("DUNKINGSHEEP_TMUX_REGISTRY") or os.path.join(
        config_dir(), "tmux-panes.json")
    try:
        with open(path, "r", encoding="utf-8") as handle:
            data = json.load(handle)
    except (OSError, ValueError):
        return set()
    return set((data.get("panes") or {}).keys())


def tmux_panes(globals_=()):
    """Read-only pane inventory for the policy inputs (never a mutation)."""
    try:
        proc = subprocess.run(
            [real_tmux(), *globals_, "list-panes", "-a", "-F",
             "#{pane_id}\t#{session_id}\t#{window_id}"],
            capture_output=True, text=True, timeout=5)
    except (OSError, subprocess.SubprocessError):
        return []
    if proc.returncode != 0:
        return []
    panes = []
    for line in proc.stdout.splitlines():
        fields = line.split("\t")
        if len(fields) >= 3:
            panes.append({"pane_id": fields[0], "workspace_id": fields[1],
                          "tab_id": fields[2]})
    return panes


def _target_panes(kind, target, panes):
    if kind == "pane":
        return [pane for pane in panes if pane.get("pane_id") == target]
    field = "tab_id" if kind == "tab" else "workspace_id"
    return [pane for pane in panes if pane.get(field) == target]


def close_decision(kind, target, *, panes=(), protected=(), managed=(),
                   caller_pane=None):
    """Decide a kill-pane/kill-window/kill-session without performing it.

    This is deliberately pure so the decision can be tested with the VP's real
    pane id: a guard whose decision cannot be evaluated without acting is a
    guard nobody can test.
    """
    target_panes = _target_panes(kind, target, panes)
    target_ids = [pane.get("pane_id") for pane in target_panes]
    if any(pane_id in protected for pane_id in target_ids):
        return Decision(False, "refusing to close a superuser pane/container")
    if caller_pane:
        own = next((pane for pane in panes
                    if pane.get("pane_id") == caller_pane), None)
        field = {"pane": "pane_id", "tab": "tab_id",
                 "workspace": "workspace_id"}[kind]
        if own and own.get(field) == target:
            return Decision(False, "refusing to close the caller's own "
                                   f"{kind} ({target})")
    if not target_panes or any(pane_id not in managed for pane_id in target_ids):
        return Decision(False, "refusing to close something Dunking Sheep did "
                               "not create")
    return Decision(True, "allowed")


def is_guard(path):
    try:
        with open(path, "rb") as handle:
            return MARKER in handle.read(400)
    except OSError:
        return False


def real_tmux():
    """The actual tmux binary, never this wrapper."""
    override = os.environ.get("DUNKINGSHEEP_TMUX_BIN")
    if override and os.path.exists(override) and not is_guard(override):
        return override
    beside = os.path.join(GUARD_DIR, "tmux-real")
    if os.path.isfile(beside) and os.access(beside, os.X_OK):
        return beside
    for directory in os.environ.get("PATH", "").split(os.pathsep):
        if not directory or os.path.realpath(directory) == GUARD_DIR:
            continue
        candidate = os.path.join(directory, "tmux")
        if os.path.isfile(candidate) and os.access(candidate, os.X_OK):
            if is_guard(candidate):
                continue
            return candidate
    return "/usr/bin/tmux"


def passthrough(argv):
    binary = real_tmux()
    try:
        os.execv(binary, [binary, *argv])
    except OSError as error:
        print("tmux guard: cannot run %s: %s" % (binary, error), file=sys.stderr)
        return 127


def split_global(argv):
    """Return (global_options, command, rest). tmux accepts its global flags
    before the command; they must be forwarded verbatim on reads."""
    index = 0
    globals_ = []
    while index < len(argv):
        arg = argv[index]
        if arg in GLOBAL_WITH_ARG:
            globals_ += argv[index:index + 2]
            index += 2
            continue
        if arg in GLOBAL_FLAGS:
            globals_.append(arg)
            index += 1
            continue
        if arg == "--":
            index += 1
            break
        if arg.startswith("-"):
            # Unknown global flag: refuse rather than guess.
            return globals_, None, argv[index:]
        break
    return globals_, (argv[index] if index < len(argv) else None), argv[index + 1:]


def option_value(rest, name="-t"):
    for index, arg in enumerate(rest):
        if arg == name and index + 1 < len(rest):
            return rest[index + 1]
        if arg.startswith(name) and len(arg) > len(name):
            return arg[len(name):]
    return None


def current_target(kind, pane_id, globals_):
    """kill-window/kill-session with no -t mean the current window/session."""
    if kind == "pane":
        return pane_id
    fmt = "#{window_id}" if kind == "tab" else "#{session_id}"
    try:
        proc = subprocess.run(
            [real_tmux(), *globals_, "display-message", "-p", "-t", pane_id, fmt],
            capture_output=True, text=True, timeout=10)
    except Exception:  # noqa: BLE001 - fail closed
        return None
    if proc.returncode != 0:
        return None
    return proc.stdout.strip() or None


def own_pane():
    """The pane this shell is in, or None. An empty TMUX_PANE is not a pane."""
    return os.environ.get("TMUX_PANE") or None


def route_close(kind, target, globals_):
    if not target:
        target = current_target(kind, own_pane(), globals_)
    if not target:
        print("tmux guard: no target pane; refusing close", file=sys.stderr)
        return 1
    command = [sys.executable, DUNKINGSHEEP, "--json", "close", kind, target]
    env = dict(os.environ, DUNKINGSHEEP_GUARD="1")
    try:
        proc = subprocess.run(command, env=env, capture_output=True, text=True,
                              timeout=60)
    except Exception as error:  # noqa: BLE001 - fail closed
        print("tmux guard: daemon unreachable (%s); refusing close" % error,
              file=sys.stderr)
        return 1
    if proc.returncode != 0:
        print((proc.stderr or "tmux guard: refused").strip(), file=sys.stderr)
        return proc.returncode or 1
    sys.stdout.write(proc.stdout)
    return 0


def refuse(command, message):
    print("tmux guard: refusing `tmux %s`: %s" % (command, message),
          file=sys.stderr)
    return 2


def main(argv):
    globals_, command, rest = split_global(list(argv))
    if not command:
        if "-V" in globals_ or "-v" in globals_:
            return passthrough(argv)
        return refuse("", "attaching/creating a session outside the daemon "
                          "bypasses the close guard; use the Dunking Sheep "
                          "workspace commands")
    if command in READ_ONLY:
        return passthrough(argv)
    if command in DESTRUCTIVE:
        if "-a" in rest or "--all" in rest:
            return refuse(command, "killing every pane/window/session bypasses "
                                   "the close policy")
        kind = DESTRUCTIVE[command]
        target = option_value(rest)
        if not target:
            target = current_target(kind, own_pane(), globals_)
        decision = close_decision(
            kind, target, panes=tmux_panes(globals_), protected=load_protected(),
            managed=load_managed(), caller_pane=own_pane())
        if not decision.allowed:
            return refuse(command, decision.reason)
        return route_close(kind, target, globals_)
    if command == "kill-server":
        return refuse(command, "killing the tmux server would take every pane "
                               "with it")
    if command in TYPING:
        return refuse(command, "typing bypasses the human-input hold; use "
                               "`dunkingsheep send` / `dunkingsheep send-keys`")
    if command in ("respawn-pane", "respawn-window"):
        return refuse(command, "respawn kills the pane's process; use the "
                               "Dunking Sheep API so the close policy runs")
    return refuse(command, "mutating tmux state outside the daemon bypasses "
                           "the close and typing guards; use the Dunking Sheep "
                           "workspace/tab/pane commands")


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
