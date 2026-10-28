#!/usr/bin/env python3
"""
The herdr guard: a `herdr` wrapper that routes typing through Dunking Sheep.

Dunking Sheep never types over someone who is mid-sentence, but that only
covers text it sends itself. Agents also reach for the herdr CLI directly
(`herdr pane run <pane> "..."`, `herdr pane send-text`, `herdr agent send`,
`herdr pane send-keys <pane> Enter`), and those go straight to the terminal
with no idea a human is writing there. On 2026-09-23 exactly that landed in
the middle of a sentence and Enter submitted the mangled result.

`dunkingsheep guard install` puts a small `herdr` wrapper in a directory that
comes first on PATH. The wrapper forwards typing and destructive/layout
subcommands to the daemon (which queues typing behind the human and enforces
the superuser allowlist) and execs the real herdr for read-only commands.

Recursion is avoided from both ends: the wrapper sets DUNKINGSHEEP_GUARD=1
before calling back into dunkingsheep, and re-execs the real binary when it
sees that variable; `herdr_client._find_herdr` skips the guard directory when
resolving herdr for the daemon itself.
"""

import os
import shlex
import shutil
import stat
import subprocess
import sys

CONFIG_DIR = os.environ.get("DUNKINGSHEEP_DIR") or os.path.expanduser(
    "~/.config/dunkingsheep"
)
GUARD_DIR = os.path.join(CONFIG_DIR, "bin")
GUARD_PATH = os.path.join(GUARD_DIR, "herdr")
TMUX_GUARD_DIR = os.path.join(CONFIG_DIR, "tmux-guard")
TMUX_WRAPPER_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                 "dunk_tmux_guard.py")
BASHRC = os.path.expanduser("~/.bashrc")
MARKER = "# dunkingsheep herdr guard"

GUARD_MARKER = "dunkingsheep-herdr-guard"
REAL_SUFFIX = "-real"

WRAPPER_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                             "dunk_guard_wrapper.py")


def _wrapper():
    """The guard source, with the CLI path substituted."""
    with open(WRAPPER_PATH, "r", encoding="utf-8") as handle:
        return handle.read().replace("__DUNKINGSHEEP__", repr(_cli_path()))


PATH_LINE = 'export PATH="{guard_dir}:$PATH"'


LAUNCHER = """#!/bin/sh
# Dunking Sheep launcher, installed by `dunkingsheep install`.
# Keeps the CLI on PATH under a plain name, so an agent can run
# `dunkingsheep inbox read` the way it runs `git` or `ls`, instead of
# executing a file inside a workspace - which a sandboxed agent should
# refuse, and does.
exec {python} {cli} "$@"
"""


def install_launcher(bin_dir=None):
    """Put `dunkingsheep` on PATH as an ordinary command.

    Agents herdr starts inherit the server's environment, which has
    ~/.local/bin first but never had this CLI in it. That left them invoking
    the absolute path into the project, which Claude's auto mode denies as
    external code - correctly. A plain command on PATH is the supported shape.
    """
    import sys as _sys
    bin_dir = os.path.expanduser(bin_dir or "~/.local/bin")
    os.makedirs(bin_dir, exist_ok=True)
    target = os.path.join(bin_dir, "dunkingsheep")
    with open(target, "w", encoding="utf-8") as handle:
        handle.write(LAUNCHER.format(python=_sys.executable, cli=_cli_path()))
    os.chmod(target, 0o755)
    # The sidebar poller is deliberately a tiny stdlib script, not the CLI:
    # three CLI startups per poll were the CPU cost the owner flagged.
    poll_target = os.path.join(bin_dir, "dunk-poll")
    with open(poll_target, "w", encoding="utf-8") as handle:
        handle.write("#!/bin/sh\nexec %s %s \"$@\"\n" % (
            shlex.quote(_sys.executable), shlex.quote(_poll_path())))
    os.chmod(poll_target, 0o755)
    on_path = any(os.path.realpath(d) == os.path.realpath(bin_dir)
                  for d in os.environ.get("PATH", "").split(os.pathsep) if d)
    return {"launcher": target, "poll_launcher": poll_target,
            "runs": _cli_path(), "poll_runs": _poll_path(),
            "on_path_here": on_path,
            "resolves_to": shutil.which("dunkingsheep"),
            "poll_resolves_to": shutil.which("dunk-poll")}


def _cli_path():
    return os.path.join(os.path.dirname(os.path.abspath(__file__)), "dunkingsheep")


def _poll_path():
    return os.path.join(os.path.dirname(os.path.abspath(__file__)), "tools",
                        "dunk_poll.py")


