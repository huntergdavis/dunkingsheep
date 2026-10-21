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


class FlockClient:
    def __init__(self, socket_path=None, self_pane_id=None, timeout=15.0):
        self.socket_path = socket_path or SOCKET_PATH
        explicit = self_pane_id is not None
        self.self_pane_id = (
            self_pane_id if explicit
            else os.environ.get("HERDR_PANE_ID") or os.environ.get("TMUX_PANE")
        )
        # The terminal already exported our identity. Only pay for a
        # `pane current` round-trip when asked to verify a possibly-stale env.
        if not explicit and (
            self.self_pane_id is None
            or os.environ.get("DUNKINGSHEEP_VERIFY_SELF") == "1"
        ):
            self.self_pane_id = _current_pane_id() or self.self_pane_id
        self.timeout = timeout
        self._next_id = 1

    # -- transport ------------------------------------------------------------

    def call(self, cmd, **args):
        """Send one command; return its result or raise DunkError / OSError."""
        request = {
            "id": self._next_id,
            "cmd": cmd,
            "args": {key: value for key, value in args.items() if value is not ...},
            "self_pane_id": self.self_pane_id,
        }
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
        backend = (os.environ.get("DUNKINGSHEEP_TERMINAL_BACKEND")
                   or "herdr").strip().lower()
        if backend == "tmux" and not os.environ.get("DUNKINGSHEEP_TMUX_SOCKET"):
            raise DaemonUnavailable(
                "refusing to auto-start a tmux daemon without "
                "DUNKINGSHEEP_TMUX_SOCKET; start it explicitly with the full "
                "backend environment")
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
        os.makedirs(CONFIG_DIR, exist_ok=True)
        env = dict(os.environ)
        env["DUNKINGSHEEP_SOCKET"] = self.socket_path
        # The daemon must not inherit the caller's pane identity.
        env.pop("HERDR_PANE_ID", None)
        log_handle = open(LOG_FILE, "ab")
        try:
            subprocess.Popen(
                [sys.executable, CLI_PATH, "serve"],
                stdin=subprocess.DEVNULL,
                stdout=log_handle,
                stderr=log_handle,
                start_new_session=True,
                close_fds=True,
                cwd=HERE,
                env=env,
            )
        finally:
            log_handle.close()
