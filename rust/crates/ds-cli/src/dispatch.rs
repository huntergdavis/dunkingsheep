//! T44, slice 6: run a parsed command. `main.rs` used to stop at "parsed but not ported yet";
//! this runs the commands whose bodies exist in the CLI crate and says so honestly for the
//! rest. Wired today: `context` (the READ subcommands, `context_cmd`), `usage`
//! (`usage_cmd`), `install` and `secrets-check` (`secrets_cmd`), the daemon-backed `gh-token`,
//! `git-credential` and `doctor` (`daemon_cmd`), and `github wrappers` (`github_cmd`). Everything
//! else, including the `context` write subcommands, keeps the "parsed but not ported yet" answer
//! (exit 1).
//!
//! `--json` for `context`: Python prints `json.dumps(result, indent=2)`; the ordered mode in
//! `usage_cmd` preserves daemon/local object insertion order with Python-compatible scalars.

use serde_json::{Map, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::context_cmd::context_read;
use crate::usage_cmd::{ordered_json, usage_command, Outcome};
use ds_core::context::digest::{digest_both, select_stores, DEFAULT_CAP};
use ds_core::context::ledger::Ledger;
use ds_core::context::store::Store;

fn text<'a>(ns: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    ns.get(key).and_then(Value::as_str)
}

