//! The owner's launch templates (t5249 slice 2): the permission-widening settings a new worker or
//! reviewer lane of a project starts with, from `launch-templates.json` in the config directory.
//!
//! ```json
//! {"version": 1, "projects": {"LookingGlassNX": {"codex": {"sandbox": "danger-full-access", "approval": "never"}},
//!                             "global": {"claude": {"permission_bypass": true}}}}
//! ```
//!
//! Rules (accepted design note on t5249):
//! * widening settings come only from an explicit argument or this owner-written file, never from
//!   a parent lane or a provider default;
//! * an explicit flag on the launch command wins;
//! * the file counts only when it is a regular file (not a symbolic link) owned by the daemon's
//!   user and not writable by group or others, and it parses strictly; otherwise it is ignored
//!   with one warning and launches behave as before;
//! * a field set to `false` or left out never adds or removes a flag;
//! * WARN-ONLY for now: a Codex worker or reviewer with no sandbox/approval anywhere still
//!   launches as before, with a "would refuse" line and `source: unset` recorded.

use std::path::Path;

use serde_json::{json, Map, Value};

pub const FILE: &str = "launch-templates.json";
const CODEX_SANDBOX: [&str; 3] = ["read-only", "workspace-write", "danger-full-access"];
const CODEX_APPROVAL: [&str; 4] = ["untrusted", "on-failure", "on-request", "never"];

/// One scope's template.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Template {
    pub codex_sandbox: Option<String>,
    pub codex_approval: Option<String>,
    pub claude_permission_bypass: bool,
}

/// Parse the file's text strictly: anything but the documented shape is `Err`.
pub fn parse(text: &str) -> Result<Map<String, Value>, String> {
    let value: Value = serde_json::from_str(text).map_err(|e| format!("not JSON: {e}"))?;
    let object = value.as_object().ok_or("not an object")?;
    if object
        .keys()
        .any(|key| key != "version" && key != "projects")
    {
        return Err("only 'version' and 'projects' are allowed".into());
    }
    if object.get("version") != Some(&json!(1)) {
        return Err("version must be 1".into());
    }
    let projects = object
        .get("projects")
        .and_then(Value::as_object)
        .ok_or("'projects' must be an object")?;
    for (scope, providers) in projects {
        let providers = providers
            .as_object()
            .ok_or_else(|| format!("{scope}: must be an object"))?;
        for (provider, fields) in providers {
            let fields = fields
                .as_object()
                .ok_or_else(|| format!("{scope}.{provider}: must be an object"))?;
            for (field, value) in fields {
                let ok = match (provider.as_str(), field.as_str()) {
                    ("codex", "sandbox") => {
                        value.as_str().is_some_and(|v| CODEX_SANDBOX.contains(&v))
                    }
                    ("codex", "approval") => {
                        value.as_str().is_some_and(|v| CODEX_APPROVAL.contains(&v))
                    }
                    ("claude", "permission_bypass") => value.is_boolean(),
                    _ => false,
                };
                if !ok {
                    return Err(format!(
                        "{scope}.{provider}.{field}: not an allowed setting or value"
                    ));
                }
            }
        }
    }
    Ok(projects.clone())
}

