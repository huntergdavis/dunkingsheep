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
comes first on PATH. The wrapper forwards the four typing subcommands to the
daemon (which queues them behind the human) and execs the real herdr for
everything else, so `herdr pane list`, `tab create` and the rest are untouched.

Recursion is avoided from both ends: the wrapper sets DUNKINGSHEEP_GUARD=1
before calling back into dunkingsheep, and re-execs the real binary when it
sees that variable; `herdr_client._find_herdr` skips the guard directory when
resolving herdr for the daemon itself.
"""

import os
import shutil
import stat
import subprocess
import sys

CONFIG_DIR = os.environ.get("DUNKINGSHEEP_DIR") or os.path.expanduser(
    "~/.config/dunkingsheep"
)
GUARD_DIR = os.path.join(CONFIG_DIR, "bin")
GUARD_PATH = os.path.join(GUARD_DIR, "herdr")
BASHRC = os.path.expanduser("~/.bashrc")
MARKER = "# dunkingsheep herdr guard"

GUARD_MARKER = "dunkingsheep-herdr-guard"
REAL_SUFFIX = "-real"

WRAPPER = '''#!/usr/bin/env python3
# dunkingsheep-herdr-guard
"""herdr guard - installed by `dunkingsheep guard install`; do not edit.

Routes pane-typing subcommands through the Dunking Sheep daemon so they wait
for the human to stop typing. Everything else execs the real herdr unchanged.
"""
import os
import shutil
import subprocess
import sys

DUNKINGSHEEP = {dunkingsheep!r}
GUARD_DIR = os.path.dirname(os.path.realpath(__file__))


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
        sys.stderr.write("herdr guard: cannot run %s: %s\\n" % (binary, error))
        return 127


def route(kind, target, payload, argv):
    """Hand a typing command to the daemon. Falls back to the real herdr if the
    daemon cannot be reached, so the guard never makes herdr unusable."""
    cmd = [sys.executable, DUNKINGSHEEP, "--json", "send", target, payload] \\
        if kind == "text" else \\
        [sys.executable, DUNKINGSHEEP, "--json", "send-keys", target, payload]
    if kind == "text-no-enter":
        cmd = [sys.executable, DUNKINGSHEEP, "--json", "send", target, payload,
               "--no-enter"]
    env = dict(os.environ, DUNKINGSHEEP_GUARD="1")
    try:
        proc = subprocess.run(cmd, env=env, capture_output=True, text=True, timeout=120)
    except Exception as error:  # noqa: BLE001 - never block the caller
        sys.stderr.write("herdr guard: daemon unreachable (%s); sending directly\\n" % error)
        return passthrough(argv)
    if proc.returncode != 0:
        sys.stderr.write(proc.stderr or "herdr guard: send failed\\n")
        return proc.returncode
    sys.stdout.write(proc.stdout)
    return 0


def main(argv):
    # Already inside the guard (the daemon calling herdr), or explicitly
    # bypassed: go straight to the real binary.
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
    except Exception as error:  # noqa: BLE001 - a broken guard must not break herdr
        sys.stderr.write("herdr guard: %s; sending directly\\n" % error)
    return passthrough(argv)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
'''

PATH_LINE = 'export PATH="{guard_dir}:$PATH"'


def _cli_path():
    return os.path.join(os.path.dirname(os.path.abspath(__file__)), "dunkingsheep")


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
        "guarded_commands": ["pane run", "pane send-text", "pane send-keys", "agent send"],
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
        handle.write(WRAPPER.format(dunkingsheep=_cli_path()))
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
    body = WRAPPER.format(dunkingsheep=_cli_path())
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
