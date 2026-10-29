#!/usr/bin/env python3
"""
Dunking Sheep daemon: owns the `Flock` and serves it over a unix socket.

Protocol (newline-delimited JSON, any number of requests per connection):

    -> {"id": 1, "cmd": "add_dunk", "args": {...}, "self_pane_id": "w8:p8"}
    <- {"id": 1, "ok": true, "result": {...}}
    <- {"id": 1, "ok": false, "error": "no dunk with id 'd9'"}

`cmd` is any name in `dunk_api.COMMANDS`. `self_pane_id` is optional and lets
`target: "self"` refer to the caller's pane. Socket path defaults to
`~/.config/dunkingsheep/dunkingsheep.sock` (override with DUNKINGSHEEP_SOCKET).
"""

import errno
import json
import logging
import os
import signal
import socket
import sqlite3
import struct
import sys
import time
import threading

from dunk_api import dispatch
from dunk_callers import caller_from_peer
from dunk_core import CONFIG_DIR, VERSION, DunkError, Flock

DEFAULT_SOCKET = os.path.join(CONFIG_DIR, "dunkingsheep.sock")
SOCKET_PATH = os.environ.get("DUNKINGSHEEP_SOCKET") or DEFAULT_SOCKET
LOG_FILE = os.path.join(CONFIG_DIR, "daemon.log")

log = logging.getLogger("dunkingsheep.server")


class AlreadyRunning(RuntimeError):
    pass


class FlockServer:
    def __init__(self, flock, socket_path=SOCKET_PATH):
        self.flock = flock
        self.socket_path = socket_path
        self.sock = None
        self.stop_event = threading.Event()
        self._connections = set()
        self._conn_lock = threading.Lock()

    # -- lifecycle ----------------------------------------------------------

    def bind(self):
        os.makedirs(os.path.dirname(self.socket_path) or ".", exist_ok=True)
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            sock.bind(self.socket_path)
        except OSError as error:
            if error.errno != errno.EADDRINUSE:
                sock.close()
                raise
            if _socket_alive(self.socket_path):
                sock.close()
                raise AlreadyRunning(
                    f"a daemon is already listening on {self.socket_path}"
                ) from None
            # Stale socket file left by a daemon that died; reclaim it.
            os.unlink(self.socket_path)
            sock.bind(self.socket_path)
        os.chmod(self.socket_path, 0o600)
        sock.listen(128)
        sock.settimeout(0.5)
        self.sock = sock
        log.info("dunkingsheep %s listening on %s (pid %d)", VERSION, self.socket_path, os.getpid())

    def serve_forever(self):
        if self.sock is None:
            self.bind()
        try:
            while not self.stop_event.is_set():
                try:
                    conn, _addr = self.sock.accept()
                except socket.timeout:
                    continue
                except OSError:
                    if self.stop_event.is_set():
                        break
                    raise
                thread = threading.Thread(target=self._serve_connection, args=(conn,), daemon=True)
                thread.start()
        finally:
            self.close()

    def shutdown(self):
        self.stop_event.set()

    def close(self):
        self.flock.close()
        if self.sock is not None:
            try:
                self.sock.close()
            except OSError:
                pass
            self.sock = None
        try:
            if os.path.exists(self.socket_path):
                os.unlink(self.socket_path)
        except OSError:
            pass
        log.info("daemon stopped")

    # -- requests -----------------------------------------------------------

    def _serve_connection(self, conn):
        with self._conn_lock:
            self._connections.add(conn)
        try:
            conn.settimeout(None)
            peer_pid = self._peer_pid(conn)
            reader = conn.makefile("rb")
            for raw in reader:
                line = raw.decode("utf-8", "replace").strip()
                if not line:
                    continue
                response, stop_after = self.handle_line(line, peer_pid=peer_pid)
                conn.sendall((json.dumps(response) + "\n").encode("utf-8"))
                if stop_after:
                    self.shutdown()
                    break
        except (BrokenPipeError, ConnectionResetError):
            pass
        except Exception:  # noqa: BLE001 - one bad client must not kill the daemon
            log.exception("connection handler crashed")
        finally:
            with self._conn_lock:
                self._connections.discard(conn)
            try:
                conn.close()
            except OSError:
                pass

    @staticmethod
    def _peer_pid(conn):
        """The connecting process, from SO_PEERCRED (Linux); None elsewhere."""
        try:
            raw = conn.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED,
                                  struct.calcsize("3i"))
            pid, _uid, _gid = struct.unpack("3i", raw)
            return pid or None
        except (AttributeError, OSError, struct.error):
            return None

    def handle_line(self, line, peer_pid=None):
        """Return (response dict, stop_after).

        The socket peer is the short-lived CLI; the caller identity is its
        nearest agent ancestor (or session leader), resolved by dunk_callers
        and attached to task leases for `/proc` liveness.
        """
        try:
            request = json.loads(line)
        except ValueError:
            return {"id": None, "ok": False, "error": "invalid JSON"}, False
        if not isinstance(request, dict):
            return {"id": None, "ok": False, "error": "request must be an object"}, False
        req_id = request.get("id")
        name = request.get("cmd")
        args = request.get("args") or {}
        self_pane_id = request.get("self_pane_id")
        if not isinstance(args, dict):
            return {"id": req_id, "ok": False, "error": "args must be an object"}, False
        caller = caller_from_peer(peer_pid, self_pane_id) if peer_pid else None
        self.flock.set_caller(caller)
        # Transport provenance for completion authorization (t423): a socket
        # request whose peer credentials failed must not degrade into the
        # absent-caller state that trusted in-process code uses.
        self.flock.set_socket_peer(peer_pid is not None)
        try:
            try:
                result = dispatch(self.flock, name, args,
                                  self_pane_id=self_pane_id)
            except DunkError as error:
                reply = {"id": req_id, "ok": False, "error": str(error)}
                if getattr(error, "retryable", False):
                    reply["retryable"] = True
                return reply, False
            except Exception as error:  # noqa: BLE001
                if (isinstance(error, sqlite3.OperationalError)
                        and (getattr(error, "sqlite_errorcode", 0) & 255)
                        in (sqlite3.SQLITE_BUSY, sqlite3.SQLITE_LOCKED)):
                    return {"id": req_id, "ok": False, "retryable": True,
                            "error": "database busy; retry the request"}, False
                log.exception("command %s failed", name)
                return {"id": req_id, "ok": False,
                        "error": f"internal error: {error}"}, False
        finally:
            self.flock.clear_caller()
            self.flock.clear_socket_peer()
        return {"id": req_id, "ok": True, "result": result}, name == "shutdown"


