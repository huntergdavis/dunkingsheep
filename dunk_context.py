"""The declarative context store: .dunkingsheep/ per project, one global.

Files are the source of truth (one Markdown entry with YAML-ish front matter).
`.index.db` is a DISPOSABLE FTS5 cache: delete it and `index` rebuilds the same
search results. A ground truth never expires and is never edited in place; only
supersession changes it, and the chain is the audit trail. An agent may propose
a ground truth but only an owner act promotes one - the indexer refuses an
agent-authority ground truth rather than downgrading it silently.

Spec: vp-of-agentry/docs/CONTEXT_STORE.md (owner #1167/#1169/#1171/#1235).
"""

from __future__ import annotations

import json
import os
import re
import sqlite3
from dunk_sqlite import connect as sqlite_connect
import time
from datetime import date, datetime, timedelta, timezone
from pathlib import Path

CLASSES = ("ground-truth", "fact", "provisional", "history")
AUTHORITIES = ("owner", "measured", "agent")
TYPES = ("principle", "decision", "reference", "lesson", "measurement", "incident",
         "rule", "wiki")
# Types only the owner may write (docs/FLAT_FLEET.md 8): a rule is a standing
# instruction, and standing instructions come from one person.
OWNER_ONLY_TYPES = ("rule", "principle")
LEDGER_PATH = Path(os.environ.get(
    "DUNKINGSHEEP_DIR", str(Path.home() / ".config" / "dunkingsheep"))) / "context-ledger.db"
SCOPES = ("global", "project")
DEFAULT_CAP = 8192
PROVISIONAL_DAYS = 7
GLOBAL_DIR = Path(os.environ.get(
    "DUNKINGSHEEP_DIR", str(Path.home() / ".config" / "dunkingsheep"))) / "context"


class ContextError(ValueError):
    """A named refusal for the context store."""


def _sha(text):
    import hashlib
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


class Ledger:
    """What the daemon wrote: the table the digest trusts (FLAT_FLEET.md 8).

    Every lane runs as the same Unix user, so a file under `context/` proves
    nothing about who wrote it. The daemon records (store, slug, sha256) for
    every entry it writes; the digest injects a ground truth only when the
    file on disk still matches a row here, and `check` names every file that
    does not. A lane can forge this table too; the ADR says so. It is the
    strongest guarantee a same-uid fleet allows, and it stops every accident.

    With no rows at all (before `context ledger adopt`) the ledger is in
    bootstrap: nothing is refused, and `check` says to adopt.
    """

    def __init__(self, path=None):
        self.path = Path(path or os.environ.get("DUNKINGSHEEP_CONTEXT_LEDGER") or LEDGER_PATH)
        self._db = None

    def _connect(self):
        if self._db is None:
            self.path.parent.mkdir(parents=True, exist_ok=True)
            self._db = sqlite3.connect(str(self.path), timeout=5)
            self._db.row_factory = sqlite3.Row
            self._db.execute(
                "CREATE TABLE IF NOT EXISTS entries (store TEXT NOT NULL, slug TEXT NOT NULL, "
                "sha256 TEXT NOT NULL, cls TEXT, authority TEXT, written_at REAL NOT NULL, "
                "by TEXT, PRIMARY KEY (store, slug))")
            self._db.commit()
            try:
                os.chmod(self.path, 0o600)
            except OSError:
                pass
        return self._db

    def close(self):
        if self._db is not None:
            self._db.close()
            self._db = None

    def record(self, store_root_path, slug, text, cls=None, authority=None, by=None):
        db = self._connect()
        db.execute("INSERT OR REPLACE INTO entries (store, slug, sha256, cls, authority, "
                   "written_at, by) VALUES (?,?,?,?,?,?,?)",
                   (str(store_root_path), slug, _sha(text), cls, authority, time.time(), by))
        db.commit()

    def status(self, store_root_path, slug, text):
        """'recorded' | 'modified' | 'unrecorded' | 'bootstrap' (no rows at all)."""
        db = self._connect()
        if not db.execute("SELECT 1 FROM entries LIMIT 1").fetchone():
            return "bootstrap"
        row = db.execute("SELECT sha256 FROM entries WHERE store = ? AND slug = ?",
                         (str(store_root_path), slug)).fetchone()
        if row is None:
            return "unrecorded"
        return "recorded" if row["sha256"] == _sha(text) else "modified"

    def count(self):
        return self._connect().execute("SELECT COUNT(*) FROM entries").fetchone()[0]


