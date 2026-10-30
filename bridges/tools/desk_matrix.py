#!/usr/bin/env python3
"""One desk accessibility matrix, runnable against any build (t160).

The problem this replaces: every desk PR re-implements the widths, so reviewers end up
comparing matrices instead of looking at the product. This is the ONE tracked harness.
It runs a named set of rules over the width matrix and every registered theme, and it
prints and records, for each failure, WHICH assertion tripped and against what
(selector, expected vs got) - never a bare count.

    python3 tools/desk_matrix.py --dist web/dist --artifacts /tmp/desk-matrix

It never writes to the desk: every request that is not GET is RECORDED and fails
(`no-writes`), and aborted as well wherever a route can match it (requests under
/api/**); a non-API write under --url is recorded but cannot be blocked, so the rule
fails on the attempt rather than pretending it was stopped. The app is served from a
local static copy, and every API answer is a synthetic fixture from this file.

What this first version covers, and what it does NOT (the matrix is grown rule by rule,
each with a negative control):

  covered   no-descendant-overflow  (scrollWidth > clientWidth on any element)
            no-page-scrollbar       (documentElement scrollWidth > clientWidth)
            target-size             (WCAG 2.5.8: every interactive control >= 24x24 CSS px)
            dialog-closed           (capture only with no role=dialog in the DOM)
            no-writes               (no non-GET request reached the app)
            renders                 (the app shell mounted before any rule is judged)

  not yet   per-width nav rules, clipped-control, Tab walk, text 200%, PAINT
            verification, contrast thresholds.
            They are deliberately absent rather than silently skipped: `not_covered`
            in the JSON names them, so a reader cannot mistake this run for the full
            matrix.

Emulation is viewport + deviceScaleFactor, NOT browser zoom, and the JSON says so.

Negative control (the rule must be able to fail): pass --fixture-url/--fixture, or run
`--self-check`, which serves a page with a deliberately overflowing element and asserts
the tool FAILS on it. A rule that cannot fail is not evidence.
"""
import argparse
import hashlib
import json
import os
import subprocess
import tempfile
import threading
import time
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit

ROOT = Path(__file__).resolve().parents[1]

# The matrix the owner asked for (#1843/#1846). Named widths keep the owner's physical
# sizes straight: they are CSS px at deviceScaleFactor 1.8 where applicable.
WIDTHS = (
    ("owner-2560", 1422, 1.8),
    ("owner-1920", 1067, 1.8),
    ("owner-1440", 800, 1.8),
    ("1440", 1440, 1),
    ("1280", 1280, 1),
    ("1100", 1100, 1),
    ("1024", 1024, 1),
    ("960", 960, 1),
    ("900", 900, 1),
    ("800", 800, 1),
    ("720", 720, 1),
    ("700", 700, 1),
    ("480", 480, 1),
    ("390", 390, 1),
    ("320", 320, 1),
)

NOT_COVERED = [
    "per-width nav rules (five links / Boards+inline status / second row) - not implemented here",
    "clipped-control - not implemented here",
    "Tab walk at 480/320 and the text-200% check - not implemented here",
    "PAINT verification of visible controls (pixel evidence, not geometry) - not implemented here",
    "contrast thresholds; live deployed desk; real browser zoom or physical devices",
]

# WCAG 2.5.8 (AA) minimum target size. Named as one constant so a future pass that
# keeps the same number cannot drift from the rule it is enforcing.
MIN_TARGET_PX = 24

# Every read the desk makes on first paint. A route not listed here 404s and is reported,
# so a feature PR that adds a request must add it here in the SAME PR (harness rule).
SYNTHETIC_APIS = {
    "/api/channels": {"channels": [
        {"id": "cos", "name": "Chief of Agent Staff", "status": "working",
         "can_message": True, "pane_id": "fixture"},
        {"id": "vp", "name": "VP of Agentry", "status": "idle",
         "can_message": True, "pane_id": "fixture"},
    ]},
    "/api/messages": {"items": [
        {"id": f"i{n}", "role": "agent", "created_at": "2026-10-28T12:00:00-07:00",
         "delivery": "posted", "attachments": [],
         "body": f"Fixture message {n}. " + "Readable body text. " * 12}
        for n in range(1, 9)
    ]},
    "/api/game/status": {"running": False, "states": []},
    "/api/terminal": {"url": "", "available": False, "label": "Terminals"},
    "/api/herdr-webui": {"url": "", "available": False, "label": "Terminals"},
    "/api/health": {"ok": True},
    "/api/version": {"version": "matrix-fixture"},
    "/api/roster": {"lanes": []},
}

