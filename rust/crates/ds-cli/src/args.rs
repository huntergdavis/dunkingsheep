//! A clap `Command` built from the generated table, and the argparse `Namespace` shape
//! (`dest -> value`) read back from the matches, so every later command body can consume
//! the same dictionary the Python CLI's `args` object carries.
//!
//! The builder API, not derive: the tree is data (`cli_table::ROOT`), generated from
//! `build_parser()`; there are no per-command Rust types to derive from.

use std::ffi::OsString;

use clap::builder::PossibleValuesParser;
use clap::{value_parser, ArgAction, ArgGroup, ArgMatches, Command};
use serde_json::{json, Map, Value};

use crate::cli_table::ROOT;
use crate::spec::{Arg, Cmd, Kind, Nargs, Ty};

fn build_arg(arg: &'static Arg) -> clap::Arg {
    let mut a = clap::Arg::new(arg.id);
    if let Some(help) = arg.help {
        a = a.help(help);
    }
    if arg.hidden {
        a = a.hide(true);
    }
    if let Some(metavar) = arg.metavar {
        a = a.value_name(metavar);
    }
    if arg.positional {
        a = a.required(matches!(arg.nargs, Nargs::One | Nargs::Plus));
    } else {
        let mut long_seen = false;
        for flag in arg.flags {
            if let Some(long) = flag.strip_prefix("--") {
                if long_seen {
                    a = a.visible_alias(long);
                } else {
                    a = a.long(long);
                    long_seen = true;
                }
            } else if let Some(short) = flag.strip_prefix('-').and_then(|s| s.chars().next()) {
                a = a.short(short);
            }
        }
        a = a.required(arg.required);
    }
    match arg.kind {
        Kind::StoreTrue => return a.action(ArgAction::SetTrue),
        Kind::StoreFalse => return a.action(ArgAction::SetFalse),
        Kind::Store => a = a.action(ArgAction::Set),
        Kind::Append => a = a.action(ArgAction::Append),
        Kind::Version => unreachable!("the version flag is set on the Command"),
    }
    a = match arg.nargs {
        Nargs::Opt => a.num_args(0..=1),
        Nargs::Plus => a.num_args(1..),
        Nargs::Star => a.num_args(0..),
        Nargs::One | Nargs::Zero => a,
    };
    a = match arg.ty {
        Ty::Int => a.value_parser(value_parser!(i64)),
        Ty::Float => a.value_parser(value_parser!(f64)),
        Ty::Str if !arg.choices.is_empty() => {
            a.value_parser(PossibleValuesParser::new(arg.choices.iter().copied()))
        }
        Ty::Str => a.value_parser(value_parser!(String)),
    };
    if let Some(default) = arg.default {
        a = a.default_value(default);
    }
    a
}

fn build_cmd(cmd: &'static Cmd, root: bool) -> Command {
    let mut command = Command::new(cmd.name)
        .infer_long_args(true)
        .disable_help_subcommand(true)
        .disable_version_flag(true);
    if !cmd.aliases.is_empty() {
        command = command.visible_aliases(cmd.aliases.iter().copied());
    }
    if let Some(about) = cmd.about.or(cmd.help) {
        command = command.about(about);
    }
    if let Some(epilog) = cmd.epilog {
        command = command.after_help(epilog);
    }
    if cmd.raw {
        command = command.disable_help_flag(true);
    }
    for arg in cmd.args {
        if cmd.raw && arg.dest == "json" {
            continue;
        }
        if arg.kind == Kind::Version {
            if root {
                // argparse prints "dunkingsheep X (code Y)"; clap prints "<name> <version>".
                let text = arg.version.unwrap_or("");
                let text = text.strip_prefix("dunkingsheep ").unwrap_or(text);
                command = command.version(text).disable_version_flag(false);
            }
            continue;
        }
        let built = build_arg(arg);
        command = command.arg(if cmd.raw {
            built.allow_hyphen_values(true).trailing_var_arg(true)
        } else {
            built
        });
    }
    for (index, group) in cmd.groups.iter().enumerate() {
        command = command.group(
            // Ids are `&'static str`; the tree is built once per process, so leaking is fine.
            ArgGroup::new(&*Box::leak(format!("_exclusive{index}").into_boxed_str()))
                .args(group.members.iter().copied())
                .multiple(false)
                .required(group.required),
        );
    }
    if !cmd.subs.is_empty() {
        command = command.subcommand_required(cmd.sub_required);
        if let Some(metavar) = cmd.sub_metavar {
            command = command.subcommand_value_name(metavar.trim_matches('<').trim_matches('>'));
        }
        for sub in cmd.subs {
            command = command.subcommand(build_cmd(sub, false));
        }
    }
    command
}