def _socket_alive(path):
    """True if something accepts connections on the unix socket at `path`."""
    probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    probe.settimeout(1.0)
    try:
        probe.connect(path)
        return True
    except OSError:
        return False
    finally:
        probe.close()


def configure_logging(to_file=True, level=logging.INFO):
    """Log to the daemon log file, plus stderr when it is a terminal (a detached
    daemon has stderr redirected into the same file, so skip it then)."""
    handlers = []
    if to_file:
        try:
            os.makedirs(CONFIG_DIR, exist_ok=True)
            handlers.append(logging.FileHandler(LOG_FILE))
        except OSError:
            pass
    if not handlers or sys.stderr.isatty():
        handlers.append(logging.StreamHandler(sys.stderr))
    logging.basicConfig(
        level=level,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
        handlers=handlers,
        force=True,
    )


def kill_own_process_group(reason="shutdown"):
    """SIGTERM the daemon's whole process group (t212).

    A child outlives a parent watch: when the daemon dies its children are reparented to init and keep
    polling - the guard shim forks twice per poll, and 218 of those were measured at load 78. For a
    TEST spawn the daemon is a session leader, so signalling its group is the bound that holds. It is a
    no-op for a lane's auto-started daemon, which must outlive the call that started it.
    """
    if os.environ.get("DUNKINGSHEEP_TEST_SPAWN") != "1":
        return False
    try:
        os.killpg(os.getpgid(0), signal.SIGTERM)
        return True
    except OSError:
        return False


