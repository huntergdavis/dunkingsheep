"""Private Poolrad DOS oracle session streamed to the owner's authenticated desk."""

from __future__ import annotations

import hashlib
import json
import os
import re
import secrets
import shlex
import shutil
import signal
import subprocess
import threading
import time
from datetime import datetime, timezone
from pathlib import Path

from server.emulator_build import describe_emulator, pinned_binary
from server.state_archive import archive_status


class GameError(Exception):
    pass


class GameSession:
    SAVE_SLOT_DEFAULT = 1
    SAVE_SLOT_MAX = 99
    FRAME_INSET = 16
    KEYS = {"Up", "Down", "Left", "Right", "Return", "Escape", "space", "BackSpace", "Tab", "Delete", "Home", "End", "Page_Up", "Page_Down"}
    KEYS.update(chr(n) for n in range(ord("a"), ord("z") + 1))
    KEYS.update(str(n) for n in range(10))
    KEYS.update(f"F{n}" for n in range(1, 13))

    def __init__(self, repo: Path, data: Path):
        self.repo = repo
        self.data = data
        self.run_root = repo / "resources" / "oracle-runs"
        self.last_file = data / "poolrad-last-run.txt"
        self.lock = threading.RLock()
        self._streams: dict[str, subprocess.Popen] = {}
        self.display: str | None = None
        self.run_dir: Path | None = None
        self.xvfb: subprocess.Popen | None = None
        self.tmux_session: str | None = None
        self.sink_module: str | None = None
        self.sink_name: str | None = None
        self.active_slot = self.SAVE_SLOT_DEFAULT
        self.play_build = os.getenv("VP_PLAY_BUILD", "original")
        if self.play_build not in {"original", "replacement"}:
            raise GameError("VP_PLAY_BUILD must be original or replacement")
        configured = (os.getenv("VP_REPLACEMENT_DOSBOX_X", pinned_binary())
                      if self.play_build == "replacement" else os.getenv("VP_DOSBOX_X", "/usr/bin/dosbox-x"))
        resolved = shutil.which(configured)
        if resolved is not None:
            self.dosbox_x = str(Path(resolved).resolve())
        elif Path(configured).is_absolute():
            self.dosbox_x = configured  # Status/start reports an unavailable absolute executable.
        else:
            raise GameError("Configured DOSBox-X executable is unavailable on the server PATH")
        self._launched_emulator: dict | None = None
        self.archive_manifest = Path(os.getenv("VP_STATE_ARCHIVE_MANIFEST",
                                              str(repo / "resources/state-archives/manifest.json")))
        self.log_files = []

    def _run(self, cmd: list[str], **kwargs):
        return subprocess.run(cmd, check=True, capture_output=True, text=True, timeout=70, **kwargs)

    def _valid_run(self, name: str) -> Path:
        if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", name):
            raise GameError("Invalid run name")
        path = self.run_root / name
        if not (path / ".oracle-input-sha256").is_file() or not (path / "POOLRAD" / "START.EXE").is_file():
            raise GameError("Prepared Poolrad run is unavailable")
        return path

    def status(self) -> dict:
        with self.lock:
            if self.tmux_session and not self._game_running():
                self._stop_locked()
            last = self.last_file.read_text().strip() if self.last_file.exists() else None
            running = self.tmux_session is not None
            identity = (self._launched_emulator or dict(kind="other", build_id=None, sha256=None, release=None)
                        if running else describe_emulator(self.dosbox_x))
            return {"running": self.tmux_session is not None, "audio": self.sink_name is not None,
                    "build": self.play_build,
                    "emulator": {**identity, "scope": "running_session" if running else "next_start"},
                    "state_archive": archive_status(self.archive_manifest),
                    "run_name": self.run_dir.name if self.run_dir else last}

    def _game_running(self) -> bool:
        return bool(self.tmux_session and subprocess.run(
            ["tmux", "-L", "vp-poolrad", "has-session", "-t", self.tmux_session],
            capture_output=True, timeout=2).returncode == 0)

    def recover_orphans(self):
        """Clear only the dedicated game's terminal and Pulse sinks after a crash."""
        subprocess.run(["tmux", "-L", "vp-poolrad", "kill-server"], capture_output=True, timeout=5)
        try:
            result = subprocess.run(["pactl", "list", "short", "modules"], capture_output=True, text=True, timeout=5)
            for line in result.stdout.splitlines():
                fields = line.split("\t", 3)
                if len(fields) >= 3 and fields[1] == "module-null-sink" and re.search(r"\bsink_name=vp_poolrad_\d+\b", fields[2]):
                    subprocess.run(["pactl", "unload-module", fields[0]], capture_output=True, timeout=5)
        except (OSError, subprocess.SubprocessError):
            pass

    def start(self, fresh: bool = False) -> dict:
        with self.lock:
            if self._game_running():
                return self.status()
            self._stop_locked()
            if not self.repo.is_dir():
                raise GameError("Poolrad checkout is unavailable")
            if fresh or not self.last_file.exists():
                name = "vp-" + datetime.now(timezone.utc).strftime("%Y%m%d-%H%M%S") + "-" + secrets.token_hex(2)
                try:
                    self._run([str(self.repo / "tools/oracle/prepare_run.sh"), name], cwd=self.repo)
                except (OSError, subprocess.SubprocessError) as exc:
                    raise GameError(f"Could not prepare verified game copy: {exc}") from exc
            else:
                name = self.last_file.read_text().strip()
            run_dir = self._valid_run(name)
            self.run_dir = run_dir
            self.last_file.write_text(name + "\n")
            self.last_file.chmod(0o600)
            try:
                play_env = self._play_environment(run_dir)
                launch_identity = describe_emulator(self.dosbox_x)
                if launch_identity["sha256"] is None:
                    raise GameError("Configured DOSBox-X binary is unavailable")
                for n in range(90, 130):
                    if Path(f"/tmp/.X{n}-lock").exists() or Path(f"/tmp/.X11-unix/X{n}").exists():
                        continue
                    display = f":{n}"
                    auth = run_dir / ".vp-xauthority"
                    auth.touch(mode=0o600, exist_ok=True)
                    self._run(["xauth", "-f", str(auth), "add", display, ".", secrets.token_hex(16)])
                    log = (run_dir / "vp-xvfb.log").open("ab")
                    self.log_files.append(log)
                    xvfb = subprocess.Popen(["Xvfb", display, "-screen", "0", "800x600x24", "-nolisten", "tcp", "-auth", str(auth)],
                                            stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
                    for _ in range(30):
                        if xvfb.poll() is not None:
                            break
                        if self._x_ready(display, auth):
                            self.xvfb = xvfb
                            self.display = display
                            break
                        time.sleep(0.1)
                    if self.xvfb:
                        break
                    xvfb.terminate()
                if not self.xvfb:
                    raise GameError("Could not start private display")
                sink = "vp_poolrad_" + str(os.getpid())
                try:
                    result = self._run(["pactl", "load-module", "module-null-sink", f"sink_name={sink}", "rate=48000", "channels=2"])
                    self.sink_module = result.stdout.strip()
                    self.sink_name = sink
                except (OSError, subprocess.SubprocessError):
                    pass  # Video remains usable on a host without PulseAudio.
                env = os.environ.copy()
                env.update(play_env)
                env.update({"DISPLAY": self.display, "XAUTHORITY": str(auth),
                            "SDL_AUDIODRIVER": "pulse" if self.sink_name else "dummy",
                            "XDG_CONFIG_HOME": str(run_dir / ".config"),
                            "XDG_CACHE_HOME": str(run_dir / ".cache"),
                            "XDG_DATA_HOME": str(run_dir / ".local" / "share")})
                if self.sink_name:
                    env["PULSE_SINK"] = self.sink_name
                for key in (".config", ".cache", ".local/share", ".dosbox-saves"):
                    (run_dir / key).mkdir(parents=True, exist_ok=True)
                cmd = self._dosbox_command(run_dir)
                session = "oracle"
                env_args = self._launch_env_args(env, play_env)
                if self.sink_name:
                    env_args.append(f"PULSE_SINK={self.sink_name}")
                command = "exec " + shlex.join(["env", *env_args, *cmd])
                self._run(["tmux", "-L", "vp-poolrad", "new-session", "-d", "-s", session, "-x", "140", "-y", "40", command])
                self.tmux_session = session
                self.active_slot = self.SAVE_SLOT_DEFAULT
                # Record the session in new tags. A later load is verified by the
                # native loader's result; the session timestamp is provenance.
                self._session_started = time.time()
                self._run(["tmux", "-L", "vp-poolrad", "pipe-pane", "-o", "-t", session, f"cat >> {shlex.quote(str(run_dir / 'vp-dosbox-terminal.log'))}"])
                time.sleep(1)
                if not self._game_running():
                    raise GameError("DOSBox-X exited during startup; inspect private vp-dosbox-terminal.log")
                self._launched_emulator = launch_identity
                return self.status()
            except Exception as exc:
                self._stop_locked()
                if isinstance(exc, GameError):
                    raise
                raise GameError(f"Could not start Poolrad: {exc}") from exc

    def _dosbox_command(self, run_dir: Path) -> list[str]:
        """Both builds launch START.EXE; replacement additionally routes selected C bodies."""
        return ["bwrap", "--unshare-net", "--ro-bind", "/", "/", "--dev-bind", "/dev", "/dev",
                "--bind", str(run_dir), str(run_dir), "--chdir", str(run_dir),
                self.dosbox_x, "-defaultconf", "-defaultmapper", "-nogui", "-nomenu", "-fastlaunch",
                *(["-set", "cpu core=normal"] if self.play_build == "replacement" else []),
                "-set", "sdl windowresolution=640x480",
                "-set", f"dosbox saveslot={self.SAVE_SLOT_DEFAULT}",
                "-set", "dosbox saveremark=false",
                "-savedir", str(run_dir / ".dosbox-saves"),
                "-c", f"mount c {run_dir}", "-c", "c:", "-c", "cd \\POOLRAD", "-c", "START"]

    def _play_environment(self, run_dir: Path) -> dict[str, str]:
        if self.play_build == "original":
            return {}
        registry = Path(os.getenv("VP_REPLACEMENT_REGISTRY", str(self.repo / "resources/hybrid/registry_all.tsv")))
        dispatch_file = Path(os.getenv("VP_REPLACEMENT_DISPATCH", str(self.repo / "resources/hybrid/play-dispatch.txt")))
        if not shutil.which(self.dosbox_x):
            raise GameError("Replacement DOSBox-X binary is unavailable")
        try:
            registry_bytes = registry.read_bytes()
            dispatch = dispatch_file.read_text().strip()
        except (OSError, UnicodeError) as exc:
            raise GameError("Replacement registry or dispatch list is unavailable") from exc
        ids = dispatch.split(",")
        if (not ids or any(not re.fullmatch(r"(?:RES|OVL)_[0-9A-F]{4}_[0-9A-F]{4}", name) for name in ids)
                or len(set(ids)) != len(ids)):
            raise GameError("Replacement dispatch list must name unique registry entries")
        self._validate_replacement_registry(registry_bytes, ids)
        copied = run_dir / "vp-replacement-registry.tsv"
        copied.write_bytes(registry_bytes)
        (run_dir / "vp-replacement-dispatch.txt").write_text(dispatch + "\n")
        return {"PR_HYBRID_REGISTRY": str(copied), "PR_HYBRID_LOG": str(run_dir / "vp-replacement"),
                "PR_HYBRID_DISPATCH": dispatch, "PR_HYBRID_BUS_MODE": "route"}

    @staticmethod
    def _validate_replacement_registry(data: bytes, selected: list[str]) -> None:
        # Match the observer's physical LF framing, not Unicode splitlines or
        # text-mode newline translation. Keep the copied bytes unchanged.
        # The registry generator also emits count=..., '-' and comma-separated
        # C symbols in unselected/metadata columns.
        data_alphabet = frozenset(b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_=,-\t")
        comment_alphabet = frozenset(range(32, 127)) | {9}
        rows = {name: [] for name in selected}
        for line in data.split(b"\n"):
            # fgets(line, 4096, ...) must consume the complete physical line,
            # including LF. A long comment must never expose a hidden row.
            if len(line) > 4094:
                raise GameError("Invalid replacement registry physical line length")
            comment = line.startswith(b"#")
            if not set(line) <= (comment_alphabet if comment else data_alphabet):
                raise GameError("Invalid replacement registry byte outside format whitelist")
            if comment or not line:
                continue
            row = line.decode("ascii").split("\t")
            if row[0] in rows:
                rows[row[0]].append(row)
        for name, matches in rows.items():
            if len(matches) != 1:
                raise GameError(f"Replacement requires exactly one unique registry row for {name}")
            row = matches[0]
            if len(row) not in (6, 7) or row[5] != "unique":
                raise GameError(f"Invalid replacement registry columns or uniqueness for {name}")
            ip, length, signature, mask = row[1:5]
            if not re.fullmatch(r"[0-9A-Fa-f]{4}", ip) or int(ip, 16) != int(name[-4:], 16):
                raise GameError(f"Invalid replacement registry entry IP for {name}")
            if not re.fullmatch(r"[1-9][0-9]{0,2}", length) or not 1 <= int(length) <= 256:
                raise GameError(f"Invalid replacement registry signature length for {name}")
            size = int(length)
            if any(len(value) != size * 2 or not re.fullmatch(r"[0-9A-Fa-f]+", value)
                   for value in (signature, mask)):
                raise GameError(f"Invalid replacement registry signature or mask encoding for {name}")
            mask_bytes = bytes.fromhex(mask)
            if not any(mask_bytes) or not set(mask_bytes) <= {0, 255}:
                raise GameError(f"Invalid replacement registry byte mask for {name}")

    @staticmethod
    def _launch_env_args(env: dict[str, str], play_env: dict[str, str]) -> list[str]:
        # tmux can retain an older server environment. Clear known controls even
        # when absent from this server process, plus every inherited test knob.
        clear = {"PR_HYBRID_REGISTRY", "PR_HYBRID_LOG", "PR_HYBRID_DISPATCH",
                 "PR_HYBRID_BUS_MODE", "PR_HYBRID_SEED_PIN", "PR_PARITY_TARGET", "PR_PARITY_PREFIX"}
        clear.update(k for k in env if k.startswith(("PR_HYBRID_", "PR_PARITY_")))
        args = [part for key in sorted(clear) for part in ("-u", key)]
        args.extend(f"{key}={env[key]}" for key in ("DISPLAY", "XAUTHORITY", "SDL_AUDIODRIVER",
                    "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME"))
        args.extend(f"{key}={value}" for key, value in play_env.items())
        return args

    @staticmethod
    def _x_ready(display: str, auth: Path) -> bool:
        try:
            subprocess.run(["xdotool", "getdisplaygeometry"], env={**os.environ, "DISPLAY": display, "XAUTHORITY": str(auth)},
                           capture_output=True, check=True, timeout=1)
            return True
        except (OSError, subprocess.SubprocessError):
            return False

    @staticmethod
    def _terminate(process: subprocess.Popen | None):
        if process and process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGTERM)
                process.wait(timeout=4)
            except (ProcessLookupError, subprocess.TimeoutExpired):
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait(timeout=2)

    def _stop_locked(self):
        if self.tmux_session:
            subprocess.run(["tmux", "-L", "vp-poolrad", "kill-session", "-t", self.tmux_session], capture_output=True, timeout=5)
        self._terminate(self.xvfb)
        if self.sink_module:
            try:
                subprocess.run(["pactl", "unload-module", self.sink_module], capture_output=True, timeout=5)
            except (OSError, subprocess.SubprocessError):
                pass
        for log in self.log_files:
            log.close()
        self.log_files.clear()
        self.xvfb = None
        self.tmux_session = None
        self._launched_emulator = None
        self.display = self.sink_module = self.sink_name = None
        self.run_dir = None

    def stop(self) -> dict:
        with self.lock:
            self._stop_locked()
            return self.status()

    # --- Save states --------------------------------------------------------

    @staticmethod
    def _saves_dir(run_dir: Path) -> Path:
        return run_dir / ".dosbox-saves"

    @staticmethod
    def _states_path(run_dir: Path) -> Path:
        return run_dir / "vp-savestates.json"

    def _read_states(self, run_dir: Path) -> list[dict]:
        path = self._states_path(run_dir)
        if not path.is_file():
            return []
        try:
            payload = json.loads(path.read_text())
        except (OSError, ValueError):
            return []
        states = payload.get("states") if isinstance(payload, dict) else None
        if not isinstance(states, list):
            return []
        return [s for s in states if isinstance(s, dict) and isinstance(s.get("slot"), int)]

    def _write_states(self, run_dir: Path, states: list[dict]):
        path = self._states_path(run_dir)
        tmp = path.with_name(path.name + ".tmp")
        tmp.write_text(json.dumps({"version": 1, "run": run_dir.name, "states": states}, indent=2) + "\n")
        tmp.chmod(0o600)
        tmp.replace(path)

    def _last_run_dir(self) -> Path | None:
        if self.run_dir:
            return self.run_dir
        if self.last_file.exists():
            try:
                return self._valid_run(self.last_file.read_text().strip())
            except GameError:
                return None
        return None

    def _env(self) -> dict:
        return {**os.environ, "DISPLAY": self.display or "", "XAUTHORITY": str(self.run_dir / ".vp-xauthority")}

    def _require_running(self):
        if not self._game_running() or not self.display or not self.run_dir:
            raise GameError("Poolrad is not running")

    def _focus_game(self):
        found = self._run(["xdotool", "search", "--onlyvisible", "--name", "DOSBox"], env=self._env())
        windows = found.stdout.split()
        if windows:
            self._run(["xdotool", "windowfocus", "--sync", windows[0]], env=self._env())

    def _chord(self, combo: str):
        self._run(["xdotool", "key", "--clearmodifiers", "--delay", "70", combo], env=self._env())

    def _record(self, action: str, **fields):
        with (self.run_dir / "vp-input.jsonl").open("a") as output:
            output.write(json.dumps({"time": datetime.now(timezone.utc).isoformat(), "action": action, **fields}) + "\n")

    def _goto_slot(self, target: int):
        """Move DOSBox-X's selected save slot with the mapper F12-[/F12-] chords."""
        target = max(1, min(self.SAVE_SLOT_MAX, target))
        if target == self.active_slot:
            return
        forward = (target - self.active_slot) % self.SAVE_SLOT_MAX
        backward = (self.active_slot - target) % self.SAVE_SLOT_MAX
        key = "F12+period" if forward <= backward else "F12+comma"
        for _ in range(min(forward, backward)):
            self._chord(key)
            time.sleep(0.08)
        self.active_slot = target

    def _log_size(self) -> int:
        log = self.run_dir / "vp-dosbox-terminal.log"
        try:
            return log.stat().st_size if log.is_file() else 0
        except OSError:
            return 0

    def _wait_for_saved_slot(self, offset: int, timeout: float = 8.0) -> int | None:
        """Read the slot DOSBox-X actually saved to from its terminal log."""
        log = self.run_dir / "vp-dosbox-terminal.log"
        pattern = re.compile(rb"Saved\. \(Slot (\d+)\)")
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                fresh = log.read_bytes()[offset:] if log.is_file() else b""
            except OSError:
                fresh = b""
            hits = pattern.findall(fresh)
            if hits:
                return int(hits[-1])
            time.sleep(0.1)
        return None

    def _wait_for_stable(self, path: Path, timeout: float = 8.0) -> bool:
        # DOSBox-X creates the file before it finishes writing; wait for a stable size.
        deadline = time.monotonic() + timeout
        stable = -1
        while time.monotonic() < deadline:
            size = path.stat().st_size if path.is_file() else 0
            if size > 0 and size == stable:
                return True
            stable = size
            time.sleep(0.2)
        return False

    def list_states(self) -> dict:
        with self.lock:
            run_dir = self._last_run_dir()
            if run_dir is None:
                return {"run_name": None, "states": []}
            states = self._read_states(run_dir)
            known = {s["slot"] for s in states}
            saves = self._saves_dir(run_dir)
            if saves.is_dir():
                for path in sorted(saves.glob("*.sav")):
                    if path.stem.isdigit() and int(path.stem) not in known:
                        stamp = datetime.fromtimestamp(path.stat().st_mtime, timezone.utc).isoformat()
                        states.append({"slot": int(path.stem), "tag": f"slot {path.stem}",
                                       "created": stamp, "size": path.stat().st_size})
                        known.add(int(path.stem))
            present = [s for s in states if (saves / f"{s['slot']}.sav").is_file()]
            present.sort(key=lambda s: s["slot"])
            return {"run_name": run_dir.name, "states": present}

    def savestate(self, tag: str) -> dict:
        tag = (tag or "").strip()[:80]
        with self.lock:
            self._require_running()
            run_dir = self.run_dir
            saves = self._saves_dir(run_dir)
            saves.mkdir(exist_ok=True)
            states = self._read_states(run_dir)
            used = {s["slot"] for s in states}
            used.update(int(p.stem) for p in saves.glob("*.sav") if p.stem.isdigit())
            slot = next((n for n in range(1, self.SAVE_SLOT_MAX + 1) if n not in used), None)
            if slot is None:
                raise GameError("All save-state slots are in use")
            self._focus_game()
            self._goto_slot(slot)
            log_offset = self._log_size()
            self._chord("F12+s")
            actual = self._wait_for_saved_slot(log_offset) or slot
            self.active_slot = actual
            path = saves / f"{actual}.sav"
            if not self._wait_for_stable(path):
                raise GameError("DOSBox-X did not finish writing the save state")
            entry = {"slot": actual, "tag": tag or f"slot {actual}",
                     "created": datetime.now(timezone.utc).isoformat(),
                     "session": getattr(self, "_session_started", None),
                     "size": path.stat().st_size, "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
            states = [s for s in states if s["slot"] != actual] + [entry]
            states.sort(key=lambda s: s["slot"])
            self._write_states(run_dir, states)
            self._record("savestate", slot=actual, tag=entry["tag"])
            return entry

    def _wait_for_load_status(self, slot: int, offset: int, timeout: float = 8.0) -> str | None:
        """Return 'loaded', 'mismatch' (program-name dialog), or None on timeout."""
        log = self.run_dir / "vp-dosbox-terminal.log"
        loaded = re.compile(rb"Loaded\. \(Slot (\d+)\)")
        aborted = re.compile(rb"Aborted\. Check your program name")
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                fresh = log.read_bytes()[offset:] if log.is_file() else b""
            except OSError:
                fresh = b""
            if any(int(hit) == slot for hit in loaded.findall(fresh)):
                return "loaded"
            if aborted.search(fresh):
                return "mismatch"
            time.sleep(0.1)
        return None

    GAME_START_MARKER = b"Packed file is corrupt"

    def _game_started(self) -> bool:
        """True once START.EXE has written its startup screen (not just DOS autoexec)."""
        log = self.run_dir / "vp-dosbox-terminal.log"
        try:
            return self.GAME_START_MARKER in log.read_bytes()
        except OSError:
            return False

    def _wait_for_game_start(self, timeout: float = 20.0) -> bool:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if self._game_started():
                return True
            time.sleep(0.25)
        return self._game_started()

    def loadstate(self, slot: int) -> dict:
        with self.lock:
            self._require_running()
            run_dir = self.run_dir
            entry = next((s for s in self._read_states(run_dir) if s["slot"] == slot), None)
            path = self._saves_dir(run_dir) / f"{slot}.sav"
            if entry is None or not path.is_file():
                raise GameError("That save state is not available")
            if entry.get("sha256") and hashlib.sha256(path.read_bytes()).hexdigest() != entry["sha256"]:
                raise GameError("The save state file changed since it was recorded")
            # DOSBox-X asks "Program name mismatch. Load the state anyway?" while the
            # guest is still in autoexec (current program MOUNT/COMMAND, not START).
            # Forcing Yes can abort the emulator (observed: core dumped), so wait for
            # the game to actually start and refuse with guidance if it never does.
            if not self._wait_for_game_start():
                raise GameError("The game is still starting. Wait for the title screen, then load the state.")
            self._focus_game()
            self._goto_slot(slot)
            log_offset = self._log_size()
            self._chord("F12+l")
            status = self._wait_for_load_status(slot, log_offset)
            if status == "mismatch":
                self._record("loadstate", slot=slot, tag=entry.get("tag", ""), verified=False,
                             reason="program-name dialog")
                raise GameError("DOSBox-X is still at its startup prompt. Wait for the title screen, then load the state.")
            self._record("loadstate", slot=slot, tag=entry.get("tag", ""), verified=status == "loaded")
            return {**entry, "loaded": True, "verified": status == "loaded"}

    # --- Frame capture and code-word recognition ----------------------------

    def capture_frame(self, inset: int = FRAME_INSET) -> tuple[bytes, int, int]:
        """Grab the live game window as RGB24, dropping the decorative border."""
        with self.lock:
            self._require_running()
            env = self._env()
            found = self._run(["xdotool", "search", "--onlyvisible", "--name", "DOSBox"], env=env)
            windows = found.stdout.split()
            if not windows:
                raise GameError("The game window is unavailable")
            geometry = self._run(["xdotool", "getwindowgeometry", "--shell", windows[0]], env=env).stdout
            values = dict(line.split("=", 1) for line in geometry.splitlines() if "=" in line)
            try:
                x, y, width, height = (int(values[key]) for key in ("X", "Y", "WIDTH", "HEIGHT"))
            except (KeyError, ValueError) as exc:
                raise GameError("Could not read the game window geometry") from exc
            crop_width = max(1, width - 2 * inset)
            crop_height = max(1, height - 2 * inset)
            command = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-f", "x11grab", "-draw_mouse", "0",
                       "-video_size", f"{width}x{height}", "-i", f"{self.display}+{x},{y}",
                       "-vf", f"crop={crop_width}:{crop_height}:{inset}:{inset}",
                       "-frames:v", "1", "-pix_fmt", "rgb24", "-f", "rawvideo", "pipe:1"]
            result = subprocess.run(command, env=env, capture_output=True, timeout=30)
            if result.returncode != 0 or len(result.stdout) != crop_width * crop_height * 3:
                raise GameError("Could not capture the game frame")
            return result.stdout, crop_width, crop_height

    def _recognize_once(self, templates: list, inset: int) -> dict:
        from server import codeword
        rgb, width, height = self.capture_frame(inset)
        return codeword.recognize_wheel(codeword.bitmap_rgb(rgb, width, height, "rune"), templates)

    def recognize_codeword(self, templates: list, inset: int = FRAME_INSET) -> dict:
        """Capture, recognize, and auto-type only when two frames agree confidently."""
        first = self._recognize_once(templates, inset)
        if not first.get("confident"):
            return {**first, "typed": False}
        time.sleep(0.18)
        second = self._recognize_once(templates, inset)
        if not second.get("confident") or second.get("signature") != first.get("signature"):
            return {"confident": False, "typed": False, "path": second.get("path"),
                    "glyph_count": second.get("glyph_count"),
                    "reason": "The rune prompt changed during recognition."}
        self.type_text(first["word"], submit=True)
        self._record("autocodeword", word=first["word"], a=first["a"], b=first["b"], path=first["path"])
        return {**first, "typed": True}

    def key(self, key: str):
        if key not in self.KEYS:
            raise GameError("Unsupported game key")
        with self.lock:
            if not self._game_running() or not self.display or not self.run_dir:
                raise GameError("Poolrad is not running")
            env = {**os.environ, "DISPLAY": self.display, "XAUTHORITY": str(self.run_dir / ".vp-xauthority")}
            self._run(["xdotool", "key", "--clearmodifiers", "--delay", "70", key], env=env)
            with (self.run_dir / "vp-input.jsonl").open("a") as output:
                output.write(json.dumps({"time": datetime.now(timezone.utc).isoformat(), "key": key}) + "\n")

    def type_text(self, text: str, submit: bool = True):
        if not isinstance(text, str) or not text or len(text) > 64 or any(ord(ch) < 32 for ch in text):
            raise GameError("Unsupported text")
        with self.lock:
            if not self._game_running() or not self.display or not self.run_dir:
                raise GameError("Poolrad is not running")
            env = {**os.environ, "DISPLAY": self.display, "XAUTHORITY": str(self.run_dir / ".vp-xauthority")}
            self._run(["xdotool", "type", "--clearmodifiers", "--delay", "60", text], env=env)
            if submit:
                self._run(["xdotool", "key", "--clearmodifiers", "--delay", "70", "Return"], env=env)
            with (self.run_dir / "vp-input.jsonl").open("a") as output:
                output.write(json.dumps({"time": datetime.now(timezone.utc).isoformat(), "type": text, "submit": submit}) + "\n")

    def stream(self, kind: str):
        with self.lock:
            if not self._game_running() or not self.run_dir:
                raise GameError("Poolrad is not running")
            if kind == "audio" and not self.sink_name:
                raise GameError("Audio capture is unavailable")
            env = {**os.environ, "DISPLAY": self.display or "", "XAUTHORITY": str(self.run_dir / ".vp-xauthority")}
            if kind == "video":
                # DOSBox-X re-centers and can resize its output per video mode, so a fixed
                # crop (previously crop=640:400:80:100) clipped the frame in some modes and
                # looked offset. Capture the whole private display and integer-upscale 2x so
                # nothing can ever be cut; the browser contains it with object-fit.
                cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-f", "x11grab", "-draw_mouse", "0", "-framerate", "15",
                       "-video_size", "800x600", "-i", self.display or "",
                       "-vf", "scale=1600:1200:flags=neighbor",
                       "-q:v", "6", "-fflags", "nobuffer", "-flush_packets", "1", "-f", "mpjpeg", "pipe:1"]
            else:
                cmd = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-f", "pulse", "-i", f"{self.sink_name}.monitor",
                       "-ac", "2", "-ar", "48000", "-c:a", "libopus", "-b:a", "64k", "-page_duration", "200000", "-flush_packets", "1", "-f", "ogg", "pipe:1"]
            proc = subprocess.Popen(cmd, env=env, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, start_new_session=True)
            # One stream per kind. A browser refresh would otherwise leave the old
            # ffmpeg grabbing the display, and the orphans accumulate CPU load.
            previous = self._streams.get(kind)
            self._streams[kind] = proc
        if previous is not None:
            self._terminate(previous)
        try:
            while data := proc.stdout.read1(262144):
                yield data
        finally:
            with self.lock:
                if self._streams.get(kind) is proc:
                    self._streams.pop(kind, None)
            self._terminate(proc)
