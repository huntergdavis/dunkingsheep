#!/usr/bin/env python3
"""Daily watch of free models on OpenRouter and OpenCode.

Writes the Dunking Sheep context article subject `free-model-catalog` (class
fact, global) and keeps first_seen / last_seen in data/free_catalog_state.json.
An arrival is only LISTED here; it counts as working after the four-body gate.
"""
import json
import subprocess
import sys
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
STATE = ROOT / "data" / "free_catalog_state.json"
SUBJECT = "free-model-catalog"


def now():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def openrouter_free():
    request = urllib.request.Request("https://openrouter.ai/api/v1/models",
                                     headers={"User-Agent": "vp-free-catalog"})
    with urllib.request.urlopen(request, timeout=30) as response:
        rows = json.load(response)["data"]
    ids = set()
    for row in rows:
        pricing = row.get("pricing") or {}
        if str(row["id"]).endswith(":free") or (
                pricing.get("prompt") == "0" and pricing.get("completion") == "0"):
            ids.add("openrouter/" + row["id"])
    if not ids:
        raise RuntimeError(f"parsed zero free models from {len(rows)} OpenRouter rows")
    return ids


def opencode_free():
    """OpenCode models: free-tier ids, plus every opencode-go id (plan-included)."""
    out = subprocess.run(["opencode", "models"], capture_output=True, text=True,
                         timeout=240, check=True).stdout.split()
    models = {m for m in out if m.startswith("opencode-go/") or m.endswith("-free")
              or m.endswith(":free")}
    if not models:  # an unparseable answer is a source error, never "no models"
        raise RuntimeError(f"parsed zero models from {len(out)} tokens of `opencode models`")
    return models


def main():
    state = json.loads(STATE.read_text()) if STATE.exists() else {"models": {}}
    previous_check = state.get("last_checked")
    sources, errors = {}, []
    for name, fetch in (("openrouter", openrouter_free), ("opencode", opencode_free)):
        try:
            sources[name] = fetch()
        except Exception as exc:  # a failed source is reported, never read as "no models"
            errors.append(f"{name}: {type(exc).__name__}: {exc}")
    stamp = now()
    seen = set().union(*sources.values()) if sources else set()
    arrivals = sorted(m for m in seen if m not in state["models"])
    first_run = not state["models"]
    for model in seen:
        row = state["models"].setdefault(model, {"first_seen": stamp})
        row["last_seen"] = stamp
    checked_sources = set(sources)
    gone = sorted(m for m, row in state["models"].items()
                  if m not in seen and row.get("last_seen") and m.split("/")[0] in checked_sources
                  and not row.get("gone_at"))
    for model in gone:
        state["models"][model]["gone_at"] = stamp
    state.setdefault("store_began", stamp)
    state["last_checked"] = stamp
    state["errors"] = errors
    STATE.parent.mkdir(exist_ok=True)
    STATE.write_text(json.dumps(state, indent=1, sort_keys=True))

    lines = [f"# Free model catalogue (watched, not verified)", "",
             f"Checked {stamp}. Sources ok: {', '.join(sorted(sources)) or 'none'}.",
             f"Previous check: {previous_check or 'none (baseline run)'}. "
             f"This store began {state['store_began']}: first_seen dates mean 'known to us since', "
             "not 'released then'."]
    if errors:
        lines += ["", "SOURCE ERRORS (a failed source is not an empty one):"] + [f"- {e}" for e in errors]
    lines += ["", "Test results for these models live in the global article `free-model-test-results` "
              "(written by the tester lane); this page only says what EXISTS.", "",
              "An arrival is LISTED, not working: a model becomes usable only after "
              "the four-body gate (proposals that reproduced a body).", ""]
    lines += [f"## Arrivals this check ({len(arrivals)})"]
    lines += [f"- {m}" for m in arrivals] if arrivals and not first_run else \
        (["- baseline run: all models below are first-seen now"] if first_run else ["- none"])
    lines += ["", f"## Departed this check ({len(gone)})"] + ([f"- {m}" for m in gone] or ["- none"])
    lines += ["", f"## Current ({len(seen)})"]
    lines += [f"- {m} (first seen {state['models'][m]['first_seen']})" for m in sorted(seen)]
    body = "\n".join(lines) + "\n"

    tmp = ROOT / "data" / "free_catalog_article.md"
    tmp.write_text(body)
    slug = state.get("article_slug")
    if slug:
        result = subprocess.run(["dunkingsheep", "context", "supersede", "--global", slug, str(tmp)],
                                capture_output=True, text=True)
    else:
        result = subprocess.run(["dunkingsheep", "context", "add", "--class", "fact",
                                 "--subject", SUBJECT, "--global", "--source", "tools/free_catalog.py",
                                 str(tmp)], capture_output=True, text=True)
    exists = " already exists" in result.stderr
    if result.returncode != 0 and exists and not slug:
        # state lost but the article is there: recover its slug and supersede in place
        slug = result.stderr.split("entry ", 1)[1].split(" already exists")[0].strip()
        state["article_slug"] = slug
        STATE.write_text(json.dumps(state, indent=1, sort_keys=True))
        result = subprocess.run(["dunkingsheep", "context", "supersede", "--global", slug, str(tmp)],
                                capture_output=True, text=True)
    if result.returncode != 0:
        print("article write FAILED:", result.stderr.strip() or result.stdout.strip(), file=sys.stderr)
        return 2
    # `supersede` prints "superseded by <new-slug>": the NEW entry is the head, and the
    # next run must supersede it, not the original (that leaves orphans in the digest).
    word = (result.stdout.strip().split() or [""])[-1].split("/")[-1]
    if word:
        state["article_slug"] = word
        STATE.write_text(json.dumps(state, indent=1, sort_keys=True))
    print(f"catalogue: {len(seen)} free/included models, {len(arrivals)} arrivals, "
          f"{len(gone)} departed, errors={len(errors)}")
    for model in arrivals:
        print("  arrival:", model)
    return 0 if not errors else 1


if __name__ == "__main__":
    sys.exit(main())
