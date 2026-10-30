def header():
    """The title line names the commit this document was generated FROM.

    It used to be a frozen string naming 038d9aa, which made `--check` pass on a
    document that described a tree three hours old - the "green check measuring
    nothing" defect this task exists to catch (t766). The revision is read from the
    tree at generation time, so a manifest generated before a merge fails its own
    check afterwards, which is the correct answer: the document IS stale then.
    """
    out = subprocess.run(["git", "-C", str(ROOT), "rev-parse", "--short", "HEAD"],
                         capture_output=True, text=True)
    revision = out.stdout.strip() if out.returncode == 0 else "unknown"
    stamp = time.strftime("%Y-%m-%d", time.gmtime())
    return (f"# Port manifest (generated from {revision}, {stamp})\n\n"
            "Everything the Rust port must reproduce, generated from the tree. "
            "Regenerate with `tools/gen_manifest.py` (TASKS.md T00) when main moves; "
            "`--check` fails when this document no longer matches the tree it names.")

MODULE_MAP = """## Module map

| python | rust | crate | phase | notes |
|---|---|---|---|---|
| dunk_core.py (Flock, Dunker, Message) | flock/{mod,dunks,sends,commands_*}.rs | ds-core | 1 | split by method group; loops become tokio tasks (5.3) |
| dunk_api.py | api.rs + registry.rs | ds-core | 1 | handlers become typed arg structs; registry drives CLI + MCP |
| dunk_server.py | server.rs | ds-core | 1 | tokio UnixListener; SO_PEERCRED via nix; identity binding verbatim |
| dunk_client.py | client.rs | ds-core | 1 | identity resolution (TMUX_PANE vs tty) verbatim; service token |
| dunk_callers.py | proc.rs | ds-core | 1 | ROOT OF TRUST: port first, test exhaustively |
| dunk_roles.py | roles.rs | ds-core | 1 | ALLOW tables as const; notify-based refresh |
| dunk_superusers.py | roles.rs (legacy reader only) | ds-core | 1 | parse_allowlist for the completion late-check |
| dunk_tasks.py | tasks/{store,claim,metrics}.rs | ds-core | 1 | schemas verbatim; claim_next in the writer actor |
| dunk_next_job.py | tasks/next_job.rs | ds-core | 1 | |
| dunk_inbox.py | inbox/store.rs | ds-core | 1 | blob dir, digests, retention |
| dunk_terminal.py | terminal/{control,tmux,parse}.rs | ds-core | 1 | control mode + subprocess fallback (5.1) |
| dunk_tmux_guard.py, dunk_guard.py, dunk_guard_wrapper.py | ds-cli guard subcommands + the shim scripts (kept as sh) | ds-cli | 2 | the wrappers are shell; only the installer ports |
| dunk_panes.py | panes.rs | ds-core | 1 | lanes.json, retired-panes.json |
| dunk_activity.py | activity.rs | ds-core | 1 | turn state, TTL |
| dunk_stuck.py | stuck.rs | ds-core | 1 | pure state machine; port tests 1:1 |
| dunk_dispatch.py | dispatch.rs | ds-core | 2 | tick runs from the timer binary |
| dunk_owner.py | owner.rs | ds-core | 1 | owner-push.db |
| dunk_metrics.py | metrics.rs | ds-core | 1 | append-only events |
| dunk_state.py | state.rs | ds-core | 2 | agent-state.db capture |
| dunk_load.py, dunk_policy.py | load.rs, policy.rs | ds-core | 2 | /proc sampling, kill decisions |
| dunk_context.py | context/{store,ledger,digest}.rs | ds-core | 2 | markdown front matter parser; FTS index; ledger |
| dunk_memory.py | memory/{ingest,recall}.rs | ds-core | 2 | FTS5 bundled; JSONL ingest |
| dunk_github.py, dunk_review.py, dunk_poolrad.py | github.rs, review.rs, poolrad.rs | ds-core | 2 | jsonwebtoken RS256; reqwest |
| dunk_watchdog.py, dunk_watchdog_deadman.py | watchdog/{rules,deadman}.rs | ds-core | 2 | rule engine; same digests |
| dunk_health.py, dunk_usage.py, dunk_views.py, dunk_pm.py, dunk_help.py | health.rs, usage.rs, views.rs, pm.rs, help.rs | ds-core | 2 | views reads the desk db read-only until phase 3 |
| dunk_allocator.py, dunk_fallback.py, dunk_recovery*.py, dunk_native_session.py, dunk_http.py | allocator.rs, fallback.rs, recovery.rs, native_session.rs, http.rs | ds-core | 2 | http facade :8792 read-only; keep |
| dunk_mcp.py | ds-mcp | ds-mcp | 1 | stdio JSON-RPC; tools from the registry filtered by role |
| dunkingsheep (CLI) | ds-cli | ds-cli | 1 | generated parser + ported printers; local-only commands (context reads, github one-shots) |
| herdr_client.py | (dropped) | - | - | the herdr backend is retired (tmux default since 2026-10-24); keep the Terminal trait so a backend can return |
| dunking_sheep_tui.py, text_editor.py | ds-tui (ratatui) | ds-tui | 3 | |
| web/, bridges/, ui/ | ds-web (axum) + unchanged React | ds-web | 3 | FastAPI routes 1:1; the owner mailbox/report land here |"""

