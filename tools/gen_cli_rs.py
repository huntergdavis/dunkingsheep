#!/usr/bin/env python3
"""Generate rust/crates/ds-cli/src/cli_table.rs from the Python argparse tree (T42).

`dunkingsheep`'s argparse tree (`build_parser()`) is the source of truth until the cutover,
so the Rust table is GENERATED from it by introspection: a hand-typed copy of ~190
subparsers is the thing that drifts.

    python3 tools/gen_cli_rs.py --write    # rewrite cli_table.rs
    python3 tools/gen_cli_rs.py --check    # exit 1 when the table and the Python disagree
"""
import argparse
import importlib.machinery
import json
import importlib.util
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
OUT = ROOT / "rust/crates/ds-cli/src/cli_table.rs"
FIXTURE = ROOT / "rust/crates/ds-cli/tests/cli_namespaces.json"
HELP_FIXTURE = ROOT / "rust/crates/ds-cli/tests/cli_help.json"


def load_parser():
    sys.path.insert(0, str(ROOT))
    loader = importlib.machinery.SourceFileLoader("ds_cli_src", str(ROOT / "dunkingsheep"))
    spec = importlib.util.spec_from_loader("ds_cli_src", loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module.build_parser()


def s(text):
    """A Rust string literal."""
    out = ['"']
    for ch in text:
        if ch == "\\":
            out.append("\\\\")
        elif ch == '"':
            out.append('\\"')
        elif ch == "\n":
            out.append("\\n")
        elif ch == "\r":
            out.append("\\r")
        elif ch == "\t":
            out.append("\\t")
        elif ord(ch) < 32 or ord(ch) == 127:
            out.append("\\u{%x}" % ord(ch))
        else:
            out.append(ch)
    out.append('"')
    return "".join(out)


def opt(value):
    return "None" if value is None else "Some(%s)" % s(value)


def slist(values):
    return "&[%s]" % ", ".join(s(str(v)) for v in values)


def kind_of(action):
    name = type(action).__name__
    return {
        "_StoreAction": "Kind::Store",
        "_StoreTrueAction": "Kind::StoreTrue",
        "_StoreFalseAction": "Kind::StoreFalse",
        "_AppendAction": "Kind::Append",
        "_VersionAction": "Kind::Version",
    }[name]


def ty_of(action):
    t = getattr(action.type, "__name__", None)
    return {None: "Ty::Str", "int": "Ty::Int", "float": "Ty::Float"}[t]


def nargs_of(action):
    return {None: "Nargs::One", "?": "Nargs::Opt", "+": "Nargs::Plus", "*": "Nargs::Star",
            0: "Nargs::Zero"}[action.nargs]


def default_of(action):
    """The default as text, only for a Store/Append that has one (None = no default)."""
    d = action.default
    if d is None or d is argparse.SUPPRESS:
        return None
    if isinstance(d, bool):
        return None  # store_true/false defaults are implied by Kind
    return str(d)


def default_is_int(action):
    """argparse applies `type` only to STRING defaults: a float arg with `default=10`
    reports the int 10, not 10.0 (send --wait, metrics --window)."""
    d = action.default
    return isinstance(d, int) and not isinstance(d, bool) and getattr(action.type, "__name__", None) == "float"


def help_of(action):
    if action.help is None or action.help is argparse.SUPPRESS:
        return None
    return action.help.replace("%%", "%")


def emit_arg(a, id_, tri_inherited):
    positional = not a.option_strings
    flags = list(a.option_strings)
    for f in flags:
        assert f.startswith("--") or (len(f) == 2 and f[0] == "-"), f
    version = None
    if type(a).__name__ == "_VersionAction":
        version = a.version
    # store_true/false whose default is None (not a bool): tri-state, absent means None.
    tri = tri_inherited or (type(a).__name__ in ("_StoreTrueAction", "_StoreFalseAction")
           and a.default is None)
    return ("Arg { id: %s, dest: %s, tri: %s, default_int: %s, flags: %s, positional: %s, kind: %s, ty: %s, nargs: %s, "
            "choices: %s, default: %s, required: %s, help: %s, hidden: %s, metavar: %s, "
            "version: %s }" % (
                s(id_), s(a.dest), str(tri).lower(), str(default_is_int(a)).lower(), slist(flags), str(positional).lower(), kind_of(a), ty_of(a),
                nargs_of(a), slist(a.choices or []), opt(default_of(a)),
                str(bool(a.required)).lower(), opt(help_of(a)),
                str(a.help is argparse.SUPPRESS).lower(),
                opt(a.metavar if isinstance(a.metavar, str) else None), opt(version)))


def emit_cmd(name, parser, aliases, help_text, indent):
    pad = "    " * indent
    args = []
    seen_ids = set()
    tri_by_dest = {}
    subs = []
    sub_dest = sub_metavar = None
    sub_required = False
    sub_at = None
    for a in parser._actions:
        if isinstance(a, argparse._HelpAction):
            continue
        if isinstance(a, argparse._SubParsersAction):
            sub_at = len(args)
            sub_dest, sub_metavar, sub_required = a.dest, a.metavar, bool(a.required)
            by_parser = {}
            order = []
            for sub_name, sp in a.choices.items():
                if id(sp) not in by_parser:
                    by_parser[id(sp)] = (sp, [])
                    order.append(id(sp))
                by_parser[id(sp)][1].append(sub_name)
            helps = {ca.dest: ca.help for ca in a._choices_actions}
            for key in order:
                sp, names = by_parser[key]
                subs.append(emit_cmd(names[0], sp, names[1:], helps.get(names[0]), indent + 2))
            continue
        # argparse lets two flags share one dest (`--multi` / `--no-multi`); clap needs
        # unique ids, so the second gets a suffix and `dest` keeps the namespace key.
        id_, n = a.dest, 1
        while id_ in seen_ids:
            n += 1
            id_ = "%s#%d" % (a.dest, n)
        seen_ids.add(id_)
        # argparse takes the dest's default from the FIRST action that names it.
        tri_first = tri_by_dest.get(a.dest, False)
        args.append(emit_arg(a, id_, tri_first))
        if a.dest not in tri_by_dest:
            tri_by_dest[a.dest] = (type(a).__name__ in ("_StoreTrueAction", "_StoreFalseAction")
                                   and a.default is None)
    groups = []
    for g in parser._mutually_exclusive_groups:
        groups.append("Group { members: %s, required: %s }" % (
            slist([a.dest for a in g._group_actions]), str(bool(g.required)).lower()))
    lines = [pad + "Cmd {"]
    lines.append(pad + "    name: %s, aliases: %s," % (s(name), slist(aliases)))
    lines.append(pad + "    help: %s," % opt(help_text))
    # prefix_chars="\x00" (herdr): every token, flags included, is a positional.
    lines.append(pad + "    raw: %s," % str(parser.prefix_chars == "\x00").lower())
    lines.append(pad + "    raw_desc: %s," % str(
        issubclass(parser.formatter_class, argparse.RawDescriptionHelpFormatter)).lower())
    lines.append(pad + "    sub_at: %s," % ("None" if sub_at is None else "Some(%d)" % sub_at))
    lines.append(pad + "    about: %s," % opt(parser.description))
    lines.append(pad + "    epilog: %s," % opt(parser.epilog))
    lines.append(pad + "    args: &[")
    lines.extend(pad + "        " + x + "," for x in args)
    lines.append(pad + "    ],")
    lines.append(pad + "    groups: &[%s]," % ", ".join(groups))
    lines.append(pad + "    sub_dest: %s, sub_metavar: %s, sub_required: %s," % (
        opt(sub_dest), opt(sub_metavar), str(sub_required).lower()))
    lines.append(pad + "    subs: &[")
    lines.extend(x + "," for x in subs)
    lines.append(pad + "    ],")
    lines.append(pad + "}")
    return "\n".join(lines)


HEADER = '''//! The `dunkingsheep` argparse tree as data (T42).
//!
//! GENERATED by `tools/gen_cli_rs.py` from `dunkingsheep::build_parser()` - do not edit by
//! hand. `--check` fails when this table and the Python parser disagree.

use crate::spec::{Arg, Cmd, Group, Kind, Nargs, Ty};

#[rustfmt::skip]
pub static ROOT: Cmd = '''


def render():
    parser = load_parser()
    return HEADER + emit_cmd("dunkingsheep", parser, [], None, 0).lstrip() + ";\n"


def sample_value(a):
    if a.choices:
        return str(list(a.choices)[0])
    t = getattr(a.type, "__name__", None)
    return {"int": "1", "float": "1.5"}.get(t, "x")


def leaves(parser, path):
    """Every (argv-path, parser) that has no subparsers of its own (aliases skipped)."""
    subs = [a for a in parser._actions if isinstance(a, argparse._SubParsersAction)]
    if not subs:
        yield path, parser
        return
    seen = set()
    for name, sp in subs[0].choices.items():
        if id(sp) in seen:
            continue
        seen.add(id(sp))
        yield from leaves(sp, path + [name])


def namespace_cases():
    """argv -> the Python's parsed Namespace, as JSON: the differential fixture."""
    parser = load_parser()
    cases = []

    def run(argv):
        try:
            ns = vars(parser.parse_args(argv))
        except SystemExit:
            return
        cases.append({"argv": argv, "namespace": ns})

    for path, sp in leaves(parser, []):
        base = list(path)
        flags = []
        for a in sp._actions:
            if isinstance(a, (argparse._HelpAction, argparse._SubParsersAction)):
                continue
            if not a.option_strings:
                if a.nargs in (None, "+"):
                    base.append(sample_value(a))
            elif a.required:
                flags += [a.option_strings[0]] + ([] if a.nargs == 0 else [sample_value(a)])
        base += flags
        run(base)
        run(base + ["--json"])
        run(["--json"] + base)
        for a in sp._actions:
            if isinstance(a, (argparse._HelpAction, argparse._SubParsersAction)) or not a.option_strings:
                continue
            if a.required or a.dest == "json":
                continue
            extra = [a.option_strings[0]]
            if type(a).__name__ in ("_StoreAction", "_AppendAction"):
                extra.append(sample_value(a))
            run(base + extra)
    return json.dumps(cases, indent=1, sort_keys=True, default=str) + "\n"


def help_cases():
    """Every command path -> the Python's `--help` text at COLUMNS=80 (no colour)."""
    import os
    os.environ["COLUMNS"] = "80"
    os.environ["NO_COLOR"] = "1"
    os.environ.pop("FORCE_COLOR", None)
    parser = load_parser()
    cases = []

    def walk(p, path):
        cases.append({"path": path, "help": p.format_help()})
        for a in p._actions:
            if isinstance(a, argparse._SubParsersAction):
                seen = set()
                for name, sp in a.choices.items():
                    if id(sp) in seen:
                        continue
                    seen.add(id(sp))
                    walk(sp, path + [name])

    walk(parser, [])
    return json.dumps(cases, indent=1, sort_keys=True) + "\n"


def main():
    ap = argparse.ArgumentParser()
    mode = ap.add_mutually_exclusive_group(required=True)
    mode.add_argument("--write", action="store_true")
    mode.add_argument("--check", action="store_true")
    ns = ap.parse_args()
    text = render()
    if ns.write:
        OUT.parent.mkdir(parents=True, exist_ok=True)
        OUT.write_text(text)
        FIXTURE.parent.mkdir(parents=True, exist_ok=True)
        FIXTURE.write_text(namespace_cases())
        HELP_FIXTURE.write_text(help_cases())
        print("wrote", OUT.relative_to(ROOT), "and both fixtures")
        return 0
    current = OUT.read_text() if OUT.exists() else ""
    fixture = FIXTURE.read_text() if FIXTURE.exists() else ""
    helps = HELP_FIXTURE.read_text() if HELP_FIXTURE.exists() else ""
    if current != text or fixture != namespace_cases() or helps != help_cases():
        print("cli_table.rs is out of date; run tools/gen_cli_rs.py --write")
        return 1
    print("cli_table.rs matches the Python parser")
    return 0


if __name__ == "__main__":
    sys.exit(main())