fn task_stamp(at: f64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(at.floor() as i64, 0)
        .single()
        .map(|time| time.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

fn not_ported(command: &str) -> Outcome {
    Outcome {
        stdout: String::new(),
        stderr: format!("dunkingsheep (rust port): `{command}` is parsed but not ported yet\n"),
        code: 1,
    }
}

fn context_local(ns: &Map<String, Value>, json: bool) -> Option<Outcome> {
    let sub = text(ns, "context_command").unwrap_or("");
    let path = text(ns, "path").map(PathBuf::from);
    let global = matches!(ns.get("global_store"), Some(Value::Bool(true)));
    let now = chrono::Local::now().naive_local();

    let outcome = |value: Value, text: String| Outcome {
        stdout: if json {
            format!("{}\n", ordered_json(&value))
        } else {
            text
        },
        stderr: String::new(),
        code: 0,
    };
    let failed = |message: String| Outcome {
        stdout: String::new(),
        stderr: format!("{message}\n"),
        code: 1,
    };

    match sub {
        "init" => {
            let store = Store::new(path.as_deref(), global);
            match store.ensure() {
                Ok(root) => {
                    let root = root.display().to_string();
                    Some(outcome(
                        serde_json::json!({"root": root}),
                        format!("store at {root}\n"),
                    ))
                }
                Err(error) => Some(failed(error.0)),
            }
        }
        "snapshot" => {
            let as_of = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let selected = if global {
                Some("global")
            } else if matches!(ns.get("project"), Some(Value::Bool(true))) {
                Some("project")
            } else {
                None
            };
            let mut ledger = Ledger::new(None);
            let digest = digest_both(
                path.as_deref(),
                selected,
                DEFAULT_CAP,
                None,
                false,
                &mut ledger,
                now,
            );
            let stores = select_stores(path.as_deref(), selected);
            let mut snapshots = Map::new();
            for (name, store) in stores {
                let stats = match store.stats() {
                    Ok(stats) => stats,
                    Err(error) => return Some(failed(error.0)),
                };
                let entries = match store.entry_stats(&mut ledger) {
                    Ok(entries) => entries,
                    Err(error) => return Some(failed(error.0)),
                };
                let mut snapshot = Map::new();
                snapshot.insert(
                    "status".into(),
                    Value::String(if store.exists() { "ok" } else { "absent" }.into()),
                );
                snapshot.insert(
                    "path".into(),
                    Value::String(store.root.display().to_string()),
                );
                snapshot.insert(
                    "counts".into(),
                    stats.get("counts").cloned().unwrap_or(Value::Null),
                );
                snapshot.insert(
                    "reads_7d".into(),
                    stats.get("reads_7d").cloned().unwrap_or(Value::Null),
                );
                snapshot.insert(
                    "injected_7d".into(),
                    stats.get("injected_7d").cloned().unwrap_or(Value::Null),
                );
                snapshot.insert(
                    "never_read_count".into(),
                    stats
                        .get("never_read_count")
                        .cloned()
                        .unwrap_or(Value::Null),
                );
                snapshot.insert(
                    "never_delivered_count".into(),
                    stats
                        .get("never_delivered_count")
                        .cloned()
                        .unwrap_or(Value::Null),
                );
                snapshot.insert("entries".into(), Value::Array(entries));
                snapshots.insert(name, Value::Object(snapshot));
            }
            let mut value = Map::new();
            value.insert("schema".into(), Value::String("context-snapshot/1".into()));
            value.insert("as_of".into(), Value::from(as_of));
            value.insert("digest".into(), Value::String(digest));
            value.insert("stores".into(), Value::Object(snapshots));
            let value = Value::Object(value);
            Some(outcome(value.clone(), ordered_json(&value) + "\n"))
        }
        "ledger" if text(ns, "action") == Some("adopt") => {
            if std::env::var("DUNKINGSHEEP_SUPERUSER").ok().as_deref() != Some("1") {
                return Some(failed(
                    "refused: `context ledger adopt` is the owner's bootstrap; run it with DUNKINGSHEEP_SUPERUSER=1 from the owner's terminal".into(),
                ));
            }
            let store = Store::new(path.as_deref(), global);
            let mut ledger = Ledger::new(None);
            let adopted = store.adopt(&mut ledger, "adopt");
            let ledger_path = ledger.path.display().to_string();
            let value = serde_json::json!({"adopted": adopted, "ledger": ledger_path});
            let count = value["adopted"].as_array().map_or(0, Vec::len);
            Some(outcome(
                value,
                format!("adopted {count} entries into {ledger_path}\n"),
            ))
        }
        _ => None,
    }
}

fn context(ns: &Map<String, Value>, json: bool) -> Outcome {
    let sub = text(ns, "context_command").unwrap_or("");
    if let Some(out) = context_local(ns, json) {
        return out;
    }
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
                format!("{}\n", ordered_json(&out.value))
            } else {
                out.text
            },
            stderr: String::new(),
            code: 0,
        },
        // `context add/promote/supersede/retire` local writes are not ported.
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

/// `github wrappers [--bin-dir DIR]` and `github protect` (the owner's one-shot setup).
fn github(ns: &Map<String, Value>) -> Outcome {
    match text(ns, "github_command") {
        Some("protect") => crate::github_cmd::protect(
            text(ns, "repo").unwrap_or(""),
            ns.get("approvals").and_then(Value::as_i64).unwrap_or(1),
            matches!(ns.get("bypass_reviewer_app"), Some(Value::Bool(true))),
            matches!(ns.get("dry_run"), Some(Value::Bool(true))),
        ),
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

fn install_launcher(ns: &Map<String, Value>, json: bool) -> Outcome {
    let requested_dir = text(ns, "bin_dir").unwrap_or("~/.local/bin");
    let bin_dir = if requested_dir == "~" {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(requested_dir))
    } else if let Some(rest) = requested_dir.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(rest)
    } else {
        PathBuf::from(requested_dir)
    };
    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => return install_error(error.to_string()),
    };
    let project_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")));
    let poller = project_root.join("tools/dunk_poll.py");
    let path_env = std::env::var("PATH").unwrap_or_default();
    let python = find_executable("python3", &path_env);

    let result = (|| -> Result<Value, String> {
        std::fs::create_dir_all(&bin_dir)
            .map_err(|error| format!("cannot create {}: {error}", bin_dir.display()))?;
        if !poller.is_file() {
            return Err(format!("no dunk-poll script at {}", poller.display()));
        }
        let python = python.ok_or_else(|| "no python3 found on PATH for dunk-poll".to_string())?;
        let launcher = bin_dir.join("dunkingsheep");
        let poll_launcher = bin_dir.join("dunk-poll");
        let launcher_body = format!(
            "#!/bin/sh\n# Dunking Sheep Rust CLI launcher, installed by `dunkingsheep install`.\nexec {} \"$@\"\n",
            shell_quote(&executable.display().to_string())
        );
        let poll_body = format!(
            "#!/bin/sh\nexec {} {} \"$@\"\n",
            shell_quote(&python.display().to_string()),
            shell_quote(&poller.display().to_string())
        );
        write_executable(&launcher, &launcher_body)?;
        write_executable(&poll_launcher, &poll_body)?;

        let on_path_here = std::env::split_paths(&path_env)
            .filter(|path| !path.as_os_str().is_empty())
            .any(|path| real_path(&path) == real_path(&bin_dir));
        let mut result = Map::new();
        result.insert(
            "launcher".into(),
            Value::String(launcher.display().to_string()),
        );
        result.insert(
            "poll_launcher".into(),
            Value::String(poll_launcher.display().to_string()),
        );
        result.insert(
            "runs".into(),
            Value::String(executable.display().to_string()),
        );
        result.insert(
            "poll_runs".into(),
            Value::String(poller.display().to_string()),
        );
        result.insert("on_path_here".into(), Value::Bool(on_path_here));
        result.insert(
            "resolves_to".into(),
            find_executable("dunkingsheep", &path_env)
                .map(|path| Value::String(path.display().to_string()))
                .unwrap_or(Value::Null),
        );
        result.insert(
            "poll_resolves_to".into(),
            find_executable("dunk-poll", &path_env)
                .map(|path| Value::String(path.display().to_string()))
                .unwrap_or(Value::Null),
        );
        Ok(Value::Object(result))
    })();

    match result {
        Ok(value) if json => Outcome {
            stdout: format!("{}\n", ordered_json(&value)),
            stderr: String::new(),
            code: 0,
        },
        Ok(value) => {
            let mut stdout = String::new();
            if let Some(object) = value.as_object() {
                for (key, value) in object {
                    stdout.push_str(&format!("{key:<16} {}\n", python_str(value)));
                }
            }
            if value["resolves_to"].is_null() {
                stdout.push_str(&format!(
                    "\nNote: {requested_dir} is not on this shell's PATH.\n"
                ));
            }
            stdout.push_str(
                "\nSuperuser setup: run `dunkingsheep superuser recover` from the VP pane, then `dunkingsheep superuser status`.\n",
            );
            Outcome {
                stdout,
                stderr: String::new(),
                code: 0,
            }
        }
        Err(message) => install_error(message),
    }
}

