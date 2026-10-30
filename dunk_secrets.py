#!/usr/bin/env python3
"""Refuse to publish the desk's own secrets (t756).

On 2026-10-29 two commits on two public PRs added `data/` to this repo:
`data/auth.json`, `data/owner-credentials` (plaintext desk password and API
token) and `data/messages.sqlite3` (the owner's conversation with the fleet).
One was a lane's `git add -A` in a worktree where a test had just made `data/`;
the other was mine, an hour later, the same way. The owner rotated the password
and the token that night.

`.gitignore` alone does not stop it: it cannot ignore a file that is already
staged, and `git add -A` stages everything not ignored at that moment. So there
are two defences and this module is both:

1. the names are ignored (`.gitignore`), and
2. anything that would PUBLISH them is refused by name - `gh pr create` and the
   credential handshake `git push` performs.

RELATIONSHIP to tools/check_public_secrets.py (t746, on main): that scanner runs in
CI on every push and pull request and reads TRACKED files. This module is the layer
BEFORE it - it refuses at the credential handshake, before the commit leaves the
host. Both are needed and neither replaces the other, because t746's own finding was
that a force-push does not unpublish a SHA: CI catches the exposure after
raw.githubusercontent is already serving it, and only a local refusal prevents that.
The rule lists are deliberately separate (a .gitignore cannot import Python, and a
workflow cannot see an uncommitted index), and tests/test_dunk_secrets.py asserts
they AGREE on the leaked names so they cannot drift apart silently.

What this does NOT do, stated because a guard that overclaims is worse than none:

* it does not rewrite history, so a commit that already leaked stays fetchable by
  SHA on GitHub until they garbage-collect it (a support request, not a push);
* the credential-helper path refuses the lane's App token, but if the owner's own
  helper is configured behind it (it is, as the fallback) a `git push` may still
  authenticate as the owner. The `gh pr create` check has no such hole;
* it only looks at ADDED paths in the range it can see, and a range it cannot
  determine is a warning, not a refusal - a guard that fails closed on a
  detached worktree would block every lane.
"""

from __future__ import annotations

import fnmatch
import os
import subprocess

# The names that must never be published. Kept as globs so the message can name
# the rule a file matched, not just the file.
SECRET_PATTERNS = (
    "data/",
    "data",
    "*.sqlite3",
    "*-credentials",
    "owner-credentials",
    "*.pem",
    "*token*",
    "*token*/*",
    ".env",
    "*.key",
)


def matches(path):
    """The pattern that makes `path` unpublishable, or None."""
    path = str(path)
    while path.startswith("./"):        # not lstrip("./"), which eats the dot of ".env"
        path = path[2:]
    for pattern in SECRET_PATTERNS:
        if pattern.endswith("/"):
            if path == pattern[:-1] or path.startswith(pattern):
                return pattern
            continue
        if fnmatch.fnmatch(path, pattern):
            return pattern
        # a directory component: 'data/auth.json' matches 'data/'
        parts = path.split("/")
        if any(fnmatch.fnmatch(part, pattern) for part in parts):
            return pattern
    return None


def leaks(paths):
    """[(path, pattern)] for every path that must not be published."""
    found = []
    for path in paths or ():
        pattern = matches(path)
        if pattern:
            found.append((str(path), pattern))
    return found


def _git(args, cwd):
    done = subprocess.run(["git", "-C", cwd, *args], capture_output=True, text=True)
    return done.returncode, done.stdout, done.stderr


def added_paths(cwd=None, base=None):
    """Paths this branch ADDS relative to `base`, plus anything staged.

    Returns (paths, error). `error` is a sentence when the range could not be
    determined, so the caller can warn instead of pretending it checked.
    """
    cwd = cwd or os.getcwd()
    paths, error = [], None
    if base is None:
        for candidate in ("origin/main", "main", "@{upstream}"):
            code, _out, _err = _git(["rev-parse", "--verify", "--quiet", candidate], cwd)
            if code == 0:
                base = candidate
                break
    if base is None:
        error = "no base ref (origin/main, main, upstream) to diff against"
    else:
        code, out, err = _git(["diff", "--name-only", "--diff-filter=A",
                               f"{base}...HEAD"], cwd)
        if code != 0:
            error = f"cannot diff against {base}: {(err or '').strip()[:160]}"
        else:
            paths.extend(line for line in out.splitlines() if line.strip())
    # Staged additions matter even when the branch has no commits yet: this is the
    # exact moment `git add -A` created the incident.
    code, out, _err = _git(["diff", "--cached", "--name-only", "--diff-filter=A"], cwd)
    if code == 0:
        paths.extend(line for line in out.splitlines() if line.strip())
    return sorted(set(paths)), error


def refusal(cwd=None, base=None):
    """(message, error) - the named refusal, or (None, error-or-None)."""
    paths, error = added_paths(cwd=cwd, base=base)
    found = leaks(paths)
    if not found:
        return None, error
    lines = [f"REFUSED: this push would publish {len(found)} file(s) that must never "
             f"leave this host:"]
    for path, pattern in found:
        lines.append(f"  {path}   (matches {pattern!r})")
    lines.append("These are the desk's own secrets - the owner's password, API token, "
                 "cookie store and conversation. They were leaked twice on 2026-10-29 "
                 "(PR #143 and PR #146) and the owner rotated the credentials both times.")
    lines.append("Fix: `git rm --cached <path>`, commit, and add the name to "
                 ".gitignore. If it is already in a pushed commit, the commit must be "
                 "rewritten and the credential rotated - a force-push does not unpublish "
                 "the SHA.")
    return "\n".join(lines), error


def main(argv=None):
    """CLI: `dunkingsheep secrets-check [--base REF]` - exit 1 on a refusal."""
    import argparse
    import sys
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", help="diff against this ref (default: origin/main)")
    parser.add_argument("--cwd", help="repository to check (default: cwd)")
    parser.add_argument("--quiet", action="store_true")
    args = parser.parse_args(argv)
    message, error = refusal(cwd=args.cwd, base=args.base)
    if message:
        print(message, file=sys.stderr)
        return 1
    if error and not args.quiet:
        print(f"secrets-check: WARNING could not fully check: {error}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