CLI_COMMANDS = """## CLI top-level commands

`allocation` `status` `turn` `poll` `list` `get` `add` `update` `remove` `start` `stop` `toggle` `stop-all` `fire` `send` `send-keys` `install` `guard` `inbox` `context` `load` `pm` `task` `gh-token` `git-credential` `review` `poolrad` `github` `whoami` `token` `superuser` `memory` `health` `watchdog` `watchdog-deadman` `usage` `recover` `doctor` `owner` `state` `roster` `message-state` `reply-stats` `project-summary` `api` `capability` `lane` `retired` `dispatch` `clear-box` `delivery` `flush` `messages` `cancel-message` `metrics` `panes` `workspaces` `new-workspace` `new-tab` `split` `start-agent` `respawn-pane` `rename` `focus` `close` `herdr` `herdr-help` `read` `shutdown` `serve` `tui` `mcp` `mcp-setup` `guide` `commands` `mcp-map`"""

WEB_ROUTES = """## Web routes (web/app.py as of PR #120, for T62)

- `post /api/login` (line 320)
- `post /api/logout` (line 342)
- `post /api/password` (line 357)
- `post /api/viewport` (line 419)
- `get /api/channels` (line 450)
- `get /api/pane-text` (line 472)
- `get /api/capability` (line 558)
- `post /api/pane-send` (line 575)
- `get /api/fleet` (line 697)
- `get /api/ops-metrics` (line 832)
- `get /api/ops-metrics/history` (line 846)
- `post /api/ops-metrics/refresh` (line 863)
- `get /api/ops-board` (line 877)
- `post /api/ops-board/refresh` (line 893)
- `get /api/game/status` (line 1260)
- `get /api/game/codewheel/{filename}` (line 1265)
- `post /api/game/start` (line 1278)
- `post /api/game/stop` (line 1286)
- `post /api/game/key` (line 1291)
- `post /api/game/type` (line 1300)
- `get /api/game/states` (line 1309)
- `post /api/game/savestate` (line 1314)
- `post /api/game/loadstate` (line 1322)
- `post /api/game/autocodeword` (line 1330)
- `get /api/game/video` (line 1342)
- `get /api/game/audio` (line 1350)
- `get /api/messages` (line 1358)
- `get /api/deliveries` (line 1378)
- `get /api/messages/{message_id}/context` (line 1398)
- `post /api/messages` (line 1426)
- `post /api/messages/{message_id}/send-now` (line 1490)
- `post /api/brief` (line 1553)
- `get /api/search` (line 1580)
- `post /api/uploads` (line 1594)
- `get /api/uploads/{attachment_id}` (line 1611)
- `get /api/panes/activity` (line 1631)
- `get /api/activity` (line 1666)
- `get /api/poolrad-dashboard/progress` (line 1747)
- `post /api/poolrad-dashboard/refresh` (line 1781)
- `get /api/poolrad-dashboard/dashboard.html` (line 1809)
- `get /api/poolrad-dashboard/state/reports/{filename}` (line 1820)
- `get /api/herdr-webui` (line 1857)
- `get /api/health` (line 1871)
- `get /api/version` (line 1876)
- `get /dash` (line 1929)
- `get /dash/` (line 1930)
- `get /dash/{path:path}` (line 1935)"""

