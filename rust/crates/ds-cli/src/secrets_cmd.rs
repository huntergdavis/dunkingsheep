//! T44, slice 4: `dunkingsheep secrets-check` and the real secrets guard behind
//! `git-credential` (`dunk_secrets.main` / `dunk_secrets.refusal()` called from
//! `_git_credential`). The scanner is `ds_core::secrets`; this is the command body and the
//! binding of its answer to the `local::Refusal` shape.

use std::path::{Path, PathBuf};

use crate::local::Refusal;
use crate::usage_cmd::Outcome;

/// `secrets-check [--base REF] [--cwd DIR] [--quiet]`: exit 1 with the named refusal on stderr
/// when this branch ADDS a path that must never leave the host; a range that cannot be
/// determined is a warning (unless `--quiet`), never a refusal.
pub fn secrets_check_command(cwd: Option<&str>, base: Option<&str>, quiet: bool) -> Outcome {
    let dir: PathBuf = match cwd.filter(|c| !c.is_empty()) {
        Some(c) => PathBuf::from(c),
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    let (code, stderr) = ds_core::secrets::check(&dir, base, quiet);
    Outcome {
        stdout: String::new(),
        stderr,
        code,
    }
}

/// `dunk_secrets.refusal()` as `_git_credential` calls it: the repository is the current
/// directory and the base is the default (`origin/main`, `main`, upstream).
pub fn guard_refusal(cwd: &Path) -> Refusal {
    ds_core::secrets::refusal(cwd, None)
}
