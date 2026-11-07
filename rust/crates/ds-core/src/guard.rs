//! `dunk_guard.status()` and `tmux_status()`: what the herdr and tmux guards look like right
//! now. The daemon's `doctor` reads it through `RecoverEnv::guard_status` and the CLI's
//! `guard status` prints it, so the one producer lives here.
//!
//! `dunk_guard` fixes `CONFIG_DIR` and `BASHRC` at import time from the process environment and
//! reads PATH when asked. [`GuardPaths`] carries the same three values; the daemon takes them
//! from the environment it captured at startup (equal to the live one, because a daemon's
//! environment does not change), so a request never reads the process environment.

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

use serde_json::{json, Value};

use crate::config::{CapturedPythonHome, Env};

/// `MARKER`: the `~/.bashrc` line the guard installer appends.
pub const MARKER: &str = "# dunkingsheep herdr guard";
/// `GUARD_MARKER`: what identifies a wrapper script as the guard (`_is_guard`).
pub const GUARD_MARKER: &[u8] = b"dunkingsheep-herdr-guard";
/// `REAL_SUFFIX`: the shadowed binary's new name.
pub const REAL_SUFFIX: &str = "-real";
/// `_is_guard` reads the first 400 bytes only.
const GUARD_PROBE_BYTES: u64 = 400;
/// `os.confstr("CS_PATH")` on glibc: `shutil.which`'s search path when PATH is unset.
const CS_PATH: &str = "/bin:/usr/bin";

/// `status()["guarded_commands"]`.
const GUARDED_COMMANDS: [&str; 12] = [
    "pane run",
    "pane send-text",
    "pane send-keys",
    "agent send",
    "pane close",
    "pane kill",
    "tab close",
    "workspace close",
    "pane split/rename/focus",
    "tab create/rename/focus",
    "workspace create/rename/focus",
    "agent start",
];

/// The module-level paths `dunk_guard` computes at import time, plus the PATH that
/// `shutil.which` and `_resolve_real_herdr` read.
#[derive(Debug, Clone)]
pub struct GuardPaths {
    /// `CONFIG_DIR`: `DUNKINGSHEEP_DIR` when set and non-empty, else `~/.config/dunkingsheep`.
    pub config_dir: PathBuf,
    /// `os.path.expanduser("~")`: HOME, else the account's home, else the literal `~`.
    pub home: PathBuf,
    /// `PATH`, when set.
    pub path: Option<OsString>,
}

impl GuardPaths {
    /// From a captured environment; the account home is looked up only when HOME is absent.
    pub fn from_env(env: &Env) -> Self {
        Self::from_captured(env, &CapturedPythonHome::capture(env))
    }

    /// From a captured environment and an already captured home.
    pub fn from_captured(env: &Env, home: &CapturedPythonHome) -> Self {
        // `os.path.expanduser` keeps the tilde when there is no HOME and no passwd entry.
        let home = match home.expanduser_home() {
            Ok(Some(home)) => home.to_path_buf(),
            Ok(None) | Err(_) => PathBuf::from("~"),
        };
        let config_dir = env
            .get("DUNKINGSHEEP_DIR")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config/dunkingsheep"));
        GuardPaths {
            config_dir,
            home,
            path: env.get("PATH").map(OsString::from),
        }
    }

    /// The CLI's view: the real process environment.
    pub fn from_process() -> Self {
        Self::from_env(&Env::from_process())
    }
}

/// `dunk_guard.status()`. `Err` is the text of the exception `status()` raises (an unreadable
/// or non-UTF-8 `~/.bashrc`), which `Flock._guard_status` reports as `{"error": ...}`.
pub fn status(paths: &GuardPaths) -> Result<Value, String> {
    let guard_dir = paths.config_dir.join("bin");
    let guard_path = guard_dir.join("herdr");
    let installed = guard_path.exists();
    let resolved = which("herdr", paths.path.as_deref());
    let on_path = resolved.as_deref().is_some_and(|resolved| {
        realpath(resolved.parent().unwrap_or_else(|| Path::new(""))) == realpath(&guard_dir)
    });
    let bashrc_path = paths.home.join(".bashrc");
    let bashrc = if bashrc_path.exists() {
        read_text(&bashrc_path)?.contains(MARKER)
    } else {
        false
    };
    let (herdr_path, real_path) = resolve_real_herdr(paths, &guard_dir);
    let shadowed = is_guard(&herdr_path) && real_path.exists();
    Ok(json!({
        "installed": installed,
        "wrapper": if installed { text(&guard_path) } else { Value::Null },
        "shell_configured": bashrc,
        "active_in_this_shell": on_path,
        "shadowing_herdr": shadowed,
        "covers_agents_started_by_herdr": shadowed,
        "herdr_resolves_to": resolved.as_deref().map_or(Value::Null, text),
        "real_herdr": text(if shadowed { &real_path } else { &herdr_path }),
        "guarded_commands": GUARDED_COMMANDS,
        "tmux_guard": tmux_status(paths),
    }))
}