def resolve_real_tmux(guard_dir=None):
    """Resolve an executable outside the guard chain, without launching it.

    Used only for installation and explicit fixture-controller selection.
    Resolve symlinks before returning: reinstalling must not link to a wrapper
    or to a symlink that the installer is about to replace.
    """
    from dunk_tmux_guard import is_guard
    directory = guard_dir or TMUX_GUARD_DIR
    candidates = [os.environ.get("DUNKINGSHEEP_TMUX_BIN"),
                  os.path.join(directory, "tmux-real")]
    candidates.extend(os.path.join(part, "tmux")
                      for part in os.environ.get("PATH", "").split(os.pathsep)
                      if part and os.path.realpath(part) != os.path.realpath(directory))
    for candidate in candidates:
        if not candidate:
            continue
        resolved = os.path.realpath(candidate)
        if (os.path.isfile(resolved) and os.access(resolved, os.X_OK)
                and not is_guard(resolved)):
            return resolved
    raise FileNotFoundError("no executable non-guard tmux found; configure "
                            "DUNKINGSHEEP_TMUX_BIN or install tmux on PATH")


def tmux_status():
    """What the tmux guard looks like right now."""
    wrapper = os.path.join(TMUX_GUARD_DIR, "tmux")
    real = os.path.join(TMUX_GUARD_DIR, "tmux-real")
    return {
        "installed": os.path.exists(wrapper),
        "wrapper": wrapper if os.path.exists(wrapper) else None,
        "real_tmux": real if os.path.exists(real) else None,
        "covers": "agent panes started by TmuxBackend (PATH prefix); user shells untouched",
    }


def install_tmux():
    """Install the tmux close/typing guard for the agent panes we start.

    Unlike the herdr guard this is deliberately *not* added to ~/.bashrc: it
    must never wrap the user's own tmux. `TmuxBackend.start_agent` puts this
    directory first on the agent pane's PATH.
    """
    real = resolve_real_tmux(guard_dir=TMUX_GUARD_DIR)
    target = os.path.join(TMUX_GUARD_DIR, "tmux")
    if os.path.exists(target) and os.path.samefile(target, real):
        raise RuntimeError("cannot replace the selected real tmux with its guard")
    os.makedirs(TMUX_GUARD_DIR, exist_ok=True)
    link = os.path.join(TMUX_GUARD_DIR, "tmux-real")
    # A prior symlink fallback may have copied the binary here. Preserve that
    # regular file on reinstall instead of replacing it with a link to itself.
    if not (os.path.isfile(link) and not os.path.islink(link)
            and os.path.samefile(link, real)):
        if os.path.lexists(link):
            os.remove(link)
        try:
            os.symlink(real, link)
        except OSError:
            shutil.copy2(real, link)
    with open(TMUX_WRAPPER_PATH, "r", encoding="utf-8") as handle:
        source = handle.read()
    source = source.replace(
        'os.path.expanduser(\n    "~/.local/bin/dunkingsheep")',
        repr(_cli_path()))
    target = os.path.join(TMUX_GUARD_DIR, "tmux")
    with open(target, "w", encoding="utf-8") as handle:
        handle.write(source)
    os.chmod(target, 0o755)
    return {"ok": True, "wrapper": target, "real_tmux": real,
            "guard_dir": TMUX_GUARD_DIR,
            "note": "agent panes started by the tmux backend get this first on "
                    "PATH; the user's shell is untouched"}


def status():
    """What the guard looks like right now."""
    installed = os.path.exists(GUARD_PATH)
    on_path = False
    resolved = shutil.which("herdr")
    if resolved:
        on_path = os.path.realpath(os.path.dirname(resolved)) == os.path.realpath(GUARD_DIR)
    bashrc = False
    if os.path.exists(BASHRC):
        with open(BASHRC, "r", encoding="utf-8") as handle:
            bashrc = MARKER in handle.read()
    herdr_path, real_path = _resolve_real_herdr()
    shadowed = _is_guard(herdr_path) and os.path.exists(real_path)
    return {
        "installed": installed,
        "wrapper": GUARD_PATH if installed else None,
        "shell_configured": bashrc,
        "active_in_this_shell": on_path,
        "shadowing_herdr": shadowed,
        "covers_agents_started_by_herdr": shadowed,
        "herdr_resolves_to": resolved,
        "real_herdr": real_path if shadowed else herdr_path,
        "guarded_commands": [
            "pane run", "pane send-text", "pane send-keys", "agent send",
            "pane close", "pane kill", "tab close", "workspace close",
            "pane split/rename/focus", "tab create/rename/focus",
            "workspace create/rename/focus", "agent start",
        ],
        "tmux_guard": tmux_status(),
    }


def install(shadow_binary=False):
    """Write the wrapper and put its directory first on PATH for new shells.

    `shadow_binary` additionally takes herdr's own place, which is what covers
    agents herdr exec's itself. That renames a binary outside this project, so
    it never happens implicitly: only an explicit `dunkingsheep guard install`
    asks for it. (A test that called `install()` without sandboxing PATH once
    renamed the real herdr out from under a running daemon.)"""
    os.makedirs(GUARD_DIR, exist_ok=True)
    with open(GUARD_PATH, "w", encoding="utf-8") as handle:
        handle.write(_wrapper())
    os.chmod(GUARD_PATH, os.stat(GUARD_PATH).st_mode | stat.S_IEXEC | stat.S_IXGRP
             | stat.S_IXOTH)
    added = _ensure_bashrc()
    shadow_error = None
    if shadow_binary:
        try:
            shadow()
        except (OSError, RuntimeError) as error:
            shadow_error = str(error)
    result = status()
    result["bashrc_updated"] = added
    if shadow_error:
        result["shadow_error"] = shadow_error
    return result


