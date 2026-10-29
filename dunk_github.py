#!/usr/bin/env python3
"""GitHub Apps for the flat fleet: two bot identities, tokens minted on demand.

`docs/FLAT_FLEET.md` section 6. The owner has one GitHub login and no
sub-accounts. Two GitHub Apps owned by that login give the fleet two more
identities the review rules can tell apart:

    dunkingsheep-author    worker lanes push branches and open PRs as it
    dunkingsheep-reviewer  reviewer lanes approve as it; the review timer posts
                           the `dunkingsheep/review` commit status as it

The daemon alone holds the private keys (`github-app-<kind>.pem`, 0600) and
mints short-lived installation tokens (one hour) per request through
`gh_token`, role-gated: a worker gets the author token and nothing else; a
reviewer the reviewer token; the owner's own `gh` login is never touched by
a lane. Lanes reach the tokens through two thin shims that never see a key:
`dunkingsheep git-credential` (a git credential helper) and the `gh` wrapper
in `~/.config/dunkingsheep/bin`.

Dependencies: RS256 signing needs PyJWT + cryptography, both Ubuntu apt
packages on this host (`python3-jwt`, `python3-cryptography`); the import is
guarded so a host without them gets a sentence naming the packages, not a
traceback. The boring fallback is a fine-grained PAT in GH_TOKEN, which is
the owner's login again and loses the author != approver guarantee.
"""

from __future__ import annotations

import json
import logging
import os
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

CONFIG_DIR = Path(os.environ.get("DUNKINGSHEEP_DIR",
                                 str(Path.home() / ".config" / "dunkingsheep")))
API = os.environ.get("DUNKINGSHEEP_GITHUB_API", "https://api.github.com")
KINDS = ("author", "reviewer")
DEFAULT_REPOS = ("huntergdavis/dunkingsheep", "huntergdavis/poolrad-decomp")
REVIEW_CONTEXT = "dunkingsheep/review"
# Refresh a cached installation token this long before GitHub expires it, so a
# push that starts at minute 59 does not die mid-transfer.
REFRESH_MARGIN_S = 10 * 60

log = logging.getLogger("dunkingsheep.github")


class GitHubError(RuntimeError):
    """A named refusal or API failure."""


def _jwt_module():
    try:
        import jwt  # PyJWT
        return jwt
    except ImportError:
        raise GitHubError(
            "GitHub App tokens need PyJWT and cryptography: "
            "`sudo apt install python3-jwt python3-cryptography`") from None


def app_file(kind):
    return CONFIG_DIR / f"github-app-{kind}.json"


def load_app(kind):
    """{"id", "slug", "pem"} for an app kind, or raise naming what is missing."""
    if kind not in KINDS:
        raise GitHubError(f"app kind must be one of {', '.join(KINDS)}; got {kind!r}")
    path = app_file(kind)
    try:
        with open(path, encoding="utf-8") as handle:
            data = json.load(handle)
    except OSError:
        raise GitHubError(f"no GitHub App registered as {kind!r}: {path} is missing "
                          "(create it with the manifest page, docs/FLAT_FLEET.md 6.1)") from None
    pem = data.get("pem") or str(CONFIG_DIR / f"github-app-{kind}.pem")
    if not os.path.exists(pem):
        raise GitHubError(f"GitHub App {kind!r} private key missing: {pem}")
    return {"id": int(data["id"]), "slug": data.get("slug") or f"dunkingsheep-{kind}",
            "pem": pem}


def app_jwt(app, now=None):
    jwt = _jwt_module()
    now = int(now or time.time())
    with open(app["pem"], encoding="utf-8") as handle:
        key = handle.read()
    return jwt.encode({"iat": now - 60, "exp": now + 540, "iss": app["id"]},
                      key, algorithm="RS256")