/// The owner's templates, or `None` (today's behaviour) with the reason when the file is
/// absent, untrusted or malformed.
pub fn load(config_dir: &Path, daemon_uid: u32) -> Result<Map<String, Value>, String> {
    use std::os::unix::fs::MetadataExt;
    let path = config_dir.join(FILE);
    let meta = std::fs::symlink_metadata(&path).map_err(|_| "absent".to_string())?;
    if meta.file_type().is_symlink() || !meta.file_type().is_file() {
        return Err(format!(
            "{} is ignored: it must be a regular file, not a link",
            path.display()
        ));
    }
    if meta.uid() != daemon_uid || meta.mode() & 0o022 != 0 {
        return Err(format!(
            "{} is ignored: it must be owned by the daemon's user and not writable by group or others",
            path.display()
        ));
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    parse(&text).map_err(|why| format!("{} is ignored: {why}", path.display()))
}

/// The template for a lane scope: the project's name, or `global`.
pub fn template_for(templates: &Map<String, Value>, scope: &str) -> Template {
    let entry = &templates.get(scope).cloned().unwrap_or(Value::Null);
    Template {
        codex_sandbox: entry["codex"]["sandbox"].as_str().map(String::from),
        codex_approval: entry["codex"]["approval"].as_str().map(String::from),
        claude_permission_bypass: entry["claude"]["permission_bypass"] == true,
    }
}

fn has_flag(argv: &[String], names: &[&str]) -> bool {
    argv.iter().any(|token| {
        names
            .iter()
            .any(|name| token == name || token.starts_with(&format!("{name}=")))
    })
}

/// What applying a template did: the argv to launch, each widening field's source, and the
/// warn-only "would refuse" line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    pub argv: Vec<String>,
    pub sources: Map<String, Value>,
    pub would_refuse: Option<String>,
}

