//! Immutable release layout for the Rust CLI (t3229, audit P0-1).
//!
//! A release is the directory `<prefix>/releases/<id>/`. It holds the Rust
//! binary, a copy of the `dunk-poll` script and a MANIFEST. Launchers and the
//! systemd drop-in point only at that directory. They never point at a Cargo
//! build tree and never at the dev checkout's Python. The scripts in
//! `packaging/rust-release/` drive this layout. The launcher rendering lives here,
//! so it is tested in one place.

use std::path::{Component, Path, PathBuf};

/// The copy of `tools/dunk_poll.py` shipped inside each release. Until `dunk-poll`
/// is a Rust verb (audit P1-9), the poll wrapper runs this copy.
pub const POLL_SCRIPT: &str = "dunk-poll.py";

/// First-line marker of the Rust launcher. `uninstall-release.sh` matches on it.
pub const LAUNCHER_MARKER: &str = "Dunking Sheep Rust CLI launcher";

/// First-line marker of the poll wrapper. `uninstall-release.sh` matches on it.
pub const POLL_MARKER: &str = "Dunking Sheep poll wrapper";

/// Release identifiers accepted by the layout and by the install scripts: a
/// letter or digit first, then letters, digits, `.`, `_` or `-`.
pub fn is_release_id(id: &str) -> bool {
    let mut chars = id.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphanumeric())
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

/// True when any path component is `target`, the Cargo output directory.
pub fn is_build_tree(path: &Path) -> bool {
    path.components()
        .any(|part| matches!(part, Component::Normal(name) if name == "target"))
}

/// The release directory that `executable` belongs to. Only
/// `<prefix>/releases/<id>/<binary>` qualifies. A build-tree or dev-checkout
/// binary is refused.
pub fn release_dir_of(executable: &Path) -> Result<PathBuf, String> {
    if is_build_tree(executable) {
        return Err(refusal(executable));
    }
    let release = executable.parent().ok_or_else(|| refusal(executable))?;
    let releases = release.parent().ok_or_else(|| refusal(executable))?;
    let id = release
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let in_releases = releases.file_name().is_some_and(|name| name == "releases");
    if in_releases && is_release_id(id) {
        Ok(release.to_path_buf())
    } else {
        Err(refusal(executable))
    }
}

/// The refusal for a launcher source outside an installed release.
fn refusal(executable: &Path) -> String {
    format!(
        "refusing to install launchers from {}: run the installed release binary at \
         <prefix>/releases/<id>/dunkingsheep, not a build-tree or dev-checkout binary",
        executable.display()
    )
}

/// POSIX single-quote escaping for one argument.
pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// The Rust CLI launcher: it execs the release binary and nothing else.
pub fn launcher_body(release_binary: &Path) -> String {
    format!(
        "#!/bin/sh\n# {LAUNCHER_MARKER}, installed by `dunkingsheep install`.\nexec {} \"$@\"\n",
        shell_quote(&release_binary.display().to_string())
    )
}

/// The poll wrapper: a pinned python3 running the release's poller copy.
pub fn poll_launcher_body(python: &Path, poller: &Path) -> String {
    format!(
        "#!/bin/sh\n# {POLL_MARKER}, installed by `dunkingsheep install`.\nexec {} {} \"$@\"\n",
        shell_quote(&python.display().to_string()),
        shell_quote(&poller.display().to_string())
    )
}