def _resolve_real_herdr():
    """The real herdr on PATH, ignoring any guard wrapper."""
    for directory in os.environ.get("PATH", "").split(os.pathsep):
        if not directory or os.path.realpath(directory) == os.path.realpath(GUARD_DIR):
            continue
        candidate = os.path.join(directory, "herdr")
        if os.path.isfile(candidate) and os.access(candidate, os.X_OK):
            if _is_guard(candidate):
                sibling = os.path.join(directory, "herdr" + REAL_SUFFIX)
                if os.path.isfile(sibling):
                    return candidate, sibling      # already shadowed
                continue
            return candidate, os.path.join(directory, "herdr" + REAL_SUFFIX)
    fallback = os.path.expanduser("~/.local/bin/herdr")
    return fallback, fallback + REAL_SUFFIX


def _is_guard(path):
    try:
        with open(path, "rb") as handle:
            return GUARD_MARKER.encode() in handle.read(400)
    except OSError:
        return False


def shadow():
    """Put the guard where herdr itself lives.

    A PATH entry only reaches shells that read ~/.bashrc, and herdr starts
    agents by exec'ing them from the server process, so they inherit the
    server's environment and never see it. Shadowing the binary covers every
    caller, including agents already running: the real herdr is renamed to
    `herdr-real` and the wrapper takes its place."""
    herdr_path, real_path = _resolve_real_herdr()
    if not os.path.exists(herdr_path):
        raise RuntimeError(f"cannot find the herdr binary (looked at {herdr_path})")
    if not _is_guard(herdr_path):
        if os.path.exists(real_path):
            raise RuntimeError(f"{real_path} already exists; move it aside first")
        os.rename(herdr_path, real_path)
    os.makedirs(GUARD_DIR, exist_ok=True)
    body = _wrapper()
    for target in (herdr_path, GUARD_PATH):
        with open(target, "w", encoding="utf-8") as handle:
            handle.write(body)
        os.chmod(target, 0o755)
    return {"shadowed": True, "wrapper": herdr_path, "real_herdr": real_path,
            **status()}


def unshadow():
    """Put the real herdr back in its own place."""
    herdr_path, real_path = _resolve_real_herdr()
    restored = False
    if _is_guard(herdr_path) and os.path.exists(real_path):
        os.replace(real_path, herdr_path)
        restored = True
    return {"restored": restored, "herdr": herdr_path, **status()}


def _ensure_bashrc():
    line = PATH_LINE.format(guard_dir=GUARD_DIR)
    block = f"\n{MARKER} (routes agent typing through the daemon; dunkingsheep guard uninstall to remove)\n{line}\n"
    if os.path.exists(BASHRC):
        with open(BASHRC, "r", encoding="utf-8") as handle:
            if MARKER in handle.read():
                return False
    with open(BASHRC, "a", encoding="utf-8") as handle:
        handle.write(block)
    return True


def uninstall():
    """Remove the wrapper, restore herdr, and drop the PATH line."""
    unshadow()
    removed = False
    if os.path.exists(GUARD_PATH):
        os.remove(GUARD_PATH)
        removed = True
    cleaned = False
    if os.path.exists(BASHRC):
        with open(BASHRC, "r", encoding="utf-8") as handle:
            lines = handle.readlines()
        keep, skip_next = [], False
        for line in lines:
            if MARKER in line:
                skip_next = True
                cleaned = True
                continue
            if skip_next and line.strip() == PATH_LINE.format(guard_dir=GUARD_DIR):
                skip_next = False
                continue
            skip_next = False
            keep.append(line)
        if cleaned:
            with open(BASHRC, "w", encoding="utf-8") as handle:
                handle.writelines(keep)
    return {"wrapper_removed": removed, "bashrc_cleaned": cleaned, **status()}


def check():
    """Prove the guard intercepts: run the wrapper with a harmless command that
    would type, against a pane id that does not exist, and confirm it went to
    the daemon rather than to herdr."""
    if not os.path.exists(GUARD_PATH):
        return {"ok": False, "detail": "guard is not installed"}
    env = dict(os.environ)
    env.pop("DUNKINGSHEEP_GUARD", None)
    env.pop("HERDR_GUARD_OFF", None)
    proc = subprocess.run(
        [sys.executable, GUARD_PATH, "pane", "run", "no-such-pane-xyz", "echo hi"],
        env=env, capture_output=True, text=True, timeout=60,
    )
    blob = (proc.stdout + proc.stderr).lower()
    routed = "no herdr pane matches" in blob or "dunk" in blob
    return {"ok": routed, "detail": (proc.stdout + proc.stderr).strip()[:300]}