/// `tmux_status()`.
pub fn tmux_status(paths: &GuardPaths) -> Value {
    let dir = paths.config_dir.join("tmux-guard");
    let wrapper = dir.join("tmux");
    let real = dir.join("tmux-real");
    json!({
        "installed": wrapper.exists(),
        "wrapper": if wrapper.exists() { text(&wrapper) } else { Value::Null },
        "real_tmux": if real.exists() { text(&real) } else { Value::Null },
        "covers": "agent panes started by TmuxBackend (PATH prefix); user shells untouched",
    })
}

fn text(path: &Path) -> Value {
    Value::String(path.to_string_lossy().into_owned())
}

/// `shutil.which("herdr")` on POSIX: an unset PATH means `CS_PATH`, an empty one means nothing,
/// an empty entry is the working directory, and a hit exists, is not a directory and is
/// executable by this user.
fn which(name: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    let path = path.map_or_else(|| OsString::from(CS_PATH), OsStr::to_os_string);
    if path.is_empty() {
        return None;
    }
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| {
            candidate.exists() && executable_by_caller(candidate) && !candidate.is_dir()
        })
}

/// `os.access(path, os.X_OK)`: this user's permission, not a bit in the mode.
fn executable_by_caller(path: &Path) -> bool {
    nix::unistd::access(path, nix::unistd::AccessFlags::X_OK).is_ok()
}

/// `os.path.realpath` without `strict`: the canonical path when it exists, else the absolute,
/// lexically normalised path.
fn realpath(path: &Path) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// `_resolve_real_herdr()`: the real herdr on PATH, ignoring the guard's own directory; when
/// PATH already holds a shadow wrapper, that wrapper and its adjacent real binary.
fn resolve_real_herdr(paths: &GuardPaths, guard_dir: &Path) -> (PathBuf, PathBuf) {
    let guard_real = realpath(guard_dir);
    if let Some(path) = &paths.path {
        for directory in std::env::split_paths(path) {
            if directory.as_os_str().is_empty() || realpath(&directory) == guard_real {
                continue;
            }
            let candidate = directory.join("herdr");
            if !(candidate.is_file() && executable_by_caller(&candidate)) {
                continue;
            }
            let sibling = directory.join(format!("herdr{REAL_SUFFIX}"));
            if is_guard(&candidate) {
                if sibling.is_file() {
                    return (candidate, sibling);
                }
                continue;
            }
            return (candidate, sibling);
        }
    }
    let fallback = paths.home.join(".local/bin/herdr");
    let real = PathBuf::from(format!("{}{REAL_SUFFIX}", fallback.to_string_lossy()));
    (fallback, real)
}

/// `_is_guard(path)`: the marker within the first 400 bytes; any read error is False.
fn is_guard(path: &Path) -> bool {
    use std::io::Read;
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = Vec::new();
    if file.take(GUARD_PROBE_BYTES).read_to_end(&mut head).is_err() {
        return false;
    }
    head.windows(GUARD_MARKER.len())
        .any(|window| window == GUARD_MARKER)
}

/// `open(path, encoding="utf-8").read()`: the exception text for an OS error or invalid UTF-8.
fn read_text(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|error| crate::pm::os_error_text(&error, path))?;
    String::from_utf8(bytes)
        .map_err(|error| unicode_decode_error_text(error.as_bytes(), error.utf8_error()))
}

