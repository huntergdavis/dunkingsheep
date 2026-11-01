//! T44, slice 6: run a parsed command. `main.rs` used to stop at "parsed but not ported yet";
//! this runs the commands whose bodies exist in the CLI crate and says so honestly for the
//! rest. Wired today: `context` (the READ subcommands, `context_cmd`), `usage`
//! (`usage_cmd`) and `secrets-check` (`secrets_cmd`), the daemon-backed `gh-token`, `git-credential` and `doctor`
//! (`daemon_cmd`), and `github wrappers` (`github_cmd`). Everything else, including the `context` write subcommands, keeps the
//! "parsed but not ported yet" answer (exit 1).
//!
//! `--json` for `context`: the Python prints `json.dumps(result, indent=2)` (insertion order);
//! serde_json's maps are key-sorted, so this prints the same document with SORTED keys
//! (`usage_cmd::sorted_json`). Valid, equal as JSON, not byte-identical: held with the other
//! json.dumps printers pending the owner's `preserve_order` decision.

use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

use crate::context_cmd::context_read;
use crate::usage_cmd::{sorted_json, usage_command, Outcome};

fn text<'a>(ns: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    ns.get(key).and_then(Value::as_str)
}

fn not_ported(command: &str) -> Outcome {
    Outcome {
        stdout: String::new(),
        stderr: format!("dunkingsheep (rust port): `{command}` is parsed but not ported yet\n"),
        code: 1,
    }
}

fn context(ns: &Map<String, Value>, json: bool) -> Outcome {
    let sub = text(ns, "context_command").unwrap_or("");
    // add / promote / supersede / retire go to the daemon (role-gated there) unless --local;
    // `--local` writes the files directly, which is not ported.
    if matches!(sub, "add" | "promote" | "supersede" | "retire") {
        if matches!(ns.get("local"), Some(Value::Bool(true))) {
            return not_ported(&format!("context {sub} --local"));
        }
        let stdin = std::io::stdin();
        return crate::daemon_cmd::context_write(ns, &mut stdin.lock());
    }
    let now = chrono::Local::now().naive_local();
    match context_read(sub, ns, None, now) {
        Ok(out) => Outcome {
            stdout: if json {
                format!("{}\n", sorted_json(&out.value))
            } else {
                out.text
            },
            stderr: String::new(),
            code: 0,
        },
        // `context init/add/promote/supersede/retire/ledger adopt` are writes (not ported).
        Err(error) if error.to_string().contains("not a read-side command") => {
            not_ported(&format!("context {sub}"))
        }
        // Python's `SystemExit("message")`: the message on stderr, exit 1.
        Err(error) => Outcome {
            stdout: String::new(),
            stderr: format!("{error}\n"),
            code: 1,
        },
    }
}

/// `github wrappers [--bin-dir DIR]` (the owner's one-shot setup). `github protect` needs
/// `dunk_github` (unported) and keeps the not-ported answer.
fn github(ns: &Map<String, Value>) -> Outcome {
    match text(ns, "github_command") {
        Some("wrappers") => {
            let bin_dir = text(ns, "bin_dir").unwrap_or("~/.local/bin");
            let path = std::env::var("PATH").unwrap_or_default();
            match crate::github_cmd::wrappers_install(bin_dir, &path) {
                Ok(result) => Outcome {
                    stdout: crate::github_cmd::render_wrappers(&result),
                    stderr: String::new(),
                    code: 0,
                },
                // Python's `SystemExit("no real `gh` found on PATH to wrap")`: stderr, exit 1.
                Err(message) => Outcome {
                    stdout: String::new(),
                    stderr: format!("{message}\n"),
                    code: 1,
                },
            }
        }
        Some(other) => not_ported(&format!("github {other}")),
        None => not_ported("github"),
    }
}

fn guard_config_dir() -> PathBuf {
    std::env::var_os("DUNKINGSHEEP_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config/dunkingsheep")
        })
}

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

