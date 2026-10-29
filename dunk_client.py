#!/usr/bin/env python3
"""
Client for the Dunking Sheep daemon socket, plus daemon auto-start.

    client = FlockClient.connect_or_start()
    client.call("add_dunk", target="self", text="continue", interval_minutes=60)
    client.list_dunks()            # any command name works as a method

Every call opens a short-lived connection; unix sockets make that cheap and it
keeps the client stateless and reconnect-free.
"""

import json
import os
import shutil
import socket
import subprocess
import sys
import time

from dunk_api import COMMANDS_BY_NAME
from dunk_core import CONFIG_DIR, DunkError
from dunk_server import DEFAULT_SOCKET, LOG_FILE, SOCKET_PATH

HERE = os.path.dirname(os.path.abspath(__file__))
CLI_PATH = os.path.join(HERE, "dunkingsheep")
START_TIMEOUT_S = 8.0


SYSTEMD_UNIT = "dunkingsheep.service"


UNIT_PATH = os.path.expanduser("~/.config/systemd/user/dunkingsheep.service")


def _systemd_installed():
    """True when the systemd user unit is installed.

    Then the supervisor owns the daemon: a client asks systemd to start it and
    never spawns a second `serve`, which would race the unit for the socket and
    leave a restart loop.
    """
    return os.path.exists(UNIT_PATH)


def _systemctl(*args):
    """Run systemctl --user, supplying XDG_RUNTIME_DIR when the caller's shell
    does not have one (agents often do not)."""
    env = dict(os.environ)
    runtime = env.get("XDG_RUNTIME_DIR") or f"/run/user/{os.getuid()}"
    if os.path.isdir(runtime):
        env["XDG_RUNTIME_DIR"] = runtime
    return subprocess.run(["systemctl", "--user", *args],
                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                          timeout=30, env=env)


class DaemonUnavailable(RuntimeError):
    """The daemon is not running and could not be started."""


def _current_pane_id():
    """The herdr pane this process is actually in, per herdr 0.9.5.

    This is an *opt-in verification* (`DUNKINGSHEEP_VERIFY_SELF=1`), not the
    normal identity path. The normal path trusts the pane id the terminal
    exports (HERDR_PANE_ID / TMUX_PANE), which costs nothing; asking herdr for
    `pane current` costs 1-2 s and is only useful after a restore may have
    cloned a stale env. Only consulted inside a tty with a pane id set:
    outside a pane the command reports the focused pane, which is not us.
    """
    if not os.environ.get("HERDR_PANE_ID"):
        return None
    if not (os.isatty(0) or os.path.exists("/dev/tty")):
        return None
    try:
        result = subprocess.run(["herdr", "pane", "current"],
                                capture_output=True, text=True, timeout=5)
        if result.returncode != 0 or not result.stdout.strip():
            return None
        payload = json.loads(result.stdout)
        pane = (payload.get("result") or {}).get("pane") or {}
        return pane.get("pane_id")
    except (OSError, ValueError, subprocess.SubprocessError):
        return None


class IdentityRefused(DunkError):
    """The caller's identity could not be established without guessing.

    A separate class from a plain DunkError so a caller can tell "you are not a
    lane" from "the thing you asked for is wrong", and so tests can assert the
    refusal without matching prose.
    """


def controlling_tty(pid=None, max_depth=12):
    """The /dev/pts/N of the nearest process in this chain that has one.

    An agent's command runner often has NO controlling terminal of its own (the
    harness gives it a pipe), while the agent process above it owns the pane's
    pty. So the search walks up the parent chain - the same shape as the caller
    identity in dunk_callers - rather than reading fd 0, which is a pipe here
    (measured 2026-10-24: fd0 was "pipe:[...]" while the agent process sat on
    /dev/pts/58).
    """
    pid = pid or os.getpid()
    for _ in range(int(max_depth)):
        try:
            with open(f"/proc/{pid}/stat", "rb") as handle:
                parts = handle.read().rsplit(b")", 1)[1].split()
            tty_nr = int(parts[4])
            ppid = int(parts[1])
        except (OSError, IndexError, ValueError):
            return None
        if tty_nr:
            major, minor = os.major(tty_nr), os.minor(tty_nr)
            return f"/dev/pts/{minor}" if 136 <= major <= 143 else None
        if ppid <= 1:
            return None
        pid = ppid
    return None


