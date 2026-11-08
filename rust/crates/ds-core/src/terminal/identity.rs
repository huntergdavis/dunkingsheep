//! The caller-binding listing (t3368): a pane's id and root pid, and nothing else.
//!
//! The full `list-panes` format carries free text: the window name, the pane title, the current
//! path and command, and the session name. Any same-user process can set those, and a name that
//! carries the field separator shifts every field after it, so the pid the caller binding checks
//! could be read from the wrong column. This listing has no free-text field, so no name can move
//! `pane_pid`. A line that does not have exactly two fields is refused, not guessed at.

use super::parse::pane_separator;
use super::TerminalError;

/// `list-panes -F` for caller binding: the pane id and the root pid, in that order.
pub fn identity_format() -> &'static str {
    static FORMAT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    FORMAT
        .get_or_init(|| ["#{pane_id}", "#{pane_pid}"].join(pane_separator()))
        .as_str()
}

/// Parse `list-panes -a -F identity_format()` output into `(pane_id, root pid)` per line. An empty
/// pid field is a pane with no root pid (`None`); a field that is not a number refuses the whole
/// listing, because a listing that cannot be read is not evidence about any pane.
pub fn parse_pane_identities(stdout: &str) -> Result<Vec<(String, Option<i64>)>, TerminalError> {
    let separator = pane_separator();
    stdout
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let fields: Vec<&str> = line.split(separator).collect();
            match fields.as_slice() {
                [pane_id, pid] => {
                    let pid = match *pid {
                        "" => None,
                        text => Some(text.parse::<i64>().map_err(|_| {
                            TerminalError::refused(format!("pane pid {text:?} is not a number"))
                        })?),
                    };
                    Ok(((*pane_id).to_string(), pid))
                }
                other => Err(TerminalError::refused(format!(
                    "pane identity line has {} fields, expected 2",
                    other.len()
                ))),
            }
        })
        .collect()
}

#[cfg(test)]
#[path = "identity_tests.rs"]
mod identity_tests;
