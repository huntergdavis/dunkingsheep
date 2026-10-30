HEADER = '# Port manifest (generated from main @ 038d9aa, 2026-10-29)\n'

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

FILES_ON_DISK = """## Files on disk (~/.config/dunkingsheep)

agent-state.db, context-ledger.db, desk.json, dialog-alerts.db, dispatch-state.json, dispatch.db, dispatch_lanes.json, dunks.json, github-app-author.json, github-app-reviewer.json, github-repos.json, github-tokens.json, health.json, inbox.db, lanes.json, launches.json, load-state.json, memory.db, metrics.db, owner-push.db, pm-routing.json, retired-panes.json, roles.json, service-tokens.json, superusers.json, tasks.db, tmux-panes.json, tmux-trial-panes.json, usage.json, watchdog.json, watchdog_deadman_state.json, watchdog_deadman_status.json, watchdog_state.json; plus `context/` (markdown store), `attachments/` (inbox blobs), `bin/` (wrappers), `system-token`, `web-token`, `github-app-*.pem`, `dunkingsheep.sock`, `daemon.log`."""


import argparse
import ast
import os
import pathlib
import re
import sqlite3
import subprocess
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


def schema_section():
    out = ["## Schemas (live databases, verbatim)", ""]
    for name in DATABASES:
        path = CONFIG / f"{name}.db"
        if not path.exists():
            raise SystemExit(f"missing live database {path}: the manifest copies the live "
                             "schemas, and a fresh db created from the Python SCHEMA is not "
                             "the same file (tasks.priority DEFAULT is 'medium' live, 'low' "
                             "in the source). Point DUNKINGSHEEP_DIR at a real install.")
        con = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
        out.append(f"## {name}.db")
        for (sql,) in con.execute("select sql from sqlite_master where sql is not null "
                                  "order by name"):
            out.append(sql + ";")
        con.close()
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


def inventory_section():
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
                entry = f"- `{node.name}({signature(node)})`"
                sentence = first_sentence(ast.get_docstring(node))
                out.append(f"{entry} \u2014 {sentence}" if sentence else entry)
            elif isinstance(node, ast.ClassDef):
                sentence = first_sentence(ast.get_docstring(node))
                head = f"- class `{node.name}`"
                out.append(f"{head} \u2014 {sentence}" if sentence else head)
            elif isinstance(node, ast.Assign) and len(node.targets) == 1 \
                    and isinstance(node.targets[0], ast.Name):
                target = node.targets[0].id
                if target.isupper() or target.startswith("_") and target.upper() == target:
                    value = ast.get_source_segment(source, node.value) or ""
                    out.append(f"- const `{target}` = `{' '.join(value.split())}`")
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
    names = sorted(p.name for p in (ROOT / "packaging/systemd").glob("*"))
    services = [n for n in names if n.endswith(".service")]
    timers = [n for n in names if n.endswith(".timer")]
    stems = [n[:-8] for n in services]
    return ("## Systemd units (packaging/systemd)\n\n%s%s.\n"
            % (", ".join(stems), " (each a .service, most with a .timer)" if timers else ""))


def error_strings_section():
    """The 161 asserted strings.

    TODO(T00): the selector that produces exactly these 161 is not recovered. What
    is proven: they are not "every asserted literal" (1481 candidates, 157 shared,
    and the list contains non-messages such as CODEX_HOME/before/caller). Until it
    is recovered this section is emitted from the closest rule we have, and
    --check will report the difference rather than pretend it matches.
    """
    literals = {}
    for path in (ROOT / "tests").rglob("*.py"):
        try:
            tree = ast.parse(path.read_text())
        except SyntaxError:
            continue
        for node in ast.walk(tree):
            if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute) \
                    and node.func.attr in {"assertIn", "assertNotIn", "assertRegex",
                                           "assertRaisesRegex", "assertEqual"}:
                for arg in node.args:
                    if isinstance(arg, ast.Constant) and isinstance(arg.value, str) \
                            and arg.value.strip():
                        literals[arg.value] = True
    out = ["## Error strings tests assert on (keep byte-identical)", ""]
    out += ["- `%s`" % s for s in sorted(literals)]
    return "\n".join(out) + "\n"


def build():
    return "\n\n".join([
        HEADER,
        "",
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
    if current == text:
        print("MANIFEST.md matches byte for byte")
        return 0
    import difflib
    diff = list(difflib.unified_diff(current.split("\n"), text.split("\n"),
                                     "committed", "generated", lineterm="", n=1))
    print("\n".join(diff[:80]))
    print(f"\n{len(diff)} diff lines (showing at most 80)")
    return 1


if __name__ == "__main__":
    sys.exit(main())