/// The whole `dunkingsheep` command tree (built once per process, then cloned).
pub fn build() -> Command {
    static TREE: std::sync::OnceLock<Command> = std::sync::OnceLock::new();
    TREE.get_or_init(|| build_cmd(&ROOT, true)).clone()
}

fn value_of(arg: &Arg, matches: &ArgMatches) -> Value {
    match arg.kind {
        Kind::StoreTrue | Kind::StoreFalse => json!(matches.get_flag(arg.id)),
        Kind::Version => Value::Null,
        Kind::Store | Kind::Append => {
            let many = arg.kind == Kind::Append || matches!(arg.nargs, Nargs::Plus | Nargs::Star);
            macro_rules! read {
                ($t:ty, $wrap:expr) => {
                    if many {
                        match matches.get_many::<$t>(arg.id) {
                            Some(values) => Value::Array(values.map(|v| $wrap(v)).collect()),
                            None if arg.positional => json!([]),
                            None => Value::Null,
                        }
                    } else {
                        matches
                            .get_one::<$t>(arg.id)
                            .map_or(Value::Null, |v| $wrap(v))
                    }
                };
            }
            match arg.ty {
                Ty::Str => read!(String, |v: &String| json!(v)),
                Ty::Int => read!(i64, |v: &i64| json!(v)),
                Ty::Float
                    if arg.default_int
                        && matches.value_source(arg.id)
                            == Some(clap::parser::ValueSource::DefaultValue) =>
                {
                    json!(arg.default.and_then(|d| d.parse::<i64>().ok()))
                }
                Ty::Float => read!(f64, |v: &f64| json!(v)),
            }
        }
    }
}

fn fill(cmd: &'static Cmd, matches: &ArgMatches, ns: &mut Map<String, Value>, depth: usize) {
    for arg in cmd.args {
        if arg.kind == Kind::Version {
            continue;
        }
        if arg.tri {
            if matches.value_source(arg.id) == Some(clap::parser::ValueSource::CommandLine) {
                ns.insert(arg.dest.to_string(), value_of(arg, matches));
            } else {
                ns.entry(arg.dest.to_string()).or_insert(Value::Null);
            }
            continue;
        }
        if cmd.raw && arg.dest == "json" {
            continue;
        }
        let value = value_of(arg, matches);
        // A nested parser's `--json` has `default=SUPPRESS`: absent unless given, and it
        // must not overwrite a `--json` given before the subcommand.
        if depth > 0 && arg.dest == "json" && value == json!(false) {
            continue;
        }
        ns.insert(arg.dest.to_string(), value);
    }
    if let Some((name, sub_matches)) = matches.subcommand() {
        if let Some(sub) = cmd.subs.iter().find(|s| s.name == name) {
            if let Some(dest) = cmd.sub_dest {
                ns.insert(dest.to_string(), json!(name));
            }
            fill(sub, sub_matches, ns, depth + 1);
        }
    }
}

/// `parser.parse_args(argv)` as a dictionary; `Err` carries clap's own exit code and text
/// (0 for `--help` / `--version`, 2 for a usage error, as argparse).
pub fn try_parse<I, T>(argv: I) -> Result<Map<String, Value>, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let matches = build().try_get_matches_from(argv)?;
    let mut ns = Map::new();
    fill(&ROOT, &matches, &mut ns, 0);
    Ok(ns)
}

/// Parse, printing help/usage and exiting exactly as clap does (0 or 2).
pub fn parse_or_exit<I, T>(argv: I) -> Map<String, Value>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    match try_parse(argv) {
        Ok(ns) => ns,
        Err(error) => error.exit(),
    }
}