# t766: the asserted error strings, recorded (see error_strings_section).
# The selector the manifest was generated from is NOT recoverable as a rule;
# this list is the decision, and error_strings_section checks every entry is
# still asserted under tests/ so the document cannot claim a string the suite
# dropped.
ASSERTED_ERROR_STRINGS = [
    '/tmp/my-own-scratch',
    '01a18644',
    'CODEX_HOME',
    'DECLARED by more than one live pane',
    'DUNKINGSHEEP_TMUX_SOCKET',
    'Meta-dunking',
    'RAISED',
    'UNKNOWN',
    '[socket/cli only]',
    'a verdict needs evidence',
    'active turn',
    'add_dunk(',
    'alive_source|fixture',
    'already cancelled',
    'already done with verdict ',
    'already exists',
    'already gone',
    'ambiguous',
    'attributed_cores',
    'before',
    'body or an attachment',
    'caller',
    'caller is not authenticated',
    'cannot be verified',
    'cannot change status',
    'canonical JSON',
    'changed during completion',
    'channel',
    'clamped to 60 min',
    'command is required',
    'completed by its reviewer',
    'confirm-idle',
    'confirmed by the turn',
    'confirmed idle',
    'could not create the workspace x: workspace limit',
    'could not rename tab w2:t2: nope',
    'could not start agent ',
    'cwd mismatch',
    'daemon',
    'dead pane id is not a mailbox',
    'dead since 2026-10-24 08:00',
    'default',
    'did not submit',
    'direction',
    'dispatch',
    'does not support',
    'dunk_sqlite.py',
    'dunkingsheep-tests-',
    'duplicate task_started',
    'ever run',
    'exited immediately',
    'expected',
    'expected ',
    'filed by the review timer',
    'gone at the end',
    'has no start',
    'herdr pane commands',
    'ignoring ambient DUNKINGSHEEP_CLAUDE_STATUS',
    'ignoring ambient DUNKINGSHEEP_DIR',
    'inbox.undeliverable',
    'is cancelled',
    'isolated per-lane',
    'keys are required',
    'kind must be',
    'label is required',
    'live task lease',
    'live wrapper|no codex child|rollback not verified',
    'malformed',
    'may not review',
    'may only act as the author App',
    'may only type into its own pane|requires',
    'missing required',
    'names are no longer matched',
    'names thread',
    'needs --verdict',
    'needs TMUX_PANE',
    'needs repo, pr and head',
    'never became a process',
    'never ran|No such file',
    'never ran|not found',
    'never ran|not found|cannot execute',
    'newest-body',
    'newline-delimited JSON',
    'no dunk with id',
    'no dunkingsheep daemon',
    'no lanes list',
    'no live pane',
    'no mailbox given',
    'no message with id',
    'no owner is registered',
    'no pane matches',
    'no process id',
    'no queued message',
    'no such file',
    'no tab matches',
    'no target',
    'no workspace matches',
    'non-guard tmux',
    'not a git repository',
    'not allowed',
    'not in the private state',
    'not inside pane',
    'not live',
    'not owned',
    'not permitted for role anonymous',
    'not permitted for role desk|requires a superuser',
    'not permitted for role web',
    'not permitted for role worker',
    'not permitted for role worker|requires a superuser',
    'not visible',
    'not yours',
    'not.*complet|owned|owner',
    'not.there',
    'nothing after the sigil',
    'nothing to submit',
    'only the owner supersedes a rule',
    'only the owner writes rules',
    'only to yourself',
    'outbox',
    'partial line',
    'pass --force',
    'private',
    'process start',
    'reason=dead_lane',
    'reason=unhoused_lane',
    'reason=unknown_name',
    'refused',
    'refused before mutation',
    'refusing to auto-start a tmux daemon',
    'refusing to close',
    'request',
    'requires a superuser',
    'requires a superuser|not permitted',
    'requires a superuser|not permitted for role',
    'requires the owner role',
    'requires the system token',
    'retry the request',
    'rolled back',
    'rollout does not exist',
    'scope must be',
    'selected real tmux',
    'self-pane',
    'send_text',
    'session_meta',
    'session_meta names',
    'shared',
    'slot.1',
    'split must be',
    'submission unconfirmed',
    'timed out',
    'type must be one of',
    'unattributed_cores',
    'unfinished turn.*turn-old',
    'unknown argument',
    'unknown command',
    'unknown provider',
    'unknown terminal backend',
    'unknown_name',
    'unread',
    'width too small',
    'working',
]