fn file_has_marker(path: &Path, marker: &[u8]) -> bool {
    std::fs::read(path)
        .map(|bytes| bytes.windows(marker.len()).any(|window| window == marker))
        .unwrap_or(false)
}

/// Python `_resolve_real_herdr()`: ignore the config wrapper directory and, when
/// PATH already contains a shadow wrapper, return its adjacent real executable.
fn resolve_real_herdr(guard_dir: &Path, home: &Path) -> (PathBuf, PathBuf) {
    let guard_real = std::fs::canonicalize(guard_dir).ok();
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            if directory.as_os_str().is_empty()
                || std::fs::canonicalize(&directory).ok() == guard_real
            {
                continue;
            }
            let candidate = directory.join("herdr");
            if !executable(&candidate) {
                continue;
            }
            let sibling = candidate.with_file_name("herdr-real");
            if file_has_marker(&candidate, b"dunkingsheep-herdr-guard") {
                if sibling.is_file() {
                    return (candidate, sibling);
                }
                continue;
            }
            return (candidate, sibling);
        }
    }
    let fallback = home.join(".local/bin/herdr");
    (fallback.clone(), fallback.with_file_name("herdr-real"))
}

fn real_tmux(guard_dir: &Path) -> Result<PathBuf, String> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("DUNKINGSHEEP_TMUX_BIN") {
        candidates.push(PathBuf::from(path));
    }
    candidates.push(guard_dir.join("tmux-real"));
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(
            std::env::split_paths(&path)
                .filter(|p| std::fs::canonicalize(p).ok() != std::fs::canonicalize(guard_dir).ok())
                .map(|p| p.join("tmux")),
        );
    }
    for path in candidates {
        let resolved = std::fs::canonicalize(&path).unwrap_or(path);
        if executable(&resolved) {
            let is_guard = std::fs::read(&resolved)
                .map(|b| {
                    b.windows(b"dunkingsheep-tmux-guard".len())
                        .any(|w| w == b"dunkingsheep-tmux-guard")
                })
                .unwrap_or(false);
            if !is_guard {
                return Ok(resolved);
            }
        }
    }
    Err("no executable non-guard tmux found; configure DUNKINGSHEEP_TMUX_BIN or install tmux on PATH".into())
}

fn guard_install_tmux() -> Result<Value, String> {
    let dir = guard_config_dir().join("tmux-guard");
    let real = real_tmux(&dir)?;
    let target = dir.join("tmux");
    if std::fs::canonicalize(&target)
        .ok()
        .is_some_and(|p| p == real)
    {
        return Err("cannot replace the selected real tmux with its guard".into());
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let link = dir.join("tmux-real");
    let existing_copy = link.is_file()
        && !std::fs::symlink_metadata(&link)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        && std::fs::canonicalize(&link).ok().as_ref() == Some(&real);
    if !existing_copy {
        if std::fs::symlink_metadata(&link).is_ok() {
            std::fs::remove_file(&link).map_err(|e| e.to_string())?;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link)
            .or_else(|_| std::fs::copy(&real, &link).map(|_| ()))
            .map_err(|e| e.to_string())?;
    }
    let source = include_str!("../../../../dunk_tmux_guard.py");
    let cli = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../dunkingsheep");
    let cli = std::fs::canonicalize(&cli).unwrap_or(cli);
    let template = "os.path.expanduser(\n    \"~/.local/bin/dunkingsheep\")";
    let rendered = source.replace(template, &format!("{:?}", cli.to_string_lossy()));
    std::fs::write(&target, &rendered).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
    }
    Ok(serde_json::json!({
        "ok": true,
        "wrapper": target,
        "real_tmux": real,
        "guard_dir": dir,
        "note": "agent panes started by the tmux backend get this first on PATH; the user's shell is untouched"
    }))
}