def _request(method, url, token=None, bearer=None, payload=None, timeout=20):
    headers = {"Accept": "application/vnd.github+json",
               "User-Agent": "dunkingsheep", "X-GitHub-Api-Version": "2022-11-28"}
    if bearer:
        headers["Authorization"] = f"Bearer {bearer}"
    elif token:
        headers["Authorization"] = f"token {token}"
    body = None
    if payload is not None:
        body = json.dumps(payload).encode("utf-8")
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=body, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read()
            return resp.status, (json.loads(raw) if raw else None)
    except urllib.error.HTTPError as error:
        try:
            detail = json.loads(error.read() or b"{}")
        except ValueError:
            detail = {}
        raise GitHubError(f"GitHub {method} {url.replace(API, '')} -> {error.code}: "
                          f"{detail.get('message') or error.reason}") from None
    except (urllib.error.URLError, TimeoutError, OSError) as error:
        raise GitHubError(f"GitHub unreachable: {error}") from None


class Tokens:
    """Installation-token cache, one per app kind, refreshed before expiry.

    Persisted to `github-tokens.json` (0600) so a daemon restart does not mint
    a fresh token for every lane at once; the file holds tokens, not keys, and
    every token in it dies within the hour regardless.
    """

    def __init__(self, path=None):
        self.path = Path(path or os.environ.get("DUNKINGSHEEP_GITHUB_TOKENS_FILE")
                         or CONFIG_DIR / "github-tokens.json")
        self._lock = threading.Lock()
        self._cache = {}
        self._load()

    def _load(self):
        try:
            with open(self.path, encoding="utf-8") as handle:
                data = json.load(handle)
            self._cache = {k: v for k, v in (data.get("tokens") or {}).items()
                           if isinstance(v, dict) and v.get("expires_at")}
        except (OSError, ValueError):
            self._cache = {}

    def _save(self):
        self.path.parent.mkdir(parents=True, exist_ok=True)
        temporary = str(self.path) + ".tmp"
        fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump({"tokens": self._cache}, handle, indent=1)
        os.replace(temporary, self.path)
        try:
            os.chmod(self.path, 0o600)
        except OSError:
            pass

    def installation_id(self, app, bearer):
        _status, installs = _request("GET", f"{API}/app/installations", bearer=bearer)
        if not installs:
            raise GitHubError(f"GitHub App {app['slug']} is not installed on any account; "
                              "install it on the three repos")
        return int(installs[0]["id"])

    def get(self, kind, now=None):
        """A live installation token for `kind`: {token, expires_at, kind, slug}."""
        now = float(now or time.time())
        with self._lock:
            cached = self._cache.get(kind)
            if cached and float(cached["expires_at"]) - REFRESH_MARGIN_S > now:
                return dict(cached, kind=kind, cached=True)
            app = load_app(kind)
            bearer = app_jwt(app, now)
            install = cached.get("installation_id") if cached else None
            if not install:
                install = self.installation_id(app, bearer)
            _status, data = _request(
                "POST", f"{API}/app/installations/{install}/access_tokens", bearer=bearer)
            expires = data.get("expires_at")
            # GitHub returns ISO-8601 Z; keep a float for comparisons.
            try:
                import calendar
                expires_at = calendar.timegm(time.strptime(expires, "%Y-%m-%dT%H:%M:%SZ"))
            except (TypeError, ValueError):
                expires_at = now + 3600
            entry = {"token": data["token"], "expires_at": expires_at,
                     "installation_id": install, "slug": app["slug"],
                     "minted_at": now, "permissions": data.get("permissions") or {}}
            self._cache[kind] = entry
            self._save()
            log.info("minted %s installation token (expires %s)", kind, expires)
            return dict(entry, kind=kind, cached=False)

    def forget(self, kind=None):
        with self._lock:
            if kind:
                self._cache.pop(kind, None)
            else:
                self._cache.clear()
            self._save()


# -- role -> app kind ---------------------------------------------------------

ROLE_KIND = {"worker": "author", "planner": None, "desk": None,
             "reviewer": "reviewer", "system": "reviewer", "web": None,
             "owner": None, "anonymous": None}


