#!/usr/bin/env python3
"""Restore the pinned pre-retirement daemon sources for tests, never for deployment.

Run in a clean checkout before Python differential tests or ``cargo test``. The
files are untracked test inputs. This tool refuses to overwrite any path.
"""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
MANIFEST = ROOT / "docs/rust/python-daemon-archive.json"


def git(*args):
    return subprocess.run(["git", "-C", str(ROOT), *args], check=True, stdout=subprocess.PIPE).stdout


def main():
    manifest = json.loads(MANIFEST.read_text(encoding="utf-8"))
    commit = manifest["commit"]
    tag = manifest["tag"]
    files = manifest["files"]
    if commit != "1b14a9fb15b7ccf8763b2e15a2ac428824cdc3a8" or tag != "python-pre-retirement-1b14a9fb":
        raise ValueError("unexpected oracle archive identity")
    if len(files) != 51 or len({item["path"] for item in files}) != 51:
        raise ValueError("unexpected oracle manifest entries")
    for item in files:
        name = item["path"]
        if not (name == "dunkingsheep" or (name.startswith("dunk") and name.endswith(".py"))) or "/" in name:
            raise ValueError("invalid oracle path")
        if item.get("mode") not in ("0o644", "0o755"):
            raise ValueError(f"invalid oracle mode: {name}")
        if os.path.lexists(ROOT / name):
            raise FileExistsError(f"refusing to overwrite {name}")
    if subprocess.run(["git", "-C", str(ROOT), "cat-file", "-e", f"{commit}^{{commit}}"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode:
        git("fetch", "--no-tags", "--depth=1", "origin", f"refs/tags/{tag}")
        if git("rev-parse", "FETCH_HEAD^{commit}").decode().strip() != commit:
            raise ValueError("remote archive tag does not match pinned commit")
    restored = []
    for item in files:
        data = git("show", f"{commit}:{item['path']}")
        if len(data) != item["bytes"] or hashlib.sha256(data).hexdigest() != item["sha256"]:
            raise ValueError(f"archive verification failed: {item['path']}")
        restored.append((item, data))
    created = []
    try:
        for item, data in restored:
            path = ROOT / item["path"]
            with path.open("xb") as output:
                created.append(item["path"])
                output.write(data)
                os.fchmod(output.fileno(), int(item["mode"], 8))
    except OSError as error:
        # Do not delete files here: another process may have changed them.
        raise OSError(f"{error}; partial restore created {created}; see recovery in docs/rust/PYTHON_DAEMON_RETIREMENT.md") from error
    print(f"Restored {len(restored)} verified, untracked Python oracle files from {commit}")


if __name__ == "__main__":
    try:
        main()
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"oracle restoration failed: {error}", file=sys.stderr)
        sys.exit(1)