def pane_for_tty(tty=None, runner=None):
    """(pane_id, reason): which tmux pane owns this terminal, or why we cannot say.

    Refuses on zero matches and on more than one: a TTY that names two panes is
    not an identity, it is a question, and guessing is how a lane gets another
    lane's mail.
    """
    tty = tty or controlling_tty()
    if not tty:
        return None, "no controlling terminal found in this process chain"
    run = runner or (lambda argv: subprocess.run(argv, capture_output=True, text=True,
                                                 timeout=5))
    try:
        result = run(["tmux", "list-panes", "-a", "-F", "#{pane_id} #{pane_tty}"])
    except (OSError, subprocess.SubprocessError) as error:
        return None, f"could not ask tmux for panes: {error}"
    if getattr(result, "returncode", 1) != 0:
        return None, f"tmux refused: {(getattr(result, 'stderr', '') or '').strip()}"
    matches = [line.split()[0] for line in (result.stdout or "").splitlines()
               if len(line.split()) == 2 and line.split()[1] == tty]
    if len(matches) == 1:
        return matches[0], f"resolved by {tty}"
    if not matches:
        return None, f"no tmux pane owns {tty}"
    return None, f"{tty} is claimed by {len(matches)} panes ({', '.join(matches)})"


def exported_pane_id(environ=None):
    """The pane this process is actually in, per the terminal that owns it.

    TMUX_PANE wins over HERDR_PANE_ID on purpose. A tmux pane brought up for a
    herdr lane inherits that lane's environment, so HERDR_PANE_ID keeps naming
    the pane the lane used to be in. Measured 2026-10-24: every tmux pane in the
    fleet carried its dead herdr twin's id, so this lane (tmux %4, title
    'π - dunkingsheep') believed it was w9:pH and read, wrote and *attributed*
    mail as the vp lane. An id the terminal exports for the terminal we are in
    cannot be stale; one inherited from the other backend can be.
    """
    env = os.environ if environ is None else environ
    return env.get("TMUX_PANE") or env.get("HERDR_PANE_ID") or None


def resolve_identity(explicit=None, environ=None, tty_lookup=None, detect=None):
    """(pane_id, note) for this caller, refusing only a LYING environment.

    The terminal we are IN beats a claim inherited from somewhere else, and a claim
    that disagrees with it is refused rather than preferred: an inherited TMUX_PANE
    is exactly how this lane spent a day reading another lane's mail (2026-10-24).

    The distinction that matters, and that a first version got wrong: an ABSENT
    identity is not a lying one. A systemd unit (dispatch, health, state, watchdog,
    deadman, desk) has no pane and no pty - that is what a unit looks like - so it
    is ANONYMOUS. Refusing there took every one of those units down (VP i24050).
    A refusal is for the case where a terminal EXISTS and either disagrees with the
    claim or does not resolve to exactly one pane.

    The TTY fallback is for IDENTITY ONLY: it confers no superuser and no
    capability (drains stays observed).
    """
    env = os.environ if environ is None else environ
    if explicit is not None:
        return explicit, "explicit"
    if env.get("DUNKINGSHEEP_VERIFY_SELF") == "1":
        detect = _current_pane_id if detect is None else detect
        detected = detect()
        if detected:
            return detected, "verified by the backend (VERIFY_SELF)"
    claimed = (env.get("TMUX_PANE") or "").strip()
    herdr_claim = (env.get("HERDR_PANE_ID") or "").strip()
    claim = claimed or herdr_claim
    if env.get("DUNKINGSHEEP_TTY_IDENTITY", "1") == "0":
        detect = _current_pane_id if detect is None else detect
        return (claim or detect() or None), "env only (tty check off)"

    lookup = tty_lookup or pane_for_tty
    pane, note = lookup()
    if claim:
        if pane and pane != claim:
            raise IdentityRefused(
                f"identity refused: TMUX_PANE/HERDR_PANE_ID says {claim!r} but this "
                f"terminal is pane {pane!r} ({note}). The terminal you are in wins, "
                "and a claim that disagrees with it is not corrected silently - if "
                "you inherited it from another lane, unset it or set it to yours.")
        return claim, (f"env, confirmed by {note}" if pane else f"env only ({note})")
    if pane:
        return pane, f"resolved by terminal ({note})"
    if _terminal_exists(lookup):
        # A terminal exists but identifies no single pane: a question, not an id.
        raise IdentityRefused(
            f"identity refused: the terminal exists but resolved to no single pane "
            f"({note}). Set TMUX_PANE, or run inside a tmux pane.")
    detect = _current_pane_id if detect is None else detect
    detected = detect()
    if detected:
        return detected, "detected by the backend (no terminal in this process chain)"
    return None, f"anonymous: no pane in the environment and no terminal ({note})"


