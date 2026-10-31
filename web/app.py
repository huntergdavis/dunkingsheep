"""Owner-only LAN dashboard for existing Herdr agents.

The web process sends chat directly through Dunking Sheep and stores local history.
Herdr pane output is a live view, not a structured transcript; authored briefs
are stored in SQLite.
"""

from __future__ import annotations

import hashlib
import hmac
import importlib.util
import json
import math
import os
import re
import secrets
import sqlite3
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from contextlib import contextmanager
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from typing import Annotated
from urllib.parse import quote, urlsplit, urlunsplit

from fastapi import Depends, FastAPI, File, HTTPException, Request, Response, UploadFile
from fastapi.responses import FileResponse, RedirectResponse, StreamingResponse
from fastapi.staticfiles import StaticFiles
from pydantic import BaseModel, Field
from web.character_editor import EditorStore, router as character_editor_router

ROOT = Path(__file__).resolve().parents[1]
# The desk's state lives OUTSIDE the repo, always (docs/FLAT_FLEET.md 9.2 rule 4): an
# import inside a checkout or worktree used to create data/ beside the code, and two
# lane branches then committed auth.json and the owner's messages to a public repo
# (2026-10-29). There is no repo-root fallback.
DESK_DIR_DEFAULT = Path(os.environ.get("DUNKINGSHEEP_DIR") or str(Path.home() / ".config" / "dunkingsheep")) / "desk"
# `or`, NOT os.getenv's default: an EMPTY VP_DATA_DIR (Environment=VP_DATA_DIR=, a
# supervisor that passes an unset variable as "", `env VP_DATA_DIR= ...`) would make
# Path("") the CURRENT WORKING DIRECTORY - i.e. the repo, when a lane imports this
# module from a checkout, which is the 2026-10-29 incident shape. The default only
# applies to an unset variable, so the empty case has to be handled explicitly.
DATA = Path(os.environ.get("VP_DATA_DIR") or DESK_DIR_DEFAULT).expanduser().resolve()
DATA.mkdir(parents=True, exist_ok=True)
UPLOADS = DATA / "uploads"
UPLOADS.mkdir(exist_ok=True)
DB = DATA / "messages.sqlite3"
AUTH = DATA / "auth.json"
CREDENTIALS = DATA / "owner-credentials"
CODEWHEEL_DIR = Path(os.getenv("VP_CODEWHEEL_DIR", DATA / "codewheel")).resolve()
DUNKING_SHEEP = Path(os.getenv("DUNKING_SHEEP_DIR", ROOT))   # one repo: the daemon is beside us
sys.path.insert(0, str(DUNKING_SHEEP))
sys.path.insert(0, str(ROOT / "bridges"))   # ops_board, ops_metrics, fleet_inventory
import poolrad_paths  # noqa: E402
DASHBOARD_DIR = Path(os.getenv("POOLRAD_DASHBOARD_DIR", ROOT / "bridges" / "integrations" / "poolrad-dashboard")).resolve()
DASHBOARD_REPO = poolrad_paths.repo("POOLRAD_DASHBOARD_REPO", "POOLRAD_REPO").resolve()
DASHBOARD_REPORTS = {"SLOT_A_FIRST_EGA_ROW64.md", "SLOT_A_ROW6_COL38_PRINTER_PAGE.md"}

def _load_channels():
    override = os.getenv("VP_CHANNELS_FILE")
    path = Path(override).expanduser() if override else ROOT / "channels.json"
    if not path.exists():
        if override:
            # t735: the units hardcode VP_CHANNELS_FILE=<desk>/channels.json, and the
            # documented install ("cp -a vp-of-agentry/data desk") does not create it -
            # channels.json lives one level ABOVE data/. A bare FileNotFoundError sent
            # the operator hunting; name the source and the fix.
            raise FileNotFoundError(
                f"VP_CHANNELS_FILE does not exist: {path}. The channel registry is the "
                "owner's channels.json, which the units point at inside the desk data "
                "dir; copy it there (cp <checkout>/channels.json "
                f"{path}) or unset VP_CHANNELS_FILE to fall back to "
                "bridges/channels.example.json.")
        path = ROOT / "bridges" / "channels.example.json"
    payload = json.loads(path.read_text())
    if not isinstance(payload, list) or not payload:
        raise ValueError(f"{path}: expected a nonempty list of channels")
    ids = set()
    for item in payload:
        if not isinstance(item, dict) or not all(
            isinstance(item.get(key), str) and item[key].strip()
            for key in ("id", "name", "match", "kind")
        ):
            raise ValueError(f"{path}: each channel needs nonempty id, name, match, and kind")
        if not re.fullmatch(r"[a-z][a-z0-9_-]*", item["id"]):
            raise ValueError(f"{path}: invalid channel id {item['id']!r}")
        if item["kind"] not in {"active", "standby", "monitor"}:
            raise ValueError(f"{path}: invalid kind for channel {item['id']!r}")
        if item["id"] in ids:
            raise ValueError(f"{path}: duplicate channel id {item['id']!r}")
        if "dashboard" in item and not isinstance(item["dashboard"], bool):
            raise ValueError(f"{path}: dashboard must be true or false")
        ids.add(item["id"])
    return payload


CHANNELS = _load_channels()
CHANNEL_INDEX = {item["id"]: item for item in CHANNELS}
from web.roster import build_roster, discovered_id, is_lane, match_pane, presence
from web import codeword
from web.gameplay import GameError, GameSession
GAME = GameSession(poolrad_paths.repo("POOLRAD_REPO_DIR", "POOLRAD_REPO").resolve(), DATA)
MAX_UPLOAD = 25 * 1024 * 1024
LOGIN_ATTEMPTS: dict[str, list[float]] = {}
DELIVERY_LOCK = threading.Lock()
DELIVERY_STOP = threading.Event()
READ_STATUS_WINDOW_S = 8 * 24 * 3600  # Inbox retains read messages for seven days.
PANES_ACTIVITY_TTL_S = 4
PANES_ACTIVITY_LOCK = threading.Lock()
PANES_ACTIVITY_CACHE = {"expires": 0.0, "data": None}
POOLRAD_PROGRESS_TTL_S = 60
POOLRAD_PROGRESS_CACHE = {"expires": 0.0, "data": None}
DASHBOARD_MODULES: dict[str, object] = {}


@contextmanager
def _db():
    conn = sqlite3.connect(DB, timeout=10)
    try:
        conn.row_factory = sqlite3.Row
        conn.execute("PRAGMA journal_mode=WAL")
        conn.execute("PRAGMA foreign_keys=ON")
        with conn:
            yield conn
    finally:
        conn.close()


def _initialize():
    with _db() as conn:
        conn.executescript("""
            CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                channel TEXT NOT NULL,
                role TEXT NOT NULL,
                body TEXT NOT NULL,
                delivery TEXT NOT NULL DEFAULT 'posted',
                delivery_detail TEXT NOT NULL DEFAULT '',
                transport_id TEXT,
                transport_attempts INTEGER NOT NULL DEFAULT 0,
                transport_attempt_at REAL NOT NULL DEFAULT 0,
                external_id TEXT,
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
            );
            CREATE INDEX IF NOT EXISTS messages_channel_id ON messages(channel,id);
            CREATE TABLE IF NOT EXISTS attachments (
                id TEXT PRIMARY KEY,
                filename TEXT NOT NULL,
                mime TEXT NOT NULL,
                size INTEGER NOT NULL,
                path TEXT NOT NULL,
                message_id INTEGER REFERENCES messages(id),
                created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
            );
            CREATE INDEX IF NOT EXISTS attachments_message_id ON attachments(message_id);
            CREATE TABLE IF NOT EXISTS sessions (
                token_hash TEXT PRIMARY KEY,
                expires_at REAL NOT NULL
            );
            CREATE INDEX IF NOT EXISTS sessions_expires_at ON sessions(expires_at);
        """)
        session_columns = {row["name"] for row in conn.execute("PRAGMA table_info(sessions)")}
        for name, kind in (("viewport_width", "INTEGER"), ("viewport_height", "INTEGER"), ("viewport_dpr", "REAL"),
                           ("viewport_screen_width", "INTEGER"), ("viewport_screen_height", "INTEGER"), ("viewport_at", "REAL")):
            if name not in session_columns:
                conn.execute(f"ALTER TABLE sessions ADD COLUMN {name} {kind}")
        columns = {row["name"] for row in conn.execute("PRAGMA table_info(messages)")}
        if "transport_id" not in columns:
            conn.execute("ALTER TABLE messages ADD COLUMN transport_id TEXT")
        if "delivery_detail" not in columns:
            conn.execute("ALTER TABLE messages ADD COLUMN delivery_detail TEXT NOT NULL DEFAULT ''")
        if "transport_attempts" not in columns:
            conn.execute("ALTER TABLE messages ADD COLUMN transport_attempts INTEGER NOT NULL DEFAULT 0")
        if "transport_attempt_at" not in columns:
            conn.execute("ALTER TABLE messages ADD COLUMN transport_attempt_at REAL NOT NULL DEFAULT 0")
        if "answer_id" not in columns:
            conn.execute("ALTER TABLE messages ADD COLUMN answer_id INTEGER")
        if "answered_at" not in columns:
            conn.execute("ALTER TABLE messages ADD COLUMN answered_at TEXT")
        if "direct_transport" not in columns:
            conn.execute("ALTER TABLE messages ADD COLUMN direct_transport TEXT")
        for name in ("delivered_at", "delivery_latency_s"):
            if name not in columns:
                conn.execute(f"ALTER TABLE messages ADD COLUMN {name} REAL")
        if "external_id" not in columns:
            conn.execute("ALTER TABLE messages ADD COLUMN external_id TEXT")
        conn.execute("CREATE UNIQUE INDEX IF NOT EXISTS messages_external_id ON messages(channel,external_id) WHERE external_id IS NOT NULL")
    if not AUTH.exists():
        password = os.getenv("VP_PASSWORD") or secrets.token_urlsafe(18)
        token = secrets.token_urlsafe(32)
        salt = secrets.token_bytes(16)
        digest = hashlib.pbkdf2_hmac("sha256", password.encode(), salt, 400_000)
        AUTH.write_text(json.dumps({"salt": salt.hex(), "digest": digest.hex(), "token": token}))
        AUTH.chmod(0o600)
        CREDENTIALS.write_text(f"Password: {password}\nAPI token: {token}\n")
        CREDENTIALS.chmod(0o600)


_initialize()
app = FastAPI(title="VP of Agentry", docs_url=None, redoc_url=None)


def _auth_record():
    return json.loads(AUTH.read_text())


def _password_matches(password: str, record: dict):
    digest = hashlib.pbkdf2_hmac("sha256", password.encode(), bytes.fromhex(record["salt"]), 400_000)
    return hmac.compare_digest(digest, bytes.fromhex(record["digest"]))