fn guard_install(no_shadow: bool) -> Result<Value, String> {
    // The wrapper and shell setup are confined to the caller's config directory.
    // Shadowing herdr is intentionally only requested by the default install form.
    let dir = guard_config_dir();
    let guard_dir = dir.join("bin");
    std::fs::create_dir_all(&guard_dir).map_err(|e| e.to_string())?;
    let target = guard_dir.join("herdr");
    let source = include_str!("../../../../dunk_guard_wrapper.py");
    let cli = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../dunkingsheep");
    let cli = std::fs::canonicalize(&cli).unwrap_or(cli);
    let rendered = source.replace("__DUNKINGSHEEP__", &format!("{:?}", cli.to_string_lossy()));
    std::fs::write(&target, &rendered).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
    }
    let bashrc = PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".bashrc");
    let marker = "# dunkingsheep herdr guard";
    let already = std::fs::read_to_string(&bashrc)
        .map(|s| s.contains(marker))
        .unwrap_or(false);
    let updated = if already {
        false
    } else {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&bashrc)
            .map_err(|e| e.to_string())?;
        writeln!(file, "\n{marker} (routes agent typing through the daemon; dunkingsheep guard uninstall to remove)\nexport PATH=\"{}:$PATH\"", guard_dir.display()).map_err(|e| e.to_string())?;
        true
    };
    let mut shadow_error = None;
    if !no_shadow {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let (herdr, sibling) = resolve_real_herdr(&guard_dir, &home);
        if executable(&herdr) {
            let is_guard = file_has_marker(&herdr, b"dunkingsheep-herdr-guard");
            let shadow_result = if is_guard {
                if executable(&sibling) {
                    Ok(())
                } else {
                    Err(format!(
                        "cannot find the herdr binary (looked at {})",
                        sibling.display()
                    ))
                }
            } else if sibling.exists() {
                Err(format!(
                    "{} already exists; move it aside first",
                    sibling.display()
                ))
            } else {
                std::fs::rename(&herdr, &sibling).map_err(|e| e.to_string())
            };
            match shadow_result {
                Ok(()) => {
                    if let Err(e) = std::fs::write(&herdr, &rendered) {
                        shadow_error = Some(e.to_string());
                    } else {
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            if let Err(e) = std::fs::set_permissions(
                                &herdr,
                                std::fs::Permissions::from_mode(0o755),
                            ) {
                                shadow_error = Some(e.to_string());
                            }
                        }
                    }
                }
                Err(e) => shadow_error = Some(e),
            }
        } else {
            shadow_error = Some(format!(
                "cannot find the herdr binary (looked at {})",
                herdr.display()
            ));
        }
    }
    let resolved = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|p| p.join("herdr"))
        .find(|p| executable(p));
    let bashrc_configured = std::fs::read_to_string(&bashrc)
        .map(|s| s.contains(marker))
        .unwrap_or(false);
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let (herdr_path, herdr_real_path) = resolve_real_herdr(&guard_dir, &home);
    let shadowing =
        file_has_marker(&herdr_path, b"dunkingsheep-herdr-guard") && herdr_real_path.exists();
    let real_herdr = if shadowing {
        herdr_real_path
    } else {
        herdr_path
    };
    let tmux_dir = dir.join("tmux-guard");
    let tmux_wrapper = tmux_dir.join("tmux");
    let tmux_real = tmux_dir.join("tmux-real");
    let tmux_installed = tmux_wrapper.exists();
    let mut value = serde_json::json!({
        "installed": true, "wrapper": target, "shell_configured": bashrc_configured,
        "active_in_this_shell": resolved.as_ref().is_some_and(|p| p.parent() == Some(guard_dir.as_path())),
        "shadowing_herdr": shadowing, "covers_agents_started_by_herdr": shadowing,
        "herdr_resolves_to": resolved, "real_herdr": real_herdr,
        "guarded_commands": ["pane run", "pane send-text", "pane send-keys", "agent send", "pane close", "pane kill", "tab close", "workspace close", "pane split/rename/focus", "tab create/rename/focus", "workspace create/rename/focus", "agent start"],
        "tmux_guard": {
            "installed": tmux_installed,
            "wrapper": if tmux_installed { Some(tmux_wrapper) } else { None::<PathBuf> },
            "real_tmux": if tmux_installed { Some(tmux_real) } else { None::<PathBuf> },
            "covers": "agent panes started by TmuxBackend (PATH prefix); user shells untouched"
        },
        "bashrc_updated": updated
    });
    if let Some(error) = shadow_error {
        value["shadow_error"] = Value::String(error);
    }
    Ok(value)
}