/// Apply `template` (from scope `scope_label`, e.g. `project:X` or `global`) to a worker or
/// reviewer's launch `argv`. Flags already on the command win; a template value is appended;
/// nothing is ever removed.
pub fn apply(argv: &[String], template: &Template, scope_label: &str) -> Applied {
    let mut out = argv.to_vec();
    let mut sources = Map::new();
    let mut would_refuse = None;
    let program = super::launched_program(argv).unwrap_or("").to_lowercase();
    match program.as_str() {
        "codex" => {
            let bypass = has_flag(argv, &["--dangerously-bypass-approvals-and-sandbox"]);
            for (field, names, value, flag) in [
                (
                    "sandbox",
                    &["-s", "--sandbox"][..],
                    &template.codex_sandbox,
                    "-s",
                ),
                (
                    "approval",
                    &["-a", "--ask-for-approval"][..],
                    &template.codex_approval,
                    "-a",
                ),
            ] {
                let source = if bypass || has_flag(argv, names) {
                    "explicit".to_string()
                } else if let Some(value) = value {
                    out.extend([flag.to_string(), value.clone()]);
                    scope_label.to_string()
                } else {
                    "unset".to_string()
                };
                sources.insert(field.into(), json!(source));
            }
            if sources.values().all(|source| source == "unset") {
                would_refuse = Some(format!(
                    "would refuse (warn-only, launched as before): codex sandbox/approval for {scope_label} not set: pass -s/-a or add them to {FILE}"
                ));
            }
        }
        "claude" => {
            let source = if has_flag(argv, &["--dangerously-skip-permissions"]) {
                "explicit"
            } else if template.claude_permission_bypass {
                out.push("--dangerously-skip-permissions".into());
                scope_label
            } else {
                "unset"
            };
            sources.insert("permission_bypass".into(), json!(source));
        }
        _ => {}
    }
    Applied {
        argv: out,
        sources,
        would_refuse,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|t| t.to_string()).collect()
    }

    const GOOD: &str = r#"{"version": 1, "projects": {
        "LookingGlassNX": {"codex": {"sandbox": "danger-full-access", "approval": "never"}},
        "global": {"claude": {"permission_bypass": true}, "codex": {"approval": "on-request"}}}}"#;

    #[test]
    fn the_file_parses_strictly() {
        let parsed = parse(GOOD).unwrap();
        assert_eq!(
            template_for(&parsed, "LookingGlassNX")
                .codex_sandbox
                .as_deref(),
            Some("danger-full-access")
        );
        assert_eq!(template_for(&parsed, "nope"), Template::default());
        for bad in [
            r#"{"version": 2, "projects": {}}"#,
            r#"{"version": 1, "projects": {}, "extra": 1}"#,
            r#"{"version": 1, "projects": {"p": {"codex": {"sandbox": "yolo"}}}}"#,
            r#"{"version": 1, "projects": {"p": {"codex": {"model": "x"}}}}"#,
            r#"{"version": 1, "projects": {"p": {"pi": {"sandbox": "read-only"}}}}"#,
            r#"{"version": 1, "projects": {"p": {"claude": {"permission_bypass": "yes"}}}}"#,
            "not json",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_file_counts_only_when_private_regular_and_the_daemon_users() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let uid = std::fs::metadata(dir.path())
            .map(|m| std::os::unix::fs::MetadataExt::uid(&m))
            .unwrap();
        assert_eq!(load(dir.path(), uid).unwrap_err(), "absent");
        let path = dir.path().join(FILE);
        std::fs::write(&path, GOOD).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load(dir.path(), uid).is_ok());
        assert!(load(dir.path(), uid.wrapping_add(1))
            .unwrap_err()
            .contains("owned by the daemon"));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o620)).unwrap();
        assert!(load(dir.path(), uid).unwrap_err().contains("not writable"));
        // A symbolic link to a good file is not accepted.
        let real = dir.path().join("real.json");
        std::fs::rename(&path, &real).unwrap();
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&real, &path).unwrap();
        assert!(load(dir.path(), uid).unwrap_err().contains("not a link"));
    }

    /// Acceptance (a)-(d) of the design note, at the launch-command level.
    #[test]
    fn explicit_wins_template_adds_unset_only_warns_and_false_adds_nothing() {
        let parsed = parse(GOOD).unwrap();
        let lgnx = template_for(&parsed, "LookingGlassNX");
        // (a) the incident's shape (no -s/-a) in a project with a template: the flags are added.
        let applied = apply(
            &v(&["codex", "-m", "MODEL", "--no-daemon"]),
            &lgnx,
            "project:LookingGlassNX",
        );
        assert_eq!(
            applied.argv,
            v(&[
                "codex",
                "-m",
                "MODEL",
                "--no-daemon",
                "-s",
                "danger-full-access",
                "-a",
                "never"
            ])
        );
        assert_eq!(applied.sources["sandbox"], "project:LookingGlassNX");
        assert_eq!(applied.would_refuse, None);
        // (c) an explicit flag wins over the template, and is recorded as explicit.
        let applied = apply(
            &v(&["codex", "-s", "workspace-write"]),
            &lgnx,
            "project:LookingGlassNX",
        );
        assert_eq!(applied.sources["sandbox"], "explicit");
        assert_eq!(applied.sources["approval"], "project:LookingGlassNX");
        assert_eq!(
            applied.argv,
            v(&["codex", "-s", "workspace-write", "-a", "never"])
        );
        // (b) no template at all: WARN-ONLY: launched unchanged, with the would-refuse line.
        let applied = apply(
            &v(&["env", "TMPDIR=/s/x", "codex", "-m", "M"]),
            &Template::default(),
            "project:Other",
        );
        assert_eq!(applied.argv, v(&["env", "TMPDIR=/s/x", "codex", "-m", "M"]));
        assert_eq!(applied.sources["sandbox"], "unset");
        assert!(applied
            .would_refuse
            .unwrap()
            .starts_with("would refuse (warn-only"));
        // The bypass flag counts as explicit for both fields.
        let applied = apply(
            &v(&["codex", "--dangerously-bypass-approvals-and-sandbox"]),
            &Template::default(),
            "global",
        );
        assert_eq!(applied.would_refuse, None);
        // claude: true adds the flag; false or absent adds nothing and removes nothing.
        let global = template_for(&parsed, "global");
        assert_eq!(
            apply(&v(&["claude"]), &global, "global").argv,
            v(&["claude", "--dangerously-skip-permissions"])
        );
        let off = Template::default();
        assert_eq!(apply(&v(&["claude"]), &off, "global").argv, v(&["claude"]));
        assert_eq!(
            apply(
                &v(&["claude", "--dangerously-skip-permissions"]),
                &off,
                "global"
            )
            .argv,
            v(&["claude", "--dangerously-skip-permissions"]),
            "a false/absent field never removes a flag"
        );
        // Other programs are untouched.
        assert_eq!(
            apply(&v(&["pi"]), &lgnx, "project:LookingGlassNX").argv,
            v(&["pi"])
        );
    }
}