FILES_ON_DISK = """## Files on disk (~/.config/dunkingsheep)

agent-state.db, context-ledger.db, desk.json, dialog-alerts.db, dispatch-state.json, dispatch.db, dispatch_lanes.json, dunks.json, github-app-author.json, github-app-reviewer.json, github-repos.json, github-tokens.json, health.json, inbox.db, lanes.json, launches.json, load-state.json, memory.db, metrics.db, owner-push.db, pm-routing.json, retired-panes.json, roles.json, service-tokens.json, superusers.json, tasks.db, tmux-panes.json, tmux-trial-panes.json, usage.json, watchdog.json, watchdog_deadman_state.json, watchdog_deadman_status.json, watchdog_state.json; plus `context/` (markdown store), `attachments/` (inbox blobs), `bin/` (wrappers), `system-token`, `web-token`, `github-app-*.pem`, `dunkingsheep.sock`, `daemon.log`."""


import argparse
import json
import ast
import os
import pathlib
import re
import sqlite3
import subprocess
import time
import sys
import textwrap

ROOT = pathlib.Path(__file__).resolve().parents[1]
OUT = ROOT / "docs/rust/MANIFEST.md"
CONFIG = pathlib.Path(os.environ.get("DUNKINGSHEEP_DIR",
                                     str(pathlib.Path.home() / ".config" / "dunkingsheep")))
DATABASES = ("tasks", "inbox", "metrics", "dispatch", "agent-state", "owner-push", "memory")


def module_files():
    return sorted(p.name for p in ROOT.glob("*.py"))


def load_api():
    """The registry table, read from the code that owns it (dunk_api.COMMANDS)."""
    src = (ROOT / "dunk_api.py").read_text()


    def close(text, i, o, c):
        depth = 0
        for k in range(i, len(text)):
            if text[k] == o:
                depth += 1
            elif text[k] == c:
                depth -= 1
                if depth == 0:
                    return k
        raise SystemExit("unbalanced COMMANDS literal")

    i = src.index("COMMANDS = [")
    expr = src[i + len("COMMANDS = "):close(src, i, "[", "]") + 1]

    class FakeCommand:
        def __init__(self, name, description, handler=None, params=None, required=(),
                     mcp=True, mcp_required=None):
            self.name = name
            self.params = params or {}
            self.required = list(required)
            self.mcp = mcp
            self.mcp_required = (list(mcp_required) if mcp_required is not None
                                 else self.required)

    class Fallback(dict):
        def __missing__(self, key):
            return lambda *a, **k: None

    commands = eval(expr, Fallback({"Command": FakeCommand}))  # noqa: S307 - our own literal
    j = src.index("SUPERUSER_ONLY = {")
    superuser_only = eval(src[j + len("SUPERUSER_ONLY = "):close(src, j + len("SUPERUSER_ONLY = ") - 1, "{", "}") + 1])  # noqa: S307
    k = src.index("MCP_GROUPS = (")
    groups = eval(src[k + len("MCP_GROUPS = "):close(src, k, "(", ")") + 1])  # noqa: S307
    return commands, superuser_only, groups


def load_roles():
    ns = {"__name__": "dunk_roles_manifest"}
    exec(compile((ROOT / "dunk_roles.py").read_text(), "dunk_roles.py", "exec"), ns)  # noqa: S102
    return ns["ROLES"], ns["ALLOW"], ns["WEB_DENIED"]


def registry_section():
    commands, superuser_only, _ = load_api()
    roles, allow, web_denied = load_roles()

    def permitted(role, command):
        if role == "web" and command in web_denied:
            return False
        table = allow.get(role)
        return True if table is None else command in table

    rows = ["## Commands (the registry) with role permissions", "",
            "| command | required | params | mcp | owner-only | roles allowed |",
            "|---|---|---|---|---|---|"]
    for command in sorted(commands, key=lambda c: c.name):
        params = ", ".join(command.params) or "-"
        required = ", ".join(command.required) or "-"
        allowed = ", ".join(role for role in roles if permitted(role, command.name))
        rows.append(f"| `{command.name}` | {required} | {params} | "
                    f"{'y' if command.mcp else 'n'} | "
                    f"{'y' if command.name in superuser_only else ''} | {allowed} |")
    return "\n".join(rows) + "\n"