OVERFLOW_JS = """
() => {
  const root = document.documentElement;
  const over = [];
  for (const el of document.querySelectorAll('body *')) {
    const s = getComputedStyle(el);
    if (s.display === 'none' || s.visibility === 'hidden') continue;
    const sw = Math.ceil(el.scrollWidth), cw = Math.ceil(el.clientWidth);
    if (cw > 0 && sw > cw + 1) {
      over.push({selector: cssPath(el), tag: el.tagName.toLowerCase(),
                 scrollWidth: sw, clientWidth: cw});
    }
  }
  return {overflow: over.slice(0, 40), overflowCount: over.length,
          pageScrollWidth: Math.ceil(root.scrollWidth),
          pageClientWidth: Math.ceil(root.clientWidth),
          docScrollWidth: Math.ceil(document.body.scrollWidth),
          // WCAG 2.5.8 target size, measured on the CONTROL'S OWN box. The first version
          // of this rule took max(own, parent) to model a padded hit area - and that made
          // the rule unable to fail on the negative control (a 10x10 button inside <main>
          // inherited main's box and passed). Own box only, so the rule can fail.
          // Exemption, per WCAG: a link inline in a sentence ("inline" exception).
          smallTargets: (() => {
            const sel = 'a[href], button, input, select, textarea, summary, [role="button"], ' +
                        '[role="link"], [role="tab"], [role="menuitem"], [tabindex]:not([tabindex="-1"])';
            const out = [];
            for (const el of document.querySelectorAll(sel)) {
              const s = getComputedStyle(el);
              if (s.display === 'none' || s.visibility === 'hidden' || s.opacity === '0') continue;
              const r = el.getBoundingClientRect();
              if (r.width === 0 && r.height === 0) continue;          // not rendered
              if (el.tagName === 'A' && s.display === 'inline') continue;   // WCAG inline exception
              // COMPARE UNROUNDED. Rounding first let a 23.75px control round to 24 and
              // pass a >=24 floor (reviewer %65, t432 false-pass 2). The epsilon only
              // absorbs float noise from layout, not real sub-floor sizes.
              const w = r.width, h = r.height;
              if (w < 24 - 0.01 || h < 24 - 0.01) {
                out.push({selector: cssPath(el), tag: el.tagName.toLowerCase(),
                          width: Math.round(w), height: Math.round(h)});
              }
            }
            return out.slice(0, 40);
          })(),
          dialogOpen: !!document.querySelector('[role="dialog"], dialog[open]'),
          // The desk's root is `.dash-root`; older builds used `.shell`. Both count as
          // mounted, and a login screen is reported as its own failure, not as "no app".
          shellMounted: !!document.querySelector('.dash-root, .shell'),
          loginScreen: !!document.querySelector('main.login-shell'),};
}
// A short, readable selector path: enough to find the element, not a full CSS query.
function cssPath(el) {
  const parts = [];
  let node = el;
  while (node && node.nodeType === 1 && parts.length < 5) {
    let part = node.tagName.toLowerCase();
    if (node.id) { part += '#' + node.id; parts.unshift(part); break; }
    const cls = (node.className && typeof node.className === 'string')
      ? node.className.trim().split(/\\s+/).slice(0, 3).join('.') : '';
    if (cls) part += '.' + cls;
    parts.unshift(part);
    node = node.parentElement;
  }
  return parts.join(' > ');
}
"""


class StaticHandler(SimpleHTTPRequestHandler):
    def log_message(self, *args):                       # quiet: the evidence is the JSON
        pass