def run_daemon(socket_path=SOCKET_PATH, state_file=None, log_to_file=True):
    """Blocking entry point used by `dunkingsheep serve`."""
    configure_logging(to_file=log_to_file)
    unit_path = os.path.expanduser("~/.config/systemd/user/dunkingsheep.service")
    if (socket_path == DEFAULT_SOCKET
            and os.environ.get("DUNKINGSHEEP_SUPERVISED") != "1"
            and os.path.exists(unit_path)):
        # systemd owns the default socket now. A direct spawn here would steal
        # it from the unit and put it in a restart loop.
        log.error("dunkingsheep.service supervises this daemon; refusing an "
                  "unsupervised start. Run: systemctl --user start "
                  "dunkingsheep.service")
        return 3
    # GATE 5 (the one that matters after 2026-10-24): a daemon started on a backend
    # that sees NO panes would run, answer status, and fail every delivery with
    # 'no server running on /tmp/tmux-1000/default'. That is not a degraded mode, it is
    # a wrong environment - refuse to start and name it.
    # The name in this REFUSAL must be the backend the daemon would actually use. It defaulted
    # to "herdr" long after tmux became the code default (2.70.29), so a refusal could name a
    # backend nobody was running - in a message whose whole purpose is to say what to fix.
    # dunk_terminal owns the default, so ask it rather than repeating it.
    from dunk_terminal import DEFAULT_BACKEND_NAME, backend_name as resolve_backend_name
    backend_name = resolve_backend_name()
    if os.environ.get("DUNKINGSHEEP_ALLOW_EMPTY_BACKEND") != "1":
        from dunk_terminal import get_backend
        try:
            probe = get_backend()          # honours DUNKINGSHEEP_TERMINAL_BACKEND itself
            panes = probe.list_panes()
        except Exception as error:                      # noqa: BLE001 - a refusal, not a crash
            log.error("refusing to start: backend %r cannot list panes: %s", backend_name, error)
            return 4
        if not panes:
            log.error(
                "refusing to start: backend %r sees zero panes. This daemon delivers by "
                "reading and typing into panes; with none it would accept every message "
                "and fail every delivery. Start it in an environment where the backend "
                "has panes, or set DUNKINGSHEEP_ALLOW_EMPTY_BACKEND=1 for a deliberate "
                "trial.", backend_name)
            return 4
    kwargs = {"state_file": state_file} if state_file else {}
    flock = Flock(**kwargs)
    server = FlockServer(flock, socket_path=socket_path)
    try:
        server.bind()
    except AlreadyRunning as error:
        log.error("%s", error)
        return 3

    def _kill_group():
        kill_own_process_group("daemon shutdown")

    def _on_signal(signum, _frame):
        log.info("received signal %s, shutting down", signum)
        _kill_group()
        server.shutdown()

    signal.signal(signal.SIGTERM, _on_signal)
    signal.signal(signal.SIGINT, _on_signal)

    # DIE WITH THE PARENT (t212): a test-spawned daemon watches the pid that spawned it and exits when
    # that pid goes away. Without this a killed suite leaves detached daemons at ppid=1 that nothing
    # sweeps - 121 of them were measured at load1 65.7 on 2026-10-27. Only set for test spawns, so a
    # lane's auto-started daemon still outlives the call that started it.
    # The client passes the spawning pid explicitly; a test that spawns `serve` directly gets the same
    # guarantee from its own parent pid, because TEST_SPAWN is set for the whole run.
    _parent = os.environ.get("DUNKINGSHEEP_DIE_WITH_PARENT", "").strip()
    if not _parent and os.environ.get("DUNKINGSHEEP_TEST_SPAWN") == "1":
        _parent = str(os.getppid())
    if _parent.isdigit():
        def _watch_parent(pid=int(_parent)):
            while True:
                time.sleep(2.0)
                try:
                    os.kill(pid, 0)
                except OSError:
                    log.warning("parent pid %d is gone; a test-spawned daemon exits with it", pid)
                    # Kill the group and exit HARD. Calling server.shutdown() from this thread
                    # deadlocked the daemon (measured: the watchdog logged and the process was still
                    # alive 12 s later), because shutdown() waits for serve_forever to return and
                    # serve_forever is blocked in the main thread. A test daemon whose spawner is gone
                    # has nothing left to flush: the group kill is the bound, exiting is the point.
                    try:
                        _kill_group()
                    except Exception:      # noqa: BLE001 - the group kill is best-effort
                        pass
                    os._exit(0)
        import threading
        threading.Thread(target=_watch_parent, name="die-with-parent", daemon=True).start()
    server.serve_forever()
    return 0


if __name__ == "__main__":
    sys.exit(run_daemon())
