//! `--help` text byte-identical to Python 3.14 `argparse.HelpFormatter` (t1220).
//!
//! clap's own help is not argparse's, and `insta`/`assert_cmd` are not in the offline
//! cargo cache, so this is a port of the formatter itself, driven by the generated
//! `cli_table` and checked against `tests/help/*.txt` (written by
//! `tools/gen_cli_rs.py --write` from `format_help()` at `COLUMNS=80`).
//!
//! Ported: `_format_usage` (wrapping, mutually exclusive groups), `_format_action`,
//! `_format_action_invocation`, `_format_args`, `_format_text`, the section layout and
//! `textwrap.wrap`/`fill` (including its split-after-hyphen rule and long-word breaking).
//! NOT ported: colour (`FORCE_COLOR`/tty themes), `usage=` overrides and `%(default)s`
//! help expansion (no parser in the table uses either), and terminal-size detection
//! beyond `$COLUMNS` (Python also asks the tty; here: `$COLUMNS`, else 80).

use crate::spec::{Arg, Cmd, Kind, Nargs};

const HELP_TEXT: &str = "show this help message and exit";

/// What `shutil.get_terminal_size().columns` gives when stdout is not a terminal.
pub fn columns() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(80)
}

/// One argparse action as the formatter sees it.
struct Action<'a> {
    options: Vec<&'a str>,
    dest: &'a str,
    /// `None` is argparse's default (exactly one value).
    nargs: Option<char>,
    /// `store_true`, `store_false`, `version`: `nargs=0`.
    zero: bool,
    metavar: Option<String>,
    choices: &'a [&'a str],
    help: Option<String>,
    required: bool,
    /// The `<command>` positional of `add_subparsers`.
    parser: bool,
}

fn action_of(arg: &Arg) -> Action<'_> {
    let zero = matches!(arg.kind, Kind::StoreTrue | Kind::StoreFalse | Kind::Version);
    Action {
        options: arg.flags.to_vec(),
        dest: arg.dest,
        nargs: match arg.nargs {
            Nargs::Opt => Some('?'),
            Nargs::Plus => Some('+'),
            Nargs::Star => Some('*'),
            Nargs::One | Nargs::Zero => None,
        },
        zero,
        metavar: arg.metavar.map(str::to_string),
        choices: arg.choices,
        help: arg.help.map(str::to_string),
        required: arg.required,
        parser: false,
    }
}

impl Action<'_> {
    fn positional(&self) -> bool {
        self.options.is_empty()
    }

    /// `_metavar_formatter(action, default)(1)`.
    fn metavar(&self, default: &str) -> String {
        if let Some(m) = &self.metavar {
            m.clone()
        } else if !self.choices.is_empty() {
            format!("{{{}}}", self.choices.join(","))
        } else {
            default.to_string()
        }
    }

    /// `_format_args`.
    fn format_args(&self, default: &str) -> String {
        let m = self.metavar(default);
        if self.parser {
            return format!("{m} ...");
        }
        match self.nargs {
            None => m,
            Some('?') => format!("[{m}]"),
            Some('*') => format!("[{m} ...]"),
            Some(_) => format!("{m} [{m} ...]"),
        }
    }

    fn default_metavar(&self) -> String {
        if self.positional() {
            self.dest.to_string()
        } else {
            self.dest.to_uppercase()
        }
    }

    /// `_format_action_invocation`.
    fn invocation(&self) -> String {
        if self.positional() {
            return self.metavar(self.dest);
        }
        if self.zero {
            return self.options.join(", ");
        }
        let args = self.format_args(&self.default_metavar());
        format!("{} {}", self.options.join(", "), args)
    }
}

/// A listed sub-command (`_ChoicesPseudoAction`): `name (alias, ...)` and its help.
struct Choice<'a> {
    label: String,
    help: Option<&'a str>,
}

fn clen(text: &str) -> usize {
    text.chars().count()
}

// ---- textwrap -------------------------------------------------------------------------

fn is_ws(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c')
}