def mcp_section():
    _, _, groups = load_api()
    out = ["## MCP tool groups", ""]
    for name, _description, actions in groups:
        out.append("- **%s**: %s" % (name, ", ".join(f"{a}->{c}" for a, c in actions)))
    return "\n".join(out) + "\n"


SCHEMA_FIXTURE = ROOT / "rust" / "fixtures" / "state" / "schema.json"


def _live_schema(name):
    """The schema of one live database: `sqlite_master`, in name order."""
    path = CONFIG / f"{name}.db"
    con = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
    try:
        return [row[0] for row in con.execute(
            "select sql from sqlite_master where sql is not null order by name")]
    finally:
        con.close()


def schema_section():
    """The seven schemas, from the FIXTURE (T00 decision (a)), live only as a fallback.

    Reading the live databases at generation time made the document a property of the
    HOST rather than of the tree: `--check` failed on 2026-10-30 because the live
    `tasks.db` had grown allocation_workers/offers/reviews since the manifest was
    written, and a machine with no install could not generate a manifest at all
    (`missing live database` -> SystemExit, which is what made the generator
    unusable in CI and in a test). The committed fixture is the snapshot of that
    present; tools/snapshot_state_schema.py --check-drift is what keeps it honest.
    """
    out = ["## Schemas (live databases, verbatim)", ""]
    fixture = {}
    try:
        fixture = json.loads(SCHEMA_FIXTURE.read_text())
    except (OSError, ValueError):
        fixture = {}
    if fixture:
        out[0] = (f"## Schemas (snapshot of the live databases, verbatim; "
                  f"{SCHEMA_FIXTURE.relative_to(ROOT)} - refresh with "
                  f"tools/snapshot_state_schema.py --write)")
        out.append("")
    for name in DATABASES:
        statements = fixture.get(name)
        if statements is None:
            path = CONFIG / f"{name}.db"
            if not path.exists():
                raise SystemExit(
                    f"no schema for {name!r}: {SCHEMA_FIXTURE} has no entry and the live "
                    f"database {path} is absent. Snapshot one with "
                    "tools/snapshot_state_schema.py --write, or point DUNKINGSHEEP_DIR at "
                    "a real install (a database created from the Python SCHEMA is NOT the "
                    "same file: tasks.priority DEFAULT is 'medium' live, 'low' in source).")
            statements = _live_schema(name)
        out.append(f"## {name}.db")
        out.extend(f"{sql};" for sql in statements)
        out.append("")
    return "\n".join(out).rstrip("\n") + "\n"


def tests_section():
    stems = [p.stem for p in sorted((ROOT / "tests").glob("*.py"))]
    names = ["_bootstrap" if s == "_bootstrap" else s for s in stems]
    blob = " ".join(sorted(set(names) | {"fixtures"}))
    return ("## Tests (%d files; port each 1:1 under rust/crates/*/tests or as unit tests "
            "in the module)\n\n%s\n" % (len(names), textwrap.fill(blob, width=200)))


def docstring_prompt(tree):
    doc = ast.get_docstring(tree) or ""
    return join_three_lines(doc)


def join_three_lines(doc):
    """The first THREE source lines of a docstring, blank -> two spaces (T00)."""
    lines = doc.strip().split("\n")[:3]
    text = ""
    spaced = False
    for line in lines:
        if not line.strip():
            spaced = True
            continue
        text += ("  " if spaced and text else ("" if not text else " ")) + line.rstrip()
        spaced = False
    return text


CONST_VALUE_MAX = 90          # the length every long const in the committed manifest is cut to


def signature(node):
    args = [a.arg for a in list(node.args.posonlyargs) + list(node.args.args)]
    if node.args.vararg:
        args.append("*" + node.args.vararg.arg)
    elif node.args.kwarg:
        args.append("**" + node.args.kwarg.arg)
    args += [a.arg for a in node.args.kwonlyargs]
    if node.args.kwarg and node.args.vararg:
        args.append("**" + node.args.kwarg.arg)
    return ", ".join(args)


def first_sentence(text):
    flat = " ".join((text or "").split())
    if not flat:
        return ""
    match = re.search(r"(?<=[.!?])\s", flat)
    return flat[:match.start()] if match else flat