def store_root(path=None, *, global_store=False):
    if global_store:
        return Path(path or GLOBAL_DIR).expanduser()
    return Path(path or ".dunkingsheep").expanduser()


def _slug(text):
    return re.sub(r"[^a-z0-9]+", "-", str(text).lower()).strip("-") or "entry"


def _today():
    return date.today().isoformat()


def _parse_front(text):
    if not text.startswith("---"):
        raise ContextError("entry has no front matter (must start with ---)")
    parts = text.split("---", 2)
    if len(parts) < 3:
        raise ContextError("entry front matter is not closed with ---")
    front, body = parts[1], parts[2].lstrip("\n")
    meta = {}
    for line in front.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if ":" not in line:
            raise ContextError(f"bad front-matter line: {line!r}")
        key, value = line.split(":", 1)
        value = value.strip()
        if value in ("null", "~", ""):
            value = None
        meta[key.strip()] = value
    for key in ("class", "subject", "type", "scope", "authority", "stated"):
        if not meta.get(key):
            raise ContextError(f"front matter is missing {key}")
    if meta["class"] not in CLASSES:
        raise ContextError(f"class must be one of {CLASSES}")
    if meta["authority"] not in AUTHORITIES:
        raise ContextError(f"authority must be one of {AUTHORITIES}")
    return meta, body


def _render(meta, body):
    lines = ["---"]
    for key in ("class", "subject", "type", "scope", "authority", "stated",
                "source", "recheck_after", "supersedes", "superseded_by",
                "retired_at", "retired_reason"):
        value = meta.get(key)
        lines.append(f"{key}: {value if value is not None else 'null'}")
    lines.append("---")
    return "\n".join(lines) + "\n\n" + body.strip() + "\n"


def _days(value):
    match = re.fullmatch(r"(\d+)d", str(value or "").strip())
    return int(match.group(1)) if match else None


def select_stores(project_path=None, *, which=None, cwd=None):
    """The stores a command should report on, each labelled.

    `which` is 'global', 'project' or None. None is honest: if a project store
    exists at cwd, report BOTH; otherwise report global only (and say so).
    """
    project = Path(project_path or ".dunkingsheep").expanduser()
    if which == "global":
        return {"global": Store(global_store=True)}
    if which == "project":
        return {"project": Store(project)}
    if project.is_dir():
        return {"global": Store(global_store=True), "project": Store(project)}
    return {"global": Store(global_store=True)}


def digest_both(project_path=None, *, which=None, max_bytes=DEFAULT_CAP, subject=None, record=True):
    """The session digest. Global ground truths are ALWAYS included.

    A digest scoped to the working directory is correct-looking and wrong: a
    fresh agent in a project would be told there are zero rules. Global truths
    never expire, so they are never omitted by scope; project facts are selected
    by subject under the byte cap.
    """
    stores = select_stores(project_path, which=which)
    ledger = Ledger()
    truths, relevant, unverified = [], [], []
    for name, store in stores.items():
        for entry in store.live_entries():
            if entry.get("error"):
                continue
            if entry["class"] == "ground-truth":
                if store.verified(entry, ledger):
                    truths.append((name, entry))
                else:
                    unverified.append((name, entry))
            elif subject is None or entry.get("subject") == subject:
                relevant.append((name, entry))
    included = [(name, e) for name, e in truths]
    lines = [f"## Ground truths ({len(truths)})"]
    lines += [f"- [{e['subject']}] {e['body'].splitlines()[0]}{_stale_mark(e)}"
              for _n, e in truths] or ["- none"]
    if unverified:
        lines.append(f"- ({len(unverified)} ground-truth file(s) not written by the daemon "
                     f"were NOT injected: {', '.join(e['slug'] for _n, e in unverified)}; "
                     "see `dunkingsheep context check`)")
    lines += ["", "## Relevant"]
    used = len("\n".join(lines).encode("utf-8"))
    omitted = 0
    for name, entry in relevant:
        line = f"- [{name}/{entry['class']}/{entry['subject']}] {entry['body'].splitlines()[0]}"
        stale = ""
        if entry["class"] != "ground-truth" and _days(entry.get("recheck_after")):
            try:
                horizon = datetime.fromisoformat(str(entry["stated"])) + timedelta(days=_days(entry["recheck_after"]))
                if datetime.now() > horizon:
                    stale = " (STALE - re-check before trusting)"
            except ValueError:
                pass
        size = len((line + stale + "\n").encode("utf-8"))
        if used + size > max_bytes:
            omitted += 1
            continue
        lines.append(line + stale)
        included.append((name, entry))
        used += size
    if omitted:
        lines.append(f"omitted: {omitted} entries over the {max_bytes}-byte cap")
    text = "\n".join(lines) + "\n"
    if record:
        # A dashboard rendering the digest is a VIEW of what agents receive, not a
        # delivery to an agent. Recording it would make injected_7d count polls.
        for name, entry in included:
            stores[name].log_access(entry["slug"], kind="injected")
    return text