# The NUL-safe command that reproduces release_recipe_sha256 EXACTLY, including
# filenames containing spaces. The older whitespace-splitting pipeline
# (`find . -type f | LC_ALL=C sort | xargs sha256sum | sha256sum`) is still recorded, but
# as LEGACY_RECIPE and flagged non-reproducing when a supported filename has whitespace:
# advertising a command that silently hashes 3 of 4 files under a name that promises the
# release hash is how a receipt becomes a lie (reviewer %31, t463 P2).
BUILD_RECIPE = ("cd <dist> && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum"
                " | sha256sum   # NUL-delimited: filenames with spaces stay one path")
BUILD_PIPELINE = ("find . -type f -print0 | LC_ALL=C sort -z | xargs -0 sha256sum | sha256sum")
LEGACY_RECIPE = ("cd <dist> && find . -type f | LC_ALL=C sort | xargs sha256sum | sha256sum"
                 "   # LEGACY: splits names on whitespace; only valid for trees with no spaces")


def _manifest(folder: Path, follow_dir_symlinks=True):
    """Walk and hash in Python. Returns (digest, entries, problems).

    Reviewer %31 (t458, P2): the shell recipe silently tolerated failures - `xargs`
    split a filename on whitespace, `sha256sum` reported missing-file errors, and the
    trailing `sha256sum` still printed the digest of the files it DID see, so a
    four-file tree was reported with the three-file digest and passed. Nothing here
    shells out: every entry is opened and hashed individually, ANY error becomes a
    problem, and a manifest with a problem yields NO digest at all - an incomplete
    manifest is a failure, never a smaller number.

    Symlink policy, stated rather than implied: file symlinks are read through (their
    target's bytes are what the static server sends), directory symlinks are followed
    with a realpath cycle guard (a cycle is a problem, not an infinite walk), and every
    entry is recorded by its path INSIDE the tree, verbatim, spaces included.
    """
    root = str(folder)
    entries, problems, seen = [], [], set()

    def _onerror(error):
        # os.walk SKIPS a directory it cannot scan and says nothing. Without this the
        # files inside it are invisible, the digest is a partial digest and problems
        # stays empty - a passing matrix for a tree that was never fully hashable
        # (reviewer %31, t460 P2#2, measured with a real chmod 000 directory).
        try:
            where = os.path.relpath(getattr(error, "filename", None) or root, root)
        except Exception:                              # noqa: BLE001 - never fail the report
            where = "<unknown>"
        problems.append({"path": "./" + where,
                         "problem": "%s: %s" % (type(error).__name__, error)})

    for dirpath, dirnames, filenames in os.walk(root, followlinks=follow_dir_symlinks,
                                                onerror=_onerror):
        real = os.path.realpath(dirpath)
        if real in seen:
            problems.append({"path": "./" + os.path.relpath(dirpath, root),
                             "problem": "directory symlink cycle"})
            dirnames[:] = []
            continue
        seen.add(real)
        for name in filenames:
            full = os.path.join(dirpath, name)
            rel = "./" + os.path.relpath(full, root)
            if os.path.isdir(full) and not os.path.islink(full):
                continue
            if not follow_dir_symlinks and os.path.islink(full):
                # `find . -type f` does NOT list a symlink, so the recipe-side digest must
                # not include one either - otherwise the field named after that recipe is
                # a different number under a familiar name (reviewer %31, t460 P2#3).
                continue
            try:
                with open(full, "rb") as handle:
                    data = handle.read()
            except OSError as error:
                problems.append({"path": rel, "problem": "%s: %s"
                                 % (type(error).__name__, error)})
                continue
            entries.append((rel, hashlib.sha256(data).hexdigest()))
    entries.sort()
    digest = None
    if not problems:
        payload = "".join("%s  %s\n" % (digest_hex, path) for path, digest_hex in entries)
        digest = hashlib.sha256(payload.encode()).hexdigest()
    return digest, entries, problems


def _hazards(path):
    """Why a name may defeat an advertised command (empty list = it does not)."""
    found = []
    if any(ch.isspace() for ch in path):
        found.append("whitespace")
    if "'" in path or '"' in path:
        found.append("quote")
    if "\\" in path:
        found.append("backslash")
    if any(ord(ch) < 32 for ch in path):
        found.append("control-character")
    return found