def _private_write(path: Path, content: str):
    """Replace a credential file atomically without exposing its contents in Git."""
    fd, temporary = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        os.fchmod(fd, 0o600)
        with os.fdopen(fd, "w") as stream:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def _sync_edited_credentials():
    """Honor a local owner's edited Password line on the next login."""
    if not CREDENTIALS.exists() or CREDENTIALS.stat().st_mtime_ns <= AUTH.stat().st_mtime_ns:
        return
    match = re.search(r"^Password: (.+)$", CREDENTIALS.read_text(), re.MULTILINE)
    if not match:
        return
    password = match.group(1)
    record = _auth_record()
    if _password_matches(password, record):
        return
    salt = secrets.token_bytes(16)
    record["salt"] = salt.hex()
    record["digest"] = hashlib.pbkdf2_hmac("sha256", password.encode(), salt, 400_000).hex()
    _private_write(AUTH, json.dumps(record))
    with _db() as conn:
        conn.execute("DELETE FROM sessions")


def _session_key(cookie: str) -> str:
    return hashlib.sha256(cookie.encode()).hexdigest()


def _same_origin(request: Request):
    origin = request.headers.get("origin")
    if origin and origin != f"{request.url.scheme}://{request.headers.get('host')}":
        raise HTTPException(403, "Cross-origin request refused")


def require_auth(request: Request):
    _same_origin(request)
    bearer = request.headers.get("authorization", "")
    if bearer.startswith("Bearer ") and hmac.compare_digest(bearer[7:], _auth_record()["token"]):
        return "api"
    cookie = request.cookies.get("vp_session", "")
    if cookie:
        with _db() as conn:
            row = conn.execute("SELECT expires_at FROM sessions WHERE token_hash=?", (_session_key(cookie),)).fetchone()
            if row and row["expires_at"] > time.time():
                return "owner"
            if row:
                conn.execute("DELETE FROM sessions WHERE token_hash=?", (_session_key(cookie),))
    raise HTTPException(401, "Sign in required")


app.include_router(character_editor_router(
    EditorStore(DATA / 'character-editor', Path(os.getenv(
        'POOLRAD_EDITOR_TOOL',
        poolrad_paths.repo('POOLRAD_REPO_DIR', 'POOLRAD_REPO') / 'tools/character/edit_save.py'))),
    require_auth))


class Login(BaseModel):
    password: str
    next: str | None = None


def _session_valid(request: Request) -> bool:
    """True when the request carries a live owner session (cookie or token)."""
    bearer = request.headers.get("authorization", "")
    if bearer.startswith("Bearer ") and hmac.compare_digest(bearer[7:], _auth_record()["token"]):
        return True
    cookie = request.cookies.get("vp_session", "")
    if not cookie:
        return False
    with _db() as conn:
        row = conn.execute("SELECT expires_at FROM sessions WHERE token_hash=?", (_session_key(cookie),)).fetchone()
    return bool(row and row["expires_at"] > time.time())


def _herdr_webui_port() -> int | None:
    configured = os.getenv("VP_HERDR_WEBUI_URL", "").strip()
    if not configured:
        return None
    try:
        return urlsplit(configured).port
    except ValueError:
        return None


def _safe_next(value: str | None, request: Request) -> str | None:
    """A return target for the login, limited to this host.

    A path is always safe. An absolute URL is accepted only when it names
    this host and the desk's own port. Retired terminal-manager URLs must
    not send a successful login back into the old interface.
    """
    if not value:
        return None
    candidate = value.strip()
    if not candidate:
        return None
    if candidate.startswith("/") and not candidate.startswith("//"):
        return candidate
    try:
        parsed = urlsplit(candidate)
        host = (parsed.hostname or "").lower()
        port = parsed.port
    except ValueError:
        return None
    if not parsed.scheme or not host or not port:
        return None
    if host != (request.url.hostname or "").lower():
        return None
    if parsed.scheme not in {"http", "https"} or port != request.url.port:
        return None
    return candidate


@app.post("/api/login")
def login(body: Login, request: Request, response: Response):
    _same_origin(request)
    _sync_edited_credentials()
    addr = request.client.host if request.client else "unknown"
    now = time.time()
    attempts = [t for t in LOGIN_ATTEMPTS.get(addr, []) if now - t < 300]
    if len(attempts) >= 8:
        raise HTTPException(429, "Too many attempts; retry in five minutes")
    record = _auth_record()
    if not _password_matches(body.password, record):
        LOGIN_ATTEMPTS[addr] = attempts + [now]
        raise HTTPException(401, "Incorrect password")
    LOGIN_ATTEMPTS.pop(addr, None)
    session = secrets.token_urlsafe(32)
    with _db() as conn:
        conn.execute("DELETE FROM sessions WHERE expires_at<=?", (now,))
        conn.execute("INSERT INTO sessions(token_hash,expires_at) VALUES(?,?)", (_session_key(session), now + 7 * 24 * 3600))
    response.set_cookie("vp_session", session, httponly=True, samesite="strict", max_age=7 * 24 * 3600)
    return {"ok": True, "next": _safe_next(body.next, request) or "/dash/cos"}


@app.post("/api/logout")
def logout(request: Request, response: Response, _: Annotated[str, Depends(require_auth)]):
    cookie = request.cookies.get("vp_session", "")
    if cookie:
        with _db() as conn:
            conn.execute("DELETE FROM sessions WHERE token_hash=?", (_session_key(cookie),))
    response.delete_cookie("vp_session")
    return {"ok": True}


class PasswordChange(BaseModel):
    current_password: str
    new_password: str = Field(min_length=12, max_length=200)


@app.post("/api/password")
def change_password(body: PasswordChange, request: Request, identity: Annotated[str, Depends(require_auth)]):
    if identity != "owner":
        raise HTTPException(403, "Owner sign-in required")
    record = _auth_record()
    if not _password_matches(body.current_password, record):
        raise HTTPException(403, "Current password is incorrect")
    salt = secrets.token_bytes(16)
    record["salt"] = salt.hex()
    record["digest"] = hashlib.pbkdf2_hmac("sha256", body.new_password.encode(), salt, 400_000).hex()
    _private_write(AUTH, json.dumps(record))
    if CREDENTIALS.exists():
        _private_write(CREDENTIALS, f"Password: {body.new_password}\nAPI token: {record['token']}\n")
    current = request.cookies.get("vp_session", "")
    with _db() as conn:
        conn.execute("DELETE FROM sessions WHERE token_hash<>?", (_session_key(current),))
    return {"ok": True}


def _flock():
    try:
        from dunk_client import FlockClient
        return FlockClient.connect_or_start()
    except Exception as exc:
        raise HTTPException(503, f"Dunking Sheep unavailable: {exc}") from exc


def _panes():
    return _flock().call("list_panes")["panes"]


def _target(channel: str, panes: list[dict]):
    cfg = CHANNEL_INDEX.get(channel)
    if cfg:
        return match_pane(cfg, panes)
    for pane in panes:
        if is_lane(pane) and discovered_id(pane.get("pane_id", "")) == channel:
            return pane
    if _known_channel(channel):
        return None
    raise HTTPException(404, "Unknown channel")


def _presence(raw_status: str | None):
    # Herdr's "blocked" can mean a transient tool wait. It is not evidence of
    # a project blocker or a decision needed from the owner.
    return presence(raw_status)


def _known_channel(channel: str) -> bool:
    return channel in CHANNEL_INDEX or bool(re.fullmatch(r"pane-[a-z0-9-]+", channel))


class ViewportSample(BaseModel):
    model_config = {"extra": "forbid"}
    innerWidth: int = Field(strict=True, ge=1, le=32768)
    innerHeight: int = Field(strict=True, ge=1, le=32768)
    screenWidth: int = Field(strict=True, ge=1, le=32768)
    screenHeight: int = Field(strict=True, ge=1, le=32768)
    devicePixelRatio: float = Field(strict=True, gt=0, le=16, allow_inf_nan=False)


@app.post("/api/viewport")
async def report_viewport(request: Request, _: Annotated[str, Depends(require_auth)]):
    try:
        body = ViewportSample.model_validate(await request.json())
    except ValueError:
        # Do not echo arbitrary input (including non-JSON NaN) in an error response.
        raise HTTPException(422, "Invalid viewport sample")
    cookie = request.cookies.get("vp_session", "")
    now = time.time()
    with _db() as conn:
        changed = conn.execute(
            "UPDATE sessions SET viewport_width=?,viewport_height=?,viewport_dpr=?,viewport_screen_width=?,viewport_screen_height=?,viewport_at=? "
            "WHERE token_hash=? AND expires_at>?",
            (body.innerWidth, body.innerHeight, body.devicePixelRatio, body.screenWidth, body.screenHeight, now, _session_key(cookie), now),
        ).rowcount
    if not changed:
        raise HTTPException(401, "Browser session required")
    return {"ok": True}


def _viewports():
    # Authenticated status exposes dimensions only, never session identifiers.
    with _db() as conn:
        return [dict(row) for row in conn.execute(
            "SELECT viewport_width AS innerWidth,viewport_height AS innerHeight,viewport_dpr AS devicePixelRatio, "
            "viewport_screen_width AS screenWidth,viewport_screen_height AS screenHeight,viewport_at "
            "FROM sessions WHERE expires_at>? AND viewport_at IS NOT NULL ORDER BY rowid",
            (time.time(),),
        )]