fn is_letter(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_word_punct(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '!' | '"' | '\'' | '&' | '.' | ',' | '?')
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `TextWrapper._split`: `wordsep_re` as a hand-written scanner.
fn split_chunks(text: &str) -> Vec<String> {
    let s: Vec<char> = text.chars().collect();
    let n = s.len();
    let mut chunks = Vec::new();
    let mut i = 0;
    while i < n {
        if is_ws(s[i]) {
            let start = i;
            while i < n && is_ws(s[i]) {
                i += 1;
            }
            chunks.push(s[start..i].iter().collect());
            continue;
        }
        // em-dash between words: (?<=wp) -{2,} (?=\w)
        if s[i] == '-' && i > 0 && is_word_punct(s[i - 1]) {
            let mut j = i;
            while j < n && s[j] == '-' {
                j += 1;
            }
            if j - i >= 2 && j < n && is_word(s[j]) {
                chunks.push(s[i..j].iter().collect());
                i = j;
                continue;
            }
        }
        // word: nws+? followed by a hyphen break, the end of the word, or an em-dash
        let start = i;
        let mut p = i + 1;
        loop {
            if p < n && s[p] == '-' {
                let behind_two = p >= 2 && is_letter(s[p - 2]) && is_letter(s[p - 1]);
                let behind_four =
                    p >= 3 && is_letter(s[p - 3]) && s[p - 2] == '-' && is_letter(s[p - 1]);
                // `(?<=lt{2}-)` / `(?<=lt-lt-)` are evaluated after the hyphen
                let ahead = p + 1 < n
                    && is_letter(s[p + 1])
                    && ((p + 2 < n && is_letter(s[p + 2]))
                        || (p + 3 < n && s[p + 2] == '-' && is_letter(s[p + 3])));
                if (behind_two || behind_four) && ahead {
                    p += 1;
                    break;
                }
            }
            if p >= n || is_ws(s[p]) {
                break;
            }
            if s[p] == '-' && is_word_punct(s[p - 1]) {
                let mut j = p;
                while j < n && s[j] == '-' {
                    j += 1;
                }
                if j - p >= 2 && j < n && is_word(s[j]) {
                    break;
                }
            }
            p += 1;
        }
        chunks.push(s[start..p].iter().collect());
        i = p;
    }
    chunks
}

/// `textwrap.wrap(text, width)` with the defaults argparse uses; `indent` is both the
/// initial and the subsequent indent (what `fill` gets from `_fill_text`).
fn wrap(text: &str, width: usize, indent: &str) -> Vec<String> {
    let mut chunks = split_chunks(text);
    chunks.reverse();
    let indent_len = clen(indent);
    let mut lines: Vec<String> = Vec::new();
    while !chunks.is_empty() {
        let mut cur: Vec<String> = Vec::new();
        let mut cur_len = 0usize;
        let avail = width.saturating_sub(indent_len);
        if chunks.last().is_some_and(|c| c.trim().is_empty()) && !lines.is_empty() {
            chunks.pop();
        }
        while let Some(last) = chunks.last() {
            let l = clen(last);
            if cur_len + l <= avail {
                cur_len += l;
                cur.push(chunks.pop().expect("checked"));
            } else {
                break;
            }
        }
        if chunks.last().is_some_and(|c| clen(c) > avail) {
            // _handle_long_word
            let space_left = if avail >= 1 { avail - cur_len } else { 1 };
            let chunk: Vec<char> = chunks.last().expect("checked").chars().collect();
            let mut end = space_left;
            if chunk.len() > space_left {
                let hyphen = chunk[..space_left].iter().rposition(|c| *c == '-');
                if let Some(h) = hyphen {
                    if h > 0 && chunk[..h].iter().any(|c| *c != '-') {
                        end = h + 1;
                    }
                }
            }
            cur.push(chunk[..end].iter().collect());
            *chunks.last_mut().expect("checked") = chunk[end..].iter().collect();
        }
        if cur.last().is_some_and(|c| c.trim().is_empty()) {
            cur.pop();
        }
        if !cur.is_empty() {
            lines.push(format!("{indent}{}", cur.concat()));
        }
    }
    lines
}

fn collapse_ws(text: &str) -> String {
    text.split_ascii_whitespace().collect::<Vec<_>>().join(" ")
}

/// `_split_lines` (help text): collapse whitespace, wrap, no indent.
fn split_lines(text: &str, width: usize) -> Vec<String> {
    wrap(&collapse_ws(text), width, "")
}

/// `_format_text`: a description or epilog, followed by a blank line.
fn format_text(text: &str, raw: bool, width: usize, indent: usize) -> String {
    let text_width = width.saturating_sub(indent).max(11);
    let pad = " ".repeat(indent);
    let body = if raw {
        text.split_inclusive('\n')
            .map(|line| format!("{pad}{line}"))
            .collect::<String>()
    } else {
        wrap(&collapse_ws(text), text_width, &pad).join("\n")
    };
    format!("{body}\n\n")
}

// ---- usage ----------------------------------------------------------------------------

/// `_get_actions_usage_parts` for `[help] + optionals + positionals`; returns the parts and
/// the index where positionals start.
fn usage_parts(
    actions: &[Action],
    groups: &[Vec<usize>],
    group_required: &[bool],
) -> (Vec<String>, usize) {
    // (required, members in display order)
    let mut taken = vec![false; actions.len()];
    let group_of = |index: usize| groups.iter().position(|g| g.contains(&index));
    let mut positionals: Vec<(bool, Vec<usize>)> = Vec::new();
    for (i, a) in actions.iter().enumerate() {
        if a.positional() && !taken[i] {
            taken[i] = true;
            match group_of(i) {
                Some(g) => {
                    let mut members: Vec<usize> = groups[g]
                        .iter()
                        .copied()
                        .filter(|m| !actions[*m].positional() && !taken[*m])
                        .collect();
                    for m in &members {
                        taken[*m] = true;
                    }
                    members.push(i);
                    positionals.push((group_required[g], members));
                }
                None => positionals.push((false, vec![i])),
            }
        }
    }
    let mut optionals: Vec<(bool, Vec<usize>)> = Vec::new();
    for (i, a) in actions.iter().enumerate() {
        if !a.positional() && !taken[i] {
            taken[i] = true;
            match group_of(i) {
                Some(g) => {
                    let mut members = vec![i];
                    for m in groups[g].iter().copied() {
                        if !actions[m].positional() && !taken[m] {
                            taken[m] = true;
                            members.push(m);
                        }
                    }
                    optionals.push((group_required[g], members));
                }
                None => optionals.push((false, vec![i])),
            }
        }
    }
    let pos_index = optionals.len();
    let mut parts: Vec<String> = Vec::new();
    let mut pos_start = None;
    for (n, (required, members)) in optionals.iter().chain(positionals.iter()).enumerate() {
        let start = parts.len();
        if n == pos_index {
            pos_start = Some(start);
        }
        let in_group = members.len() > 1;
        for &m in members {
            let a = &actions[m];
            let mut part;
            if a.positional() {
                part = a.format_args(a.dest);
                if in_group && part.starts_with('[') && part.ends_with(']') {
                    part = part[1..part.len() - 1].to_string();
                }
            } else {
                let first = a.options[0];
                if a.zero {
                    part = first.to_string();
                } else {
                    part = format!("{first} {}", a.format_args(&a.default_metavar()));
                }
                if !(a.required || *required || in_group) {
                    part = format!("[{part}]");
                }
            }
            parts.push(part);
        }
        if in_group {
            let open = if *required { '(' } else { '[' };
            parts[start] = format!("{open}{}", parts[start]);
            let last = parts.len() - 1;
            for part in &mut parts[start..last] {
                part.push_str(" |");
            }
            parts[last].push(if *required { ')' } else { ']' });
        }
    }
    let pos_start = pos_start.unwrap_or(parts.len());
    (parts, pos_start)
}

fn get_lines(
    parts: &[String],
    indent: &str,
    prefix: Option<&str>,
    text_width: usize,
) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut line: Vec<&str> = Vec::new();
    let indent_len = clen(indent);
    let mut line_len = match prefix {
        Some(p) => clen(p) as isize - 1,
        None => indent_len as isize - 1,
    };
    for part in parts {
        let part_len = clen(part) as isize;
        if line_len + 1 + part_len > text_width as isize && !line.is_empty() {
            lines.push(format!("{indent}{}", line.join(" ")));
            line.clear();
            line_len = indent_len as isize - 1;
        }
        line.push(part);
        line_len += part_len + 1;
    }
    if !line.is_empty() {
        lines.push(format!("{indent}{}", line.join(" ")));
    }
    if prefix.is_some() && !lines.is_empty() {
        lines[0] = lines[0].chars().skip(indent_len).collect();
    }
    lines
}