def reproduces_block(entries):
    """Compatibility claims DERIVED from the names, not asserted.

    Reviewer %31 (t465 P2) broke the previous version with two supported names:
    `it's.js` makes the legacy pipeline fail on xargs' unmatched quote even with no
    whitespace, and a name containing a newline makes sha256sum ESCAPE its OUTPUT line,
    so the NUL-safe pipeline's digest no longer equals our raw-path payload even though
    it exits 0. Both flags were unconditional claims; both are now computed.
    The complete logical hash is unchanged: if a name defeats a command, the command is
    declared non-reproducing, never the hash."""
    legacy_risk, nul_risk = [], []
    for path, _ in entries:
        hazards = _hazards(path)
        if hazards:
            legacy_risk.append((path, hazards))
        if any(h in ("backslash", "control-character") for h in hazards):
            nul_risk.append((path, hazards))

    def _reason(risks):
        return None if not risks else ("%d name(s) defeat this command: %s"
                                       % (len(risks), "; ".join(
                                           "%s (%s)" % (p, ",".join(h))
                                           for p, h in risks[:3])))

    return {"nul_safe_command": BUILD_PIPELINE,
            "legacy_command": LEGACY_RECIPE,
            "supported_names_contract": "flags hold for names with no whitespace, no quote, "
                                        "no backslash and no control character; anything else "
                                        "is reported as a hazard on the command it defeats",
            "nul_safe_reproduces": not nul_risk,
            "nul_safe_reason": _reason(nul_risk),
            "legacy_recipe_reproduces": not legacy_risk,
            "legacy_reason": _reason(legacy_risk),
            "payload": "sorted '<hex>  ./path' lines with RAW paths, sha256 of the joined text"}


def snapshot_build(dist: Path):
    """Serve an IMMUTABLE SNAPSHOT of the dist, and bind the receipt to it.

    Reviewer %31 (t455, P2): the first version hashed the input tree once and then kept
    serving that same writable directory, so a rebuild during the run produced a passing
    report whose digest named an older tree. Hashing twice would only have detected it
    after the fact; serving a copy removes the window.

    Two digests are recorded, on purpose:
      * release_recipe_sha256 - the desk's own recipe (find . -type f ...). It is what a
        release manifest uses, so it stays for identity with releases.
      * served_sha256 - the same walk INCLUDING symlinks, because SimpleHTTPRequestHandler
        serves a symlinked file's target content while `find -type f` skips it (also
        reviewer %31, t455 P2). This is the number that binds what the browser saw.
    Both are recomputed after the run; a change is a failure, not a footnote.
    """
    if dist is None:
        return None, {"manifest_sha256": None, "command": BUILD_PIPELINE,
                      "reason": "no --dist given: a --url run is NOT bound to immutable bytes",
                      "file_count": None}
    if not dist.is_dir():
        return None, {"manifest_sha256": None, "command": BUILD_PIPELINE,
                      "reason": "dist path is not a directory: %s" % dist, "file_count": None}
    snapshot = Path(tempfile.mkdtemp(prefix="desk-matrix-snapshot-"))
    # DEREFERENCE. `cp -a` keeps links to files and directories OUTSIDE the snapshot, so
    # the static server would keep following mutable external targets while the manifests
    # hashed them once - a before/after comparison catches a persistent change, not one
    # restored after its response (reviewer %31, t460 P2). `-L` copies target CONTENT, so
    # the snapshot is self-contained and the bytes hashed are the bytes served.
    copy = subprocess.run(["cp", "-aL", str(dist) + "/.", str(snapshot)],
                          capture_output=True, text=True)
    if copy.returncode != 0:
        return None, {"manifest_sha256": None, "command": BUILD_PIPELINE,
                      "reason": "snapshot copy failed: %s" % (copy.stderr or "")[:120],
                      "file_count": None}
    served, entries, problems = _manifest(snapshot, follow_dir_symlinks=True)
    strict, strict_entries, strict_problems = _manifest(snapshot, follow_dir_symlinks=False)
    block = {"manifest_sha256": served,
             "served_sha256": served,
             "release_recipe_sha256": strict,
             "served_files": len(entries),
             "release_files": len(strict_entries),
             "command": BUILD_RECIPE,
             "served_command": "python: sorted './path' + sha256(file bytes), sha256sum-like"
                               " payload, digest of that payload",
             "reproduces": reproduces_block(entries),
             "symlink_policy": "snapshot dereferences links (cp -L): served bytes are copies "
                               "inside the snapshot, so an external target cannot change what "
                               "the browser got after the manifest was taken; a dangling link "
                               "or cycle is a problem, not a silently skipped entry",
             "problems": problems + strict_problems,
             "input_tree": str(dist),
             "served_snapshot": str(snapshot),
             "reason": None}
    if problems:
        # An incomplete manifest must not masquerade as a binding for a smaller tree.
        block["manifest_sha256"] = None
        block["served_sha256"] = None
        block["reason"] = ("manifest incomplete: %d problem(s); no digest is reported "
                           "for a tree that could not be fully hashed" % len(problems))
    return snapshot, block


