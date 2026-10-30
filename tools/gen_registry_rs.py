#!/usr/bin/env python3
"""Generate rust/crates/ds-core/src/registry.rs + the expected-schema fixture (T04).

The registry is the single source of truth in Python (`dunk_api.COMMANDS`), so the
Rust table is GENERATED from it rather than typed by hand: a hand-typed 124-row
table is the thing that drifts, and T04's acceptance is "every MANIFEST command
present with identical params" plus "input_schema() equals the Python's".

    python3 tools/gen_registry_rs.py --write    # rewrite registry.rs + the fixture
    python3 tools/gen_registry_rs.py --check    # exit 1 and print the difference
"""
import argparse
import json
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
REGISTRY = ROOT / "rust/crates/ds-core/src/registry.rs"
FIXTURE = ROOT / "rust/crates/ds-core/tests/registry_expected.json"


def _close(text, i, opening, closing):
    depth = 0
    for k in range(i, len(text)):
        if text[k] == opening:
            depth += 1
        elif text[k] == closing:
            depth -= 1
            if depth == 0:
                return k
    raise SystemExit("unbalanced literal")


def load_python_registry():
    """The Command table, without importing the daemon (its handlers need a Flock)."""
    src = (ROOT / "dunk_api.py").read_text()
    i = src.index("COMMANDS = [")
    expr = src[i + len("COMMANDS = "):_close(src, i, "[", "]") + 1]

    class FakeCommand:
        def __init__(self, name, description, handler=None, params=None, required=(),
                     mcp=True, mcp_required=None):
            self.name = name
            self.description = description
            self.params = params or {}
            self.required = list(required)
            self.mcp_required = (list(mcp_required) if mcp_required is not None
                                 else self.required)
            self.mcp = mcp

        def input_schema(self):
            return {
                "type": "object",
                "properties": {k: dict(v) for k, v in self.params.items()},
                "required": list(self.mcp_required),
                "additionalProperties": False,
            }

    # The literal references module-level constants (TARGET_PARAM and friends).
    # Bind the ones that are plain data by literal_eval'ing their assignments, so the
    # registry can be read WITHOUT importing the daemon (whose imports need a Flock).
    import ast

    sys.path.insert(0, str(ROOT))     # so `from dunk_tasks import ...` resolves
    namespace = {"Command": FakeCommand}
    builtins = {"list": list, "dict": dict, "tuple": tuple, "len": len, "str": str,
                "int": int, "sorted": sorted}
    for node in ast.parse(src).body:
        if not isinstance(node, ast.ImportFrom) or not node.module:
            continue
        try:
            module = __import__(node.module, fromlist=["*"])
        except Exception:  # noqa: BLE001 - a heavy import is not needed for data
            continue
        for alias in node.names:
            target = alias.asname or alias.name
            if target.isupper() and hasattr(module, alias.name) and target not in namespace:
                namespace[target] = getattr(module, alias.name)

    assignments = [node for node in ast.parse(src).body
                   if isinstance(node, ast.Assign) and len(node.targets) == 1
                   and isinstance(node.targets[0], ast.Name)
                   and node.targets[0].id.isupper()]
    # Fixpoint: a constant may be built from another one (TASK_STATUS_PARAM needs
    # TASK_STATUSES), so keep resolving until nothing new binds.
    for _ in range(6):
        progressed = False
        for node in assignments:
            name = node.targets[0].id
            if name in namespace:
                continue
            try:
                expression = ast.Expression(body=node.value)
                ast.fix_missing_locations(expression)
                namespace[name] = eval(compile(expression, "<registry>", "eval"),
                                       {"__builtins__": builtins}, namespace)
                progressed = True
            except (NameError, ValueError, SyntaxError):
                continue    # a call into the daemon: the registry does not need it
        if not progressed:
            break

    # Some constants are imported from sibling modules (TASK_STATUSES comes from
    # dunk_tasks). Those two import only the standard library, so importing them is
    # cheap and honest; anything heavier would be a reason to stop, not to guess.
    sys.path.insert(0, str(ROOT))
    for module_name in ("dunk_tasks", "dunk_roles"):
        try:
            module = __import__(module_name)
        except Exception as error:  # noqa: BLE001 - report, never guess
            raise SystemExit(f"cannot import {module_name} to resolve registry "
                             f"constants ({error}); the generator reads the Python "
                             "tables rather than a copy of them")
        for key in dir(module):
            if key.isupper() and key not in namespace:
                namespace[key] = getattr(module, key)

    class Fallback(dict):
        """Handlers are opaque to us: they become no-op callables."""

        def __missing__(self, key):
            return lambda *a, **k: None

    unresolved_constants = sorted({name.id for name in ast.walk(ast.parse(expr))
                                   if isinstance(name, ast.Name) and name.id not in namespace
                                   and not isinstance(name.ctx, ast.Store)
                                   and not name.id.startswith("h_")})
    real_missing = [name for name in unresolved_constants
                    if name.isupper() or name.endswith("_PARAM")]
    if real_missing:
        raise SystemExit("the registry literal names data constants this generator did "
                         "not bind: " + ", ".join(real_missing))
    commands = eval(expr, Fallback(namespace))  # noqa: S307 - our own literal
    j = src.index("SUPERUSER_ONLY = {")
    superuser_only = eval(src[j + len("SUPERUSER_ONLY = "):
                              _close(src, src.index("{", j), "{", "}") + 1])  # noqa: S307
    k = src.index("MCP_GROUPS = (")
    groups = eval(src[k + len("MCP_GROUPS = "):_close(src, k, "(", ")") + 1])  # noqa: S307
    roles_ns = {"__name__": "dunk_roles_registry"}
    exec(compile((ROOT / "dunk_roles.py").read_text(), "dunk_roles.py", "exec"),
         roles_ns)  # noqa: S102 - reading the allowlist tables
    return commands, superuser_only, groups, roles_ns