/// `str(UnicodeDecodeError)` for the first invalid sequence, as the utf-8 codec reports it.
fn unicode_decode_error_text(bytes: &[u8], error: std::str::Utf8Error) -> String {
    let start = error.valid_up_to();
    let byte = bytes[start];
    let (length, reason) = match error.error_len() {
        None => (bytes.len() - start, "unexpected end of data"),
        Some(length) if (0xC2..=0xF4).contains(&byte) => (length, "invalid continuation byte"),
        Some(length) => (length, "invalid start byte"),
    };
    if length == 1 {
        format!("'utf-8' codec can't decode byte 0x{byte:02x} in position {start}: {reason}")
    } else {
        format!(
            "'utf-8' codec can't decode bytes in position {start}-{}: {reason}",
            start + length - 1
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn executable(path: &Path, body: &[u8]) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn paths(dir: &Path) -> GuardPaths {
        let home = dir.join("home");
        let config = dir.join("config");
        let bin = dir.join("bin");
        for directory in [&home, &config, &bin] {
            std::fs::create_dir_all(directory).unwrap();
        }
        GuardPaths::from_env(&Env::from_pairs(&[
            ("HOME", home.to_str().unwrap()),
            ("DUNKINGSHEEP_DIR", config.to_str().unwrap()),
            ("PATH", bin.to_str().unwrap()),
        ]))
    }

    #[test]
    fn paths_follow_dunk_guard_import_time_rules() {
        let env = Env::from_pairs(&[("HOME", "/h/"), ("DUNKINGSHEEP_DIR", ""), ("PATH", "/x")]);
        let paths = GuardPaths::from_env(&env);
        assert_eq!(paths.home, PathBuf::from("/h"));
        assert_eq!(paths.config_dir, PathBuf::from("/h/.config/dunkingsheep"));
        assert_eq!(paths.path.as_deref(), Some(OsStr::new("/x")));
        let env = Env::from_pairs(&[("HOME", "/h"), ("DUNKINGSHEEP_DIR", "/cfg")]);
        let paths = GuardPaths::from_env(&env);
        assert_eq!(paths.config_dir, PathBuf::from("/cfg"));
        assert_eq!(paths.path, None);
    }

    #[test]
    fn nothing_installed_reports_the_home_fallback() {
        let dir = crate::testsupport::dir("guard-status");
        let paths = paths(dir.path());
        let report = status(&paths).unwrap();
        let home = dir.path().join("home");
        assert_eq!(
            report,
            json!({
                "installed": false, "wrapper": null, "shell_configured": false,
                "active_in_this_shell": false, "shadowing_herdr": false,
                "covers_agents_started_by_herdr": false, "herdr_resolves_to": null,
                "real_herdr": home.join(".local/bin/herdr").to_str().unwrap(),
                "guarded_commands": GUARDED_COMMANDS,
                "tmux_guard": {"installed": false, "wrapper": null, "real_tmux": null,
                               "covers": "agent panes started by TmuxBackend (PATH prefix); user shells untouched"},
            })
        );
    }

    #[test]
    fn installed_active_and_shadowed_wrappers_are_recognised() {
        let dir = crate::testsupport::dir("guard-status-installed");
        let mut paths = paths(dir.path());
        let guard_dir = paths.config_dir.join("bin");
        std::fs::create_dir_all(&guard_dir).unwrap();
        executable(
            &guard_dir.join("herdr"),
            b"#!/bin/sh\n# dunkingsheep-herdr-guard\n",
        );
        std::fs::write(paths.home.join(".bashrc"), format!("x\n{MARKER}\n")).unwrap();
        let bin = dir.path().join("bin");
        executable(&bin.join("herdr"), b"#!/bin/sh\necho real\n");
        paths.path = Some(std::env::join_paths([&guard_dir, &bin]).unwrap());
        let report = status(&paths).unwrap();
        assert_eq!(report["installed"], true);
        assert_eq!(report["wrapper"], guard_dir.join("herdr").to_str().unwrap());
        assert_eq!(report["shell_configured"], true);
        assert_eq!(report["active_in_this_shell"], true);
        assert_eq!(report["shadowing_herdr"], false);
        assert_eq!(
            report["herdr_resolves_to"],
            guard_dir.join("herdr").to_str().unwrap()
        );
        assert_eq!(report["real_herdr"], bin.join("herdr").to_str().unwrap());
        // The shadow: the wrapper where herdr lived, the real binary beside it.
        executable(
            &bin.join("herdr"),
            b"#!/bin/sh\n# dunkingsheep-herdr-guard\n",
        );
        executable(&bin.join("herdr-real"), b"#!/bin/sh\necho real\n");
        let report = status(&paths).unwrap();
        assert_eq!(report["shadowing_herdr"], true);
        assert_eq!(report["covers_agents_started_by_herdr"], true);
        assert_eq!(
            report["real_herdr"],
            bin.join("herdr-real").to_str().unwrap()
        );
        // The marker counts only within the first 400 bytes, as `_is_guard` reads them.
        let mut late = vec![b'#'; 400];
        late.extend_from_slice(b"\n# dunkingsheep-herdr-guard\n");
        executable(&bin.join("herdr"), &late);
        assert_eq!(status(&paths).unwrap()["shadowing_herdr"], false);
    }

    #[test]
    fn an_unreadable_bashrc_is_the_python_exception_text() {
        let dir = crate::testsupport::dir("guard-status-bashrc");
        let paths = paths(dir.path());
        std::fs::write(paths.home.join(".bashrc"), b"caf\xe9\n").unwrap();
        assert_eq!(
            status(&paths).unwrap_err(),
            "'utf-8' codec can't decode byte 0xe9 in position 3: invalid continuation byte"
        );
        std::fs::write(paths.home.join(".bashrc"), b"ok \xff\n").unwrap();
        assert_eq!(
            status(&paths).unwrap_err(),
            "'utf-8' codec can't decode byte 0xff in position 3: invalid start byte"
        );
        std::fs::write(paths.home.join(".bashrc"), b"ok \xe2\x82").unwrap();
        assert_eq!(
            status(&paths).unwrap_err(),
            "'utf-8' codec can't decode bytes in position 3-4: unexpected end of data"
        );
    }

    #[test]
    fn which_follows_shutil_rules() {
        let dir = crate::testsupport::dir("guard-which");
        let bin = dir.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        executable(&bin.join("herdr"), b"#!/bin/sh\n");
        std::fs::create_dir_all(bin.join("herdr-dir")).unwrap();
        assert_eq!(
            which("herdr", Some(bin.as_os_str())),
            Some(bin.join("herdr"))
        );
        assert_eq!(which("herdr-dir", Some(bin.as_os_str())), None);
        assert_eq!(which("herdr", Some(OsStr::new(""))), None);
        std::fs::write(bin.join("plain"), b"x").unwrap();
        assert_eq!(which("plain", Some(bin.as_os_str())), None);
    }
}
