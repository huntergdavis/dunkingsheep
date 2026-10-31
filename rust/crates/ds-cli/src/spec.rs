//! The shape of the generated argparse table (`cli_table.rs`).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Store,
    StoreTrue,
    StoreFalse,
    Append,
    Version,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ty {
    Str,
    Int,
    Float,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nargs {
    One,
    Opt,
    Plus,
    Star,
    Zero,
}

#[derive(Debug)]
pub struct Arg {
    /// Unique within a command (clap); `dest` is the argparse namespace key.
    pub id: &'static str,
    pub dest: &'static str,
    /// store_true/false with a `None` default: absent means `None`, not a bool.
    pub tri: bool,
    /// A float arg whose Python default is an int: reported as the int.
    pub default_int: bool,
    pub flags: &'static [&'static str],
    pub positional: bool,
    pub kind: Kind,
    pub ty: Ty,
    pub nargs: Nargs,
    pub choices: &'static [&'static str],
    pub default: Option<&'static str>,
    pub required: bool,
    pub help: Option<&'static str>,
    pub hidden: bool,
    pub metavar: Option<&'static str>,
    pub version: Option<&'static str>,
}

#[derive(Debug)]
pub struct Group {
    pub members: &'static [&'static str],
    pub required: bool,
}

#[derive(Debug)]
pub struct Cmd {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub help: Option<&'static str>,
    /// `prefix_chars="\0"`: no flags at all, every token is a positional (herdr).
    pub raw: bool,
    /// `RawDescriptionHelpFormatter`: `about` and `epilog` keep their line breaks.
    pub raw_text: bool,
    pub about: Option<&'static str>,
    pub epilog: Option<&'static str>,
    pub args: &'static [Arg],
    pub groups: &'static [Group],
    pub sub_dest: Option<&'static str>,
    pub sub_metavar: Option<&'static str>,
    pub sub_required: bool,
    pub subs: &'static [Cmd],
}
