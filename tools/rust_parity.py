#!/usr/bin/env python3
"""Rust parity watcher (t1057; ground truth `rust-parity`).

Until the Rust cutover every Python behaviour change is mirrored in Rust. This diffs the
Python tree (dunk_*.py, dunkingsheep, packaging/) between the commit named in
docs/rust/MANIFEST.md and a head, lists added / changed / removed functions, registry
commands and schema columns, and checks each against the Rust tree (rust/crates) BY NAME.

  tools/rust_parity.py                  report (exit 0)
  tools/rust_parity.py --warn           same, formatted as GitHub ::warning lines (CI; exit 0)
  tools/rust_parity.py --file-tasks     file one 'Rust parity: <module>' task per module with
                                        unported items, deduped by title (the daily timer)
  tools/rust_parity.py --check          exit 1 when anything is unported

A name match is evidence of a counterpart, not of equal behaviour: `ported` here means
"the name exists in rust/crates". A CHANGED item whose Rust file is older than the Python
change is reported STALE; the differential harness is still the judge of behaviour.
"""

import argparse
import ast
import os
import re
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MANIFEST = "docs/rust/MANIFEST.md"
PY_GLOBS = ("dunk_", "dunkingsheep")          # path prefixes under the repo root
SQL_KEYWORDS = {"PRIMARY", "UNIQUE", "FOREIGN", "CHECK", "CONSTRAINT", "IF", "NOT", "EXISTS"}
COMMAND_RE = re.compile(r"\bCommand\(\s*[\"'](\w+)[\"']")
TABLE_RE = re.compile(r"CREATE TABLE(?: IF NOT EXISTS)?\s+(\w+)\s*\((.*?)\n?\)\s*;?", re.S)
MIGRATE_RE = re.compile(r"\(\s*[\"'](\w+)[\"']\s*,\s*[\"'](?:TEXT|REAL|INTEGER|BLOB)[\"']")
IDENT_RE = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")


def manifest_commit(text):
    """The commit named by the manifest's first line: '# Port manifest (generated from X, ...)'."""
    match = re.search(r"generated from ([0-9a-f]{7,40})", text)
    if not match:
        raise ValueError("manifest does not name a commit")
    return match.group(1)


def _is_python_source(path):
    return path.endswith(".py") and os.path.basename(path).startswith("dunk_") or \
        path == "dunkingsheep"


def extract(sources):
    """{item_key: fingerprint} for one tree. `sources` is {path: text}.

    Keys: 'fn:<file>::<Class.>name', 'cmd:<name>', 'col:<table>.<column>'.
    The fingerprint of a function is its unparsed source, so a comment-only edit is no change.
    """
    items = {}
    for path, text in sources.items():
        stem = os.path.basename(path)
        try:
            tree = ast.parse(text)
        except SyntaxError:
            tree = None
        if tree is not None:
            def walk(body, prefix=""):
                for node in body:
                    if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                        if not (node.name.startswith("__") and node.name.endswith("__")):
                            items[f"fn:{stem}::{prefix}{node.name}"] = ast.dump(node)
                    elif isinstance(node, ast.ClassDef):
                        walk(node.body, f"{node.name}.")
            walk(tree.body)
        for name in COMMAND_RE.findall(text):
            items[f"cmd:{name}"] = name
        for table, body in TABLE_RE.findall(text):
            for line in body.split(","):
                words = line.split()
                if words and words[0].upper() not in SQL_KEYWORDS and IDENT_RE.fullmatch(words[0]):
                    items[f"col:{table}.{words[0]}"] = words[0]
        for column in MIGRATE_RE.findall(text):
            items.setdefault(f"col:*.{column}", column)
    return items


def diff_items(base, head):
    added = sorted(k for k in head if k not in base)
    removed = sorted(k for k in base if k not in head)
    changed = sorted(k for k in head if k in base and head[k] != base[k])
    return {"added": added, "changed": changed, "removed": removed}


def rust_names(rust_sources):
    """Every identifier and string-literal word in the Rust tree."""
    names = set()
    for text in rust_sources.values():
        names.update(IDENT_RE.findall(text))
    return names


def module_of(key):
    kind, _, rest = key.partition(":")
    if kind == "fn":
        return rest.split("::")[0].rsplit(".", 1)[0]
    if kind == "cmd":
        return "dunk_api"
    return "schema"


def bare_name(key):
    kind, _, rest = key.partition(":")
    if kind == "fn":
        return rest.split("::")[1].split(".")[-1]
    if kind == "col":
        return rest.split(".")[-1]
    return rest


def has_counterpart(key, names):
    name = bare_name(key)
    return name in names or name.lstrip("_") in names