def _terminal_exists(lookup):
    """True when the lookup's reason says a TERMINAL was found but did not resolve.

    pane_for_tty reports 'no controlling terminal found…' when there is none, and
    '<tty> is claimed by N panes' / 'no tmux pane owns <tty>' when one exists. Only
    the second class is a refusal.
    """
    try:
        _pane, note = lookup()
    except Exception:                            # noqa: BLE001 - absence is not an error
        return False
    text = str(note or "")
    return (text.startswith("/dev/") or "owns /dev/" in text or "claimed by" in text)


class FlockClient:
    def __init__(self, socket_path=None, self_pane_id=None, timeout=15.0):
        self.socket_path = socket_path or SOCKET_PATH
        explicit = self_pane_id is not None
        if explicit:
            self.self_pane_id, self.identity_note = self_pane_id, "explicit"
        else:
            self.self_pane_id, self.identity_note = resolve_identity()
        # The terminal already exported our identity. Only pay for a
        # `pane current` round-trip when asked to verify a possibly-stale env.
        if not explicit and (
            self.self_pane_id is None
            or os.environ.get("DUNKINGSHEEP_VERIFY_SELF") == "1"
        ):
            self.self_pane_id = _current_pane_id() or self.self_pane_id
        self.timeout = timeout
        self._next_id = 1

    @staticmethod
    def service_token():
        """A service token for a pane-less process (web desk, timers), from
        DUNKINGSHEEP_SERVICE_TOKEN or the 0600 file DUNKINGSHEEP_SERVICE_TOKEN_FILE."""
        token = (os.environ.get("DUNKINGSHEEP_SERVICE_TOKEN") or "").strip()
        if token:
            return token
        path = (os.environ.get("DUNKINGSHEEP_SERVICE_TOKEN_FILE") or "").strip()
        if path:
            try:
                with open(path, encoding="utf-8") as handle:
                    return handle.read().strip() or None
            except OSError:
                return None
        return None

    # -- transport ------------------------------------------------------------

    def call(self, cmd, **args):
        """Send one command; return its result or raise DunkError / OSError."""
        # A SYSTEM SENDER has no pane by definition (the watchdog runs from a timer).
        # Attaching one - from the environment or a parent service - made the daemon
        # refuse the claim, so monitoring went silent while the rule still fired.
        pane_id = None if args.get("system_sender") else self.self_pane_id
        request = {
            "id": self._next_id,
            "cmd": cmd,
            "args": {key: value for key, value in args.items() if value is not ...},
            "self_pane_id": pane_id,
        }
        token = self.service_token()
        if token:
            request["service_token"] = token
        self._next_id += 1
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(self.timeout)
        # Retry only the connect: a 20-agent burst can overflow a short listen
        # backlog. Once connected, do not retry (the command may have run).
        for attempt in range(3):
            try:
                sock.connect(self.socket_path)
                break
            except (ConnectionRefusedError, ConnectionResetError,
                    TimeoutError, socket.timeout):
                if attempt == 2:
                    raise
                time.sleep(0.05 * (attempt + 1))
        try:
            sock.sendall((json.dumps(request) + "\n").encode("utf-8"))
            reader = sock.makefile("rb")
            raw = reader.readline()
        finally:
            sock.close()
        if not raw:
            raise DaemonUnavailable("daemon closed the connection without replying")
        response = json.loads(raw.decode("utf-8", "replace"))
        if not response.get("ok"):
            raise DunkError(response.get("error") or "unknown error")
        return response.get("result")

    def __getattr__(self, name):
        if name in COMMANDS_BY_NAME:
            return lambda **args: self.call(name, **args)
        raise AttributeError(name)

    # -- availability -----------------------------------------------------------

    def is_running(self):
        try:
            self.call("status")
            return True
        except (OSError, DaemonUnavailable, DunkError, ValueError):
            return False

    @classmethod
    def connect_or_start(cls, socket_path=None, self_pane_id=None, start=True,
                         timeout=START_TIMEOUT_S):
        """Return a client whose daemon is reachable, starting one if needed."""
        client = cls(socket_path=socket_path, self_pane_id=self_pane_id)
        if client.is_running():
            return client
        if not start:
            raise DaemonUnavailable(f"no dunkingsheep daemon on {client.socket_path}")
        client.start_daemon()
        deadline = time.time() + timeout
        while time.time() < deadline:
            if client.is_running():
                return client
            time.sleep(0.1)
        raise DaemonUnavailable(
            f"daemon did not come up on {client.socket_path}; see {LOG_FILE}"
        )

    def start_daemon(self):
        """Start the daemon: hand off to systemd when it supervises, else spawn
        a detached `dunkingsheep serve` that outlives this process.

        Refuse a partial backend environment: a client that auto-starts with
        only DUNKINGSHEEP_DIR/SOCKET once started a tmux trial daemon on the
        herdr backend, where it silently saw the wrong panes. A component that
        guesses its environment produces a plausible wrong answer.
        """
        # The literal default socket belongs to the unit, even when a parent
        # service exported DUNKINGSHEEP_SOCKET to that same path.
        supervised = self.socket_path == DEFAULT_SOCKET
        if supervised and _systemd_installed() and shutil.which("systemctl"):
            try:
                probe = _systemctl("start", SYSTEMD_UNIT)
            except (OSError, subprocess.SubprocessError):
                probe = None
            if probe is not None and probe.returncode == 0:
                return
            raise DaemonUnavailable(
                "the dunkingsheep.service unit is installed but could not be "
                "started; run `systemctl --user start dunkingsheep.service`")
        # The backend the DAEMON would use, from the one place that defines the default. This
        # carried its own literal "herdr" long after tmux became the default, and it had teeth:
        # with the variable unset the guard below never fired, so auto-start proceeded without the
        # tmux-socket requirement - the very failure its own docstring describes (reviewer on #40).
        #
        # THE GUARD APPLIES TO THE SPAWN PATH ONLY, and that ordering is deliberate: the historical
        # failure was a client SPAWNING a trial daemon with a guessed environment. A unit started by
        # systemd carries its own environment, so requiring DUNKINGSHEEP_TMUX_SOCKET in the CALLER
        # would refuse the handoff to a correctly configured daemon - a fleet-wide outage caused by
        # a fix. The guard belongs where the guess would happen.
        from dunk_terminal import backend_name as resolve_backend_name
        backend = resolve_backend_name()
        if backend == "tmux" and not os.environ.get("DUNKINGSHEEP_TMUX_SOCKET"):
            raise DaemonUnavailable(
                "refusing to auto-start a tmux daemon without "
                "DUNKINGSHEEP_TMUX_SOCKET; start it explicitly with the full "
                "backend environment")
        os.makedirs(CONFIG_DIR, exist_ok=True)
        env = dict(os.environ)
        env["DUNKINGSHEEP_SOCKET"] = self.socket_path
        # A TEST-SPAWNED DAEMON MUST NOT OUTLIVE ITS PARENT (t212). A suite run that is killed,
        # times out or crashes cannot run its exit sweep, and the detached daemon then sits at ppid=1
        # forever: 121 of them once put this host at load1 65. The suite marks its spawns with
        # DUNKINGSHEEP_TEST_SPAWN=1 and the daemon then watches the spawning pid and exits when it
        # goes away. A normal auto-start is unchanged - a lane's daemon is supposed to outlive the
        # call that started it.
        env.pop("HERDR_PANE_ID", None)
        if env.get("DUNKINGSHEEP_TEST_SPAWN") == "1":
            # The SUITE's pid, not this CLI call's: a test invokes the CLI many times, and each call is
            # a short-lived process. Tying the daemon to the caller would kill it between calls and
            # every subsequent command would auto-start a fresh one (two test_cli tests failed exactly
            # that way). The suite exports its own pid, and that is what the daemon watches.
            env.setdefault("DUNKINGSHEEP_DIE_WITH_PARENT", str(os.getpid()))
        log_handle = open(LOG_FILE, "ab")
        # A PROCESS-GROUP BOUND, not just a parent watch (reviewer, t212): a child the daemon spawned
        # outlives a parent-watch, because it is reparented to init when the daemon dies. Two mechanisms,
        # one for each way a run can end:
        #   * PR_SET_PDEATHSIG on the daemon: the kernel kills it when the spawning process dies, even
        #     by SIGKILL, which no userspace watch can survive;
        #   * the daemon is a session leader (start_new_session=True) and kills its whole process group
        #     on shutdown, so its children go with it.
        # Both are limited to test spawns: a lane's auto-started daemon must outlive the call.
        test_spawn = env.get("DUNKINGSHEEP_TEST_SPAWN") == "1"
        # PR_SET_PDEATHSIG arms against the process that FORKS, so it is only correct when that process
        # is the one being watched. In the CLI path the forker is a short-lived CLI call, so arming it
        # SIGTERMs the daemon the moment the call returns - the daemon then dies between commands and
        # every later command auto-starts a fresh one (reviewer pi-review-2 measured this as
        # test_cli's send_count reading 0 and a socket reset mid-line). It is defence in depth for a
        # DIRECT spawn, where the forker is the suite; the userspace watch covers the CLI path.
        watched_pid = env.get("DUNKINGSHEEP_DIE_WITH_PARENT") or ""
        preexec = None
        if test_spawn and watched_pid == str(os.getpid()):
            def preexec():                      # runs in the child, before exec
                try:
                    import ctypes
                    libc = ctypes.CDLL("libc.so.6", use_errno=True)
                    PR_SET_PDEATHSIG = 1
                    # SIGTERM, not SIGKILL: the daemon must still be able to run its shutdown and kill
                    # its process group. A SIGKILL from the kernel would leave its children orphaned,
                    # which is precisely the leak this is meant to bound.
                    libc.prctl(PR_SET_PDEATHSIG, 15)
                except Exception:               # noqa: BLE001 - the group bound still applies
                    pass
        try:
            subprocess.Popen(
                [sys.executable, CLI_PATH, "serve"],
                stdin=subprocess.DEVNULL,
                stdout=log_handle,
                stderr=log_handle,
                start_new_session=True,
                preexec_fn=preexec,
                close_fds=True,
                cwd=HERE,
                env=env,
            )
        finally:
            log_handle.close()
