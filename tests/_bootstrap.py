"""Test isolation: never touch the live Dunking Sheep state.

Must be imported before any `dunk_*` module, because those modules compute
their default paths from DUNKINGSHEEP_DIR at import time. Without this, the
Flock tests used the real tasks/inbox databases and their fake reaper requeued
real t6/t29 claims (the 2026-10-21 false-reap incident).
"""
import os
import tempfile

_TMP = tempfile.mkdtemp(prefix="dunkingsheep-tests-")
os.environ.setdefault("DUNKINGSHEEP_DIR", _TMP)
os.environ.setdefault("DUNKINGSHEEP_SOCKET", os.path.join(_TMP, "test.sock"))

# Tests must not shell out to `claude agents --json` (2.9 s per call).
os.environ.setdefault("DUNKINGSHEEP_CLAUDE_STATUS", "0")
os.environ.setdefault("DUNKINGSHEEP_MIN_POLL_S", "0")  # tests call the CLI repeatedly

# The CLI auto-starts a detached daemon on its temp socket. A detached daemon
# outlives the test process, so every run of the suite used to leak one: 44 had
# accumulated at ~34 MB each before anyone looked. Sweep them at exit.
import atexit as _atexit
import signal as _signal


def _sweep_spawned_daemons():
    """SIGTERM any daemon holding one of our temp sockets.

    Found by /proc/<pid>/fd rather than `ss`/`lsof` so the sweep depends on
    nothing that may be missing on the host.
    """
    wanted = {os.path.realpath(os.path.join(_TMP, "test.sock")), os.path.realpath(_TMP)}
    me = os.getpid()
    for entry in os.listdir("/proc"):
        if not entry.isdigit() or int(entry) == me:
            continue
        fd_dir = os.path.join("/proc", entry, "fd")
        try:
            handles = os.listdir(fd_dir)
        except OSError:
            continue
        for handle in handles:
            try:
                target = os.readlink(os.path.join(fd_dir, handle))
            except OSError:
                continue
            if target.startswith("socket:[") or not any(target.startswith(w) for w in wanted):
                continue
            try:
                os.kill(int(entry), _signal.SIGTERM)
            except OSError:
                pass
            break


_atexit.register(_sweep_spawned_daemons)