@app.get("/api/channels")
def channels(_: Annotated[str, Depends(require_auth)]):
    try:
        panes = _panes()
        daemon = True
    except HTTPException:
        panes, daemon = [], False
    rows = build_roster(CHANNELS, panes, can_message=_can_message)
    return {"channels": rows, "daemon": daemon, "viewports": _viewports(),
            "checked_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}


_CAP_CACHE: dict[str, tuple[float, dict]] = {}
_CAP_TTL_S = 20.0
_FLEET_CACHE = {"at": 0.0, "payload": None}
_FLEET_TTL_S = 3.0


_PANE_TEXT_TTL_S = 1.5
_PANE_TEXT_CACHE: dict[str, tuple[float, str]] = {}


@app.get("/api/pane-text")
def api_pane_text(pane: str, lines: int = 40):
    """The current screen of a live pane, read with tmux capture-pane. No app scrollback.

    Unauthenticated on purpose, same rule as /api/fleet: this endpoint cannot write, and the
    page that calls it is already behind the desk login. Every mutating endpoint requires auth.

    Typing travels on the delivery path (see /api/pane-send); a textarea wired to tmux
    send-keys would type over whatever the agent is doing.
    """
    pane = (pane or "").strip()
    if not pane.startswith("%"):
        raise HTTPException(status_code=400, detail="a tmux pane id looks like %9")
    now = time.time()
    hit = _PANE_TEXT_CACHE.get(pane)
    if hit and (now - hit[0]) < _PANE_TEXT_TTL_S:
        return {"pane": pane, "text": hit[1], "readonly": True}
    # The daemon owns the pane read (`read_pane`), including how many lines and
    # what a dead pane looks like; the desk does not shell out to tmux
    # (FLAT_FLEET.md 9.2 rule 1).
    try:
        result = _flock().call("read_pane", target=pane, lines=max(1, min(lines, 200)))
    except HTTPException:
        raise
    except Exception as error:  # noqa: BLE001 - an unreadable pane is a sentence
        return {"pane": pane, "text": "", "readonly": True,
                "reason": f"the daemon could not read the pane: {error}"}
    text = str((result or {}).get("text") or "")
    _PANE_TEXT_CACHE[pane] = (now, text)
    return {"pane": pane, "text": text, "readonly": True,
            "note": "read-only: typing goes through the delivery path, not this endpoint"}


def _pending_by_pane() -> dict | None:
    """Counts of untaken pushes, or None when the source cannot establish a count."""
    # One socket call, not a CLI process: the desk reads the daemon, never spawns
    # it (FLAT_FLEET.md 9.2 rule 1). The payload shape is the same dict the CLI
    # printed, so the checks below are unchanged.
    try:
        payload = _flock().call("owner_stats")
    except Exception:  # noqa: BLE001 - a read that cannot answer is "unknown", not an error
        return None
    if (not isinstance(payload, dict) or payload.get("ok") is False
            or not isinstance(payload.get("pending"), list)):
        return None
    counts = {}
    for item in payload["pending"]:
        if not isinstance(item, dict):
            return None
        pane = item.get("pane_id")
        if pane is not None and not isinstance(pane, str):
            return None
        if pane:
            counts[pane] = counts.get(pane, 0) + 1
    return counts


def _capability(pane: str) -> dict:
    """The daemon owns lane capability; the desk must not branch on agent names.

    Returns {} when the daemon cannot answer, so callers can refuse rather than guess.
    """
    try:
        # The handler reads `target`, not `pane` (dunk_api.h_delivery_capability).
        # Passing the wrong keyword raised, the except below swallowed it, and every
        # lane came back with no capability - the send path 409'd and the fleet view
        # said can_type False for everything (t641 review, finding 1).
        payload = _flock().call("delivery_capability", target=pane)
        if isinstance(payload, dict):
            _CAP_CACHE[pane] = (time.time(), payload)
            return payload
    except Exception:  # noqa: BLE001 - an unanswered capability is {}, never a guess
        pass
    return {}


def _can_message(pane) -> bool:
    """May the desk send to this pane? (t1150)

    The backend's `agent` string is empty for every tmux pane and `agent_status` is only set
    while a lane has a recent turn signal, so neither can prove a QUIET lane is an agent (a
    fresh or idle desk read as a bare shell and the 409 came back). The daemon can: its
    capability carries `agent_alive`, from the process table. True = agent process here;
    False = a bare shell (never type owner text into it); None = cannot say, refused.
    """
    if not pane or not pane.get("pane_id"):
        return False
    if is_agent_pane(pane):
        return True
    pane_id = pane["pane_id"]
    cached = _CAP_CACHE.get(pane_id)
    cap = cached[1] if cached and (time.time() - cached[0]) < _CAP_TTL_S else _capability(pane_id)
    return cap.get("agent_alive") is True


@app.get("/api/capability")
def api_capability(pane: str):
    """What would happen if the desk sent to this lane, in the daemon's own words.

    Unauthenticated on purpose, like /api/fleet and /api/pane-text: it discloses a route, not
    a message or a keystroke. Everything that WRITES takes Depends(require_auth).
    """
    pane = (pane or "").strip()
    cached = _CAP_CACHE.get(pane)
    if cached and (time.time() - cached[0]) < _CAP_TTL_S:
        return cached[1]
    cap = _capability(pane)
    if not cap:
        raise HTTPException(status_code=502, detail="the daemon could not describe this lane")
    return cap


@app.post("/api/pane-send")
async def api_pane_send(request: Request, identity: Annotated[str, Depends(require_auth)]):
    body = await request.json()
    if body.get("channel"):
        data = MessageIn(channel=body["channel"], body=body.get("text", ""),
                         attachment_ids=body.get("attachment_ids", []))
        pane = _target(data.channel, _panes())
        if not pane or pane["pane_id"] != body.get("pane"):
            raise HTTPException(409, "Channel target changed; refresh before sending")
        return _post_message(data, identity, direct=True)
    return _send_to_pane(str(body.get("pane") or "").strip(),
                         str(body.get("text") or "").strip())


def _send_to_pane(pane: str, text: str):
    """Send to a lane the way that lane can receive it, decided by the daemon.

    Two transports, never one fallback:
      send == "owner_push" -> the lane drains pushed messages itself; store, do not type.
      send == "guarded"    -> an immediate guarded send (Enter, then C-m); 'sent' means it
                              left the box, not that the agent acted on it.
    Branching on the agent name here would be the desk guessing at a mapping the daemon
    owns - so this asks, and refuses if the answer is missing.
    """
    if not pane.startswith("%"):
        raise HTTPException(status_code=400, detail="a tmux pane id looks like %9")
    if not text:
        raise HTTPException(status_code=400, detail="nothing to send")
    if len(text) > 16000:
        raise HTTPException(status_code=400, detail="keep it under 16000 characters")

    cap = _capability(pane)
    if not cap:
        raise HTTPException(status_code=502, detail="the daemon could not describe this lane")
    if not cap.get("alive"):
        raise HTTPException(status_code=409, detail=f"{pane} is not alive: nothing to send to")
    route = cap.get("send")
    if route not in ("owner_push", "guarded"):
        raise HTTPException(status_code=409,
                            detail=f"{pane} declares no send route ({route!r}); not guessing")
    # One socket call per route, and the daemon's own vocabulary decides what
    # happened: the desk is the `web` role, which holds the typed route by design
    # (require_send_self exempts it), so the retired desk credential is not needed
    # to type any more.
    try:
        payload = (_flock().call("owner_push", target=pane, body=text)
                   if route == "owner_push"
                   else _flock().call("send_text", target=pane, text=text))
    except HTTPException:
        raise
    except Exception as error:  # noqa: BLE001 - a dead daemon is a 502, not a traceback
        raise HTTPException(status_code=502, detail=f"delivery path unavailable: {error}")
    if not isinstance(payload, dict):
        payload = {}
    detail = str(payload.get("message") or payload.get("outcome") or "").strip()[:400]

    if route == "owner_push":
        # Forward the daemon's own outcome (now "stored"), never synthesise "ok": a push is
        # stored, not delivered, and the difference is the whole point of the field.
        pushed = payload
        outcome = str(pushed.get("outcome") or "").strip().lower()
        if not outcome:
            raise HTTPException(status_code=502,
                                detail=("the daemon stored the message but returned no outcome "
                                        f"field, so the desk will not invent one: {detail[:160]}"))
        return {"ok": True, "pane": pane, "route": route, "outcome": outcome,
                "push_id": pushed.get("id"), "detail": detail,
                "note": ("Stored for the lane to drain; nothing typed into the pane. "
                         "'stored' is not 'read' - it has not arrived until the lane takes it.")}

    # Guarded: the daemon answers with a machine-readable outcome. Branch on THAT - not on
    # the exit code and not on prose. Two earlier versions of this function got the false
    # success and then the false failure, both by parsing sentences.
    outcome = str(payload.get("outcome") or "").strip().lower()
    message_id = payload.get("message_id")

    if outcome == "refused":
        return {"ok": False, "pane": pane, "route": route, "outcome": outcome,
                "message_id": message_id, "detail": payload.get("message") or detail}
    if not outcome:
        raise HTTPException(status_code=409, detail=detail or "No daemon outcome")
    if outcome == "queued":
        return {"ok": True, "pane": pane, "route": route, "outcome": "queued",
                "queued": True, "message_id": message_id, "detail": detail,
                "note": ("Held by the typing guard or not yet delivered. It has NOT "
                         "arrived - do not read this as delivered.")}
    if outcome == "unconfirmed":
        return {"ok": True, "pane": pane, "route": route, "outcome": "unconfirmed",
                "unconfirmed": True, "message_id": message_id, "detail": detail,
                "note": ("Neither witness fired. It may well have arrived - do NOT resend "
                         "blindly; ask the lane first.")}
    if outcome == "stored":
        # The daemon's fifth value. It only arrives on a durable push today, but the desk
        # must not 502 on a legitimate vocabulary item if that changes.
        return {"ok": True, "pane": pane, "route": route, "outcome": "stored",
                "message_id": message_id, "detail": detail,
                "note": ("Stored in a durable queue the lane drains itself. That is NOT "
                         "'read' - it has not arrived until the lane takes it.")}
    if outcome in ("ok", "sent"):
        return {"ok": True, "pane": pane, "route": route, "outcome": outcome,
                "message_id": message_id, "detail": detail,
                "note": ("Confirmed by the box clearing or by the turn it started - that is "
                         "delivery to the lane, not proof the agent acted on it.")}
    # No outcome field and no error: say so instead of inventing a verdict.
    raise HTTPException(status_code=502,
                        detail=("the sender returned no outcome field, so the desk will not "
                                f"guess: {(detail or '')[:200]}"))


@app.get("/api/fleet")
def api_fleet():
    """The live fleet as the owner needs to see it: who is running, doing what, and what is
    waiting for them.

    Source is the pinned lane registry and the daemon's own view of the panes - never the
    herdr pane list, which still reports ~40 shells and made the desk say "idle" while nine
    agents were working. A pane with no agent is not a lane and is not shown; it is a shell.
    """
    # Cached for a few seconds on purpose: the view polls, and without this every open tab
    # would spawn a `dunkingsheep panes` CLI process per poll - three tabs, three processes,
    # every five seconds. The cache is what makes the poll honest.
    cached = _FLEET_CACHE
    if cached["payload"] is not None and (time.time() - cached["at"]) < _FLEET_TTL_S:
        return cached["payload"]

    def unavailable(reason):
        payload = {"state": "unavailable", "error": reason[:300], "rows": [], "count": None,
                   "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
        cached["at"], cached["payload"] = time.time(), payload
        return payload

    cfg = Path(os.path.expanduser("~/.config/dunkingsheep"))
    try:
        listing = _flock().call("list_panes")
    except Exception as error:  # noqa: BLE001 - the view says unavailable, it does not crash
        return unavailable(f"pane roster could not be read: {error}")
    if not isinstance(listing, dict) or not isinstance(listing.get("panes"), list):
        return unavailable("pane roster response is missing its panes list")
    panes = listing["panes"]
    if any(not isinstance(pane, dict) or not isinstance(pane.get("pane_id"), str)
           for pane in panes):
        return unavailable("pane roster contains an invalid pane")
    try:
        registry = json.loads((cfg / "lanes.json").read_text())
    except (OSError, ValueError):
        registry = {}

    # The same two answers, asked of the daemon instead of its databases
    # (FLAT_FLEET.md 9.2 rule 1). `state_latest` is agent_state through the socket
    # (newest first, so the first row per pane is the latest), and `roster` already
    # carries each lane's unread count. A read that fails leaves the maps empty,
    # which the view already renders as "unknown" rather than as zero.
    activity, waiting, sessions = {}, {}, {}
    try:
        state = _flock().call("state_latest", limit=500)
        for row in (state.get("rows") or []):
            pane_key = row.get("pane_id")
            if not pane_key or pane_key in activity:
                continue
            if row.get("at") is not None:
                activity[pane_key] = row["at"]
            sessions[pane_key] = row.get("session_id")
    except Exception:  # noqa: BLE001 - a missing source is unknown, not zero
        pass
    try:
        roster = _flock().call("roster")
        for lane in (roster.get("lanes") or []):
            unread = (lane.get("inbox") or {}).get("unread")
            if unread is None:
                continue
            for key in (lane.get("name"), lane.get("pane_id")):
                if key:
                    waiting[key] = unread
    except Exception:  # noqa: BLE001
        pass

    # Last TURN, not last capture: the capture age says when the row was written, which is
    # every few minutes for every lane and therefore tells the owner nothing. The session
    # file's mtime is when that lane last did something.
    turn_at = {}
    for root in ("~/.pi/agent/sessions", "~/.claude/projects"):
        base = Path(os.path.expanduser(root))
        if not base.exists():
            continue
        for path in base.glob("*/*.jsonl"):
            stem = path.stem
            sid = stem.split("_")[-1] if "_" in stem else stem
            try:
                turn_at[sid] = path.stat().st_mtime
            except OSError:
                continue

    pending = _pending_by_pane()
    now = time.time()
    rows = []
    for pane in panes:
        pane_id = pane.get("pane_id")
        if not pane_id:
            continue
        agent = pane.get("agent") or ""
        # The lane registry's values may be a plain mailbox name or a dict extended with
        # liveness ({"mailbox": ..., "alive": false, "dead_since": ...}). Both shapes occur
        # in the same file, so normalise here rather than trusting one.
        recorded = registry.get(pane_id)
        if isinstance(recorded, dict):
            recorded = recorded.get("mailbox") or recorded.get("name")
        name = recorded or pane.get("label") or pane_id
        if not agent:
            continue          # a shell is not a lane
        cap = _capability(pane_id)          # cached 20s; one read per row, not three
        sid = sessions.get(pane_id) or ""
        last = turn_at.get(sid) or turn_at.get(sid.split("-")[0])
        age = (now - last) if last else None
        if age is None and activity.get(pane_id):
            age = now - activity[pane_id]
        rows.append({
            "pane_id": pane_id,
            "name": name,
            "project": (pane.get("cwd") or "").rstrip("/").split("/")[-1],
            "cwd": pane.get("cwd"),
            "agent": agent,
            "alive": True,
            "last_activity_s": int(age) if age is not None else None,
            "unread": waiting.get(name, 0) + waiting.get(pane_id, 0),
            "session": sessions.get(pane_id),
            # What the desk may honestly offer for this lane, answered by the daemon:
            # "owner_push" = it drains pushes; "guarded" = immediate typed send.
            "send": (cap or {}).get("send"),
            "drains": (cap or {}).get("drains"),
            "pending": pending.get(pane_id, 0) if pending is not None else None,
            "pending_state": "ok" if pending is not None else "unknown",
            # Whether the desk can actually reach the typed route for this lane: the
            # capability says what it would take, this says whether we hold the credential.
            # The typed route belongs to the `web` role now (the daemon exempts it
            # from send-self), so "can we type" is "does this lane declare the
            # guarded route", not "do we hold a retired credential".
            "can_type": (cap or {}).get("send") == "guarded",
        })
    rows.sort(key=lambda row: (row["project"] or "", row["pane_id"]))
    import fleet_inventory
    inventory = fleet_inventory.collect(row["pane_id"] for row in rows)
    payload = {"state": "ok", "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
               "rows": rows, "count": len(rows), "unrouted": inventory,
               "note": "shells and dead panes are not lanes and are not listed"}
    _FLEET_CACHE["at"], _FLEET_CACHE["payload"] = time.time(), payload
    return payload


@app.get("/api/ops-metrics")
def operations_metrics(_: Annotated[str, Depends(require_auth)]):
    from ops_metrics import connect, summary

    try:
        conn = connect()
        try:
            return summary(conn)
        finally:
            conn.close()
    except (OSError, sqlite3.Error) as exc:
        raise HTTPException(503, f"Operations metrics unavailable: {type(exc).__name__}") from exc


@app.get("/api/ops-metrics/history")
def operations_metrics_history(_: Annotated[str, Depends(require_auth)], hours: int = 24):
    from ops_metrics import connect, history

    if not 1 <= hours <= 168:
        raise HTTPException(422, "History window must be 1 to 168 hours")

    try:
        conn = connect()
        try:
            return history(conn, hours=hours)
        finally:
            conn.close()
    except (OSError, sqlite3.Error) as exc:
        raise HTTPException(503, f"Operations metrics history unavailable: {type(exc).__name__}") from exc


@app.post("/api/ops-metrics/refresh")
def refresh_operations_metrics(_: Annotated[str, Depends(require_auth)]):
    from ops_metrics import collect, connect

    try:
        conn = connect()
        try:
            return collect(conn, force_external=True)
        finally:
            conn.close()
    except (OSError, sqlite3.Error) as exc:
        raise HTTPException(503, f"Operations metrics unavailable: {type(exc).__name__}") from exc


@app.get("/api/ops-board")
def operations_board(_: Annotated[str, Depends(require_auth)]):
    """The deterministic operations board.

    Every value is read from Dunking Sheep or the private operations
    database; no model is consulted. The response is a cached snapshot that a
    background timer keeps current, so the desk never waits on a refresh.
    """
    import ops_board

    try:
        return ops_board.CACHE.snapshot()
    except Exception as exc:  # noqa: BLE001 - the desk shows the error, not a stack trace
        raise HTTPException(503, f"Operations board unavailable: {type(exc).__name__}") from exc


@app.post("/api/ops-board/answer")
def answer_owner_question(payload: dict, _: Annotated[str, Depends(require_auth)]):
    """Answer a lane's needs-owner question: record the reply as an `owner_answer` task
    note, then unblock the task.

    DESK_DESIGN 4.1 (t997): the desk's reply box. It goes through the daemon over the socket with the desk's own credential - the web never
    reads sqlite and never shells out - so the unblock is the same verb a lane would run.
    """
    task_id = str((payload or {}).get("task_id") or "").strip()
    reply = str((payload or {}).get("reply") or "").strip()
    if not task_id:
        raise HTTPException(400, "task_id is required")
    if not reply:
        raise HTTPException(400, "a reply is required")
    import ops_board

    try:
        client = ops_board._client()
        result = ops_board.answer_question(client, task_id, reply)
    except Exception as exc:  # noqa: BLE001 - the desk shows the error, not a stack trace
        raise HTTPException(503, f"Could not unblock {task_id}: {type(exc).__name__}") from exc
    return {"task_id": task_id, "unblocked": True, "task": result}


@app.post("/api/ops-board/not-now")
def not_now_owner_question(payload: dict, _: Annotated[str, Depends(require_auth)]):
    """'Not now': lower the task's priority so the question stops sitting at the top.

    The owner's ask (4.1) is a deferral, not an answer: the task stays blocked and the
    lane still waits, but the desk stops ranking it first. A priority change, nothing else.
    """
    task_id = str((payload or {}).get("task_id") or "").strip()
    if not task_id:
        raise HTTPException(400, "task_id is required")
    import ops_board

    try:
        client = ops_board._client()
        result = ops_board.defer_question(client, task_id)
    except Exception as exc:  # noqa: BLE001
        raise HTTPException(503, f"Could not lower {task_id}: {type(exc).__name__}") from exc
    return {"task_id": task_id, "priority": "low", "task": result}


@app.post("/api/ops-board/refresh")
def refresh_operations_board(_: Annotated[str, Depends(require_auth)]):
    import ops_board

    try:
        return ops_board.CACHE.refresh()
    except Exception as exc:  # noqa: BLE001
        raise HTTPException(503, f"Operations board unavailable: {type(exc).__name__}") from exc


def _message(row, conn):
    item = dict(row)
    direct = item.pop("direct_transport", None)
    if direct:
        item["transport"] = json.loads(direct)
        item["outcome"] = item["transport"].get("outcome")
    item["attachments"] = [dict(r) for r in conn.execute(
        "SELECT id,filename,mime,size FROM attachments WHERE message_id=? ORDER BY created_at", (row["id"],))]
    return item


def _prompt_for(row, conn):
    attachments = conn.execute("SELECT filename,path FROM attachments WHERE message_id=? ORDER BY created_at", (row["id"],)).fetchall()
    references = "\n".join(f"- {item['filename']}: {item['path']}" for item in attachments)
    prompt = row["body"].strip()
    if references:
        prompt += f"\n\nAttached files on this host (read these paths directly):\n{references}"
    return prompt + f"\n\n[VP web message #{row['id']}; reply through vpctl brief --channel {row['channel']} --answers {row['id']} when a concise response is ready.]"


def _send_saved(row, client, pane, force=False):
    with _db() as conn:
        prompt = _prompt_for(row, conn)
        attachments = [item["path"] for item in conn.execute(
            "SELECT path FROM attachments WHERE message_id=? ORDER BY created_at", (row["id"],))]
    # A generic model-name mailbox can be shared by unrelated panes. Until
    # Dunking Sheep gives such panes stable unique keys, address them by pane.
    mailbox = "" if force else str(client.call("inbox_count", mailbox=pane["pane_id"]).get("mailbox") or "")
    use_inbox = bool(mailbox) and mailbox.casefold() not in {"codex", "claude", "opencode"}
    if not use_inbox:
        result = client.call("send_text", target=pane["pane_id"], text=prompt,
                             hold_while_typing=not force, wait_s=0)
        delivery = "queued" if result.get("queued") else "sent" if result.get("ok") else "uncertain"
        detail = result.get("message", "")
    else:
        result = client.call("inbox_send", to=pane["pane_id"], body=prompt,
                             subject=f"VP web message #{row['id']}", attach=attachments, notify=True)
        if not result.get("message_id", "").startswith("i"):
            raise RuntimeError("Inbox send returned no message ID")
        delivery = {"queued": "inbox", "read": "read", "acked": "acked"}.get(result.get("state"), "uncertain")
        detail = {"inbox": "Stored in agent inbox", "read": "Read by agent", "acked": "Handled by agent"}.get(delivery, "Inbox state unconfirmed")
    with _db() as conn:
        conn.execute("UPDATE messages SET delivery=?,delivery_detail=?,transport_id=?,"
                     "transport_attempts=transport_attempts+1,transport_attempt_at=? WHERE id=?",
                     (delivery, detail, result.get("message_id"), time.time(), row["id"]))
    return result


def _reconcile_direct_guarded(channel: str, client=None):
    """Reconcile guarded direct sends against the daemon's durable outbox.

    A direct send that was QUEUED at post time (held behind a human typing in
    a guarded lane) keeps that state until something proves otherwise. The
    daemon owns that proof: the durable record for the message id bound at
    send time. Reconciliation here is read-only against the daemon - it never
    replays an ambiguous attempt - and it moves a row only when the record
    matches the row's own identity: the bound message id, the bound pane, and
    the creation marker the desk wrote into the prompt.
    """
    with _db() as conn:
        rows = conn.execute(
            "SELECT * FROM messages WHERE channel=? AND role='owner' AND "
            "delivery IN ('pending','queued','failed','uncertain') AND "
            "direct_transport IS NOT NULL ORDER BY id", (channel,)).fetchall()
    pending = []
    for row in rows:
        try:
            transport = json.loads(row["direct_transport"])
            # A corrupt stored receipt (list, null, string) describes only its
            # own row: reject it and keep going, so one bad receipt cannot
            # abort the pass and strand unrelated or legacy rows.
            if not isinstance(transport, dict):
                continue
            if transport.get("route") != "guarded":
                continue  # owner_push sends reconcile through pickup receipts.
            bound = str(transport.get("message_id") or "")
            if not bound or bound != (row["transport_id"] or ""):
                continue  # receipt is not identity-bound to this row: leave it.
        except (ValueError, TypeError, AttributeError):
            continue
        pending.append((row, transport, bound))
    if not pending:
        return
    if client is None:
        try:
            client = _flock()
        except Exception:
            # An outage never discards the durable owner message and never
            # invents an outcome from failing to ask.
            return
    for row, transport, bound in pending:
        try:
            record = client.call("get_message", message_id=bound)
        except Exception as exc:
            if "no message with id" in str(exc):
                # The daemon outbox no longer holds the record: in-memory
                # state was lost or the attempt never got bound. Keep the
                # durable row and mark the outcome unknown - never resend.
                with _db() as conn:
                    conn.execute(
                        "UPDATE messages SET delivery='uncertain',delivery_detail=? "
                        "WHERE id=? AND delivery IN ('pending','queued','failed','uncertain')",
                        ("Delivery unconfirmed; the transport record for this send is "
                         "unavailable. Check agent activity before sending again.", row["id"]))
            continue  # outage or another daemon error: retain last known state
        if not isinstance(record, dict) or record.get("message_id") != bound:
            continue
        if record.get("pane_id") != transport.get("pane"):
            continue  # a receipt must belong to the pane bound at send time
        text = str(record.get("text") or "")
        if f"[VP web message #{row['id']};" not in text:
            continue  # creation marker does not match: not this message
        state = record.get("status")
        if state not in ("queued", "sent", "cancelled"):
            continue  # an unrecognised state is unknown: retain last known
        detail = str(record.get("message") or state)
        # Confirmation (sent/cancelled) is terminal evidence: it may resolve a
        # row that reconciliation had marked uncertain. A still-queued record
        # is not terminal: it never overrides an unfortunate-uncertain mark.
        moving_states = ["pending", "queued", "failed"]
        if state != "queued":
            moving_states.append("uncertain")
        delivered_at = None
        if state == "sent":
            finished = record.get("finished_at")
            # Malformed daemon values stay absent; zero/negative/Infinity are
            # not timestamps (the owner_push path already validates the same
            # way). The sent status stands; the timestamp stays unknown.
            delivered_at = (finished if type(finished) in (int, float)
                            and math.isfinite(finished) and finished > 0 else None)
        with _db() as conn:
            cursor = conn.execute(
                "UPDATE messages SET delivery=?,delivery_detail=?,transport_id=? "
                "WHERE id=? AND delivery IN (%s)" % ",".join("?" for _ in moving_states),
                (state, detail, bound, row["id"], *moving_states))
            if cursor.rowcount and state == "sent":
                conn.execute(
                    "UPDATE messages SET delivered_at=?,delivery_latency_s=? "
                    "WHERE id=? AND delivery='sent' AND delivered_at IS NULL",
                    (delivered_at, None, row["id"]))
    # An error here belongs to the daemon pass as a whole (client creation);
    # per-row errors were handled above, so an outage simply leaves every row's
    # last known state intact and never invents an outcome from failing to ask.
    # No resend happens in any path of this function.


def _reconcile_owner_pushes(channel: str):
    """Match durable harness pickup receipts; an absent receipt proves nothing."""
    with _db() as conn:
        rows = conn.execute(
            "SELECT * FROM messages WHERE channel=? AND role='owner' AND delivery='stored' "
            "AND direct_transport IS NOT NULL AND delivered_at IS NULL ORDER BY id", (channel,)).fetchall()
    pending = []
    for row in rows:
        try:
            transport = json.loads(row["direct_transport"])
            push_id = transport.get("push_id")
            if (transport.get("route") != "owner_push" or type(push_id) is not int or push_id < 1
                    or str(push_id) != row["transport_id"] or not transport.get("pane")):
                continue
            pending.append((row, transport))
        except (ValueError, TypeError, AttributeError):
            continue
    if not pending:
        return
    try:
        client = _flock()
        for offset in range(0, len(pending), 100):
            batch = pending[offset:offset + 100]
            response = client.call("owner_receipts", push_ids=[t["push_id"] for _, t in batch])
            receipts = response.get("receipts")
            if not isinstance(receipts, list):
                return
            for row, transport in batch:
                matches = [r for r in receipts if isinstance(r, dict) and
                           type(r.get("id")) is int and r["id"] == transport["push_id"]]
                if len(matches) != 1:
                    continue
                receipt = matches[0]
                if receipt.get("pane_id") != transport["pane"]:
                    continue
                created, taken, started = receipt.get("created_at"), receipt.get("taken_at"), row["transport_attempt_at"]
                if not all(type(t) in (int, float) and math.isfinite(t) and t > 0
                           for t in (created, taken)):
                    continue
                # Zero is the legacy schema default: the attempt time is unknown.
                # Malformed or negative values remain invalid, not legacy rows.
                if type(started) not in (int, float) or not math.isfinite(started) or started < 0:
                    continue
                if taken > time.time():
                    continue
                # A wall-clock step invalidates the duration, not the matched
                # pickup itself. Keep the arrival evidence without inventing time.
                latency = taken - started if 0 < started <= created <= taken else None
                detail = "Picked up by agent harness"
                if started == 0:
                    detail += "; send timestamp unavailable, elapsed time unavailable"
                elif latency is None:
                    detail += "; wall clock moved, elapsed time unavailable"
                transport["pickup_receipt"] = {key: receipt[key] for key in ("id", "pane_id", "created_at", "taken_at")}
                with _db() as conn:
                    conn.execute(
                        "UPDATE messages SET delivery='delivered',delivery_detail=?,delivered_at=?,"
                        "delivery_latency_s=?,direct_transport=? WHERE id=? AND delivery='stored' AND delivered_at IS NULL",
                        (detail, taken, latency, json.dumps(transport), row["id"]))
    except Exception:
        # An older daemon, outage, or malformed response leaves the last known
        # stored state intact. Never infer pickup from pending-list absence.
        return


def _reconcile(channel: str):
    """Read transport state; never replay an ambiguous attempt automatically."""
    if not DELIVERY_LOCK.acquire(blocking=False):
        return
    try:
        _reconcile_owner_pushes(channel)
        _reconcile_direct_guarded(channel)
        read_cutoff = time.time() - READ_STATUS_WINDOW_S
        with _db() as conn:
            queued = conn.execute("SELECT * FROM messages WHERE channel=? AND "
                                  "(delivery IN ('pending','queued','failed','uncertain','inbox') "
                                  "OR (delivery='read' AND transport_attempt_at>?)) ORDER BY id",
                                  (channel, read_cutoff)).fetchall()
        if not queued:
            return
        client = _flock()
        legacy = [row for row in queued if row["direct_transport"] is None and not (row["transport_id"] or "").startswith("i")]
        recent = client.call("list_messages") if legacy else {"queued": [], "recent": []}
        queued_by_id = {item.get("message_id"): item for item in recent.get("queued", [])}
        for row in queued:
            if row["direct_transport"] is not None:
                continue  # Preserve the daemon receipt; legacy inbox reconciliation cannot interpret it.
            record = None
            transport_id = row["transport_id"]
            if transport_id and transport_id.startswith("i"):
                try:
                    inbox_record = client.call("inbox_status", message_id=transport_id)
                    state = inbox_record.get("state")
                except Exception:
                    continue  # Retain the last known state through a daemon outage.
                delivery = {"queued": "inbox", "read": "read", "acked": "acked", "cancelled": "cancelled"}.get(state)
                if delivery:
                    detail = {"inbox": "Stored in agent inbox", "read": "Read by agent",
                              "acked": "Handled by agent", "cancelled": "Cancelled"}[delivery]
                    if delivery == "inbox":
                        notify_state = (inbox_record.get("notify") or {}).get("state")
                        if notify_state == "queued":
                            detail += "; wakeup waiting for agent input to clear"
                        elif notify_state == "failed":
                            detail += "; wakeup failed"
                        elif notify_state == "sent":
                            detail += "; agent notified"
                    with _db() as conn:
                        conn.execute("UPDATE messages SET delivery=?,delivery_detail=? WHERE id=?",
                                     (delivery, detail, row["id"]))
                continue
            if transport_id:
                try:
                    record = client.call("get_message", message_id=transport_id)
                except Exception as exc:
                    if "no message with id" not in str(exc):
                        continue
            if record is None:
                matches = [item for item in recent.get("queued", []) + recent.get("recent", [])
                           if f"[VP web message #{row['id']};" in item.get("text", "")]
                if matches:
                    record = next((item for item in matches if item.get("status") == "sent"), matches[0])
                    transport_id = record.get("message_id")
            if record:
                state = record.get("status")
                if state in {"queued", "sent", "cancelled"}:
                    detail = record.get("message", "")
                    if state == "queued":
                        blocker = queued_by_id.get(transport_id, {}).get("blocked_by")
                        if blocker:
                            detail = f"Waiting: {blocker}"
                    with _db() as conn:
                        conn.execute("UPDATE messages SET delivery=?,delivery_detail=?,transport_id=? WHERE id=?",
                                     (state, detail, transport_id, row["id"]))
                    continue
            # A failed or vanished in-memory entry may actually have been
            # submitted before the daemon timed out. Keep its original ID and
            # let the owner inspect the pane; automatic replay caused duplicates.
            with _db() as conn:
                conn.execute("UPDATE messages SET delivery='uncertain',delivery_detail=? WHERE id=?",
                             ("Delivery unconfirmed. Check agent activity before sending again.", row["id"]))
    except Exception:
        # A daemon outage never discards the durable owner message.
        return
    finally:
        DELIVERY_LOCK.release()


def _delivery_watch():
    while not DELIVERY_STOP.wait(5):
        try:
            with _db() as conn:
                channels = [row[0] for row in conn.execute(
                    "SELECT DISTINCT channel FROM messages WHERE delivery IN "
                    "('pending','queued','failed','uncertain','inbox') "
                    "OR (delivery='read' AND transport_attempt_at>?)",
                    (time.time() - READ_STATUS_WINDOW_S,))]
            for channel in channels:
                _reconcile(channel)
        except Exception:
            pass


@app.on_event("startup")
def start_delivery_watch():
    import ops_board

    GAME.recover_orphans()
    DELIVERY_STOP.clear()
    app.state.delivery_thread = threading.Thread(target=_delivery_watch, name="vp-delivery-watch", daemon=True)
    app.state.delivery_thread.start()
    # Quota reads start immediately and have their own cadence, separate from board data.
    import ops_budget
    ops_budget.CACHE.snapshot()
    # The operations board refreshes itself on a timer; no model is involved.
    ops_board.CACHE.start()


@app.on_event("shutdown")
def stop_delivery_watch():
    import ops_board

    DELIVERY_STOP.set()
    app.state.delivery_thread.join(timeout=6)
    ops_board.CACHE.stop()
    import ops_budget
    ops_budget.CACHE.stop()
    GAME.stop()


class GameStart(BaseModel):
    fresh: bool = False


class GameKey(BaseModel):
    key: str = Field(min_length=1, max_length=16)


class GameType(BaseModel):
    text: str = Field(min_length=1, max_length=64)
    submit: bool = True


class GameSaveState(BaseModel):
    tag: str = Field(default="", max_length=80)


class GameLoadState(BaseModel):
    slot: int = Field(ge=1, le=99)


@app.get("/api/game/status")
def game_status(_: Annotated[str, Depends(require_auth)]):
    return GAME.status()


@app.get("/api/game/codewheel/{filename}")
def game_codewheel_image(filename: str, _: Annotated[str, Depends(require_auth)]):
    if not re.fullmatch(r"(?:esp|det)(?:0[1-9]|[12][0-9]|3[0-6])\.gif", filename):
        raise HTTPException(404, "Unknown rune template")
    path = CODEWHEEL_DIR / filename
    if path.is_symlink() or not path.is_file():
        raise HTTPException(404, "Private rune template unavailable")
    return FileResponse(path, media_type="image/gif", headers={
        "Cache-Control": "no-store", "X-Content-Type-Options": "nosniff",
        "Cross-Origin-Resource-Policy": "same-origin",
    })


@app.post("/api/game/start")
def game_start(body: GameStart, _: Annotated[str, Depends(require_auth)]):
    try:
        return GAME.start(body.fresh)
    except GameError as exc:
        raise HTTPException(503, str(exc)) from exc


@app.post("/api/game/stop")
def game_stop(_: Annotated[str, Depends(require_auth)]):
    return GAME.stop()


@app.post("/api/game/key")
def game_key(body: GameKey, _: Annotated[str, Depends(require_auth)]):
    try:
        GAME.key(body.key)
    except GameError as exc:
        raise HTTPException(400, str(exc)) from exc
    return {"ok": True}


@app.post("/api/game/type")
def game_type(body: GameType, _: Annotated[str, Depends(require_auth)]):
    try:
        GAME.type_text(body.text, body.submit)
    except GameError as exc:
        raise HTTPException(400, str(exc)) from exc
    return {"ok": True}


@app.get("/api/game/states")
def game_states(_: Annotated[str, Depends(require_auth)]):
    return GAME.list_states()


@app.post("/api/game/savestate")
def game_savestate(body: GameSaveState, _: Annotated[str, Depends(require_auth)]):
    try:
        return GAME.savestate(body.tag)
    except GameError as exc:
        raise HTTPException(400, str(exc)) from exc


@app.post("/api/game/loadstate")
def game_loadstate(body: GameLoadState, _: Annotated[str, Depends(require_auth)]):
    try:
        return GAME.loadstate(body.slot)
    except GameError as exc:
        raise HTTPException(400, str(exc)) from exc


@app.post("/api/game/autocodeword")
def game_autocodeword(_: Annotated[str, Depends(require_auth)]):
    try:
        templates = codeword.templates_cached(CODEWHEEL_DIR)
    except (OSError, subprocess.SubprocessError, ValueError):
        templates = []
    try:
        return GAME.recognize_codeword(templates)
    except GameError as exc:
        raise HTTPException(400, str(exc)) from exc


@app.get("/api/game/video")
def game_video(_: Annotated[str, Depends(require_auth)]):
    if not GAME.status()["running"]:
        raise HTTPException(404, "Poolrad is not running")
    return StreamingResponse(GAME.stream("video"), media_type="multipart/x-mixed-replace;boundary=ffmpeg",
                             headers={"Cache-Control": "no-store", "X-Content-Type-Options": "nosniff"})


@app.get("/api/game/audio")
def game_audio(_: Annotated[str, Depends(require_auth)]):
    status = GAME.status()
    if not status["running"] or not status["audio"]:
        raise HTTPException(404, "Poolrad audio is unavailable")
    return StreamingResponse(GAME.stream("audio"), media_type="audio/ogg", headers={"Cache-Control": "no-store"})


@app.get("/api/messages")
def messages(channel: str, _: Annotated[str, Depends(require_auth)], before: int | None = None, limit: int = 50):
    if not _known_channel(channel):
        raise HTTPException(404, "Unknown channel")
    _reconcile(channel)
    limit = max(1, min(limit, 100))
    with _db() as conn:
        rows = conn.execute("SELECT * FROM messages WHERE channel=? AND id<? ORDER BY id DESC LIMIT ?",
                            (channel, before or 2**63 - 1, limit + 1)).fetchall()
        more = len(rows) > limit
        rows = rows[:limit]
        last_delivery = conn.execute(
            "SELECT id,delivery,delivered_at,delivery_latency_s FROM messages "
            "WHERE channel=? AND role='owner' AND delivery='delivered' AND delivered_at IS NOT NULL "
            "ORDER BY delivered_at DESC,id DESC LIMIT 1", (channel,)).fetchone()
        return {"items": [_message(r, conn) for r in reversed(rows)], "has_more": more,
                "last_delivery": dict(last_delivery) if last_delivery else None,
                "next_before": rows[-1]["id"] if rows else None}


@app.get("/api/deliveries")
def deliveries(channel: str, ids: str, _: Annotated[str, Depends(require_auth)]):
    if not _known_channel(channel):
        raise HTTPException(404, "Unknown channel")
    try:
        message_ids = [int(part) for part in ids.split(",") if part]
    except ValueError:
        raise HTTPException(422, "Invalid message IDs")
    if len(message_ids) > 100 or any(item < 1 for item in message_ids):
        raise HTTPException(422, "Too many or invalid message IDs")
    _reconcile(channel)
    if not message_ids:
        return {"deliveries": {}}
    placeholders = ",".join("?" for _ in message_ids)
    with _db() as conn:
        rows = conn.execute(f"SELECT id,delivery,delivery_detail FROM messages WHERE channel=? AND id IN ({placeholders})",
                            [channel, *message_ids]).fetchall()
    return {"deliveries": {str(row["id"]): {"delivery": row["delivery"], "delivery_detail": row["delivery_detail"]} for row in rows}}


@app.get("/api/messages/{message_id}/context")
def message_context(message_id: int, _: Annotated[str, Depends(require_auth)]):
    with _db() as conn:
        anchor = conn.execute("SELECT * FROM messages WHERE id=?", (message_id,)).fetchone()
        if not anchor:
            raise HTTPException(404, "Message not found")
        channel = anchor["channel"]
    _reconcile(channel)
    with _db() as conn:
        anchor = conn.execute("SELECT * FROM messages WHERE id=?", (message_id,)).fetchone()
        earlier = conn.execute("SELECT * FROM messages WHERE channel=? AND id<? ORDER BY id DESC LIMIT 25",
                               (channel, message_id)).fetchall()
        later = conn.execute("SELECT * FROM messages WHERE channel=? AND id>? ORDER BY id ASC LIMIT 25",
                             (channel, message_id)).fetchall()
        rows = list(reversed(earlier)) + [anchor] + later
        has_older = conn.execute("SELECT 1 FROM messages WHERE channel=? AND id<? LIMIT 1",
                                 (channel, rows[0]["id"])).fetchone() is not None
        return {"channel": channel, "items": [_message(row, conn) for row in rows],
                "has_more": has_older, "next_before": rows[0]["id"]}


class MessageIn(BaseModel):
    channel: str
    body: str = Field(default="", max_length=12000)
    attachment_ids: list[str] = Field(default_factory=list, max_length=10)
    external_id: str | None = Field(default=None, max_length=100)


@app.post("/api/messages")
def post_message(data: MessageIn, identity: Annotated[str, Depends(require_auth)]):
    # Compatibility callers (including Telegram) share the direct pane transport.
    return _post_message(data, identity, direct=True)


def _post_message(data: MessageIn, identity: str, *, direct: bool = False):
    if not _known_channel(data.channel):
        raise HTTPException(404, "Unknown channel")
    if not data.body.strip() and not data.attachment_ids:
        raise HTTPException(422, "Message is empty")
    if data.external_id:
        with _db() as conn:
            existing = conn.execute("SELECT * FROM messages WHERE channel=? AND external_id=?",
                                    (data.channel, data.external_id)).fetchone()
            if existing:
                return _message(existing, conn)
    pane = _target(data.channel, _panes())
    if not is_lane(pane):
        raise HTTPException(409, "No active agent is available in this channel")
    with _db() as conn:
        if data.external_id:
            existing = conn.execute("SELECT * FROM messages WHERE channel=? AND external_id=?",
                                    (data.channel, data.external_id)).fetchone()
            if existing:
                return _message(existing, conn)
        attachment_rows = []
        for attachment_id in data.attachment_ids:
            row = conn.execute("SELECT * FROM attachments WHERE id=? AND message_id IS NULL", (attachment_id,)).fetchone()
            if not row:
                raise HTTPException(422, f"Attachment {attachment_id} is unavailable")
            attachment_rows.append(row)
        cursor = conn.execute("INSERT INTO messages(channel,role,body,delivery,transport_attempt_at,external_id) VALUES(?,?,?,?,?,?)",
                              (data.channel, "owner", data.body.strip(), "pending", time.time(), data.external_id))
        message_id = cursor.lastrowid
        for row in attachment_rows:
            conn.execute("UPDATE attachments SET message_id=? WHERE id=?", (message_id, row["id"]))
        if direct:
            conn.execute("UPDATE messages SET direct_transport='{}' WHERE id=?", (message_id,))
        row = conn.execute("SELECT * FROM messages WHERE id=?", (message_id,)).fetchone()
        prompt = _prompt_for(row, conn) if direct else None
    try:
        if direct:
            result = _send_to_pane(pane["pane_id"], prompt)
            with _db() as conn:
                conn.execute("UPDATE messages SET delivery=?,delivery_detail=?,transport_id=?,"
                             "direct_transport=?,transport_attempts=1 WHERE id=?",
                             (result["outcome"], result.get("detail", ""),
                              result.get("push_id") or result.get("message_id"),
                              json.dumps(result), message_id))
        else:
            result = _send_saved(row, _flock(), pane)
    except Exception as exc:
        result = {"error": str(exc)}
        with _db() as conn:
            conn.execute("UPDATE messages SET delivery='uncertain',delivery_detail=?,transport_attempts=transport_attempts+1,transport_attempt_at=? WHERE id=?",
                         ("Delivery unconfirmed. Check agent activity before sending again.", time.time(), message_id))
    with _db() as conn:
        row = conn.execute("SELECT * FROM messages WHERE id=?", (message_id,)).fetchone()
        item = _message(row, conn)
    item["transport"] = result
    return item


@app.post("/api/messages/{message_id}/send-now")
def send_now(message_id: int, identity: Annotated[str, Depends(require_auth)]):
    if identity != "owner":
        raise HTTPException(403, "Owner sign-in required")
    with DELIVERY_LOCK:
        with _db() as conn:
            row = conn.execute("SELECT * FROM messages WHERE id=? AND role='owner'", (message_id,)).fetchone()
        if not row:
            raise HTTPException(404, "Message not found")
        if row["delivery"] == "sent":
            with _db() as conn:
                return _message(row, conn)
        if row["delivery"] not in {"queued", "pending"} or not row["transport_id"]:
            raise HTTPException(409, "Message is being recovered; try again shortly")
        client = _flock()
        try:
            record = client.call("get_message", message_id=row["transport_id"])
        except Exception:
            raise HTTPException(409, "Transport state unavailable; try again shortly")
        if record.get("status") == "sent":
            with _db() as conn:
                conn.execute("UPDATE messages SET delivery='sent',delivery_detail='sent' WHERE id=?", (message_id,))
                return _message(conn.execute("SELECT * FROM messages WHERE id=?", (message_id,)).fetchone(), conn)
        if record.get("status") != "queued":
            raise HTTPException(409, "Message is no longer queued; refresh its status")
        queued = client.call("list_messages").get("queued", [])
        blocked = next((item.get("blocked_by") for item in queued
                        if item.get("message_id") == row["transport_id"]), None)
        if "someone is typing" not in record.get("message", "") and blocked != "input box contains a draft":
            raise HTTPException(409, "This message is queued for transport, not held behind a draft")
        try:
            client.call("cancel_message", message_id=row["transport_id"])
        except Exception:
            # The worker may have sent it between inspection and cancellation.
            latest = client.call("get_message", message_id=row["transport_id"])
            if latest.get("status") == "sent":
                with _db() as conn:
                    conn.execute("UPDATE messages SET delivery='sent',delivery_detail='sent' WHERE id=?", (message_id,))
                    return _message(conn.execute("SELECT * FROM messages WHERE id=?", (message_id,)).fetchone(), conn)
            raise HTTPException(409, "Message changed while sending; refresh its status")
        pane = _target(row["channel"], _panes())
        if not is_lane(pane):
            with _db() as conn:
                conn.execute("UPDATE messages SET delivery='uncertain',transport_id=NULL,delivery_detail='Agent pane unavailable after cancellation' WHERE id=?", (message_id,))
            raise HTTPException(409, "Agent pane unavailable; delivery is unconfirmed")
        try:
            _send_saved(row, client, pane, force=True)
        except Exception:
            with _db() as conn:
                conn.execute("UPDATE messages SET delivery='uncertain',transport_id=NULL,delivery_detail='Urgent send unconfirmed',"
                             "transport_attempts=transport_attempts+1,transport_attempt_at=? WHERE id=?", (time.time(), message_id))
            raise HTTPException(503, "Urgent send unconfirmed; check agent activity before resending")
        with _db() as conn:
            return _message(conn.execute("SELECT * FROM messages WHERE id=?", (message_id,)).fetchone(), conn)


class BriefIn(BaseModel):
    answers: list[Annotated[int, Field(strict=True, gt=0)]] = Field(default_factory=list, max_length=500)
    channel: str
    body: str = Field(min_length=1, max_length=20000)
    attachment_ids: list[str] = Field(default_factory=list, max_length=10)


@app.post("/api/brief")
def post_brief(data: BriefIn, identity: Annotated[str, Depends(require_auth)]):
    if identity != "api":
        raise HTTPException(403, "API token required")
    if not _known_channel(data.channel):
        raise HTTPException(404, "Unknown channel")
    with _db() as conn:
        for attachment_id in data.attachment_ids:
            row = conn.execute("SELECT id FROM attachments WHERE id=? AND message_id IS NULL",
                               (attachment_id,)).fetchone()
            if not row:
                raise HTTPException(422, f"Attachment {attachment_id} is unavailable")
        cursor = conn.execute("INSERT INTO messages(channel,role,body) VALUES(?,?,?)",
                              (data.channel, "agent", data.body.strip()))
        for attachment_id in data.attachment_ids:
            conn.execute("UPDATE attachments SET message_id=? WHERE id=?",
                         (cursor.lastrowid, attachment_id))
        row = conn.execute("SELECT * FROM messages WHERE id=?", (cursor.lastrowid,)).fetchone()
        # First explicit answer wins. Duplicate/retried links must not rewrite history.
        conn.executemany(
            "UPDATE messages SET answered_at=?,answer_id=? "
            "WHERE id=? AND channel=? AND role='owner' AND answer_id IS NULL AND answered_at IS NULL",
            [(row["created_at"], row["id"], answer, data.channel) for answer in set(data.answers)],
        )
        return _message(row, conn)


@app.get("/api/search")
def search(q: str, _: Annotated[str, Depends(require_auth)], channel: str | None = None, before: int | None = None):
    if len(q.strip()) < 2:
        return {"items": [], "has_more": False}
    if channel and not _known_channel(channel):
        raise HTTPException(404, "Unknown channel")
    clause = "AND channel=?" if channel else ""
    args = [f"%{q.strip()}%"] + ([channel] if channel else []) + [before or 2**63 - 1]
    with _db() as conn:
        rows = conn.execute(f"SELECT * FROM messages WHERE body LIKE ? {clause} AND id<? ORDER BY id DESC LIMIT 51", args).fetchall()
        return {"items": [_message(r, conn) for r in rows[:50]], "has_more": len(rows) > 50,
                "next_before": rows[49]["id"] if len(rows) >= 50 else None}


@app.post("/api/uploads")
async def upload(file: Annotated[UploadFile, File()], _: Annotated[str, Depends(require_auth)]):
    content = await file.read(MAX_UPLOAD + 1)
    if len(content) > MAX_UPLOAD:
        raise HTTPException(413, "Files must be 25 MB or smaller")
    original = Path(file.filename or "upload").name
    original = re.sub(r"[^\w. ()-]", "_", original)[:180] or "upload"
    attachment_id = uuid.uuid4().hex
    path = UPLOADS / f"{attachment_id}_{original}"
    path.write_bytes(content)
    path.chmod(0o600)
    with _db() as conn:
        conn.execute("INSERT INTO attachments(id,filename,mime,size,path) VALUES(?,?,?,?,?)",
                     (attachment_id, original, file.content_type or "application/octet-stream", len(content), str(path)))
    return {"id": attachment_id, "filename": original, "mime": file.content_type, "size": len(content)}


@app.get("/api/uploads/{attachment_id}")
def download(attachment_id: str, _: Annotated[str, Depends(require_auth)], preview: bool = False):
    with _db() as conn:
        row = conn.execute("SELECT * FROM attachments WHERE id=?", (attachment_id,)).fetchone()
    if not row:
        raise HTTPException(404, "Attachment not found")
    inline = preview and row["mime"] in {"image/png", "image/jpeg", "image/gif", "image/webp"}
    return FileResponse(row["path"], media_type=row["mime"], filename=row["filename"],
                        content_disposition_type="inline" if inline else "attachment",
                        headers={"X-Content-Type-Options": "nosniff"})


def _read_pane_activity(pane: dict, *, lines: int = 80, client=None):
    result = (client or _flock()).call("read_pane", target=pane["pane_id"], lines=lines)
    return {"pane_id": pane["pane_id"],
            "status": (_presence(result["agent_status"]) if "agent_status" in result
                       else pane.get("status", _presence(pane.get("agent_status")))),
            "text": result.get("text", "")}


@app.get("/api/panes/activity")
def panes_activity(response: Response, _: Annotated[str, Depends(require_auth)]):
    # One refresh per process, shared by all browsers. Cache failures too, so
    # a disconnected daemon cannot cause a request storm. Never persist output.
    response.headers["Cache-Control"] = "no-store"
    with PANES_ACTIVITY_LOCK:
        if time.monotonic() >= PANES_ACTIVITY_CACHE["expires"]:
            try:
                client = _flock()
                client.timeout = 2.0
                panes = client.call("list_panes")["panes"]
                rows = build_roster([], panes, include_terminals=True)

                def read(row):
                    try:
                        output = _read_pane_activity(row, lines=40, client=client)
                        # Bound the response even if an upstream ignores lines.
                        output["text"] = "\n".join(output["text"].splitlines()[-40:])[-32000:]
                        return {**row, **output, "error": None}
                    except Exception:
                        return {**row, "text": "", "error": "Pane output unavailable"}

                with ThreadPoolExecutor(max_workers=4) as readers:
                    rows = list(readers.map(read, rows))
                data = {"panes": rows, "checked_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                        "refresh_after_seconds": PANES_ACTIVITY_TTL_S}
            except Exception:
                # Socket exceptions can contain terminal details; keep errors generic.
                data = None
            PANES_ACTIVITY_CACHE.update(data=data, expires=time.monotonic() + PANES_ACTIVITY_TTL_S)
        if PANES_ACTIVITY_CACHE["data"] is None:
            raise HTTPException(503, "Herdr panes unavailable", headers={"Cache-Control": "no-store"})
        return PANES_ACTIVITY_CACHE["data"]


@app.get("/api/activity")
def activity(channel: str, _: Annotated[str, Depends(require_auth)]):
    pane = _target(channel, _panes())
    if not pane:
        raise HTTPException(404, "Pane unavailable")
    return {"channel": channel, **_read_pane_activity(pane)}


def _dashboard_module():
    """Load the standalone Poolrad dashboard script as a module (cached)."""
    script = DASHBOARD_DIR / "dashboard.py"
    key = str(script)
    module = DASHBOARD_MODULES.get(key)
    if module is None:
        if not script.is_file():
            return None
        spec = importlib.util.spec_from_file_location("vp_poolrad_dashboard", script)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        DASHBOARD_MODULES[key] = module
    return module


def _published_accounting():
    """The latest PM-reviewed accounting snapshot (published main) in the ops DB."""
    from ops_metrics import PROJECT, connect

    conn = connect()
    try:
        row = conn.execute(
            "SELECT commit_oid,strict_full_c,verified_game_identities,ceiling,bound_kind,"
            "observed_at,evidence_ref FROM accounting_snapshots WHERE project=? "
            "ORDER BY observed_at DESC,id DESC LIMIT 1", (PROJECT,),
        ).fetchone()
    finally:
        conn.close()
    if row is None:
        return None
    return {
        "commit_oid": row["commit_oid"],
        "observed_at": row["observed_at"],
        "evidence_ref": row["evidence_ref"],
        "accounting": {
            "status": "reviewed",
            "strict_full_c": row["strict_full_c"],
            "verified_identities": row["verified_game_identities"],
            "ceiling": row["ceiling"],
            "bound_kind": row["bound_kind"],
            "accounting_source": "docs/re/function_accounting_checkpoint.json",
            "evidence_doc": row["evidence_ref"],
        },
    }


def _replacement_count(commit):
    """Read recorded replacements at the same exact commit as the displayed checkpoint."""
    from ops_metrics import POOLRAD_REPO
    result = {"state": "unavailable", "count": None, "commit": commit,
              "source": "docs/re/replaced_bodies.json",
              "observed_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())}
    if not re.fullmatch(r"[0-9a-f]{40}", commit or ""):
        return result
    try:
        process = subprocess.run(["git", "-C", str(POOLRAD_REPO), "show",
                                  f"{commit}:docs/re/replaced_bodies.json"],
                                 capture_output=True, text=True, timeout=5, check=True)
        record = json.loads(process.stdout)
        bodies = record.get("bodies")
        if record.get("schema") != "replaced/1" or not isinstance(bodies, list):
            return result
        if any(not isinstance(row, dict) or not isinstance(row.get("id"), str)
               or not row.get("proven_at") or not row.get("evidence") for row in bodies):
            return result
        if len({row["id"] for row in bodies}) != len(bodies):
            return result
        result.update(state="ok", count=len(bodies), truth=record.get("truth"))
    except (OSError, subprocess.SubprocessError, ValueError, AttributeError):
        pass
    return result


@app.get("/api/poolrad-dashboard/progress")
def poolrad_dashboard_progress(_: Annotated[str, Depends(require_auth)]):
    """The compact Poolrad completion view: two percentages and one evidence line.

    The numbers prefer the PM-reviewed checkpoint published on main (read from
    the private operations database) and fall back to the configured checkout.
    A source that cannot be read stays unavailable; it never becomes a zero.
    """
    module = _dashboard_module()
    if module is None:
        raise HTTPException(404, "Poolrad dashboard unavailable")
    now = time.monotonic()
    if now >= POOLRAD_PROGRESS_CACHE["expires"]:
        try:
            published = _published_accounting()
            if published:
                view = module.compact_view(
                    DASHBOARD_REPO, published["accounting"],
                    commit=published["commit_oid"], source="published main")
                view["observed_at"] = published["observed_at"]
            else:
                view = module.progress(DASHBOARD_REPO)
                view["observed_at"] = view.get("generated")
        except HTTPException:
            raise
        except Exception as exc:  # noqa: BLE001 - keep the error generic
            raise HTTPException(503, "Poolrad dashboard progress unavailable") from exc
        view["replacements"] = _replacement_count(view.get("commit"))
        POOLRAD_PROGRESS_CACHE.update(data=view, expires=now + POOLRAD_PROGRESS_TTL_S)
    view = dict(POOLRAD_PROGRESS_CACHE["data"] or {})
    view["refresh_after_seconds"] = POOLRAD_PROGRESS_TTL_S
    return view


@app.post("/api/poolrad-dashboard/refresh")
def refresh_poolrad_dashboard(_: Annotated[str, Depends(require_auth)]):
    script = DASHBOARD_DIR / "dashboard.py"
    if not script.is_file():
        raise HTTPException(404, "Poolrad dashboard unavailable")
    published = None
    try:
        from ops_metrics import connect, published_accounting, record_accounting

        published = published_accounting()
        conn = connect()
        try:
            record_accounting(conn, published)
        finally:
            conn.close()
    except Exception:  # noqa: BLE001 - a stale published checkpoint still renders
        published = None
    try:
        result = subprocess.run([sys.executable, str(script), "--repo", str(DASHBOARD_REPO), "render"], cwd=ROOT,
                                capture_output=True, text=True, timeout=20, check=False)
    except (OSError, subprocess.TimeoutExpired):
        raise HTTPException(503, "Poolrad dashboard refresh failed") from None
    if result.returncode or not (DASHBOARD_DIR / "dashboard.html").is_file():
        raise HTTPException(503, "Poolrad dashboard refresh failed")
    POOLRAD_PROGRESS_CACHE["expires"] = 0.0
    return {"refreshed": True, "published_checkpoint": published["commit_oid"] if published else None}


@app.get("/api/poolrad-dashboard/dashboard.html")
def poolrad_dashboard_page(_: Annotated[str, Depends(require_auth)]):
    page = DASHBOARD_DIR / "dashboard.html"
    if not page.is_file():
        raise HTTPException(404, "Poolrad dashboard unavailable")
    return FileResponse(page, media_type="text/html", headers={
        "Cache-Control": "no-store", "X-Content-Type-Options": "nosniff",
        "Content-Security-Policy": "sandbox allow-scripts; default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; frame-ancestors 'self'",
    })


@app.get("/api/poolrad-dashboard/state/reports/{filename}")
def poolrad_dashboard_report(filename: str, _: Annotated[str, Depends(require_auth)]):
    if filename not in DASHBOARD_REPORTS:
        raise HTTPException(404, "Report unavailable")
    report = DASHBOARD_DIR / "state" / "reports" / filename
    if not report.is_file():
        raise HTTPException(404, "Report unavailable")
    return FileResponse(report, media_type="text/plain", headers={
        "Cache-Control": "no-store", "X-Content-Type-Options": "nosniff",
    })


def _host_agnostic_url(configured: str, hostname: str | None) -> str:
    """Point the frame at the host the owner actually signed in on.

    The Herdr web UI proxy validates the desk's own session cookie, and that
    cookie is bound to the hostname the owner used. A frame pointed at a
    different host would not carry the cookie and would answer 401, so the
    configured host is replaced with the hostname of this request while the
    port and path stay as configured.
    """
    if not configured or not hostname:
        return configured
    try:
        parsed = urlsplit(configured)
        port = parsed.port
    except ValueError:
        return configured
    if not parsed.scheme or not parsed.netloc:
        return configured
    host = hostname
    if ":" in host and not host.startswith("["):
        host = f"[{host}]"
    netloc = f"{host}:{port}" if port else host
    return urlunsplit((parsed.scheme, netloc, parsed.path or "/", parsed.query, ""))


@app.get("/api/herdr-webui")
def herdr_webui(request: Request, _: Annotated[str, Depends(require_auth)]):
    """Where the full Herdr web UI lives, if it is configured.

    The desk used to carry a read-only 'Herdr panes' grid; the live terminal
    UI is a separate service (herdr-webui). This endpoint only names it, so
    the desk can show the right frame or a link without guessing.
    """
    configured = os.getenv("VP_HERDR_WEBUI_URL", "").strip()
    url = _host_agnostic_url(configured, request.url.hostname)
    return {"url": url, "available": bool(url),
            "label": os.getenv("VP_HERDR_WEBUI_LABEL", "Herdr web UI").strip()}


@app.get("/api/health")
def health():
    return {"ok": True}


@app.get("/api/version")
def version():
    index = ROOT / "ui" / "dist" / "index.html"
    if not index.exists():
        return {"version": "unbuilt"}
    match = re.search(r'<script[^>]+src="([^"]+)"', index.read_text())
    return {"version": match.group(1) if match else str(index.stat().st_mtime_ns)}


DIST = ROOT / "ui" / "dist"
DASH_VIEWS = ("poolrad", "operations", "play", "cos", "fleet")
DESK_KEPT_PREFIXES = ("/api", "/dash", "/assets")
# Preserve useful old links without sending the owner into a session manager.
LEGACY_DASH_ALIASES = {
    "/poolrad": "poolrad",
    "/operations": "operations",
    "/play": "play",
    "/vp": "vp",
    "/cos": "cos",
    "/desk": "cos", "/dash/desk": "cos",      # the front desk (FLAT_FLEET.md 7.1); channel id stays cos
    "/vp": "cos", "/dash/vp": "cos",          # there is no VP; the desk answers
    "/fleet": "fleet",
}


def _shell_target(request: Request) -> str:
    """Keep legacy bookmarks on the current desk's conversations and boards."""
    return "/dash/" + LEGACY_DASH_ALIASES.get(request.url.path, "cos")


def _desk_keeps(path: str) -> bool:
    """True when the desk still serves this path itself.

    The middleware runs before routing, so the retired aliases below are
    listed here: they are answered by the shell redirect, not by the routes
    below them. /index.html is not kept: the bundle is reachable at "/" (the
    password page) and at the /dash/* documents, so a direct hit on the
    bundle file is retired with everything else.
    """
    if path in ("/api", "/dash", "/assets"):
        return True
    return any(path.startswith(prefix + "/") for prefix in DESK_KEPT_PREFIXES)


@app.middleware("http")
async def retire_the_desk_ui(request: Request, call_next):
    """Serve the current desk and recover old bookmarks into its own routes."""
    path = request.url.path
    if path == "/" or _desk_keeps(path):
        return await call_next(request)
    return RedirectResponse(_shell_target(request), status_code=307)


@app.get("/dash", include_in_schema=False)
@app.get("/dash/", include_in_schema=False)
def dash_root():
    """Open the chief-of-staff conversation by default."""
    return RedirectResponse("/dash/cos", status_code=307)

@app.get("/dash/{path:path}", include_in_schema=False)
def dash_route(path: str, request: Request):
    """Serve the desk bundle for a standalone dashboard route.

    The bundle reads the path and renders that dashboard without the desk
    chrome, so the herdr web UI can frame it. The session cookie is the same
    one the desk uses, so there is no second login. Without a session the
    request goes to the password UI with a return target, and no dashboard
    content is ever served.
    """
    name = path.strip("/")
    if name != path and name in DASH_VIEWS:
        return RedirectResponse(f"/dash/{name}", status_code=307)
    if not _session_valid(request):
        target = request.url.path + (f"?{request.url.query}" if request.url.query else "")
        return RedirectResponse(f"/?next={quote(target, safe='')}", status_code=302)
    if name == "poolrad":
        return RedirectResponse("/dash/operations#poolrad", status_code=307)
    index = DIST / "index.html"
    if not index.is_file():
        raise HTTPException(404, "Desk UI is not built")
    return FileResponse(index, media_type="text/html", headers={
        "Cache-Control": "no-store", "X-Content-Type-Options": "nosniff"})


if DIST.exists():
    app.mount("/", StaticFiles(directory=DIST, html=True), name="web")