def classify(delta, names, stale=lambda key: False):
    """Items needing Rust work: added/changed with no counterpart (or a stale one), and
    removed items that Rust still has. Returns {module: [(status, key)]}."""
    out = {}
    def add(key, status):
        out.setdefault(module_of(key), []).append((status, key))
    for key in delta["added"]:
        if not has_counterpart(key, names):
            add(key, "added: no Rust counterpart")
    for key in delta["changed"]:
        if not has_counterpart(key, names):
            add(key, "changed: no Rust counterpart")
        elif stale(key):
            add(key, "changed: Rust counterpart is older (STALE)")
    for key in delta["removed"]:
        if has_counterpart(key, names):
            add(key, "removed in Python: still in Rust")
    return out


def task_title(module):
    return f"Rust parity: {module}"


def task_body(module, entries, base_commit):
    lines = [f"Owner rule rust-parity: Python changed since the manifest ({base_commit}) and "
             f"the Rust tree has no matching change. Module: {module}.", ""]
    lines += [f"- {status}: {key}" for status, key in sorted(entries, key=lambda e: e[1])]
    lines += ["", "Acceptance: each item has a Rust counterpart (by name) and a test, or the PR "
              "body says why it has no Rust equivalent. Source of this list: "
              "tools/rust_parity.py (name match only; the diff harness judges behaviour)."]
    return "\n".join(lines)


def plan_tasks(report, existing_titles, base_commit):
    """[(title, body)] for modules with unported items whose title is not already filed."""
    return [(task_title(module), task_body(module, entries, base_commit))
            for module, entries in sorted(report.items())
            if task_title(module) not in existing_titles]


def format_report(report, base_commit):
    if not report:
        return f"rust parity: nothing unported since {base_commit}"
    lines = [f"rust parity: unported changes since {base_commit}"]
    for module, entries in sorted(report.items()):
        lines.append(f"  {module}: {len(entries)}")
        lines += [f"    {status}: {key}" for status, key in sorted(entries, key=lambda e: e[1])]
    return "\n".join(lines)


# ---- git/filesystem edges (kept thin; the logic above takes plain dicts) ----

def _git(*args):
    return subprocess.run(["git", "-C", ROOT, *args], capture_output=True, text=True, check=True).stdout


def python_paths(ref):
    names = _git("ls-tree", "-r", "--name-only", ref).splitlines()
    return [n for n in names if "/" not in n and _is_python_source(n)]


def read_tree(ref):
    return {path: _git("show", f"{ref}:{path}") for path in python_paths(ref)}


def read_rust():
    sources = {}
    for base, _, files in os.walk(os.path.join(ROOT, "rust", "crates")):
        for name in files:
            if name.endswith(".rs"):
                path = os.path.join(base, name)
                with open(path, encoding="utf-8", errors="replace") as handle:
                    sources[path] = handle.read()
    return sources


def existing_task_titles():
    out = subprocess.run(["dunkingsheep", "--no-start-daemon", "task", "list", "--all", "-n", "2000"],
                         capture_output=True, text=True, check=True).stdout
    return {m.group(0) for m in re.finditer(r"Rust parity: [\w.\-]+", out)}


def _commit_time(*args):
    out = _git("log", "-1", "--format=%ct", *args).strip()
    return int(out) if out else 0


def stale_checker(base, head):
    """A CHANGED function is STALE when the Python file's last change since `base` is newer
    than the newest commit touching rust/crates."""
    rust_time = _commit_time(head, "--", "rust/crates")

    def stale(key):
        kind, _, rest = key.partition(":")
        if kind != "fn":
            return False
        return _commit_time(f"{base}..{head}", "--", rest.split("::")[0]) > rust_time
    return stale


def build_report(head="HEAD", base=None):
    if base is None:
        with open(os.path.join(ROOT, MANIFEST), encoding="utf-8") as handle:
            base = manifest_commit(handle.read())
    delta = diff_items(extract(read_tree(base)), extract(read_tree(head)))
    return classify(delta, rust_names(read_rust()), stale_checker(base, head)), base


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--base", help="commit to diff from (default: the manifest's)")
    parser.add_argument("--head", default="HEAD")
    parser.add_argument("--warn", action="store_true", help="GitHub ::warning lines; never fails")
    parser.add_argument("--file-tasks", action="store_true")
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args(argv)
    report, base = build_report(args.head, args.base)
    if args.warn:
        for module, entries in sorted(report.items()):
            for status, key in entries:
                print(f"::warning title=Rust parity ({module})::{status}: {key}")
        print(format_report(report, base))
        return 0
    if args.file_tasks:
        for title, body in plan_tasks(report, existing_task_titles(), base):
            subprocess.run(["dunkingsheep", "--no-start-daemon", "task", "add", title, "-p", "high",
                            "--body", body], check=True)
            print("filed:", title)
        return 0
    print(format_report(report, base))
    return 1 if args.check and report else 0


if __name__ == "__main__":
    sys.exit(main())