def serve_dist(dist: Path):
    handler = partial(StaticHandler, directory=str(dist))
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, f"http://127.0.0.1:{server.server_address[1]}/"


def themes_available(dist: Path):
    """Registered themes, from the build's own manifests (not a hardcoded list)."""
    candidates = list((dist / "assets").glob("*.js")) + list((dist / "themes").glob("*.json"))
    ids = sorted(p.stem for p in candidates if p.suffix == ".json")
    if ids:
        return ids
    # Vite inlines the manifests into the bundle; the JSON ids are the source of truth.
    src = ROOT / "web" / "src" / "themes"
    if src.is_dir():
        return sorted(p.stem for p in src.glob("*.json"))
    return ["default"]


def run(argv=None):
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--dist", type=Path, help="built web/dist to serve")
    parser.add_argument("--url", help="already-serving base URL (instead of --dist)")
    parser.add_argument("--artifacts", type=Path, required=True, help="new directory for evidence")
    parser.add_argument("--widths", help="comma-separated subset of matrix names")
    parser.add_argument("--themes", help="comma-separated subset of theme ids")
    parser.add_argument("--self-check", action="store_true",
                        help="negative control: assert every covered rule FAILS on a broken fixture")
    args = parser.parse_args(argv)

    from playwright.sync_api import sync_playwright

    args.artifacts.mkdir(parents=True, exist_ok=True)
    server = None
    served_dir = None
    build_block = {"manifest_sha256": None, "served_sha256": None,
                   "reason": "no --dist given: a --url run is NOT bound to immutable bytes"}

    if args.dist:
        served_dir, build_block = snapshot_build(args.dist)
        if served_dir is None:
            parser.error("could not snapshot the dist: %s" % build_block.get("reason"))
        server, base = serve_dist(served_dir)
    elif args.url:
        base = args.url.rstrip("/") + "/"
    elif args.self_check:
        server, base = serve_broken_fixture()
    else:
        parser.error("one of --dist, --url or --self-check is required")
    if args.self_check and args.dist:
        server2, base_broken = serve_broken_fixture()
        server = (server, server2)

    widths = [w for w in WIDTHS if not args.widths or w[0] in args.widths.split(",")]
    if not widths:
        parser.error("no matrix width matched --widths %r; known: %s"
                     % (args.widths, ", ".join(w[0] for w in WIDTHS)))
    registered = themes_available(args.dist or ROOT / "web" / "dist")
    themes = args.themes.split(",") if args.themes else registered
    unknown = [t for t in themes if t not in registered]
    if unknown:
        parser.error("unknown theme(s) %s; available: %s"
                     % (", ".join(repr(t) for t in unknown), ", ".join(registered)))
    report = {"artifact": "desk_matrix", "schema": 1, "generated_at": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
              "input": {"dist": str(args.dist) if args.dist else None, "url": base},
              # The run is bound to bytes here, before any rule claims anything.
              "build": build_block,
              "emulation": "viewport size + deviceScaleFactor only; NOT browser zoom",
              "widths": [w[0] for w in widths], "themes": themes,
              "rules": ["renders", "no-descendant-overflow", "no-page-scrollbar",
                        "target-size", "dialog-closed", "no-writes"],
              "not_covered": NOT_COVERED, "results": [], "failures": []}

    with sync_playwright() as pw:
        browser = pw.chromium.launch(args=["--no-sandbox"])
        try:
            for theme in themes:
                for name, width, dpr in widths:
                    page = browser.new_page(viewport={"width": width,
                                                      "height": 844 if width <= 700 else 900},
                                            device_scale_factor=dpr, locale="en-US")
                    writes, route_misses, page_errors = [], [], []
                    page.add_init_script(f"localStorage.setItem('vp-theme', {json.dumps(theme)})")
                    # localStorage is per-origin, so the theme is also carried in the URL
                    # for builds that read it; both are set, neither is trusted for a claim.
                    page.on("pageerror", lambda e: page_errors.append(str(e)))

                    def note_write(request):
                        # A route on **/api/** cannot see a POST to /mutation, so a
                        # non-API write reached the server while the report passed clean
                        # (reviewer %65, t432 false-pass 1). EVERY non-GET is recorded here;
                        # the /api/** route below still aborts what it can.
                        if request.method != "GET":
                            writes.append({"method": request.method, "url": request.url})

                    page.on("request", note_write)

                    def route(r):
                        if r.request.method != "GET":
                            r.abort()
                            return
                        path = urlsplit(r.request.url).path
                        if path in SYNTHETIC_APIS:
                            r.fulfill(status=200, content_type="application/json",
                                      body=json.dumps(SYNTHETIC_APIS[path]))
                        else:
                            route_misses.append(path)
                            r.fulfill(status=404, content_type="application/json", body="{}")

                    page.route("**/api/**", route)
                    page.goto(base, wait_until="load")
                    # MOUNT BEFORE JUDGING. The first version of this tool evaluated 700 ms
                    # after domcontentloaded and reported "renders: false" for every case -
                    # a race in the harness, not a desk defect. Wait for React to commit.
                    try:
                        page.wait_for_function(
                            "document.getElementById('root') && "
                            "document.getElementById('root').children.length > 0", timeout=8000)
                    except Exception:                   # noqa: BLE001 - reported as a failure
                        pass
                    page.wait_for_timeout(600)
                    data = page.evaluate(OVERFLOW_JS)
                    shot = args.artifacts / f"{theme}-{name}.png"
                    page.screenshot(path=str(shot), full_page=False)
                    page.close()

                    case = {"theme": theme, "width": name, "css_px": width, "dpr": dpr,
                            "screenshot": shot.name, "rules": {}, "failures": []}
                    def fail(rule, what, expected, got):
                        case["rules"][rule] = False
                        f = {"theme": theme, "width": name, "css_px": width, "rule": rule,
                             "assertion": what, "expected": expected, "got": got}
                        case["failures"].append(f)
                        report["failures"].append(f)

                    if data["loginScreen"]:
                        fail("renders", "the desk shell is on screen, not the login page",
                             "authenticated desk (.dash-root)", "the login screen")
                    elif not data["shellMounted"]:
                        fail("renders", "app shell mounted before any rule is judged",
                             "an element matching .dash-root|.shell", "none")
                    if data["overflowCount"]:
                        fail("no-descendant-overflow",
                             "every element's scrollWidth <= clientWidth",
                             "0 overflowing elements",
                             "%d, first: %s (%d > %d)" % (
                                 data["overflowCount"], data["overflow"][0]["selector"],
                                 data["overflow"][0]["scrollWidth"],
                                 data["overflow"][0]["clientWidth"]))
                    if data["pageScrollWidth"] > data["pageClientWidth"] + 1:
                        fail("no-page-scrollbar", "documentElement.scrollWidth <= clientWidth",
                             str(data["pageClientWidth"]), str(data["pageScrollWidth"]))
                    if data["smallTargets"]:
                        first = data["smallTargets"][0]
                        fail("target-size",
                             "every interactive control is at least %dx%d CSS px (WCAG 2.5.8)"
                             % (MIN_TARGET_PX, MIN_TARGET_PX),
                             "0 controls under the floor",
                             "%d under it, first: %s (%dx%d)" % (
                                 len(data["smallTargets"]), first["selector"],
                                 first["width"], first["height"]))
                    if data["dialogOpen"]:
                        fail("dialog-closed", "no role=dialog / dialog[open] at capture time",
                             "no dialog", "a dialog was open")
                    if writes:
                        fail("no-writes", "the desk issues no non-GET request (any method, any path)",
                             "0 non-GET requests",
                             json.dumps(writes[:3]))
                    for rule in ("renders", "no-descendant-overflow", "no-page-scrollbar",
                                 "target-size", "dialog-closed", "no-writes"):
                        case["rules"].setdefault(rule, True)
                    case["route_misses"] = sorted(set(route_misses))
                    case["page_errors"] = page_errors[:5]
                    report["results"].append(case)
        finally:
            browser.close()
            for s in (server if isinstance(server, tuple) else (server,)):
                if s is not None:
                    s.shutdown()

    if served_dir is not None:
        after, _, after_problems = _manifest(served_dir, follow_dir_symlinks=True)
        if after_problems:
            report["failures"].append({"theme": "*", "width": "*", "rule": "build-stable",
                                       "assertion": "the served tree stays fully hashable",
                                       "expected": "no problems",
                                       "got": json.dumps(after_problems[:3])})
        report["build"]["served_sha256_after"] = after
        report["build"]["stable"] = (after == report["build"]["served_sha256"])
        if not report["build"]["stable"]:
            report["failures"].append({"theme": "*", "width": "*", "rule": "build-stable",
                                       "assertion": "the served tree is unchanged for the whole run",
                                       "expected": report["build"]["served_sha256"],
                                       "got": after})
    report["passed"] = not report["failures"]
    report["summary"] = {"cases": len(report["results"]), "failures": len(report["failures"]),
                         "by_rule": {}}
    for f in report["failures"]:
        report["summary"]["by_rule"][f["rule"]] = report["summary"]["by_rule"].get(f["rule"], 0) + 1
    out = args.artifacts / "desk-matrix.json"
    out.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({"summary": str(out), "cases": len(report["results"]),
                      "passed": report["passed"], "failures": len(report["failures"]),
                      "by_rule": report["summary"]["by_rule"]}))
    if args.self_check:
        # The negative control is the point: a rule that cannot fail is not evidence.
        covered = ("no-descendant-overflow", "no-page-scrollbar", "target-size")
        tripped = {f["rule"] for f in report["failures"]}
        missing = [r for r in covered if r not in tripped]
        print(json.dumps({"self_check": "pass" if not missing else "FAIL",
                          "tripped": sorted(tripped), "did_not_trip": missing}))
        return 0 if not missing else 1
    return 0 if report["passed"] else 1


BROKEN_FIXTURE = """<!doctype html><html><head><meta charset="utf-8"><title>broken</title>
<style>html,body{margin:0} .wide{width:2000px;height:40px;background:#333}
.tiny{width:10px;height:10px;border:0;padding:0;background:#555}
.under{width:23.75px;height:23.75px;border:0;padding:0;background:#777}</style></head>
<body><main><h1>negative control</h1><div class="wide">this element is 2000px wide on a narrow viewport</div>
<button class="tiny" aria-label="too small"></button>
<!-- 23.75 rounds to 24: this control exists to prove the floor compares UNROUNDED -->
<button class="under" aria-label="just under the floor"></button>
<script>fetch('/mutation', {method: 'POST', body: 'x'})</script>
</main></body></html>"""


def serve_broken_fixture():
    """A page that MUST trip no-descendant-overflow and no-page-scrollbar."""
    import tempfile
    folder = Path(tempfile.mkdtemp(prefix="desk-matrix-broken-"))
    (folder / "index.html").write_text(BROKEN_FIXTURE)
    return serve_dist(folder)


if __name__ == "__main__":
    raise SystemExit(run())