fn guard(ns: &Map<String, Value>) -> Outcome {
    let result = match text(ns, "action").unwrap_or("status") {
        "install" => guard_install(matches!(ns.get("no_shadow"), Some(Value::Bool(true)))),
        "install-tmux" => guard_install_tmux(),
        other => return not_ported(&format!("guard {other}")),
    };
    match result {
        Ok(value) if matches!(ns.get("json"), Some(Value::Bool(true))) => Outcome {
            stdout: crate::render_print::render_json_indent2(&value),
            stderr: String::new(),
            code: 0,
        },
        Ok(value) => Outcome {
            stdout: value
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| format!("{k:<22} {v}\n"))
                .collect(),
            stderr: String::new(),
            code: 0,
        },
        Err(message) => Outcome {
            stdout: String::new(),
            stderr: format!("{message}\n"),
            code: 1,
        },
    }
}

/// Run one parsed command (`ns` is the argparse namespace as a dictionary).
pub fn run(ns: &Map<String, Value>) -> Outcome {
    let command = text(ns, "command").unwrap_or("");
    let json = matches!(ns.get("json"), Some(Value::Bool(true)));
    match command {
        "context" => context(ns, json),
        // `secrets-check [--base REF] [--cwd DIR] [--quiet]`: exit 1 + the refusal on stderr.
        "secrets-check" => crate::secrets_cmd::secrets_check_command(
            text(ns, "cwd").filter(|c| !c.is_empty()),
            text(ns, "base").filter(|b| !b.is_empty()),
            matches!(ns.get("quiet"), Some(Value::Bool(true))),
        ),
        "usage" => {
            let config = text(ns, "config").unwrap_or("");
            usage_command(
                config,
                text(ns, "provider"),
                ns.get("max_age").and_then(Value::as_f64),
                json,
            )
        }
        "github" => github(ns),
        "guard" => guard(ns),
        "gh-token" => crate::daemon_cmd::gh_token(ns),
        // `client.call("list_panes"), print_panes`
        "panes" => crate::daemon_cmd::run_command(
            ns,
            "list_panes",
            Map::new(),
            crate::render_print::render_panes,
        ),
        // `workspaces` and its alias `tabs`: `client.call("list_workspaces"), print_workspaces`
        "workspaces" => crate::daemon_cmd::run_command(
            ns,
            "list_workspaces",
            Map::new(),
            crate::render_fleet::render_workspaces,
        ),
        "whoami" => crate::daemon_cmd::whoami(ns),
        // `client.call("status", check_herdr=args.check_herdr), print_status`
        "status" => {
            let mut args = Map::new();
            args.insert(
                "check_herdr".into(),
                Value::Bool(matches!(ns.get("check_herdr"), Some(Value::Bool(true)))),
            );
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            // `time.strftime(..., time.localtime(at))`: the host's zone, via chrono's Local.
            let stamp = |at: f64| {
                use chrono::TimeZone;
                chrono::Local
                    .timestamp_opt(at.floor() as i64, 0)
                    .single()
                    .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
                    .unwrap_or_default()
            };
            crate::daemon_cmd::run_command(ns, "status", args, |result| {
                crate::render_print::render_status(result, &stamp, now)
            })
        }
        // `client.call("doctor"), print_doctor`
        "doctor" => crate::daemon_cmd::run_command(
            ns,
            "doctor",
            Map::new(),
            crate::render_print::render_doctor,
        ),
        "git-credential" => {
            let stdin = std::io::stdin();
            crate::daemon_cmd::git_credential(ns, &mut stdin.lock())
        }
        other => not_ported(other),
    }
}
