#!/usr/bin/env python3
"""
Dunking Sheep memory and recall service (v1).

SQLite + FTS5 in WAL mode. One background writer ingests Claude/Codex/opencode/
pi session logs; queries open a read-only connection and never reindex, never
swap an index, and never wait for a writer. That separation is the fix for the
2026-10-18 two-minute stall, not a change of engine brand.

See docs/MEMORY_SERVICE_ADR.md for the design and docs/MEMORY_SERVICE_ADR.md
acceptance bars. v1 is scoped to a few million chunks on this machine; billions
is a later sharded/remote tier.
"""

import collections
import json
import math
import os
import queue
import re
import sqlite3
from dunk_sqlite import connect as sqlite_connect
import threading
import time

CONFIG_DIR = os.environ.get("DUNKINGSHEEP_DIR") or os.path.expanduser(
    "~/.config/dunkingsheep"
)
MEMORY_DB = os.path.join(CONFIG_DIR, "memory.db")
SCHEMA_VERSION = 1
DEFAULT_CHUNK_CHARS = 4000
DEFAULT_OVERLAP_CHARS = 400

SCHEMA = """
PRAGMA journal_mode = WAL;
PRAGMA synchronous = NORMAL;
PRAGMA foreign_keys = ON;
PRAGMA busy_timeout = 5000;

CREATE TABLE IF NOT EXISTS schema_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS sources (
    id          INTEGER PRIMARY KEY,
    harness     TEXT NOT NULL,
    path        TEXT NOT NULL UNIQUE,
    project     TEXT,
    mtime       REAL NOT NULL,
    size        INTEGER NOT NULL,
    sha256      TEXT,
    state       TEXT NOT NULL DEFAULT 'pending',
    attempts    INTEGER NOT NULL DEFAULT 0,
    next_at     REAL,
    lease_until REAL,
    last_ingest REAL,
    error       TEXT,
    metadata    TEXT
);
CREATE INDEX IF NOT EXISTS sources_by_state ON sources(state, next_at);

CREATE TABLE IF NOT EXISTS sessions (
    id          TEXT PRIMARY KEY,
    harness     TEXT NOT NULL,
    project     TEXT,
    cwd         TEXT,
    title       TEXT,
    started_at  REAL,
    updated_at  REAL,
    metadata    TEXT
);
CREATE INDEX IF NOT EXISTS sessions_by_project ON sessions(project, updated_at);
CREATE INDEX IF NOT EXISTS sessions_by_harness ON sessions(harness, updated_at);

CREATE TABLE IF NOT EXISTS chunks (
    id          INTEGER PRIMARY KEY,
    session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    ordinal     INTEGER NOT NULL,
    role        TEXT,
    kind        TEXT,
    text        TEXT NOT NULL,
    token_count INTEGER,
    sha256      TEXT NOT NULL,
    created_at  REAL,
    metadata    TEXT
);
CREATE INDEX IF NOT EXISTS chunks_by_session ON chunks(session_id, ordinal);
CREATE UNIQUE INDEX IF NOT EXISTS chunks_dedupe
    ON chunks(sha256, session_id, ordinal);

CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
    text,
    content='chunks',
    content_rowid='id',
    tokenize='porter unicode61',
    prefix='2 3'
);

CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
    INSERT INTO chunks_fts(rowid, text) VALUES (new.id, new.text);
END;
CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, text)
        VALUES ('delete', old.id, old.text);
END;
CREATE TRIGGER IF NOT EXISTS chunks_au AFTER UPDATE ON chunks BEGIN
    INSERT INTO chunks_fts(chunks_fts, rowid, text)
        VALUES ('delete', old.id, old.text);
    INSERT INTO chunks_fts(rowid, text) VALUES (new.id, new.text);
END;

CREATE TABLE IF NOT EXISTS ingest_queue (
    id          INTEGER PRIMARY KEY,
    source_id   INTEGER NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    state       TEXT NOT NULL DEFAULT 'queued',
    attempts    INTEGER NOT NULL DEFAULT 0,
    next_at     REAL,
    lease_until REAL,
    error       TEXT,
    created_at  REAL NOT NULL,
    finished_at REAL
);
CREATE INDEX IF NOT EXISTS ingest_queue_ready ON ingest_queue(state, next_at);

CREATE TABLE IF NOT EXISTS memory_events (
    id          INTEGER PRIMARY KEY,
    at          REAL NOT NULL,
    event       TEXT NOT NULL,
    actor       TEXT,
    latency_ms  INTEGER,
    hits        INTEGER,
    source      TEXT,
    metadata    TEXT
);
CREATE INDEX IF NOT EXISTS memory_events_by_time ON memory_events(at, event);
"""

