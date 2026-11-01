//! T44, slice 5: `dunkingsheep github wrappers` (`dunk_github_wrappers_install` and the
//! `wrappers` branch of `_github_local`): put the `gh` wrapper script in `--bin-dir` and make
//! `!dunkingsheep git-credential` the FIRST helper of `credential.https://github.com.helper`
//! in the user's global git config, keeping every helper already there, in order (t599: the
//! first version used --replace-all and dropped the owner's own `gh auth git-credential`).
//!
//! The wrapper script text is `gh_wrapper.sh`, written FROM the Python's `GH_WRAPPER` so the
//! bytes are the Python's. NOT here: `github protect` (needs `dunk_github`: app loading and
//! the branch-protection payload, unported).

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `GH_WRAPPER`, byte for byte, with `__REAL_GH__` to be replaced.
pub const GH_WRAPPER: &str = include_str!("gh_wrapper.sh");

const HELPER: &str = "!dunkingsheep git-credential";
const KEY: &str = "credential.https://github.com.helper";
const NOTE: &str = "lanes use https remotes; our helper is first and any helper already configured (the owner's gh login) is kept as the fallback (gh-token prints nothing for the owner role)";

/// What the install did (the Python's result dict, in its key order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrapperInstall {
    pub gh_wrapper: String,
    pub real_gh: String,
    pub git_credential_helper: String,
    pub helpers: Vec<String>,
    pub note: String,
}

fn expand_user(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

fn real_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// The first `gh` on `path_env` that is not in `bin_dir` and is not already our wrapper.
fn find_real_gh(path_env: &str, bin_dir: &Path) -> Option<PathBuf> {
    let bin_real = real_path(bin_dir);
    for directory in path_env.split(':') {
        let candidate = Path::new(directory).join("gh");
        if real_path(Path::new(directory)) != bin_real && is_executable_file(&candidate) {
            if let Ok(bytes) = std::fs::read(&candidate) {
                let head = &bytes[..bytes.len().min(300)];
                if head.windows(23).any(|w| w == b"dunkingsheep gh wrapper") {
                    continue;
                }
            }
            return Some(candidate);
        }
    }
    None
}

fn git_config(args: &[&str]) -> String {
    Command::new("git")
        .args(["config", "--global"])
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// `dunk_github_wrappers_install(bin_dir)`. `path_env` is `$PATH`. `Err` is the Python's
/// `SystemExit` text.
pub fn wrappers_install(bin_dir: &str, path_env: &str) -> Result<WrapperInstall, String> {
    let bin_dir = expand_user(bin_dir);
    std::fs::create_dir_all(&bin_dir)
        .map_err(|e| format!("cannot create {}: {e}", bin_dir.display()))?;
    let real = find_real_gh(path_env, &bin_dir)
        .ok_or_else(|| "no real `gh` found on PATH to wrap".to_string())?;
    let target = bin_dir.join("gh");
    let script = GH_WRAPPER.replace("__REAL_GH__", &real.display().to_string());
    std::fs::write(&target, script)
        .map_err(|e| format!("cannot write {}: {e}", target.display()))?;
    let mode = std::fs::metadata(&target)
        .map_err(|e| e.to_string())?
        .permissions()
        .mode();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode | 0o111))
        .map_err(|e| e.to_string())?;
    // Ours goes FIRST (it is the lane credential) and every helper already there stays behind
    // it, in the order it was in.
    let existing: Vec<String> = git_config(&["--get-all", KEY])
        .lines()
        .filter(|row| !row.trim().is_empty())
        .map(str::to_string)
        .collect();
    let mut wanted = vec![HELPER.to_string()];
    wanted.extend(
        existing
            .iter()
            .filter(|row| row.as_str() != HELPER)
            .cloned(),
    );
    if existing != wanted {
        git_config(&["--unset-all", KEY]);
        for row in &wanted {
            git_config(&["--add", KEY, row]);
        }
    }
    Ok(WrapperInstall {
        gh_wrapper: target.display().to_string(),
        real_gh: real.display().to_string(),
        git_credential_helper: HELPER.to_string(),
        helpers: wanted,
        note: NOTE.to_string(),
    })
}

/// The printer: `{key:<20} {value}` per key; the helpers list prints as a Python list.
pub fn render_wrappers(result: &WrapperInstall) -> String {
    let list = format!(
        "[{}]",
        result
            .helpers
            .iter()
            .map(|h| ds_core::context::store::py_repr(h))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let rows: [(&str, &str); 5] = [
        ("gh_wrapper", &result.gh_wrapper),
        ("real_gh", &result.real_gh),
        ("git_credential_helper", &result.git_credential_helper),
        ("helpers", &list),
        ("note", &result.note),
    ];
    rows.iter()
        .map(|(key, value)| format!("{key:<20} {value}\n"))
        .collect()
}