def _entry_line(node, indent="", drop_self=False):
    """`- \u0060name(args)\u0060 \u2014 first sentence` at the given indent.

    A CLASS method is shown the way the committed artefact shows it: without a
    leading `self` (they are read as you would call them on an instance). `cls`
    stays, because the artefact keeps it - `from_state(cls, data)`, `load(cls, text)`.
    """
    args = signature(node)
    if drop_self and args.split(",")[0].strip() == "self":
        args = ", ".join(part.strip() for part in args.split(",")[1:])
    entry = f"{indent}- `{node.name}({args})`"
    sentence = first_sentence(ast.get_docstring(node))
    return f"{entry} \u2014 {sentence}" if sentence else entry


def _const_line(node, source, indent=""):
    """`- const \u0060NAME\u0060 = \u0060value\u0060` for an ALL_CAPS module or class constant."""
    if not isinstance(node, ast.Assign) or len(node.targets) != 1:
        return None
    if not isinstance(node.targets[0], ast.Name):
        return None
    target = node.targets[0].id
    if not (target.isupper() or target.startswith("_") and target.upper() == target):
        return None
    value = " ".join((ast.get_source_segment(source, node.value) or "").split())
    # 90 characters is the rule the committed artefact already follows (measured on
    # docs/rust/MANIFEST.md: the longest const value is exactly 90 characters and 88
    # of them are at that length). Long constants - SCHEMA blocks, PARAM dicts - are
    # unreadable in full and the port only needs their identity, so the value is
    # truncated rather than wrapped.
    if len(value) > CONST_VALUE_MAX:
        value = value[:CONST_VALUE_MAX]
    return f"{indent}- const `{target}` = `{value}`"


def inventory_section():
    """The inventory, INCLUDING class methods (t766).

    The first version walked `ast.Module.body` only, so a class emitted one line and
    never its methods: the generated document was 2,070 lines against a committed
    2,803, and every short section was a class-heavy module (dunk_core.py 138 vs 411,
    dunk_tasks.py with 47 methods missing). A manifest that silently omits every
    method of every class is a green check measuring nothing - the same defect the
    task exists to catch.

    The shape is the COMMITTED one, because the artefact is the contract (decided on
    t766): `- class **Name**` in bold with the methods nested two spaces under it.
    """
    out = ["## Inventory: every module, class, function, constant", ""]
    for name in module_files():
        source = (ROOT / name).read_text()
        tree = ast.parse(source)
        out.append(f"## {name} ({len(source.splitlines())} lines)")
        out.append("")
        out.append("> " + docstring_prompt(tree))
        out.append("")
        for node in tree.body:
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
                out.append(_entry_line(node))
            elif isinstance(node, ast.ClassDef):
                sentence = first_sentence(ast.get_docstring(node))
                head = f"- class **{node.name}**"
                out.append(f"{head} \u2014 {sentence}" if sentence else head)
                for child in node.body:
                    if isinstance(child, (ast.FunctionDef, ast.AsyncFunctionDef)):
                        out.append(_entry_line(child, indent="  ", drop_self=True))
                    else:
                        line = _const_line(child, source, indent="  ")
                        if line:
                            out.append(line)
            else:
                line = _const_line(node, source)
                if line:
                    out.append(line)
        out.append("")
    return "\n".join(out).rstrip("\n") + "\n"


def web_routes_section():
    """Routes from web/app.py when this tree has it; the recorded table otherwise.

    The desk landed on main through PR #120, so a branch that predates that merge
    (this port branch) has no web/ to scan. Deriving from the tree is preferred and
    is what T62 must do once web/ is here; until then the recorded table is used and
    --check says so.
    """
    source_path = ROOT / "web/app.py"
    if not source_path.exists():
        return WEB_ROUTES + "\n"
    source = source_path.read_text()
    out = ["## Web routes (web/app.py as of PR #120, for T62)", ""]
    for number, line in enumerate(source.split("\n"), start=1):
        match = re.match(r'@app\.(get|post|put|delete|patch)\(\s*"([^"]+)"', line.strip())
        if match:
            out.append(f"- `{match.group(1)} {match.group(2)}` (line {number})")
    return "\n".join(out) + "\n"


def units_section():
    """The unit names, the daemon first, with the timer note the document carries."""
    stems = [p.stem for p in sorted((ROOT / "packaging/systemd").glob("*.service"))]
    if "dunkingsheep" in stems:
        stems.remove("dunkingsheep")
        stems.insert(0, "dunkingsheep")
    timers = list((ROOT / "packaging/systemd").glob("*.timer"))
    tail = " (each a .service, most with a .timer)" if len(timers) * 2 >= len(stems) else ""
    return "## Systemd units (packaging/systemd)\n\n%s%s.\n" % (", ".join(stems), tail)