# Deliberately conservative: redact the obvious credential shapes rather than
# trying to be clever. A false negative here is stored forever.
SECRET_PATTERNS = (
    (re.compile(r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----.*?-----END [A-Z0-9 ]*PRIVATE KEY-----", re.S),
     "[REDACTED:private-key]"),
    (re.compile(r"\b(sk|rk|pk|ak)-[A-Za-z0-9_\-]{16,}\b"), "[REDACTED:api-key]"),
    (re.compile(r"\bgh[pousr]_[A-Za-z0-9]{20,}\b"), "[REDACTED:github-token]"),
    (re.compile(r"\bgithub_pat_[A-Za-z0-9_]{20,}\b"), "[REDACTED:github-token]"),
    (re.compile(r"\bxox[baprs]-[A-Za-z0-9-]{10,}\b"), "[REDACTED:slack-token]"),
    (re.compile(r"\bAKIA[0-9A-Z]{16}\b"), "[REDACTED:aws-key]"),
    (re.compile(r"\bAIza[0-9A-Za-z_\-]{30,}\b"), "[REDACTED:google-key]"),
    (re.compile(r"(?i)\b(bearer)\s+[A-Za-z0-9._\-]{16,}"), r"\1 [REDACTED:bearer]"),
    (re.compile(r"(?i)\b(api[_-]?key|secret|token|password|passwd|authorization)\b\s*[:=]\s*['\"]?([^\s'\"]{6,})"),
     r"\1=[REDACTED]"),
)


def redact(text):
    """Replace credential-shaped substrings with a stable marker."""
    if not text:
        return text
    for pattern, replacement in SECRET_PATTERNS:
        text = pattern.sub(replacement, text)
    return text


def chunk_text(text, max_chars=DEFAULT_CHUNK_CHARS,
               overlap_chars=DEFAULT_OVERLAP_CHARS):
    """Split text into overlapping chunks at whitespace boundaries."""
    text = (text or "").strip()
    if not text:
        return []
    if len(text) <= max_chars:
        return [text]
    chunks = []
    start = 0
    length = len(text)
    while start < length:
        end = min(length, start + max_chars)
        if end < length:
            boundary = max(text.rfind("\n", start, end),
                           text.rfind(" ", start, end))
            if boundary > start + max_chars // 2:
                end = boundary
        piece = text[start:end].strip()
        if piece:
            chunks.append(piece)
        if end >= length:
            break
        start = max(end - overlap_chars, start + 1)
    return chunks


def fts_tokens(text):
    return [token for token in re.findall(r"[A-Za-z0-9_]+", text or "")
            if len(token) >= 2][:16]


def fts_query(text, operator="AND"):
    """Safe FTS5 query: quoted tokens joined by operator (AND or OR)."""
    tokens = fts_tokens(text)
    if not tokens:
        return None
    if len(tokens) == 1:
        return '"%s"' % tokens[0]
    return (" %s " % operator).join('"%s"' % token for token in tokens)


def make_snippet(text, tokens, max_chars=240):
    """Cheap snippet from the stored text; do not use FTS5 snippet() in the
    query, which scans every match."""
    text = text or ""
    lowered = text.lower()
    position = -1
    for token in tokens:
        found = lowered.find(token.lower())
        if found >= 0 and (position < 0 or found < position):
            position = found
    if position < 0:
        return text[:max_chars]
    start = max(0, position - max_chars // 3)
    return text[start:start + max_chars]


def memory_sources():
    """Known session sources, most recently modified first.

    The full corpus: Codex rollouts (14 GB), Claude projects (975 MB), pi
    (21 MB), the 2.1 GB Codex thread-history sqlite and the opencode DB.
    """
    import glob as _glob
    home = os.path.expanduser("~")
    patterns = (
        os.path.join(home, ".codex/sessions/**/*.jsonl"),
        os.path.join(home, ".claude/projects/**/*.jsonl"),
        os.path.join(home, ".pi/agent/sessions/**/*.jsonl"),
    )
    sources = []
    for pattern in patterns:
        sources.extend(_glob.glob(pattern, recursive=True))
    for extra in (os.path.join(home, ".codex/thread_history_1.sqlite"),
                  os.path.join(home, ".local/share/opencode/opencode.db")):
        if os.path.exists(extra):
            sources.append(extra)
    unique = {os.path.abspath(path): path for path in sources if os.path.isfile(path)}
    ordered = sorted(unique, key=lambda path: os.path.getmtime(path), reverse=True)
    return ordered


def _harness_from_path(path):
    lowered = str(path).lower()
    if "/.codex/" in lowered or "thread_history" in lowered:
        return "codex"
    if "/.claude/" in lowered:
        return "claude"
    if "/.pi/" in lowered:
        return "pi"
    if "opencode" in lowered:
        return "opencode"
    return "generic"


def _project_from_cwd(cwd):
    if not cwd:
        return None
    cwd = str(cwd).rstrip("/")
    return os.path.basename(cwd) or cwd


def _text_from_content(content):
    """Extract text from a Claude/Codex/pi content value."""
    if content is None:
        return ""
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = []
        for block in content:
            if isinstance(block, str):
                parts.append(block)
            elif isinstance(block, dict):
                if block.get("type") in ("thinking", "redacted_thinking"):
                    continue
                for key in ("text", "input_text", "output_text", "content"):
                    value = block.get(key)
                    if isinstance(value, str):
                        parts.append(value)
                        break
        return "\n".join(part for part in parts if part)
    if isinstance(content, dict):
        return _text_from_content([content])
    return str(content)


class MemoryStore:
    """The database. The writer connection is serialized; reads are read-only."""

    def __init__(self, path=MEMORY_DB, create=True):
        self.path = path
        directory = os.path.dirname(os.path.abspath(path))
        if path != ":memory:":
            os.makedirs(directory, exist_ok=True)
        self._lock = threading.RLock()
        self.db = sqlite_connect(path, check_same_thread=False, timeout=5.0)
        self.db.row_factory = sqlite3.Row
        self.db.execute("PRAGMA journal_mode = WAL")
        self.db.execute("PRAGMA wal_autocheckpoint = 200")
        self.db.execute("PRAGMA synchronous = NORMAL")
        self.db.execute("PRAGMA foreign_keys = ON")
        self.db.execute("PRAGMA busy_timeout = 5000")
        if create:
            with self._lock:
                self.db.executescript(SCHEMA)
                self.db.execute(
                    "INSERT INTO schema_meta(key, value) VALUES('version', ?) "
                    "ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                    (str(SCHEMA_VERSION),))
                self.db.commit()
        # Query metrics live in memory. A per-query INSERT on the writer would
        # put every read behind the ingest writer - exactly the 2026-10-18 bug.
        self._pool = queue.LifoQueue()
        self._pool_lock = threading.Lock()
        self._pool_max = 16
        self._stats_lock = threading.Lock()
        self._query_latencies = collections.deque(maxlen=4000)
        self._query_count = 0
        self._query_errors = 0
        self._cache = collections.OrderedDict()
        self._cache_lock = threading.Lock()
        self._cache_ttl = 15.0
        self._cache_max = 1024
        self._cache_hits = 0

    # -- connections --------------------------------------------------------

    def _new_read_connection(self):
        conn = sqlite_connect(self.path, readonly=True,
                               check_same_thread=False, timeout=5.0)
        conn.row_factory = sqlite3.Row
        conn.execute("PRAGMA query_only = ON")
        conn.execute("PRAGMA busy_timeout = 5000")
        return conn

    def _read_connection(self):
        """Reuse a pooled read-only connection; opening one per query
        serialises on the WAL shm lock under 20 concurrent clients."""
        try:
            return self._pool.get_nowait()
        except queue.Empty:
            return self._new_read_connection()

    def _release_connection(self, conn):
        with self._pool_lock:
            if self._pool.qsize() < self._pool_max:
                self._pool.put_nowait(conn)
                return
        conn.close()

    def close(self):
        while True:
            try:
                self._pool.get_nowait().close()
            except queue.Empty:
                break
        with self._lock:
            self.db.close()

    def _begin_write(self):
        if self.db.in_transaction:
            self.db.commit()
        self.db.execute("BEGIN IMMEDIATE")

    def _record_query(self, latency_ms, hits, error=False, cached=False):
        with self._stats_lock:
            self._query_count += 1
            if cached:
                self._cache_hits += 1
            if error:
                self._query_errors += 1
            self._query_latencies.append(float(latency_ms))

    def _percentiles(self):
        with self._stats_lock:
            values = sorted(self._query_latencies)
            count = self._query_count
            errors = self._query_errors
        if not values:
            return {"count": 0, "p50": None, "p95": None, "p99": None,
                    "max": None, "errors": errors}
        def pick(fraction):
            index = min(len(values) - 1, int(round(fraction * (len(values) - 1))))
            return round(values[index], 2)
        with self._stats_lock:
            hits = self._cache_hits
        return {"count": count, "p50": pick(0.5), "p95": pick(0.95),
                "p99": pick(0.99), "max": round(values[-1], 2),
                "errors": errors, "cache_hits": hits}

    # -- sources and queue --------------------------------------------------

    def enqueue(self, path, harness=None, project=None):
        path = os.path.abspath(os.path.expanduser(str(path)))
        if not os.path.exists(path):
            raise ValueError(f"source does not exist: {path}")
        harness = harness or _harness_from_path(path)
        project = (str(project).strip() or None) if project else None
        stat = os.stat(path)
        now = time.time()
        with self._lock:
            self._begin_write()
            try:
                self.db.execute(
                    "INSERT INTO sources(harness, path, project, mtime, size, "
                    "state, next_at) VALUES(?,?,?,?,?,'pending',?) "
                    "ON CONFLICT(path) DO UPDATE SET harness=excluded.harness, "
                    "project=excluded.project, mtime=excluded.mtime, "
                    "size=excluded.size, state='pending', next_at=excluded.next_at, "
                    "error=NULL",
                    (harness, path, project, stat.st_mtime, stat.st_size, now))
                row = self.db.execute("SELECT id FROM sources WHERE path=?",
                                      (path,)).fetchone()
                queue_id = self.db.execute(
                    "INSERT INTO ingest_queue(source_id, state, next_at, created_at) "
                    "VALUES(?, 'queued', ?, ?)", (row["id"], now, now)).lastrowid
                self.db.commit()
            except Exception:
                self.db.rollback()
                raise
        return {"source_id": row["id"], "queue_id": queue_id,
                "path": path, "harness": harness, "project": project}

    def checkpoint(self):
        """Best-effort WAL checkpoint. PASSIVE never blocks on a live reader, so
        a health repair cannot stall the daemon; it consolidates what it can."""
        with self._lock:
            # A checkpoint cannot run inside a transaction; stay in autocommit.
            if self.db.in_transaction:
                self.db.commit()
            self.db.execute("PRAGMA busy_timeout=5000")
            row = self.db.execute("PRAGMA wal_checkpoint(PASSIVE)").fetchone()
        return {"ok": True, "result": tuple(row) if row else None}

    def requeue_partial(self):
        """Resume sources left partial by a deliberate stop (daemon restart).

        A restart marks the in-flight source 'skipped'/'partial' with reason
        'stopped by operator'. On the next start it is returned to the queue so
        coverage is not silently missing.
        """
        now = time.time()
        with self._lock:
            self._begin_write()
            try:
                rows = self.db.execute(
                    "SELECT q.id FROM ingest_queue q JOIN sources s ON s.id=q.source_id "
                    "WHERE q.state='skipped' AND s.state='partial' "
                    "AND (q.error LIKE 'stopped%' OR s.error LIKE 'stopped%')"
                ).fetchall()
                ids = [row["id"] for row in rows]
                for queue_id in ids:
                    self.db.execute(
                        "UPDATE ingest_queue SET state='queued', next_at=?, "
                        "error=NULL, lease_until=NULL WHERE id=?", (now, queue_id))
                if ids:
                    self.db.execute(
                        "UPDATE sources SET state='pending', lease_until=NULL "
                        "WHERE state='partial'")
                self.db.commit()
            except Exception:
                self.db.rollback()
                raise
        return {"requeued": len(ids)}

    def recover_leases(self):
        """Requeue ingest leases whose worker died (expired lease).

        Called when the ingest worker starts, so a daemon restart resumes the
        source it was processing instead of stranding it in 'leased'.
        """
        now = time.time()
        with self._lock:
            self._begin_write()
            try:
                cursor = self.db.execute(
                    "UPDATE ingest_queue SET state='queued', lease_until=NULL "
                    "WHERE state='leased' "
                    "AND (lease_until IS NULL OR lease_until < ?)", (now,))
                requeued = cursor.rowcount
                self.db.execute(
                    "UPDATE sources SET state='pending', lease_until=NULL "
                    "WHERE state='ingesting'")
                self.db.commit()
            except Exception:
                self.db.rollback()
                raise
        return {"requeued": requeued}

    def claim_next(self, lease_s=120.0):
        now = time.time()
        with self._lock:
            self._begin_write()
            try:
                row = self.db.execute(
                    "SELECT q.id AS queue_id, q.source_id, q.attempts, "
                    "s.path, s.harness, s.project "
                    "FROM ingest_queue q JOIN sources s ON s.id = q.source_id "
                    "WHERE q.state IN ('queued', 'failed') "
                    "AND (q.next_at IS NULL OR q.next_at <= ?) "
                    "ORDER BY q.id LIMIT 1", (now,)).fetchone()
                if row is None:
                    self.db.commit()
                    return None
                self.db.execute(
                    "UPDATE ingest_queue SET state='leased', lease_until=?, "
                    "attempts=attempts+1 WHERE id=?",
                    (now + lease_s, row["queue_id"]))
                self.db.execute(
                    "UPDATE sources SET state='ingesting', lease_until=? WHERE id=?",
                    (now + lease_s, row["source_id"]))
                self.db.commit()
                return dict(row)
            except Exception:
                self.db.rollback()
                raise

    def finish(self, lease, ok, error=None, stats=None, skipped=False):
        now = time.time()
        stats = stats or {}
        with self._lock:
            self._begin_write()
            try:
                if skipped:
                    # A deliberate stop (operator/acceptance) is not a failure:
                    # record it as skipped with a reason so coverage is visible
                    # and the source can be resumed by a later enqueue.
                    self.db.execute(
                        "UPDATE ingest_queue SET state='skipped', finished_at=?, "
                        "error=?, lease_until=NULL WHERE id=?",
                        (now, str(error or "skipped")[:2000], lease["queue_id"]))
                    self.db.execute(
                        "UPDATE sources SET state='partial', error=?, "
                        "lease_until=NULL WHERE id=?",
                        (str(error or "skipped")[:2000], lease["source_id"]))
                    self.db.commit()
                    return {"ok": False, "skipped": True, "stats": stats,
                            "error": error}
                if ok:
                    self.db.execute(
                        "UPDATE ingest_queue SET state='done', finished_at=?, "
                        "error=NULL, lease_until=NULL WHERE id=?",
                        (now, lease["queue_id"]))
                    self.db.execute(
                        "UPDATE sources SET state='ok', last_ingest=?, "
                        "lease_until=NULL, error=NULL WHERE id=?",
                        (now, lease["source_id"]))
                    self.db.execute(
                        "INSERT INTO memory_events(at, event, source, hits, metadata) "
                        "VALUES(?, 'ingest', ?, ?, ?)",
                        (now, lease["path"], stats.get("chunks", 0),
                         json.dumps(stats)))
                else:
                    attempts = int(lease.get("attempts") or 0) + 1
                    delay = min(300, 5 * (2 ** min(attempts, 6)))
                    self.db.execute(
                        "UPDATE ingest_queue SET state='failed', error=?, "
                        "next_at=?, lease_until=NULL WHERE id=?",
                        (str(error)[:2000], now + delay, lease["queue_id"]))
                    self.db.execute(
                        "UPDATE sources SET state='error', error=?, "
                        "lease_until=NULL WHERE id=?",
                        (str(error)[:2000], lease["source_id"]))
                self.db.commit()
            except Exception:
                self.db.rollback()
                raise
        return {"ok": ok, "stats": stats, "error": error}

    # -- ingestion ----------------------------------------------------------

    def ingest_path(self, path, harness=None, project=None, should_stop=None,
                    throttle_s=0.0):
        """Parse one source. Returns counts. Never called from a query path."""
        path = os.path.abspath(os.path.expanduser(str(path)))
        harness = harness or _harness_from_path(path)
        lowered = path.lower()
        if lowered.endswith((".jsonl", ".json")):
            result = self._ingest_jsonl(path, harness, project, should_stop,
                                         throttle_s)
        elif lowered.endswith((".sqlite", ".sqlite3", ".db")):
            if "thread_history" in lowered or "codex" in harness:
                result = self._ingest_codex_history(path, project, should_stop,
                                                    throttle_s)
            else:
                result = self._ingest_opencode(path, project, should_stop,
                                               throttle_s)
        else:
            raise ValueError(f"unsupported memory source: {path}")
        self._upsert_source_row(path, harness, project)
        return result

    def _upsert_source_row(self, path, harness, project):
        stat = os.stat(path)
        now = time.time()
        with self._lock:
            self.db.execute(
                "INSERT INTO sources(harness, path, project, mtime, size, state, "
                "last_ingest) VALUES(?,?,?,?,?,'ok',?) "
                "ON CONFLICT(path) DO UPDATE SET harness=excluded.harness, "
                "project=COALESCE(excluded.project, sources.project), "
                "mtime=excluded.mtime, size=excluded.size, state='ok', "
                "last_ingest=excluded.last_ingest, error=NULL",
                (harness, path, project, stat.st_mtime, stat.st_size, now))
            self.db.commit()

    def _upsert_session(self, session_id, harness, project=None, cwd=None,
                        title=None, started_at=None, updated_at=None,
                        metadata=None):
        with self._lock:
            self.db.execute(
                "INSERT INTO sessions(id, harness, project, cwd, title, "
                "started_at, updated_at, metadata) VALUES(?,?,?,?,?,?,?,?) "
                "ON CONFLICT(id) DO UPDATE SET project=COALESCE(excluded.project, "
                "sessions.project), cwd=COALESCE(excluded.cwd, sessions.cwd), "
                "title=COALESCE(excluded.title, sessions.title), "
                "started_at=COALESCE(sessions.started_at, excluded.started_at), "
                "updated_at=MAX(COALESCE(sessions.updated_at, 0), "
                "COALESCE(excluded.updated_at, 0)), "
                "metadata=COALESCE(excluded.metadata, sessions.metadata)",
                (session_id, harness, project, cwd, title, started_at,
                 updated_at or time.time(),
                 json.dumps(metadata) if metadata else None))

    def _insert_chunks(self, session_id, chunks, role=None, kind=None,
                       created_at=None, metadata=None):
        if not chunks:
            return 0
        import hashlib
        inserted = 0
        with self._lock:
            self._begin_write()
            try:
                for ordinal, text in enumerate(chunks):
                    clean = redact(text)
                    digest = hashlib.sha256(clean.encode("utf-8")).hexdigest()
                    cursor = self.db.execute(
                        "INSERT OR IGNORE INTO chunks(session_id, ordinal, role, "
                        "kind, text, token_count, sha256, created_at, metadata) "
                        "VALUES(?,?,?,?,?,?,?,?,?)",
                        (session_id, ordinal, role, kind, clean,
                         max(1, len(clean) // 4), digest, created_at,
                         json.dumps(metadata) if metadata else None))
                    inserted += cursor.rowcount
                self.db.commit()
                try:
                    self.db.execute("PRAGMA wal_checkpoint(PASSIVE)")
                except sqlite3.OperationalError:
                    pass
            except Exception:
                self.db.rollback()
                raise
        return inserted

    def _ingest_jsonl(self, path, harness, project=None, should_stop=None,
                      throttle_s=0.0):
        session_id = os.path.splitext(os.path.basename(path))[0]
        cwd = None
        title = None
        started_at = None
        sessions = set()
        chunk_count = 0
        pending = []
        pending_role = None
        pending_at = None
        last_at = None

        def flush():
            nonlocal chunk_count, pending
            if pending:
                chunk_count += self._insert_chunks(
                    session_id, pending, role=pending_role,
                    created_at=pending_at)
                pending = []
                if throttle_s:
                    time.sleep(throttle_s)

        with open(path, "r", encoding="utf-8", errors="replace") as handle:
            for line in handle:
                if should_stop is not None and should_stop():
                    flush()
                    return {"sessions": len(sessions) or 1,
                            "chunks": chunk_count, "harness": harness,
                            "stopped": True}
                line = line.strip()
                if not line:
                    continue
                try:
                    event = json.loads(line)
                except ValueError:
                    continue
                if not isinstance(event, dict):
                    continue
                now = _event_time(event)
                if harness == "codex":
                    kind = event.get("type")
                    payload = event.get("payload") or {}
                    if kind == "session_meta":
                        session_id = str(payload.get("session_id") or
                                         payload.get("id") or session_id)
                        cwd = payload.get("cwd") or cwd
                        started_at = started_at or _iso_time(payload.get("timestamp"))
                        title = title or payload.get("originator")
                        self._upsert_session(session_id, harness,
                                             project or _project_from_cwd(cwd),
                                             cwd, title, started_at, now)
                        continue
                    if kind == "turn_context":
                        cwd = payload.get("cwd") or cwd
                        continue
                    if kind != "response_item":
                        continue
                    if payload.get("type") != "message":
                        continue
                    role = payload.get("role")
                    if role not in ("user", "assistant"):
                        continue
                    text = _text_from_content(payload.get("content"))
                elif harness == "claude":
                    if event.get("type") not in ("user", "assistant"):
                        if event.get("type") == "ai-title":
                            title = event.get("title") or title
                        continue
                    message = event.get("message") or {}
                    role = message.get("role") or event.get("type")
                    if role not in ("user", "assistant"):
                        continue
                    text = _text_from_content(message.get("content"))
                    session_id = str(event.get("sessionId") or session_id)
                    cwd = event.get("cwd") or cwd
                    started_at = started_at or _iso_time(event.get("timestamp"))
                    now = _iso_time(event.get("timestamp")) or now
                    title = title or event.get("slug")
                elif harness == "pi":
                    kind = event.get("type")
                    if kind == "session":
                        session_id = str(event.get("id") or session_id)
                        cwd = event.get("cwd") or cwd
                        started_at = started_at or _iso_time(event.get("timestamp"))
                        self._upsert_session(session_id, harness,
                                             project or _project_from_cwd(cwd),
                                             cwd, title, started_at, now)
                        continue
                    if kind != "message":
                        continue
                    message = event.get("message") or {}
                    role = message.get("role")
                    if role not in ("user", "assistant"):
                        continue
                    text = _text_from_content(message.get("content"))
                else:
                    role = event.get("role") or (event.get("message") or {}).get("role")
                    if role not in ("user", "assistant"):
                        continue
                    text = event.get("text") or _text_from_content(
                        (event.get("message") or {}).get("content"))
                    session_id = str(event.get("session_id") or session_id)
                    cwd = event.get("cwd") or cwd
                    now = _iso_time(event.get("timestamp")) or now

                if not text or not text.strip():
                    continue
                if project is None and cwd:
                    project = _project_from_cwd(cwd)
                self._upsert_session(session_id, harness, project, cwd,
                                     title, started_at, now)
                # Flush when the role changes so chunks keep role semantics.
                if pending and role != pending_role:
                    flush()
                pending_role = role
                pending_at = pending_at or now
                last_at = now
                pending.extend(chunk_text(text))
                sessions.add(session_id)
                if len(pending) >= 500:
                    flush()
        flush()
        self._upsert_session(session_id, harness, project, cwd, title,
                             started_at, last_at or time.time())
        return {"sessions": len(sessions) or 1, "chunks": chunk_count,
                "harness": harness}

    def _ingest_codex_history(self, path, project=None, should_stop=None,
                              throttle_s=0.0):
        conn = sqlite_connect(path, readonly=True)
        conn.row_factory = sqlite3.Row
        chunk_count = 0
        sessions = set()
        try:
            rows = conn.execute(
                "SELECT thread_id, created_at_ms, item_json FROM thread_items "
                "ORDER BY thread_id, rollout_ordinal")
            current = None
            pending = []
            pending_at = None
            for row in rows:
                if should_stop is not None and should_stop():
                    break
                thread_id = str(row["thread_id"])
                if thread_id != current:
                    if current is not None:
                        chunk_count += self._insert_chunks(
                            current, pending, role="assistant",
                            created_at=pending_at)
                    current = thread_id
                    pending = []
                    pending_at = (row["created_at_ms"] / 1000.0
                                  if row["created_at_ms"] else None)
                    self._upsert_session(thread_id, "codex", project,
                                         None, None, pending_at, pending_at)
                    sessions.add(thread_id)
                try:
                    item = json.loads(row["item_json"] or "{}")
                except ValueError:
                    continue
                item_type = item.get("type") or ""
                if item_type not in ("userMessage", "agentMessage"):
                    continue
                role = "user" if item_type == "userMessage" else "assistant"
                text = _text_from_content(item.get("content"))
                if not text.strip():
                    continue
                pending.extend(chunk_text(text))
                if len(pending) >= 500:
                    chunk_count += self._insert_chunks(
                        thread_id, pending, role=role,
                        created_at=row["created_at_ms"] and row["created_at_ms"] / 1000.0)
                    pending = []
            if current is not None and pending:
                chunk_count += self._insert_chunks(
                    current, pending, role="assistant", created_at=pending_at)
        finally:
            conn.close()
        return {"sessions": len(sessions), "chunks": chunk_count,
                "harness": "codex"}

    def _ingest_opencode(self, path, project=None, should_stop=None,
                         throttle_s=0.0):
        conn = sqlite_connect(path, readonly=True)
        conn.row_factory = sqlite3.Row
        chunk_count = 0
        sessions = set()
        try:
            rows = conn.execute(
                "SELECT s.id, s.title, s.directory, s.time_created, "
                "m.data AS message_data, p.data AS part_data, p.time_created "
                "FROM session s JOIN message m ON m.session_id = s.id "
                "JOIN part p ON p.message_id = m.id "
                "ORDER BY s.id, m.time_created, p.time_created")
            current = None
            pending = []
            pending_at = None
            for row in rows:
                if should_stop is not None and should_stop():
                    break
                session_id = str(row["id"])
                if session_id != current:
                    if current is not None and pending:
                        chunk_count += self._insert_chunks(
                            current, pending, role="assistant",
                            created_at=pending_at)
                    current = session_id
                    pending = []
                    pending_at = (row["time_created"] / 1000.0
                                  if row["time_created"] else None)
                    self._upsert_session(
                        session_id, "opencode",
                        project or _project_from_cwd(row["directory"]),
                        row["directory"], row["title"],
                        row["time_created"] and row["time_created"] / 1000.0,
                        pending_at)
                    sessions.add(session_id)
                try:
                    message = json.loads(row["message_data"] or "{}")
                    part = json.loads(row["part_data"] or "{}")
                except ValueError:
                    continue
                if part.get("type") not in ("text", None):
                    continue
                text = part.get("text") or ""
                if not text.strip():
                    continue
                role = message.get("role") or "assistant"
                pending.extend(chunk_text(text))
                if len(pending) >= 500:
                    chunk_count += self._insert_chunks(
                        session_id, pending, role=role, created_at=pending_at)
                    pending = []
            if current is not None and pending:
                chunk_count += self._insert_chunks(
                    current, pending, role="assistant", created_at=pending_at)
        finally:
            conn.close()
        return {"sessions": len(sessions), "chunks": chunk_count,
                "harness": "opencode"}

    # -- queries ------------------------------------------------------------

    def recall(self, query, limit=10, harness=None, project=None, since=None,
               max_chars=1200):
        started = time.perf_counter()
        tokens = fts_tokens(query)
        and_fts = fts_query(query, "AND")
        or_fts = fts_query(query, "OR")
        if not and_fts:
            self._record_query(0.0, 0)
            return []
        cache_key = (query, int(limit), harness, project, since)
        now = time.time()
        with self._cache_lock:
            entry = self._cache.get(cache_key)
            if entry is not None and entry[0] >= now:
                self._cache.move_to_end(cache_key)
                cached = [dict(item) for item in entry[1]]
                self._record_query((time.perf_counter() - started) * 1000,
                                   len(cached), cached=True)
                return cached
        def fetch(match):
            sql = (
                "SELECT c.id AS chunk_id, c.session_id, c.role, c.kind, "
                "c.ordinal, c.text, c.created_at, s.harness, s.project, s.cwd, "
                "s.title, s.updated_at, bm25(chunks_fts) AS score "
                "FROM chunks_fts JOIN chunks c ON c.id = chunks_fts.rowid "
                "JOIN sessions s ON s.id = c.session_id "
                "WHERE chunks_fts MATCH ?")
            params = [match]
            if harness:
                sql += " AND s.harness = ?"
                params.append(harness)
            if project:
                sql += " AND s.project = ?"
                params.append(project)
            if since:
                sql += " AND COALESCE(s.updated_at, c.created_at) >= ?"
                params.append(float(since))
            sql += " ORDER BY bm25(chunks_fts) LIMIT ?"
            params.append(max(int(limit) * 3, 20))
            conn = self._read_connection()
            try:
                return conn.execute(sql, params).fetchall()
            finally:
                self._release_connection(conn)

        try:
            rows = fetch(and_fts)
            if not rows and or_fts and or_fts != and_fts:
                rows = fetch(or_fts)
        except Exception:
            self._record_query((time.perf_counter() - started) * 1000, 0,
                               error=True)
            raise
        now = time.time()
        ranked = []
        for row in rows:
            updated = row["updated_at"] or row["created_at"] or now
            age_days = max(0.0, (now - updated) / 86400.0)
            score = -float(row["score"] or 0.0) * 100.0 + 0.25 * math.exp(-age_days / 30.0)
            text = row["text"] or ""
            ranked.append({
                "chunk_id": row["chunk_id"],
                "session_id": row["session_id"],
                "harness": row["harness"],
                "project": row["project"],
                "cwd": row["cwd"],
                "title": row["title"],
                "role": row["role"],
                "kind": row["kind"],
                "ordinal": row["ordinal"],
                "created_at": row["created_at"],
                "updated_at": row["updated_at"],
                "score": round(score, 4),
                "snippet": make_snippet(text, tokens)[:max_chars],
                "text": text[:max_chars],
            })
        ranked.sort(key=lambda item: item["score"], reverse=True)
        per_session = {}
        final = []
        for item in ranked:
            if per_session.get(item["session_id"], 0) >= 2:
                continue
            per_session[item["session_id"]] = per_session.get(item["session_id"], 0) + 1
            final.append(item)
            if len(final) >= int(limit):
                break
        with self._cache_lock:
            self._cache[cache_key] = (time.time() + self._cache_ttl, final)
            while len(self._cache) > self._cache_max:
                self._cache.popitem(last=False)
        self._record_query((time.perf_counter() - started) * 1000, len(final))
        return [dict(item) for item in final]

    def latest_session(self, harness, cwd=None):
        """The most recently updated indexed session for a harness and cwd.

        Read-only and exact: it returns a real session id or None. Recovery uses
        it only when no recorded launch exists; it never invents a session."""
        if not harness or not cwd:
            return None
        conn = self._read_connection()
        try:
            row = conn.execute(
                "SELECT id, harness, cwd, updated_at FROM sessions "
                "WHERE harness = ? AND cwd = ? "
                "ORDER BY updated_at DESC LIMIT 1", (harness, cwd)).fetchone()
            return dict(row) if row else None
        finally:
            self._release_connection(conn)

    def context(self, query, sessions=5, snippets_per_session=2):
        rows = self.recall(query, limit=max(20, sessions * 4))
        grouped = {}
        for row in rows:
            bucket = grouped.setdefault(row["session_id"], {
                "session_id": row["session_id"], "harness": row["harness"],
                "project": row["project"], "cwd": row["cwd"],
                "title": row["title"], "updated_at": row["updated_at"],
                "score": row["score"], "snippets": [],
            })
            if len(bucket["snippets"]) < snippets_per_session:
                bucket["snippets"].append({
                    "chunk_id": row["chunk_id"], "role": row["role"],
                    "snippet": row["snippet"], "score": row["score"],
                })
        ordered = sorted(grouped.values(), key=lambda item: item["score"],
                         reverse=True)[:sessions]
        lines = []
        for item in ordered:
            title = (item.get("title") or "").replace("\n", " ").strip()
            lines.append(f"[{item['harness']}] {item['project'] or '-'}"
                         + (f" | {title[:120]}" if title else ""))
            for snippet in item["snippets"]:
                body = " ".join((snippet["snippet"] or "").split())
                if body:
                    lines.append(f"  - {body[:280]}")
        return {
            "query": query,
            "generated_at": time.time(),
            "sessions": ordered,
            "counts": {"sessions": len(ordered),
                       "snippets": sum(len(item["snippets"]) for item in ordered)},
            "text": "\n".join(lines),
        }

    def stats(self):
        conn = self._read_connection()
        try:
            def count(table, where="", params=()):
                return conn.execute(
                    f"SELECT COUNT(*) AS n FROM {table} {where}", params
                ).fetchone()["n"]
            counts = {"sources": count("sources"),
                      "sessions": count("sessions"),
                      "chunks": count("chunks"),
                      "partial": count("sources", "WHERE state='partial'")}
            by_harness = [dict(row) for row in conn.execute(
                "SELECT harness, COUNT(*) AS chunks, COUNT(DISTINCT session_id) "
                "AS sessions FROM chunks c JOIN sessions s ON s.id = c.session_id "
                "GROUP BY harness ORDER BY chunks DESC").fetchall()]
            queue = {
                "queued": count("ingest_queue", "WHERE state='queued'"),
                "leased": count("ingest_queue", "WHERE state='leased'"),
                "failed": count("ingest_queue", "WHERE state='failed'"),
                "skipped": count("ingest_queue", "WHERE state='skipped'"),
                "done": count("ingest_queue", "WHERE state='done'"),
            }
            last = conn.execute(
                "SELECT MAX(last_ingest) AS last FROM sources").fetchone()["last"]
        finally:
            self._release_connection(conn)
        sizes = {}
        for suffix in ("", "-wal", "-shm"):
            path = self.path + suffix
            sizes[suffix or "db"] = os.path.getsize(path) if os.path.exists(path) else 0
        return {
            "path": self.path,
            "schema_version": SCHEMA_VERSION,
            "sources": counts["sources"],
            "sessions": counts["sessions"],
            "chunks": counts["chunks"],
            "harness": by_harness,
            "queue": queue,
            "queue_depth": queue["queued"] + queue["leased"] + queue["failed"],
            "partial_sources": counts["partial"],
            "db_bytes": sizes,
            "last_ingest": last,
            "queries": self._percentiles(),
        }

    def status(self):
        stats = self.stats()
        stats["ok"] = stats["queue"]["failed"] == 0
        return stats


# -- event parsing helpers ---------------------------------------------------

def _iso_time(value):
    if not value:
        return None
    try:
        from datetime import datetime
        if isinstance(value, (int, float)):
            return float(value) / (1000.0 if value > 1e12 else 1.0)
        text = str(value).replace("Z", "+00:00")
        return datetime.fromisoformat(text).timestamp()
    except (ValueError, TypeError):
        return None


def _event_time(event):
    for key in ("timestamp", "time", "created_at"):
        if key in event:
            parsed = _iso_time(event.get(key))
            if parsed:
                return parsed
    return time.time()


class MemoryIngestor:
    """The single writer. Queries never touch this thread."""

    def __init__(self, store, poll_s=0.5, throttle_s=None):
        self.store = store
        self.poll_s = poll_s
        self.throttle_s = (throttle_s if throttle_s is not None else
                           float(os.environ.get(
                               "DUNKINGSHEEP_MEMORY_THROTTLE_S", "0.1")))
        self._stop = threading.Event()
        self._paused = threading.Event()
        self._thread = None
        self._last_error = None
        self._last_result = None
        self._lock = threading.Lock()

    def start(self):
        if self._thread and self._thread.is_alive():
            return
        try:
            self.store.recover_leases()
            self.store.requeue_partial()
        except Exception:  # noqa: BLE001 - start must not fail on recovery
            pass
        self._stop.clear()
        self._thread = threading.Thread(target=self._loop, name="memory-ingest",
                                        daemon=True)
        self._thread.start()

    def pause(self):
        self._paused.set()

    def resume(self):
        self._paused.clear()

    def paused(self):
        return self._paused.is_set()

    def stop(self):
        self._stop.set()
        if self._thread:
            self._thread.join(timeout=5)
        self._thread = None

    def enqueue(self, path, harness=None, project=None):
        return self.store.enqueue(path, harness=harness, project=project)

    def status(self):
        with self._lock:
            return {"running": bool(self._thread and self._thread.is_alive()),
                    "paused": self._paused.is_set(),
                    "throttle_s": self.throttle_s,
                    "last_error": self._last_error,
                    "last_result": self._last_result}

    def _loop(self):
        while not self._stop.wait(self.poll_s):
            if self._paused.is_set():
                continue
            try:
                lease = self.store.claim_next()
            except Exception as error:  # noqa: BLE001 - keep the worker alive
                with self._lock:
                    self._last_error = f"claim: {error}"
                continue
            if not lease:
                continue
            try:
                result = self.store.ingest_path(
                    lease["path"], harness=lease["harness"],
                    project=lease.get("project"),
                    should_stop=self._stop.is_set,
                    throttle_s=self.throttle_s)
                if result.get("stopped"):
                    self.store.finish(lease, False,
                                      error="stopped by operator",
                                      skipped=True)
                else:
                    self.store.finish(lease, True, stats=result)
                with self._lock:
                    self._last_result = result
                    self._last_error = None
            except Exception as error:  # noqa: BLE001 - record and retry
                self.store.finish(lease, False, error=str(error))
                with self._lock:
                    self._last_error = str(error)
