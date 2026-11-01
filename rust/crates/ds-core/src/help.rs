//! `help.rs` (T54, slice 7): the one guide to Dunking Sheep, shared by every surface. Ports
//! `dunk_help.py`: `SHORT` and `guide(markdown)`.
//!
//! - `dunkingsheep guide` prints it (plain text or `--markdown`).
//! - `dunkingsheep --help` shows the short version as its epilog.
//! - The MCP server returns it from `initialize.instructions` and the `help` tool.
//! - The socket API returns it from the `help` command, with the command registry.
//! - `docs/GUIDE.md` is the markdown rendering.
//!
//! The text is GENERATED from the Python by `tools/gen_help_rs.py` (its `--check` is the gate):
//! the guide is 700 lines of prose built by a tiny renderer, and typing it again by hand is the
//! thing that drifts. The version is the one value the Python interpolates (`f"Dunking Sheep
//! {VERSION} guide"`), so it is the placeholder `@VERSION@` in the assets and is substituted here.

/// The placeholder the generator writes where the Python interpolates `VERSION`.
const VERSION_PLACEHOLDER: &str = "@VERSION@";

/// Where the plain guide's underlined heading goes: the underline is `=` * len(heading), so it
/// depends on the version's length and is rebuilt here, not stored.
const HEADING_MARKER: &str = "@HEADING@\n";

/// `SHORT`: the `--help` epilog.
pub const SHORT: &str = include_str!("help/short.txt");

const GUIDE_TEXT: &str = include_str!("help/guide.txt");
const GUIDE_MARKDOWN: &str = include_str!("help/guide.md");

/// `guide(markdown=False)`: the full guide as plain text (default) or GitHub markdown, for the
/// daemon `version`.
pub fn guide(markdown: bool, version: &str) -> String {
    if markdown {
        return GUIDE_MARKDOWN.replace(VERSION_PLACEHOLDER, version);
    }
    let heading = format!("Dunking Sheep {version} guide");
    let underlined = format!("{heading}\n{}\n", "=".repeat(heading.chars().count()));
    GUIDE_TEXT.replacen(HEADING_MARKER, &underlined, 1)
}

#[cfg(test)]
mod tests;
