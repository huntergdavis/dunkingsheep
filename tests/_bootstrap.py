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


# -- t88: a run's daemons are reaped BY PID, never by pattern -------------------
#
# The sweep above only knows the sockets under THIS run's _TMP. A test that hands
# the daemon its own temp dir (/tmp/dsh-trial-config, /tmp/tmpe6a1a0m2/config)
# spawns a daemon the sweep cannot see, and those are the ones that accumulated
# across runs (measured 2026-10-24: five orphans, one 3 days old, none with a
# client). A pattern-based sweep would fix that by killing other lanes' trials -
# which is the mistake this fleet already made once with worktrees.
#
# So: every Popen whose argv runs `serve` is RECORDED in a pidfile under _TMP, and
# exit reaps exactly those pids and their process groups. A pid we cannot confirm
# is reported as LEAKED with the path, so the next person sees which test did it.

import subprocess as _subprocess

_PIDFILE = os.path.join(_TMP, "spawned-daemons.pids")
_REAL_POPEN = _subprocess.Popen


def _record_pid(pid, argv):
    try:
        with open(_PIDFILE, "a", encoding="utf-8") as handle:
            handle.write(f"{pid}\t{' '.join(map(str, argv))[:200]}\n")
    except OSError:
        pass


def _popen_recording_daemons(*args, **kwargs):
    argv = args[0] if args else kwargs.get("args")
    process = _REAL_POPEN(*args, **kwargs)
    try:
        flat = " ".join(map(str, argv)) if isinstance(argv, (list, tuple)) else str(argv)
    except Exception:                                   # noqa: BLE001 - never break a test
        flat = ""
    if "serve" in flat:
        _record_pid(process.pid, flat)
    return process


_subprocess.Popen = _popen_recording_daemons


def _alive(pid):
    """Alive AND still a daemon.

    `kill(pid, 0)` is not the test: it succeeds for a ZOMBIE (a child that has
    exited but not been reaped) and for a pid that has been REUSED by something
    else. Both produced false LEAKED lines on 2026-10-24, and a word that is wrong
    twice stops being read. The state must be a live one and the cmdline must still
    be our daemon; anything else is gone for our purposes.
    """
    try:
        with open(f"/proc/{pid}/stat", "rb") as handle:
            state = handle.read().rsplit(b")", 1)[1].split()[0]
        if state in (b"Z", b"X", b"x"):
            return False
        with open(f"/proc/{pid}/cmdline", "rb") as handle:
            cmdline = handle.read()
    except OSError:
        return False
    return b"serve" in cmdline


def _pids_from_pidfile():
    try:
        with open(_PIDFILE, encoding="utf-8") as handle:
            return [int(line.split("\t", 1)[0]) for line in handle if line.strip()]
    except (OSError, ValueError):
        return []


def _sweep_recorded_daemons():
    """Reap the pids THIS run recorded. Never a pattern, never someone else's."""
    pids = _pids_from_pidfile()
    if not pids:
        return
    me = os.getpid()
    for pid in pids:
        if pid == me:
            continue
        try:
            os.killpg(os.getpgid(pid), _signal.SIGTERM)
        except OSError:
            try:
                os.kill(pid, _signal.SIGTERM)
            except OSError:
                continue
    import time as _time
    # Give a signal time to be handled before crying leak: a 0.3s window reported
    # daemons that were already on their way out, which is how a checker stops
    # being believed.
    # Two passes: a loaded host can take seconds to unwind a daemon, and a false
    # LEAKED costs more than a sentence - it teaches the next reader to ignore the
    # word. Measured 2026-10-24: a daemon that was swept correctly still printed
    # LEAKED because it exited 0.2 s after the grace.
    for grace in (3.0, 4.0):
        deadline = _time.time() + grace
        while _time.time() < deadline and any(_alive(pid) for pid in pids):
            _time.sleep(0.1)
        pids = [pid for pid in pids if _alive(pid)]
        if not pids:
            return
    # Escalate: these pids are OURS (recorded at spawn), and a test daemon that has
    # ignored SIGTERM for 7 s is not going to leave politely. LEAKED is reserved for
    # a pid that survives even SIGKILL, which means a real problem (a zombie, a
    # process in another namespace), not a slow shutdown.
    for pid in pids:
        try:
            os.kill(pid, _signal.SIGKILL)
        except OSError:
            pass
    kill_deadline = _time.time() + 1.5
    while _time.time() < kill_deadline and any(_alive(pid) for pid in pids):
        _time.sleep(0.05)
    for pid in [p for p in pids if _alive(p)]:
        # Say WHICH spawn leaked, and where its config was, instead of leaving the
        # next reader to guess from `pgrep`.
        try:
            environ = open(f"/proc/{pid}/environ", "rb").read().split(b"\0")
            config = next((part.decode("utf-8", "replace") for part in environ
                           if part.startswith(b"DUNKINGSHEEP_DIR=")), "")
        except OSError:
            config = ""
        print(f"LEAKED daemon pid={pid} {config}", file=__import__("sys").stderr)


_atexit.register(_sweep_recorded_daemons)
