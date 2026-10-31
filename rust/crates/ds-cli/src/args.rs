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

fn build_arg(arg: &'static Arg, lenient: bool) -> clap::Arg {
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
        a = a.required(!lenient && matches!(arg.nargs, Nargs::One | Nargs::Plus));
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
        a = a.required(!lenient && arg.required);
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

fn build_cmd(cmd: &'static Cmd, root: bool, lenient: bool) -> Command {
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
    // argparse's own `--help` text is rendered by `render` (byte for byte), so clap's help
    // flag is off; `-h/--help` is an exclusive flag whose presence we turn into that text.
    command = command.disable_help_flag(true);
    if !cmd.raw {
        command = command.arg(
            clap::Arg::new("help")
                .short('h')
                .long("help")
                .action(ArgAction::SetTrue)
                .exclusive(true)
                .hide(true),
        );
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
        let built = build_arg(arg, lenient);
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
                .required(!lenient && group.required),
        );
    }
    if !cmd.subs.is_empty() {
        command = command.subcommand_required(!lenient && cmd.sub_required);
        if let Some(metavar) = cmd.sub_metavar {
            command = command.subcommand_value_name(metavar.trim_matches('<').trim_matches('>'));
        }
        for sub in cmd.subs {
            command = command.subcommand(build_cmd(sub, false, lenient));
        }
    }
    command
}

/// The whole `dunkingsheep` command tree (built once per process, then cloned).
pub fn build() -> Command {
    static TREE: std::sync::OnceLock<Command> = std::sync::OnceLock::new();
    TREE.get_or_init(|| build_cmd(&ROOT, true, false)).clone()
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

/// A parse that did not produce a namespace: argparse's help text (exit 0), or a clap usage
/// error (exit 2) / clap's version text (exit 0).
#[derive(Debug)]
pub enum ParseError {
    /// `-h/--help`: argparse's text for the command, to stdout, exit 0.
    Help(String),
    Clap(clap::Error),
}

impl ParseError {
    pub fn exit_code(&self) -> i32 {
        match self {
            ParseError::Help(_) => 0,
            ParseError::Clap(e) => e.exit_code(),
        }
    }

    /// Print exactly what the process should and exit.
    pub fn exit(self) -> ! {
        match self {
            ParseError::Help(text) => {
                print!("{text}");
                std::process::exit(0)
            }
            ParseError::Clap(e) => e.exit(),
        }
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Help(text) => write!(f, "{text}"),
            ParseError::Clap(e) => write!(f, "{e}"),
        }
    }
}

impl From<clap::Error> for ParseError {
    fn from(e: clap::Error) -> Self {
        ParseError::Clap(e)
    }
}

/// The command path whose `-h/--help` was given (argparse acts on the first one it meets).
fn help_path(
    cmd: &'static Cmd,
    matches: &ArgMatches,
    path: &mut Vec<&'static str>,
) -> Option<Vec<&'static str>> {
    if !cmd.raw && matches.try_get_one::<bool>("help").ok().flatten() == Some(&true) {
        return Some(path.clone());
    }
    let (name, sub_matches) = matches.subcommand()?;
    let sub = cmd.subs.iter().find(|s| s.name == name)?;
    path.push(sub.name);
    help_path(sub, sub_matches, path)
}

/// `shutil.get_terminal_size().columns`: `$COLUMNS`, else 80. LIMIT: Python also asks the
/// tty (`os.get_terminal_size`) before falling back; that ioctl is not ported.
fn terminal_columns() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|c| *c > 0)
        .unwrap_or(80)
}

/// `parser.parse_args(argv)` as a dictionary; `Err` carries clap's own exit code and text
/// (0 for `--help` / `--version`, 2 for a usage error, as argparse).
pub fn try_parse<I, T>(argv: I) -> Result<Map<String, Value>, ParseError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let argv: Vec<OsString> = argv.into_iter().map(Into::into).collect();
    let matches = match build().try_get_matches_from(&argv) {
        Ok(matches) => matches,
        Err(error) => {
            // argparse acts on `-h` the moment it meets it, before it checks that a
            // subcommand or a required argument is present; re-read with nothing required.
            static LENIENT: std::sync::OnceLock<Command> = std::sync::OnceLock::new();
            let lenient = LENIENT.get_or_init(|| build_cmd(&ROOT, true, true)).clone();
            return match lenient.try_get_matches_from(&argv) {
                Ok(m) => match help_path(&ROOT, &m, &mut Vec::new()) {
                    Some(path) => Err(ParseError::Help(
                        crate::render::help(&path, terminal_columns()).unwrap_or_default(),
                    )),
                    None => Err(ParseError::Clap(error)),
                },
                Err(_) => Err(ParseError::Clap(error)),
            };
        }
    };
    if let Some(path) = help_path(&ROOT, &matches, &mut Vec::new()) {
        let text = crate::render::help(&path, terminal_columns()).unwrap_or_default();
        return Err(ParseError::Help(text));
    }
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