fn format_usage(
    prog: &str,
    actions: &[Action],
    groups: &[Vec<usize>],
    required: &[bool],
    width: usize,
) -> String {
    let prefix = "usage: ";
    let usage = if actions.is_empty() {
        prog.to_string()
    } else {
        let (parts, pos_start) = usage_parts(actions, groups, required);
        let mut all = vec![prog.to_string()];
        all.extend(parts.iter().cloned());
        let mut usage = all.join(" ");
        let text_width = width;
        if clen(prefix) + clen(&usage) > text_width {
            let opt_parts = &parts[..pos_start];
            let pos_parts = &parts[pos_start..];
            let prog_len = clen(prog);
            let lines: Vec<String>;
            if (clen(prefix) + prog_len) as f64 <= 0.75 * text_width as f64 {
                let indent = " ".repeat(clen(prefix) + prog_len + 1);
                if !opt_parts.is_empty() {
                    let mut first = vec![prog.to_string()];
                    first.extend(opt_parts.iter().cloned());
                    let mut l = get_lines(&first, &indent, Some(prefix), text_width);
                    l.extend(get_lines(pos_parts, &indent, None, text_width));
                    lines = l;
                } else if !pos_parts.is_empty() {
                    let mut first = vec![prog.to_string()];
                    first.extend(pos_parts.iter().cloned());
                    lines = get_lines(&first, &indent, Some(prefix), text_width);
                } else {
                    lines = vec![prog.to_string()];
                }
            } else {
                let indent = " ".repeat(clen(prefix));
                let mut joined: Vec<String> = opt_parts.to_vec();
                joined.extend(pos_parts.iter().cloned());
                let mut l = get_lines(&joined, &indent, None, text_width);
                if l.len() > 1 {
                    l = get_lines(opt_parts, &indent, None, text_width);
                    l.extend(get_lines(pos_parts, &indent, None, text_width));
                }
                let mut with_prog = vec![prog.to_string()];
                with_prog.extend(l);
                lines = with_prog;
            }
            usage = lines.join("\n");
        }
        usage
    };
    format!("{prefix}{usage}\n\n")
}

