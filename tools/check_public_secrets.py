#!/usr/bin/env python3
"""Refuse to publish secret-shaped files in a public repo (t746).

What happened, measured: commit 52847e5 of the PUBLIC repo huntergdavis/dunkingsheep
carried `data/auth.json`, `data/owner-credentials` and `data/messages.sqlite3` (a
`git add -A` after a telegram test run). Both credential files were SERVED by
raw.githubusercontent (HTTP 200) and contained a cleartext `Password:` line and an
`API token:` line. A force-push does not unpublish a SHA, so the only durable fix is
to stop the next commit of that shape, which is what this does.

Two rules, both deterministic:

  PATHS    a tracked path that is a local data directory, an auth/credential file,
           or a sqlite database.
  CONTENT  a tracked text file carrying a cleartext secret label, or a long random
           token next to one.

The scan reads `git ls-files` (TRACKED files only): an ignored file is the whole
point of .gitignore, and an untracked file is not published. It exits 1 with one
line per finding, naming the path and the rule, because a scanner whose output needs
interpretation is a scanner nobody runs.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys

# A path that must never be tracked in a public repo.
PATH_RULES = (
    (re.compile(r"(^|/)data/"), "a local data directory"),
    (re.compile(r"(^|/)auth\.json$"), "a desk auth store"),
    (re.compile(r"(^|/)owner-credentials"), "an owner credentials file"),
    (re.compile(r"\.sqlite3?$"), "a sqlite database"),
)

# A cleartext secret LABEL with a value: this is the shape that was served
# (`Password: <value>`, `API token: <value>`). A label with no value, or a label
# mentioned in prose ("the existing Password: ... harness format"), is not a
# credential - a rule that fires on the word alone flags documentation and long
# function names, and a scanner that cries wolf is a scanner nobody runs.
SECRET_ASSIGNMENT = re.compile(
    r"(?im)^[^\n]{0,80}?\b(password|passwd|api[ _-]?token|auth[ _-]?token|"
    r"bot[ _-]?token|secret|private[ _-]?key)\b\s*[:=]\s*[\"']?"
    r"([A-Za-z0-9+/=_\-]{16,})")

# Files whose content is reviewed on purpose# Files whose content is reviewed on purpose and cannot be scanned this way. Each
# entry needs a reason; an allowlist without reasons becomes a mute button.
ALLOWLIST = {
    "tools/check_public_secrets.py": "this scanner quotes the patterns it looks for",
    "tests/test_check_public_secrets.py": "the fixtures are deliberately shaped like secrets",
    "tools/memory_acceptance.py": "a REDACTION test fixture: the value is shaped like a "
                                  "key on purpose and the test asserts the store redacts it",
}

TEXT_SUFFIXES = (".json", ".yaml", ".yml", ".ini", ".cfg", ".conf", ".env", ".txt",
                 ".md", ".sh", ".py", ".sql", ".csv", ".toml", ".properties", "")


def tracked_files(repo="."):
    out = subprocess.run(["git", "-C", repo, "ls-files", "-z"],
                         capture_output=True, check=True)
    return [name for name in out.stdout.decode("utf-8", "replace").split("\0") if name]


def scan_path(name):
    for pattern, why in PATH_RULES:
        if pattern.search(name):
            return f"{name}: {why} must not be tracked in a public repo"
    return None


def _is_call(text, end):
    """True when the matched value is immediately CALLED: `x = some_name()`.

    NOT a credential (review t778 on #150). `SECRET_ASSIGNMENT` matches any
    16+ character label value, and a snake_case identifier is 16+ characters, so
    `secret = compute_owner_credentials_path()` was flagged as a cleartext secret.
    The remedy a lane would reach for is an allowlist entry, and that is how a
    secret gate quietly stops gating, so the rule is narrowed here instead: a
    value followed by '(' is a call. The check reads the text AFTER the match, so
    the pattern itself stays simple enough to read.
    """
    rest = text[end:end + 8].lstrip()
    return rest.startswith("(")


def content_findings(repo, name):
    """EVERY content finding in one file, each with its line number (t778).

    This used to be `match = ...search(...); if match: return one finding`, so a
    file holding two credentials reported one. The gate still failed, but the
    human who has to decide what to rotate was told half the truth.
    """
    path = f"{repo}/{name}"
    try:
        with open(path, "rb") as handle:
            blob = handle.read(200_000)
    except OSError:
        return []
    if b"\0" in blob[:4096]:                     # binary: the path rules cover it
        return []
    text = blob.decode("utf-8", "replace")
    findings = []
    for match in SECRET_ASSIGNMENT.finditer(text):
        if _is_call(text, match.end()):
            continue
        line = text[:match.start()].count("\n") + 1
        findings.append(f"{name}:{line}: a cleartext '{match.group(1)}:' value "
                        f"({len(match.group(2))} characters)")
    return findings


def scan_content(repo, name):
    """The first content finding in a file (kept for callers that want one)."""
    findings = content_findings(repo, name)
    return findings[0] if findings else None


def scan(repo=".", allowlist=None):
    allowlist = ALLOWLIST if allowlist is None else allowlist
    findings = []
    for name in tracked_files(repo):
        if name in allowlist:
            continue
        finding = scan_path(name)
        if finding:
            findings.append(finding)
            continue
        findings.extend(content_findings(repo, name))
    return findings


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--repo", default=".", help="repository to scan (default .)")
    args = parser.parse_args(argv)
    findings = scan(args.repo)
    if findings:
        print("PUBLIC-REPO SECRET SCAN FAILED: these tracked files must not be "
              "published (t746).", file=sys.stderr)
        for finding in findings:
            print(f"  {finding}", file=sys.stderr)
        print("Remove the file from the index (git rm --cached), add it to "
              ".gitignore, and note that a force-push does NOT unpublish an "
              "already-pushed SHA.", file=sys.stderr)
        return 1
    print("public-repo secret scan: clean "
          f"({len(tracked_files(args.repo))} tracked files checked)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