fn install_error(message: String) -> Outcome {
    Outcome {
        stdout: String::new(),
        stderr: format!("{message}\n"),
        code: 1,
    }
}

fn write_executable(path: &Path, body: &str) -> Result<(), String> {
    std::fs::write(path, body)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|error| format!("cannot chmod {}: {error}", path.display()))
}

fn real_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn find_executable(name: &str, path_env: &str) -> Option<PathBuf> {
    std::env::split_paths(path_env)
        .map(|dir| {
            if dir.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                dir
            }
        })
        .map(|dir| dir.join(name))
        .find(|path| {
            std::fs::metadata(path)
                .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
}

fn shell_quote(text: &str) -> String {
    if !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
    {
        return text.to_string();
    }
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn python_str(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::String(text) => text.clone(),
        _ => value.to_string(),
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
            "wrapper": if tmux_wrapper.exists() { Some(tmux_wrapper) } else { None::<PathBuf> },
            "real_tmux": if tmux_real.exists() { Some(tmux_real) } else { None::<PathBuf> },
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
        "install" => install_launcher(ns, json),
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
        "list" | "ls" => crate::daemon_cmd::run_command(ns, "list_dunks", Map::new(), |result| {
            crate::render_print::render_dunks(result["dunks"].as_array().map_or(&[], Vec::as_slice))
        }),
        "get" => {
            let mut args = Map::new();
            args.insert("id".into(), ns.get("id").cloned().unwrap_or(Value::Null));
            crate::daemon_cmd::run_command(ns, "get_dunk", args, |result| {
                crate::render_print::render_dunk(result)
            })
        }
        "add" => {
            let mut args = Map::new();
            args.insert(
                "text".into(),
                ns.get("text")
                    .cloned()
                    .unwrap_or_else(|| Value::String("continue".into())),
            );
            args.insert(
                "interval_minutes".into(),
                ns.get("every")
                    .cloned()
                    .unwrap_or_else(|| Value::String("10".into())),
            );
            args.insert(
                "start".into(),
                Value::Bool(!matches!(ns.get("no_start"), Some(Value::Bool(true)))),
            );
            if let Some(target) = text(ns, "target").filter(|value| !value.is_empty()) {
                args.insert("target".into(), Value::String(target.to_string()));
            }
            if let Some(name) = text(ns, "name").filter(|value| !value.is_empty()) {
                args.insert("name".into(), Value::String(name.to_string()));
            }
            if matches!(ns.get("only_idle"), Some(Value::Bool(true))) {
                args.insert("only_when".into(), Value::String("idle".into()));
            }
            if let Some(max_sends) = ns
                .get("max_sends")
                .filter(|value| crate::render_print::truthy(value))
            {
                args.insert("max_sends".into(), max_sends.clone());
            }
            if matches!(ns.get("skip_if_unconsumed"), Some(Value::Bool(true))) {
                args.insert("skip_if_unconsumed".into(), Value::Bool(true));
            }
            if matches!(ns.get("ignore_typing"), Some(Value::Bool(true))) {
                args.insert("hold_while_typing".into(), Value::Bool(false));
            }
            crate::daemon_cmd::run_command(ns, "add_dunk", args, |result| {
                crate::render_print::render_dunk(result)
            })
        }
        "inbox" => match text(ns, "inbox_command").unwrap_or("") {
            "count" => {
                let mut args = Map::new();
                args.insert(
                    "mailbox".into(),
                    ns.get("mailbox").cloned().unwrap_or(Value::Null),
                );
                crate::daemon_cmd::run_command(ns, "inbox_count", args, |result| {
                    crate::render_print::render_inbox_count(result)
                })
            }
            "send" => {
                let body_values = ns.get("body").and_then(Value::as_array);
                let body_words = body_values
                    .map(|words| {
                        words
                            .iter()
                            .map(crate::render_fleet::py_str)
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                let body = if let Some(body_file) = text(ns, "body_file").filter(|p| !p.is_empty())
                {
                    if body_values.is_some_and(|words| !words.is_empty()) {
                        return Outcome {
                            stdout: String::new(),
                            stderr: "inbox send: pass a body or --body-file, not both\n".into(),
                            code: 2,
                        };
                    }
                    if body_file == "-" {
                        let mut body = String::new();
                        if let Err(error) =
                            std::io::Read::read_to_string(&mut std::io::stdin().lock(), &mut body)
                        {
                            return Outcome {
                                stdout: String::new(),
                                stderr: format!("{error}\n"),
                                code: 3,
                            };
                        }
                        body
                    } else {
                        match std::fs::read_to_string(body_file) {
                            Ok(body) => body,
                            Err(error) => {
                                return Outcome {
                                    stdout: String::new(),
                                    stderr: format!("{error}\n"),
                                    code: 3,
                                }
                            }
                        }
                    }
                } else {
                    body_words
                };
                let mut args = Map::new();
                args.insert("to".into(), ns.get("to").cloned().unwrap_or(Value::Null));
                args.insert("body".into(), Value::String(body));
                args.insert(
                    "subject".into(),
                    ns.get("subject").cloned().unwrap_or(Value::Null),
                );
                let attachments = ns
                    .get("attach")
                    .and_then(Value::as_array)
                    .map(|paths| {
                        paths
                            .iter()
                            .map(crate::render_fleet::py_str)
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .unwrap_or_default();
                args.insert(
                    "attach".into(),
                    if attachments.is_empty() {
                        Value::Null
                    } else {
                        Value::String(attachments)
                    },
                );
                args.insert(
                    "notify".into(),
                    Value::Bool(!matches!(ns.get("no_notify"), Some(Value::Bool(true)))),
                );
                crate::daemon_cmd::run_command(ns, "inbox_send", args, |result| {
                    crate::render_print::render_inbox_sent(result)
                })
            }
            "list" => {
                let mut args = Map::new();
                args.insert(
                    "mailbox".into(),
                    ns.get("mailbox").cloned().unwrap_or(Value::Null),
                );
                args.insert(
                    "unread_only".into(),
                    Value::Bool(!matches!(ns.get("all"), Some(Value::Bool(true)))),
                );
                args.insert(
                    "limit".into(),
                    ns.get("limit").cloned().unwrap_or(Value::from(50)),
                );
                crate::daemon_cmd::run_command(ns, "inbox_list", args, |result| {
                    crate::render_print::render_inbox_list(result)
                })
            }
            _ => not_ported("inbox"),
        },
        "task" => match text(ns, "task_command").unwrap_or("") {
            "update" => {
                let mut args = Map::new();
                args.insert("id".into(), ns.get("id").cloned().unwrap_or(Value::Null));
                for (argument, field) in [
                    ("title", "title"),
                    ("body", "body"),
                    ("status", "status"),
                    ("priority", "priority"),
                    ("parent", "parent_id"),
                    ("block_reason", "block_reason"),
                    ("assign", "assignees"),
                    ("depends_on", "depends_on"),
                    ("multi_assignee", "multi_assignee"),
                ] {
                    if let Some(value) = ns.get(argument).filter(|value| !value.is_null()) {
                        args.insert(field.into(), value.clone());
                    }
                }
                crate::daemon_cmd::run_command(ns, "task_update", args, |task| {
                    crate::render_print::render_task(task, &task_stamp)
                })
            }
            "assign" => {
                let mut args = Map::new();
                args.insert("id".into(), ns.get("id").cloned().unwrap_or(Value::Null));
                args.insert(
                    "assignees".into(),
                    ns.get("assignees").cloned().unwrap_or(Value::Null),
                );
                args.insert(
                    "mode".into(),
                    ns.get("mode")
                        .cloned()
                        .unwrap_or(Value::String("add".into())),
                );
                crate::daemon_cmd::run_command(ns, "task_assign", args, |task| {
                    crate::render_print::render_task(task, &task_stamp)
                })
            }
            "complete" => {
                let mut args = Map::new();
                args.insert("id".into(), ns.get("id").cloned().unwrap_or(Value::Null));
                args.insert("by".into(), ns.get("by").cloned().unwrap_or(Value::Null));
                args.insert(
                    "verdict".into(),
                    ns.get("verdict").cloned().unwrap_or(Value::Null),
                );
                args.insert(
                    "landed".into(),
                    ns.get("landed").cloned().unwrap_or(Value::Bool(true)),
                );
                crate::daemon_cmd::run_command(ns, "task_complete", args, |task| {
                    crate::render_print::render_task(task, &task_stamp)
                })
            }
            _ => not_ported("task"),
        },
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
