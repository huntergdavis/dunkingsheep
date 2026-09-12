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
import socket
import subprocess
import sys
import time

from dunk_api import COMMANDS_BY_NAME
from dunk_core import CONFIG_DIR, DunkError
from dunk_server import LOG_FILE, SOCKET_PATH

HERE = os.path.dirname(os.path.abspath(__file__))
CLI_PATH = os.path.join(HERE, "dunkingsheep")
START_TIMEOUT_S = 8.0


class DaemonUnavailable(RuntimeError):
    """The daemon is not running and could not be started."""


class FlockClient:
    def __init__(self, socket_path=None, self_pane_id=None, timeout=15.0):
        self.socket_path = socket_path or SOCKET_PATH
        self.self_pane_id = (
            self_pane_id if self_pane_id is not None else os.environ.get("HERDR_PANE_ID")
        )
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
        try:
            sock.connect(self.socket_path)
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
        """Spawn a detached `dunkingsheep serve` that outlives this process."""
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