def error_strings_section():
    """The 161 strings tests assert on, as a recorded list (t766 decision).

    The owner-role lane decided this on t766: the list is the ASSERTED set the test
    suite carries, not the product of a source sweep. Measured 2026-10-30: every
    mechanical rule for recovering it fails - string literals that are operands of
    assert* calls number 1,481 and cover 157 of the 161, so 4 come from somewhere
    else; assertRaisesRegex alone covers 114; a permission-shaped allowlist matches
    nothing. A rule that nearly matches is worse than a list, because it changes
    silently when a test is edited.

    So the list is data and it is CHECKED rather than derived: every recorded string
    must still appear somewhere under tests/, or --check names it and fails. A
    string that is no longer asserted is a claim this document can no longer make.
    """
    haystack = _tests_text()
    absent = [text for text in ASSERTED_ERROR_STRINGS if text not in haystack]
    if absent:
        raise SystemExit("the manifest would assert strings no test asserts any more: "
                         + ", ".join(repr(text) for text in absent[:5]))
    return ("## Error strings tests assert on (keep byte-identical)\n\n"
            + "\n".join("- `%s`" % text for text in ASSERTED_ERROR_STRINGS)
            + "\n")


def _tests_text():
    """Every test source concatenated: the drift check's haystack."""
    return "\n".join(path.read_text(errors="ignore")
                     for path in sorted((ROOT / "tests").rglob("*.py")))


def body():
    """Everything BELOW the title block: the part that must match byte for byte.

    The title names the commit the document was generated from, so it changes the
    moment the document is committed - which made `--check` red at every commit that
    contains the manifest, including the one that adds it (review t896). A gate that
    always fails is a gate lanes ignore, so the check compares THIS, and the title is
    reported as an advisory instead.
    """
    return "\n\n".join([
        MODULE_MAP,
        CLI_COMMANDS,
        FILES_ON_DISK,
        units_section().rstrip("\n"),
        registry_section().rstrip("\n"),
        mcp_section().rstrip("\n"),
        error_strings_section().rstrip("\n"),
        schema_section().rstrip("\n"),
        tests_section().rstrip("\n"),
        inventory_section().rstrip("\n"),
        web_routes_section().rstrip("\n"),
    ]) + "\n"


def title_revision(text):
    """The commit named in the title block, or None."""
    first = text.split("\n", 1)[0]
    match = re.search(r"generated from (\S+?),", first)
    return match.group(1) if match else None


def body_of(text):
    """A document's [`body`]: from the first `## ` section to the end.

    The title block is the `# ` line and the prose under it; everything a reader
    compares against the tree starts at the first section header.
    """
    lines = text.split("\n")
    for index, line in enumerate(lines):
        if line.startswith("## "):
            return "\n".join(lines[index:])
    return text


def build():
    """The whole document: title block, a blank line, then [`body`]."""
    return header() + "\n\n" + body()


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true",
                        help="compare with the committed manifest, per section")
    args = parser.parse_args(argv)
    text = build()
    if not args.check:
        OUT.write_text(text)
        print(f"wrote {OUT} ({len(text.splitlines())} lines)")
        return 0
    current = OUT.read_text()
    # The title is ADVISORY: it names the commit the file was generated from, so it is
    # one commit behind by construction once the file is committed. Diffing it made the
    # gate permanently red (t896); diffing the BODY keeps the signal that matters.
    current_body = body_of(current)
    if current_body == body():
        committed_stamp = title_revision(current)
        here = title_revision(text)
        if committed_stamp and committed_stamp != here:
            print(f"advisory: the manifest names {committed_stamp}; this tree is {here}. "
                  "Its CONTENT matches, so nothing is stale - regenerate when the "
                  "inventory next changes.")
        print("MANIFEST.md matches byte for byte (title block excluded)")
        return 0
    import difflib
    diff = list(difflib.unified_diff(current_body.split("\n"), body().split("\n"),
                                     "committed", "generated", lineterm="", n=1))
    print("\n".join(diff[:80]))
    print(f"\n{len(diff)} diff lines (showing at most 80)")
    return 1


if __name__ == "__main__":
    sys.exit(main())