def _stale_mark(entry):
    """' (STALE ...)' when an entry is past its recheck_after; ground truths too."""
    days = _days(entry.get("recheck_after"))
    if not days:
        return ""
    try:
        horizon = datetime.fromisoformat(str(entry["stated"])) + timedelta(days=days)
    except ValueError:
        return ""
    return " (STALE - re-check before trusting)" if datetime.now() > horizon else ""


def report(project_path=None, *, which=None):
    """stats/check/index over every selected store, always labelled."""
    out = {}
    for name, store in select_stores(project_path, which=which).items():
        out[name] = {"path": str(store.root),
                     "stats": store.stats(), "check": store.check(),
                     "entries": len(store.live_entries())}
    return out


class Store:
    def __init__(self, path=None, *, global_store=False, ledger=None):
        self.root = store_root(path, global_store=global_store)
        self.scope = "global" if global_store else "project"
        # None means "do not record": the daemon passes its Ledger; a local
        # bootstrap run passes one explicitly; a bare Store never writes rows.
        self.ledger = ledger

    def _record(self, path, slug, meta, by=None):
        if self.ledger is not None:
            self.ledger.record(self.root, slug, Path(path).read_text(), cls=meta.get("class"),
                               authority=meta.get("authority"), by=by)

    def verified(self, entry, ledger=None):
        """Does the file on disk match what the daemon wrote? (bootstrap counts.)"""
        ledger = ledger or self.ledger or Ledger()
        try:
            text = Path(entry["path"]).read_text()
        except OSError:
            return False
        return ledger.status(self.root, entry["slug"], text) in ("recorded", "bootstrap")

    def adopt(self, ledger=None, by="adopt"):
        """Bootstrap: record every live entry as-is. Owner's one-shot."""
        ledger = ledger or self.ledger or Ledger()
        adopted = []
        for entry in self.live_entries():
            ledger.record(self.root, entry["slug"], Path(entry["path"]).read_text(),
                          cls=entry.get("class"), authority=entry.get("authority"), by=by)
            adopted.append(entry["slug"])
        return adopted

    # -- files ------------------------------------------------------------
    def ensure(self, *, meta_lines=None):
        self.root.mkdir(parents=True, exist_ok=True)
        for name in CLASSES:
            (self.root / name).mkdir(exist_ok=True)
        readme = self.root / "README.md"
        if not readme.exists():
            readme.write_text("# .dunkingsheep context store\n\nDeprecated: see vp-of-agentry/docs/CONTEXT_STORE.md.\n")
        return self.root

    def exists(self):
        """A store exists when it has a root with any class directory or README.

        An absent store is not an empty one: reporting '0 articles' for a path
        with no store is a zero where the truth is 'initialise it'.
        """
        if not self.root.is_dir():
            return False
        return any((self.root / name).is_dir() for name in CLASSES) or (self.root / "README.md").is_file()

    def entries(self):
        out = []
        if not self.root.exists():
            return out
        for name in CLASSES:
            folder = self.root / name
            if not folder.is_dir():
                continue
            for file in sorted(folder.glob("*.md")):
                try:
                    meta, body = _parse_front(file.read_text())
                except (OSError, ContextError) as exc:
                    out.append({"slug": file.stem, "path": str(file), "error": str(exc)})
                    continue
                meta.update({"slug": file.stem, "path": str(file), "body": body.strip()})
                out.append(meta)
        return out

    def find(self, subject_or_slug):
        entries = self.entries()
        for entry in entries:
            if entry.get("slug") == subject_or_slug:
                return entry
        for entry in entries:
            if entry.get("subject") == subject_or_slug:
                return entry
        return None

    @staticmethod
    def is_retired(entry):
        # Older retire commands wrote only this exact, dated body marker.
        return bool(entry.get("retired_at") or re.search(
            r"^\*\*Retired \d{4}-\d{2}-\d{2}\*\*:",
            entry.get("body", ""), re.MULTILINE))

    def live_entries(self):
        """Current heads only; entries()/find() retain the historical audit trail."""
        return [e for e in self.entries() if not e.get("error")
                and not e.get("superseded_by") and not self.is_retired(e)]

    def resolve(self, subject_or_slug):
        """Resolve a subject or historical slug to its non-retired chain head."""
        entry = self.find(subject_or_slug)
        visited = set()
        while entry is not None:
            slug = entry["slug"]
            if slug in visited:
                raise ContextError(f"supersession cycle at {slug}")
            visited.add(slug)
            if entry.get("error"):
                raise ContextError(entry["error"])
            successor = entry.get("superseded_by")
            if not successor:
                return None if self.is_retired(entry) else entry
            entry = next((e for e in self.entries() if e["slug"] == successor), None)
            if entry is None:
                raise ContextError(f"supersession target {successor} not found")
        return None

    # -- writes -----------------------------------------------------------
    def add(self, body, *, cls, subject, type_, authority="agent", source="",
            stated=None, recheck_after=None, supersedes=None, slug=None, by=None):
        if cls not in CLASSES:
            raise ContextError(f"class must be one of {CLASSES}")
        if authority not in AUTHORITIES:
            raise ContextError(f"authority must be one of {AUTHORITIES}")
        if type_ not in TYPES:
            raise ContextError(f"type must be one of {TYPES}; got {type_!r}")
        if cls == "ground-truth" and authority != "owner":
            raise ContextError("a ground truth has authority owner; an agent proposes a "
                               "fact and the owner promotes it")
        self.ensure()
        slug = slug or f"{stated or _today()}-{_slug(subject)}"
        folder = self.root / cls
        folder.mkdir(exist_ok=True)
        path = folder / f"{slug}.md"
        if path.exists():
            # A supersede on the same day derives the same slug, so the guarantee
            # "supersession is the only way an entry changes" was unimplementable
            # within a day: the new file collided with the old one. A superseding
            # entry gets a distinct, deterministic slug instead.
            if not supersedes:
                raise ContextError(f"entry {slug} already exists")
            counter = 2
            while (folder / f"{slug}-{counter}.md").exists():
                counter += 1
            slug = f"{slug}-{counter}"
            path = folder / f"{slug}.md"
        if supersedes:
            old = self.resolve(supersedes)
            if old is None:
                raise ContextError(f"supersede target {supersedes} not found")
            supersedes = old["slug"]
            old["superseded_by"] = slug
            Path(old["path"]).write_text(_render(old, old["body"]))
            self._record(old["path"], old["slug"], old, by=by)
        meta = {"class": cls, "subject": subject, "type": type_, "scope": self.scope,
                "authority": authority, "stated": stated or _today(), "source": source,
                "recheck_after": recheck_after if recheck_after is not None
                else ("7d" if cls == "provisional" else None),
                "supersedes": supersedes, "superseded_by": None}
        path.write_text(_render(meta, body))
        self._record(path, slug, meta, by=by)
        return {"slug": slug, "path": str(path), "class": cls}

    def supersede(self, slug, new_body, by=None, **meta):
        old = self.resolve(slug)
        if old is None:
            raise ContextError(f"entry {slug} not found")
        return self.add(new_body, cls=old["class"], subject=old["subject"],
                        type_=old.get("type") or "reference",
                        authority=meta.get("authority", old.get("authority") or "agent"),
                        source=meta.get("source") or old.get("source") or "",
                        recheck_after=meta.get("recheck_after", old.get("recheck_after")),
                        supersedes=old["slug"], by=by)

    def retire(self, slug, reason="", by=None):
        entry = self.resolve(slug)
        if entry is None:
            raise ContextError(f"entry {slug} not found")
        entry["retired_at"] = _today()
        entry["retired_reason"] = reason.replace("\n", " ")
        body = entry["body"] + f"\n\n**Retired {_today()}**: {reason}\n"
        path = Path(entry["path"])
        path.write_text(_render(entry, body))
        self._record(path, entry["slug"], entry, by=by)
        return {"slug": entry["slug"], "retired": True, "reason": reason}

    # -- checks -----------------------------------------------------------
    def check(self):
        problems, counts = [], {name: 0 for name in CLASSES}
        ledger = self.ledger or Ledger()
        if ledger.count() == 0:
            problems.append({"slug": "-", "warning": True,
                             "problem": "ledger is empty (bootstrap): every entry is trusted until "
                                        "`dunkingsheep context ledger adopt` runs"})
        for entry in self.entries():
            if entry.get("error"):
                problems.append({"slug": entry["slug"], "problem": entry["error"]})
                continue
            counts[entry["class"]] += 1
            if entry["class"] == "ground-truth" and entry["authority"] != "owner":
                problems.append({"slug": entry["slug"],
                                 "problem": "ground truth with authority != owner (refused)"})
            if entry["class"] == "ground-truth" and entry.get("type") not in OWNER_ONLY_TYPES + ("decision",):
                problems.append({"slug": entry["slug"], "warning": True,
                                 "problem": f"ground truth typed {entry.get('type')!r}; expected rule/principle/decision"})
            if entry["class"] == "ground-truth" and not entry.get("recheck_after") \
                    and not self.is_retired(entry):
                problems.append({"slug": entry["slug"], "warning": True,
                                 "problem": "ground truth has no recheck_after (rules go stale too)"})
            ledger_state = ledger.status(self.root, entry["slug"], Path(entry["path"]).read_text())
            if ledger_state == "unrecorded":
                problems.append({"slug": entry["slug"],
                                 "problem": "not written by the daemon (no ledger row); "
                                            "a ground truth here is NOT injected"})
            elif ledger_state == "modified":
                problems.append({"slug": entry["slug"],
                                 "problem": "edited on disk after the daemon wrote it; NOT injected"})
            if entry["class"] == "fact" and not entry.get("recheck_after"):
                problems.append({"slug": entry["slug"], "problem": "fact has no recheck_after"})
            if entry.get("supersedes") and not (self.root / entry["class"] / f"{entry['supersedes']}.md").exists():
                problems.append({"slug": entry["slug"],
                                 "problem": f"supersedes missing entry {entry['supersedes']}"})
            if entry["class"] != "ground-truth" and _days(entry.get("recheck_after")):
                try:
                    stated = datetime.fromisoformat(str(entry["stated"]))
                    horizon = stated + timedelta(days=_days(entry["recheck_after"]))
                    if datetime.now() > horizon:
                        problems.append({"slug": entry["slug"],
                                         "problem": f"stale: past recheck_after {entry['recheck_after']}",
                                         "warning": True})
                except ValueError:
                    problems.append({"slug": entry["slug"], "problem": "unparseable stated date"})
        return {"counts": counts, "problems": problems,
                "refusals": [p for p in problems if not p.get("warning")],
                "stale": [p for p in problems if p.get("warning")]}

    # -- the disposable index ---------------------------------------------
    def index_path(self):
        return self.root / ".index.db"

    # The columns search() reads. v2.35.0: files are truth, the FTS index is a
    # disposable cache - so a cache carrying anything else is disposable too.
    INDEX_COLUMNS = ("slug", "subject", "body", "class", "scope")
    # Bounded: an FTS5 table is created by exactly this header. There is no
    # schema parsing beyond it - a cache is not a document to interpret.
    _FTS5_HEADER = re.compile(
        r'^\s*CREATE\s+VIRTUAL\s+TABLE\s+["\[]?entries["\]]?\s+USING\s+fts5\s*\(', re.I)
    # The one query shape search() uses: named columns, MATCH, rank. Probing it
    # is how a cache proves it can ANSWER. Column names alone do not: an
    # ordinary table with the same five columns passes a column check and then
    # dies on MATCH ("no such column: entries", CoS review 2026-10-28).
    # The term is a bare word on purpose: FTS5 parses the MATCH operand as a
    # query, and a hyphen in it reads as an operator ("no such column: probe").
    _PROBE = "SELECT slug FROM entries WHERE entries MATCH ? ORDER BY rank LIMIT 1"
    _PROBE_TERM = "probe"

    def index_compatible(self, path):
        """True only when the cached index is an FTS5 table that can answer the
        search query this code runs.

        Three bounded controls - the fts5 creation header, the exact column
        tuple, one probe of the MATCH/rank shape - and any doubt rebuilds:
        v2.35.0 already says files are truth and the index is disposable, so a
        cache another schema built is disposable too. Drift is ordinary (an
        older release, another lane's build, a half-written file) and the
        answer is always a rebuild, never an error the operator must decode.
        """
        try:
            conn = sqlite_connect(path, readonly=True)
            try:
                row = conn.execute("SELECT sql FROM sqlite_master "
                                   "WHERE type = 'table' AND name = 'entries'").fetchone()
                if not row or not row[0] or not self._FTS5_HEADER.match(row[0]):
                    return False
                columns = conn.execute("PRAGMA table_info(entries)").fetchall()
                if tuple(column[1] for column in columns) != self.INDEX_COLUMNS:
                    return False
                conn.execute(self._PROBE, (self._PROBE_TERM,)).fetchall()
            finally:
                conn.close()
        except sqlite3.Error:
            return False  # unreadable, corrupt, or unable to answer: rebuild
        return True

    def index(self):
        self.ensure()
        path = self.index_path()
        if path.exists():
            path.unlink()
        conn = sqlite_connect(path)
        conn.execute("CREATE VIRTUAL TABLE entries USING fts5(slug, subject, body, class UNINDEXED, scope UNINDEXED)")
        for entry in self.entries():
            if entry.get("error"):
                continue
            if entry["class"] == "ground-truth" and entry["authority"] != "owner":
                continue  # refused by the indexer, not downgraded
            conn.execute("INSERT INTO entries(slug, subject, body, class, scope) VALUES (?,?,?,?,?)",
                         (entry["slug"], entry["subject"], entry["body"],
                          entry["class"], entry["scope"]))
        conn.commit()
        conn.close()
        return {"indexed": sum(1 for e in self.entries() if not e.get("error")),
                "path": str(path)}

    def search(self, query, *, cls=None, scope=None, limit=20):
        path = self.index_path()
        if not path.exists() or not self.index_compatible(path):
            self.index()
        conn = sqlite_connect(path, readonly=True)
        sql = "SELECT slug, subject, class, scope FROM entries WHERE entries MATCH ?"
        params = [query]
        if cls:
            sql += " AND class = ?"; params.append(cls)
        if scope:
            sql += " AND scope = ?"; params.append(scope)
        sql += " ORDER BY rank LIMIT ?"; params.append(limit)
        try:
            rows = conn.execute(sql, params).fetchall()
        except sqlite3.Error as exc:
            raise ContextError(f"index search failed: {exc}")
        finally:
            conn.close()
        for row in rows:
            self.log_access(row[0])
        return [{"slug": r[0], "subject": r[1], "class": r[2], "scope": r[3]} for r in rows]

    # -- access log + stats ----------------------------------------------
    def access_path(self):
        return self.root / "access.jsonl"

    def log_access(self, slug, by=None, kind="read"):
        """kind='read' when someone opened an article, 'injected' when it was delivered.

        Delivery and reading are different facts and a store that conflates them
        cannot say whether it is unused or merely unread.
        """
        if not slug:
            return
        with open(self.access_path(), "a", encoding="utf-8") as handle:
            handle.write(json.dumps({"slug": slug, "at": time.time(), "kind": kind,
                                     "by": by or os.environ.get("DUNKINGSHEEP_SELF") or "unknown"}) + "\n")

    def reads(self):
        try:
            return [json.loads(line) for line in self.access_path().read_text().splitlines() if line.strip()]
        except (OSError, ValueError):
            return []

    def stats(self):
        entries = self.live_entries()
        counts = {name: sum(1 for e in entries if e["class"] == name) for name in CLASSES}
        ages = {name: sorted((date.today() - date.fromisoformat(e["stated"])).days
                             for e in entries if e["class"] == name) for name in CLASSES}
        def pct(values, q):
            return values[min(len(values) - 1, int(round(q * (len(values) - 1))))] if values else None
        reads = self.reads()
        for record in reads:  # back-compat: records written before kinds existed are reads
            record.setdefault("kind", "read")
        now = time.time()
        recent = [r for r in reads if r.get("at", 0) >= now - 7 * 86400 and r["kind"] == "read"]
        injected = [r for r in reads if r.get("at", 0) >= now - 7 * 86400 and r["kind"] == "injected"]
        seen = {r.get("slug") for r in reads if r["kind"] == "read"}
        delivered = {r.get("slug") for r in reads}
        return {"counts": counts,
                "age_days": {name: {"median": pct(ages[name], 0.5), "p90": pct(ages[name], 0.9)}
                             for name in CLASSES},
                "reads_7d": len(recent), "reads_total": len(reads),
                "injected_7d": len(injected), "injected_total": len(reads) - sum(
                    1 for r in reads if r["kind"] == "read"),
                "never_read": sorted(e["slug"] for e in entries if e["slug"] not in seen),
                "never_read_count": sum(1 for e in entries if e["slug"] not in seen),
                "never_delivered": sorted(e["slug"] for e in entries if e["slug"] not in delivered),
                "never_delivered_count": sum(1 for e in entries if e["slug"] not in delivered)}

    def entry_stats(self):
        """Per-article access counts: what was delivered, what was opened, when.

        'never read' at the store level hides the interesting case - one article
        read forty times and eight never delivered - so the counts are per article.
        """
        reads = self.reads()
        for record in reads:
            record.setdefault("kind", "read")
        counters = {}
        for record in reads:
            slug = record.get("slug")
            if not slug:
                continue
            slot = counters.setdefault(slug, {"reads": 0, "injected": 0,
                                              "last_read": None, "last_injected": None})
            kind = record["kind"] if record["kind"] in ("read", "injected") else "read"
            slot["reads" if kind == "read" else "injected"] += 1
            key = "last_read" if kind == "read" else "last_injected"
            at = record.get("at")
            if at and (slot[key] is None or at > slot[key]):
                slot[key] = at
        rows = []
        for entry in self.live_entries():
            slot = counters.get(entry["slug"], {})
            rows.append({"slug": entry["slug"], "class": entry["class"],
                         "subject": entry.get("subject"), "authority": entry.get("authority"),
                         "stated": entry.get("stated"),
                         "reads": slot.get("reads", 0), "injected": slot.get("injected", 0),
                         "last_read": slot.get("last_read"),
                         "last_injected": slot.get("last_injected"),
                         "stale": entry["slug"] in {e["slug"] for e in self.check()["stale"]}})
        return rows

    # -- digest -----------------------------------------------------------
    def digest(self, *, max_bytes=DEFAULT_CAP, subject=None):
        entries = self.live_entries()
        truths = [e for e in entries if e["class"] == "ground-truth"]
        others = [e for e in entries if e["class"] != "ground-truth"
                  and (subject is None or e.get("subject") == subject)]
        included = list(truths)
        parts = [f"## Ground truths ({len(truths)})"]
        parts += [f"- [{e['subject']}] {e['body'].splitlines()[0]}" for e in truths] or ["- none"]
        parts.append("")
        parts.append("## Relevant")
        used = len("\n".join(parts).encode("utf-8"))
        omitted = 0
        for entry in others:
            line = f"- [{entry['class']}/{entry['subject']}] {entry['body'].splitlines()[0]}"
            size = len((line + "\n").encode("utf-8"))
            stale = ""
            if entry["class"] != "ground-truth" and _days(entry.get("recheck_after")):
                try:
                    horizon = datetime.fromisoformat(str(entry["stated"])) + timedelta(days=_days(entry["recheck_after"]))
                    if datetime.now() > horizon:
                        stale = " (STALE - re-check before trusting)"
                except ValueError:
                    pass
            if used + size + len(stale.encode("utf-8")) > max_bytes:
                omitted += 1
                continue
            parts.append(line + stale)
            included.append(entry)
            used += size + len(stale.encode("utf-8"))
        if omitted:
            parts.append(f"omitted: {omitted} entries over the {max_bytes}-byte cap")
        text = "\n".join(parts) + "\n"
        for entry in included:
            self.log_access(entry["slug"], kind="injected")
        return text
