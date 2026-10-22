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
import time
from datetime import date, datetime, timedelta, timezone
from pathlib import Path

CLASSES = ("ground-truth", "fact", "provisional", "history")
AUTHORITIES = ("owner", "measured", "agent")
TYPES = ("principle", "decision", "reference", "lesson", "measurement", "incident")
SCOPES = ("global", "project")
DEFAULT_CAP = 8192
PROVISIONAL_DAYS = 7
GLOBAL_DIR = Path(os.environ.get(
    "DUNKINGSHEEP_DIR", str(Path.home() / ".config" / "dunkingsheep"))) / "context"


class ContextError(ValueError):
    """A named refusal for the context store."""


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
                "source", "recheck_after", "supersedes", "superseded_by"):
        value = meta.get(key)
        lines.append(f"{key}: {value if value is not None else 'null'}")
    lines.append("---")
    return "\n".join(lines) + "\n\n" + body.strip() + "\n"


def _days(value):
    match = re.fullmatch(r"(\d+)d", str(value or "").strip())
    return int(match.group(1)) if match else None


class Store:
    def __init__(self, path=None, *, global_store=False):
        self.root = store_root(path, global_store=global_store)
        self.scope = "global" if global_store else "project"

    # -- files ------------------------------------------------------------
    def ensure(self, *, meta_lines=None):
        self.root.mkdir(parents=True, exist_ok=True)
        for name in CLASSES:
            (self.root / name).mkdir(exist_ok=True)
        readme = self.root / "README.md"
        if not readme.exists():
            readme.write_text("# .dunkingsheep context store\n\nDeprecated: see vp-of-agentry/docs/CONTEXT_STORE.md.\n")
        return self.root

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
        for entry in self.entries():
            if entry.get("slug") == subject_or_slug or entry.get("subject") == subject_or_slug:
                return entry
        return None

    # -- writes -----------------------------------------------------------
    def add(self, body, *, cls, subject, type_, authority="agent", source="",
            stated=None, recheck_after=None, supersedes=None, slug=None):
        if cls not in CLASSES:
            raise ContextError(f"class must be one of {CLASSES}")
        if authority not in AUTHORITIES:
            raise ContextError(f"authority must be one of {AUTHORITIES}")
        self.ensure()
        slug = slug or f"{stated or _today()}-{_slug(subject)}"
        folder = self.root / cls
        folder.mkdir(exist_ok=True)
        path = folder / f"{slug}.md"
        if path.exists():
            raise ContextError(f"entry {slug} already exists")
        if supersedes:
            old = self.find(supersedes)
            if old is None:
                raise ContextError(f"supersede target {supersedes} not found")
            old["superseded_by"] = slug
            Path(old["path"]).write_text(_render(old, old["body"]))
        meta = {"class": cls, "subject": subject, "type": type_, "scope": self.scope,
                "authority": authority, "stated": stated or _today(), "source": source,
                "recheck_after": recheck_after if recheck_after is not None
                else ("7d" if cls == "provisional" else None),
                "supersedes": supersedes, "superseded_by": None}
        path.write_text(_render(meta, body))
        return {"slug": slug, "path": str(path), "class": cls}

    def supersede(self, slug, new_body, **meta):
        old = self.find(slug)
        if old is None:
            raise ContextError(f"entry {slug} not found")
        return self.add(new_body, cls=old["class"], subject=old["subject"],
                        type_=old.get("type") or "reference",
                        authority=meta.get("authority", old.get("authority") or "agent"),
                        source=meta.get("source") or old.get("source") or "",
                        recheck_after=meta.get("recheck_after", old.get("recheck_after")),
                        supersedes=old["slug"])

    def retire(self, slug, reason=""):
        entry = self.find(slug)
        if entry is None:
            raise ContextError(f"entry {slug} not found")
        body = entry["body"] + f"\n\n**Retired {_today()}**: {reason}\n"
        path = Path(entry["path"])
        path.write_text(_render(entry, body))
        return {"slug": entry["slug"], "retired": True, "reason": reason}

    # -- checks -----------------------------------------------------------
    def check(self):
        problems, counts = [], {name: 0 for name in CLASSES}
        for entry in self.entries():
            if entry.get("error"):
                problems.append({"slug": entry["slug"], "problem": entry["error"]})
                continue
            counts[entry["class"]] += 1
            if entry["class"] == "ground-truth" and entry["authority"] != "owner":
                problems.append({"slug": entry["slug"],
                                 "problem": "ground truth with authority != owner (refused)"})
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

    def index(self):
        self.ensure()
        path = self.index_path()
        if path.exists():
            path.unlink()
        conn = sqlite3.connect(path)
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
        if not path.exists():
            self.index()
        conn = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
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

    def log_access(self, slug, by=None):
        if not slug:
            return
        with open(self.access_path(), "a", encoding="utf-8") as handle:
            handle.write(json.dumps({"slug": slug, "at": time.time(),
                                     "by": by or os.environ.get("DUNKINGSHEEP_SELF") or "unknown"}) + "\n")

    def reads(self):
        try:
            return [json.loads(line) for line in self.access_path().read_text().splitlines() if line.strip()]
        except (OSError, ValueError):
            return []

    def stats(self):
        entries = [e for e in self.entries() if not e.get("error")]
        counts = {name: sum(1 for e in entries if e["class"] == name) for name in CLASSES}
        ages = {name: sorted((date.today() - date.fromisoformat(e["stated"])).days
                             for e in entries if e["class"] == name) for name in CLASSES}
        def pct(values, q):
            return values[min(len(values) - 1, int(round(q * (len(values) - 1))))] if values else None
        reads = self.reads()
        recent = [r for r in reads if r.get("at", 0) >= time.time() - 7 * 86400]
        seen = {r.get("slug") for r in reads}
        return {"counts": counts,
                "age_days": {name: {"median": pct(ages[name], 0.5), "p90": pct(ages[name], 0.9)}
                             for name in CLASSES},
                "reads_7d": len(recent), "reads_total": len(reads),
                "never_read": sorted(e["slug"] for e in entries if e["slug"] not in seen),
                "never_read_count": sum(1 for e in entries if e["slug"] not in seen)}

    # -- digest -----------------------------------------------------------
    def digest(self, *, max_bytes=DEFAULT_CAP, subject=None):
        entries = [e for e in self.entries() if not e.get("error")]
        truths = [e for e in entries if e["class"] == "ground-truth"]
        others = [e for e in entries if e["class"] != "ground-truth"
                  and (subject is None or e.get("subject") == subject)]
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
            used += size + len(stale.encode("utf-8"))
        if omitted:
            parts.append(f"omitted: {omitted} entries over the {max_bytes}-byte cap")
        return "\n".join(parts) + "\n"