def build():
    commands, superuser_only, groups, roles_ns = load_python_registry()
    roles, allow, web_denied = roles_ns["ROLES"], roles_ns["ALLOW"], roles_ns["WEB_DENIED"]

    def permitted(role, name):
        if role == "web" and name in web_denied:
            return False
        table = allow.get(role)
        return True if table is None else name in table

    expected = {}
    lines = [
        "//! The command registry: one table the socket, the CLI and MCP all dispatch",
        "//! through (RUST_PORT.md 4.2).",
        "//!",
        "//! GENERATED by `tools/gen_registry_rs.py` from `dunk_api.COMMANDS` - do not",
        "//! edit by hand: a hand-typed copy of this table is exactly the third",
        "//! hand-maintained surface the port exists to delete. `--check` fails when the",
        "//! table and the Python disagree.",
        "",
        "use std::collections::BTreeMap;",
        "",
        "use serde_json::{json, Map, Value};",
        "",
        "/// One parameter: its name and the JSON schema `dunk_api` publishes for it.",
        "#[derive(Debug, Clone, Copy)]",
        "pub struct Param {",
        "    pub name: &'static str,",
        "    pub schema: &'static str,   // compact JSON, exactly the Python's param dict",
        "}",
        "",
        "/// One command: everything the three surfaces need to describe it.",
        "#[derive(Debug, Clone, Copy)]",
        "pub struct Command {",
        "    pub name: &'static str,",
        "    pub description: &'static str,",
        "    pub params: &'static [Param],",
        "    pub required: &'static [&'static str],",
        "    pub mcp: bool,",
        "    pub owner_only: bool,",
        "    pub roles: &'static [&'static str],",
        "}",
        "",
        "/// One MCP group: an `action` enum plus the union of its commands' params.",
        "#[derive(Debug, Clone, Copy)]",
        "pub struct Group {",
        "    pub name: &'static str,",
        "    pub description: &'static str,",
        "    pub actions: &'static [(&'static str, &'static str)],",
        "}",
        "",
        f"/// Every command the daemon serves ({len(commands)} at generation time).",
        f"pub static COMMANDS: &[Command] = &[",
    ]
    for c in sorted(commands, key=lambda c: c.name):
        params = ", ".join(
            "Param { name: %s, schema: %s }"
            % (json.dumps(k), json.dumps(json.dumps(v, separators=(",", ":")), ensure_ascii=False))
            for k, v in c.params.items())
        allowed = [r for r in roles if permitted(r, c.name)]
        lines.append("    Command {")
        lines.append(f"        name: {json.dumps(c.name)},")
        lines.append(f"        description: {json.dumps(' '.join(c.description.split()))},")
        lines.append(f"        params: &[{params}],")
        lines.append("        required: &[%s]," % ", ".join(json.dumps(r) for r in c.mcp_required))
        lines.append(f"        mcp: {'true' if c.mcp else 'false'},")
        lines.append(f"        owner_only: {'true' if c.name in superuser_only else 'false'},")
        lines.append("        roles: &[%s]," % ", ".join(json.dumps(r) for r in allowed))
        lines.append("    },")
        expected[c.name] = c.input_schema()
    lines += [
        "];",
        "",
        "/// The MCP groups, in the order `dunk_api.MCP_GROUPS` lists them.",
        "pub static GROUPS: &[Group] = &[",
    ]
    for name, description, actions in groups:
        lines.append("    Group {")
        lines.append(f"        name: {json.dumps(name)},")
        lines.append(f"        description: {json.dumps(' '.join(description.split()))},")
        lines.append("        actions: &[%s],"
                     % ", ".join('(%s, %s)' % (json.dumps(a), json.dumps(b))
                                 for a, b in actions))
        lines.append("    },")
    lines += [
        "];",
        "",
        "/// The command with this name, if it exists.",
        "pub fn command(name: &str) -> Option<&'static Command> {",
        "    COMMANDS.iter().find(|command| command.name == name)",
        "}",
        "",
        "/// `dunk_api.Command.input_schema()`: the MCP tool schema for one command.",
        "///",
        "/// Byte-for-byte the Python's shape - type object, the params in table order,",
        "/// `required` from mcp_required, `additionalProperties: false` - because agents",
        "/// and the differential harness compare it directly.",
        "pub fn input_schema(name: &str) -> Option<Value> {",
        "    let command = command(name)?;",
        "    let mut properties = Map::new();",
        "    for param in command.params {",
        "        let schema: Value = serde_json::from_str(param.schema)",
        "            .expect(\"generated param schema is valid JSON\");",
        "        properties.insert(param.name.to_string(), schema);",
        "    }",
        "    let mut schema = Map::new();",
        "    schema.insert(\"type\".into(), Value::from(\"object\"));",
        "    schema.insert(\"properties\".into(), Value::Object(properties));",
        "    schema.insert(\"required\".into(),",
        "                  Value::Array(command.required.iter()",
        "                      .map(|name| Value::from(*name)).collect()));",
        "    schema.insert(\"additionalProperties\".into(), Value::Bool(false));",
        "    Some(Value::Object(schema))",
        "}",
        "",
        "/// `dunk_api.GroupedTool.input_schema()`: one `action` enum over a group.",
        "///",
        "/// Per-action required params are validated before dispatch, so the grouped",
        "/// schema advertises only `action` as required (the Python says the same).",
        "pub fn grouped_input_schema(name: &str) -> Option<Value> {",
        "    let group = GROUPS.iter().find(|group| group.name == name)?;",
        "    let mut properties = Map::new();",
        "    properties.insert(\"action\".to_string(), json!({",
        "        \"type\": \"string\",",
        "        \"enum\": group.actions.iter().map(|(action, _)| *action).collect::<Vec<_>>(),",
        "        \"description\": \"Which operation to run.\",",
        "    }));",
        "    for (action, command_name) in group.actions {",
        "        let _ = action;",
        "        if let Some(command) = command(command_name) {",
        "            for param in command.params {",
        "                if properties.contains_key(param.name) {",
        "                    continue;",
        "                }",
        "                let schema: Value = serde_json::from_str(param.schema)",
        "                    .expect(\"generated param schema is valid JSON\");",
        "                properties.insert(param.name.to_string(), schema);",
        "            }",
        "        }",
        "    }",
        "    let mut schema = Map::new();",
        "    schema.insert(\"type\".into(), Value::from(\"object\"));",
        "    schema.insert(\"properties\".into(), Value::Object(properties));",
        "    schema.insert(\"required\".into(), json!([\"action\"]));",
        "    schema.insert(\"additionalProperties\".into(), Value::Bool(false));",
        "    Some(Value::Object(schema))",
        "}",
        "",
        "/// The group of `action -> command` pairs, for the CLI's `mcp-map`.",
        "pub fn group_map() -> BTreeMap<&'static str, &'static [(&'static str, &'static str)]> {",
        "    GROUPS.iter().map(|group| (group.name, group.actions)).collect()",
        "}",
        "",
    ]
    fixture = {
        "commands": expected,
        "groups": {name: {"actions": {a: b for a, b in actions},
                          "description": " ".join(description.split())}
                   for name, description, actions in groups},
    }
    return "\n".join(lines), json.dumps(fixture, indent=1, sort_keys=True) + "\n"


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--write", action="store_true")
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args(argv)
    rust, fixture = build()
    if args.write:
        REGISTRY.parent.mkdir(parents=True, exist_ok=True)
        REGISTRY.write_text(rust)
        FIXTURE.parent.mkdir(parents=True, exist_ok=True)
        FIXTURE.write_text(fixture)
        print(f"wrote {REGISTRY.relative_to(ROOT)} and {FIXTURE.relative_to(ROOT)}")
        return 0
    current = REGISTRY.read_text() if REGISTRY.exists() else ""
    current_fixture = FIXTURE.read_text() if FIXTURE.exists() else ""
    if current == rust and current_fixture == fixture:
        print("registry.rs and the fixture match the Python registry")
        return 0
    for label, now, want in (("registry.rs", current, rust),
                             ("fixture", current_fixture, fixture)):
        if now != want:
            import difflib
            diff = list(difflib.unified_diff(now.split("\n"), want.split("\n"),
                                             f"{label} (current)", f"{label} (python)",
                                             lineterm="", n=1))
            print("\n".join(diff[:40]))
    return 1


if __name__ == "__main__":
    sys.exit(main())