// ---- the whole help -------------------------------------------------------------------

fn format_action(
    label: &str,
    help: Option<&str>,
    indent: usize,
    help_position: usize,
    width: usize,
) -> String {
    let help_width = width.saturating_sub(help_position).max(11);
    let action_width = help_position as isize - indent as isize - 2;
    let pad = " ".repeat(indent);
    let help = help.filter(|h| !h.is_empty());
    let Some(help) = help else {
        return format!("{pad}{label}\n");
    };
    let mut out = String::new();
    let first_on_header_line = clen(label) as isize <= action_width;
    if first_on_header_line {
        out.push_str(&format!("{pad}{label:<w$}  ", w = action_width as usize));
    } else {
        out.push_str(&format!("{pad}{label}\n"));
    }
    if help.trim().is_empty() {
        if !first_on_header_line {
            // header already ended with a newline
        } else {
            out.push('\n');
        }
        return out;
    }
    let lines = split_lines(help, help_width);
    let first_indent = if first_on_header_line {
        0
    } else {
        help_position
    };
    out.push_str(&format!("{}{}\n", " ".repeat(first_indent), lines[0]));
    for line in &lines[1..] {
        out.push_str(&format!("{}{line}\n", " ".repeat(help_position)));
    }
    out
}

/// `parser.format_help()` for `cmd`, whose full program name is `prog`.
pub fn help(cmd: &Cmd, prog: &str, width_columns: usize) -> String {
    let width = width_columns.saturating_sub(2);
    let max_help_position = 24usize.min(width.saturating_sub(20).max(4));

    // parser._actions, in order: -h, the declared arguments, then the sub-parsers.
    let mut actions: Vec<Action> = Vec::new();
    {
        // `prefix_chars="\0"` (herdr): argparse's own help flag is spelled with NULs.
        let (short, long) = if cmd.raw {
            ("\0h", "\0\0help")
        } else {
            ("-h", "--help")
        };
        actions.push(Action {
            options: vec![short, long],
            dest: "help",
            nargs: None,
            zero: true,
            metavar: None,
            choices: &[],
            help: Some(HELP_TEXT.to_string()),
            required: false,
            parser: false,
        });
    }
    let mut hidden: Vec<bool> = vec![false; actions.len()];
    for arg in cmd.args {
        if cmd.raw && arg.dest == "json" {
            continue;
        }
        actions.push(action_of(arg));
        hidden.push(arg.hidden);
    }
    let mut choices: Vec<Choice> = Vec::new();
    if !cmd.subs.is_empty() {
        actions.push(Action {
            options: vec![],
            dest: cmd.sub_dest.unwrap_or("command"),
            nargs: None,
            zero: false,
            metavar: cmd.sub_metavar.map(str::to_string),
            choices: &[],
            help: None,
            required: cmd.sub_required,
            parser: true,
        });
        hidden.push(false);
        for sub in cmd.subs {
            if sub.help.is_some() {
                let label = if sub.aliases.is_empty() {
                    sub.name.to_string()
                } else {
                    format!("{} ({})", sub.name, sub.aliases.join(", "))
                };
                choices.push(Choice {
                    label,
                    help: sub.help,
                });
            }
        }
    }
    // Hidden (`help=SUPPRESS`) actions vanish from usage and from the listing, and the
    // exclusive groups are indexed by position in the visible list.
    let mut visible: Vec<Action> = Vec::new();
    let mut map: Vec<Option<usize>> = Vec::new();
    for (a, h) in actions.into_iter().zip(hidden) {
        if h {
            map.push(None);
        } else {
            map.push(Some(visible.len()));
            visible.push(a);
        }
    }
    // Table `members` are `dest`s; the namespace key is what groups were declared with.
    let mut groups: Vec<Vec<usize>> = Vec::new();
    let mut required: Vec<bool> = Vec::new();
    for g in cmd.groups {
        let members: Vec<usize> = g
            .members
            .iter()
            .filter_map(|dest| {
                visible
                    .iter()
                    .position(|a| a.dest == *dest && !a.positional())
                    .or_else(|| visible.iter().position(|a| a.dest == *dest))
            })
            .collect();
        groups.push(members);
        required.push(g.required);
    }

    let mut out = format_usage(prog, &visible, &groups, &required, width);
    if let Some(about) = cmd.about {
        out.push_str(&format_text(about, cmd.raw_text, width, 0));
    }

    // `_action_max_length`: every listed action at indent 2, sub-commands at indent 4.
    let mut max_len = 0usize;
    for a in &visible {
        max_len = max_len.max(clen(&a.invocation()) + 2);
    }
    for c in &choices {
        max_len = max_len.max(clen(&c.label) + 4);
    }
    let help_position = (max_len + 2).min(max_help_position);

    let mut positional = String::new();
    let mut optional = String::new();
    for a in &visible {
        let target = if a.positional() {
            &mut positional
        } else {
            &mut optional
        };
        target.push_str(&format_action(
            &a.invocation(),
            a.help.as_deref(),
            2,
            help_position,
            width,
        ));
        if a.parser {
            for c in &choices {
                target.push_str(&format_action(&c.label, c.help, 4, help_position, width));
            }
        }
    }
    for (title, body) in [("positional arguments", positional), ("options", optional)] {
        if !body.is_empty() {
            out.push_str(&format!("\n{title}:\n{body}\n"));
        }
    }
    if let Some(epilog) = cmd.epilog {
        out.push_str(&format_text(epilog, cmd.raw_text, width, 0));
    }
    // `_long_break_matcher`: three or more newlines become two; then one trailing newline.
    let mut collapsed = String::new();
    let mut run = 0;
    for c in out.chars() {
        if c == '\n' {
            run += 1;
            if run <= 2 {
                collapsed.push(c);
            }
        } else {
            run = 0;
            collapsed.push(c);
        }
    }
    format!("{}\n", collapsed.trim_matches('\n'))
}

/// The help for the command at `path` (names after `dunkingsheep`; aliases accepted).
pub fn help_for(root: &'static Cmd, path: &[&str], width_columns: usize) -> Option<String> {
    let mut cmd = root;
    let mut prog = root.name.to_string();
    for name in path {
        cmd = cmd
            .subs
            .iter()
            .find(|s| s.name == *name || s.aliases.contains(name))?;
        prog = format!("{prog} {}", cmd.name);
    }
    Some(help(cmd, &prog, width_columns))
}