def kind_for_role(role, requested=None):
    """Which app a role may act as. The owner and system may ask for either;
    a lane gets exactly the one its role implies; everyone else gets none."""
    if role in ("owner", "system"):
        if requested and requested not in KINDS:
            raise GitHubError(f"unknown app kind {requested!r}")
        return requested or ROLE_KIND.get(role)
    implied = ROLE_KIND.get(role)
    if implied is None:
        return None
    if requested and requested != implied:
        raise GitHubError(f"role {role} may only act as the {implied} App, not {requested}")
    return implied


# -- pull requests ------------------------------------------------------------

def open_pulls(repo, token):
    pulls = []
    for page in range(1, 11):          # 1,000 open PRs is more than any repo here has
        _status, batch = _request(
            "GET", f"{API}/repos/{repo}/pulls?state=open&per_page=100&page={page}", token=token)
        pulls.extend(batch or [])
        if not batch or len(batch) < 100:
            break
    out = []
    for pr in pulls or []:
        out.append({
            "repo": repo, "number": int(pr["number"]), "title": pr.get("title") or "",
            "url": pr.get("html_url"), "draft": bool(pr.get("draft")),
            "head": (pr.get("head") or {}).get("sha"),
            "branch": (pr.get("head") or {}).get("ref"),
            "base": (pr.get("base") or {}).get("ref"),
            "login": ((pr.get("user") or {}).get("login") or ""),
            "created_at": pr.get("created_at"), "updated_at": pr.get("updated_at"),
        })
    return out


def pull_detail(repo, number, token):
    _status, pr = _request("GET", f"{API}/repos/{repo}/pulls/{number}", token=token)
    return {"additions": pr.get("additions"), "deletions": pr.get("deletions"),
            "changed_files": pr.get("changed_files"), "mergeable": pr.get("mergeable"),
            "body": pr.get("body") or ""}


def post_status(repo, sha, state, token, description, target_url=None):
    """Commit status `dunkingsheep/review` on a head, as the reviewer App."""
    payload = {"state": state, "context": REVIEW_CONTEXT,
               "description": description[:140]}
    if target_url:
        payload["target_url"] = target_url
    return _request("POST", f"{API}/repos/{repo}/statuses/{sha}", token=token,
                    payload=payload)


def review_pull(repo, number, event, body, token):
    """Approve or request changes on a PR as the reviewer App."""
    payload = {"event": event, "body": body}
    return _request("POST", f"{API}/repos/{repo}/pulls/{number}/reviews",
                    token=token, payload=payload)


# -- branch protection (owner's own login; a one-shot, not a lane path) ----------

def protection_payload(approvals, reviewer_app_id, bypass_app_slug=None):
    """The branch-protection body for `main` (FLAT_FLEET.md 6.2)."""
    payload = {
        "required_status_checks": {
            "strict": False,
            "checks": [{"context": REVIEW_CONTEXT, "app_id": int(reviewer_app_id)}],
        },
        "enforce_admins": True,
        "required_pull_request_reviews": {
            "dismiss_stale_reviews": True,
            "require_code_owner_reviews": False,
            "required_approving_review_count": int(approvals),
        },
        "restrictions": None,
        "allow_force_pushes": False,
        "allow_deletions": False,
        "required_linear_history": False,
    }
    if bypass_app_slug:
        payload["required_pull_request_reviews"]["bypass_pull_request_allowances"] = {
            "apps": [bypass_app_slug]}
    return payload


def repos_config():
    path = CONFIG_DIR / "github-repos.json"
    try:
        with open(path, encoding="utf-8") as handle:
            data = json.load(handle)
        repos = data.get("repos") if isinstance(data, dict) else data
        if isinstance(repos, list) and repos:
            return [str(r) for r in repos]
    except (OSError, ValueError):
        pass
    return list(DEFAULT_REPOS)
