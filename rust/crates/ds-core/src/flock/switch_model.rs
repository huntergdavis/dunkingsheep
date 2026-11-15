//! Switching a lane's model by a guarded respawn (t4473 slice D).
//!
//! The lane is restarted in its own pane with the same command line, its model and effort
//! replaced, resuming the same agent session. Everything that makes a respawn safe is the
//! existing guarded `respawn_pane`: the owner role, the exact process, no active turn, no queued
//! outbox message, no unread mail, a confirmed idle input box, the Codex session check, a
//! verified start and a rollback to the old command when the new one fails. A live task lease
//! does not refuse: it is kept across the restart (owner sign-off).
//!
//! Refused instead of guessed: a harness without a known resume form, a lane whose session id
//! is not recorded, a lane Dunking Sheep did not launch. A fresh session is never started in
//! place of the old one.

use serde_json::{json, Value};

use super::respawn::{RespawnRequest, RespawnServices};
use super::Flock;
use crate::error::DunkError;

fn refusal(text: impl Into<String>) -> DunkError {
    DunkError::Refused(text.into())
}

/// Remove `flags` (and the value each takes) from `argv`, in both `--flag value` and
/// `--flag=value` form. `bare` flags take no value.
fn strip(argv: &[String], with_value: &[&str], bare: &[&str]) -> Vec<String> {
    let mut out = Vec::with_capacity(argv.len());
    let mut skip = false;
    for token in argv {
        if skip {
            skip = false;
            continue;
        }
        if with_value.contains(&token.as_str()) {
            skip = true;
            continue;
        }
        if bare.contains(&token.as_str())
            || with_value
                .iter()
                .any(|flag| token.starts_with(&format!("{flag}=")))
        {
            continue;
        }
        out.push(token.clone());
    }
    out
}

/// The flags of each harness that take a value. Every other token after the executable must be
/// a flag: a word that is not a flag's value is a positional argument (an initial prompt, a
/// session id where codex expects one), which a resumed session would replay or misread.
pub(super) fn value_flags(kind: &str) -> &'static [&'static str] {
    match kind {
        "claude" => &[
            "--model",
            "--effort",
            "--permission-mode",
            "--setting-sources",
            "--settings",
            "--mcp-config",
            "--resume",
            "-r",
            "--allowedTools",
            "--allowed-tools",
            "--disallowedTools",
            "--disallowed-tools",
            "--session-id",
            "--add-dir",
            "--append-system-prompt",
            "--system-prompt",
            "--fallback-model",
            "--agents",
            "--plugin-dir",
            "--output-format",
            "--input-format",
        ],
        "codex" => &[
            "-c",
            "--config",
            "-m",
            "--model",
            "-a",
            "--ask-for-approval",
            "-s",
            "--sandbox",
            "-C",
            "--cd",
            "-p",
            "--profile",
            "-i",
            "--image",
            "--add-dir",
        ],
        "pi" => &[
            "--model",
            "--provider",
            "--thinking",
            "--name",
            "-e",
            "--extension",
            "--session",
            "--session-id",
            "--fork",
            "--api-key",
            "--system-prompt",
        ],
        _ => &[],
    }
}

/// How many positional arguments `rest` holds, read fail-closed: a word counts unless it is the
/// value of a known value-taking flag; `--` makes everything after it positional.
fn positional_count(kind: &str, rest: &[String]) -> usize {
    let takes_value = value_flags(kind);
    let mut count = 0;
    let mut expecting_value = false;
    for (index, token) in rest.iter().enumerate() {
        if expecting_value {
            expecting_value = false;
            continue;
        }
        if token == "--" {
            return count + rest.len() - index - 1;
        }
        if token.starts_with('-') {
            expecting_value = !token.contains('=') && takes_value.contains(&token.as_str());
            continue;
        }
        count += 1;
    }
    count
}

/// The command line that resumes `session` with `model` (and `effort`), built from the lane's
/// launch command. The old model, effort and session flags are replaced, not repeated. With no
/// `model`, the session is resumed without a model flag (the harness's default, as at launch).
/// A launch command with positional arguments is refused.
pub fn switch_argv(
    argv: &[String],
    session: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<Vec<String>, String> {
    // t3912: a lane started as `env TMPDIR=<dir> <program> ...` keeps that prefix; the switch is
    // worked out on the program's own argv.
    let start = super::launched_program_index(argv).unwrap_or(0);
    let (prefix, program) = argv.split_at(start);
    let mut out = prefix.to_vec();
    out.extend(switch_program_argv(program, session, model, effort)?);
    Ok(out)
}

fn switch_program_argv(
    argv: &[String],
    session: &str,
    model: Option<&str>,
    effort: Option<&str>,
) -> Result<Vec<String>, String> {
    let Some(exe) = argv.first() else {
        return Err("the launch command is empty".into());
    };
    let kind = exe.rsplit('/').next().unwrap_or(exe).to_lowercase();
    let rest = &argv[1..];
    // A lane switched before already starts `codex resume ... SESSION`; that form is read as is.
    let checked: Vec<String> = match (kind.as_str(), rest.first().map(String::as_str)) {
        ("codex", Some("resume")) => rest[1..]
            .iter()
            .filter(|t| *t != session)
            .cloned()
            .collect(),
        _ => rest.to_vec(),
    };
    let positional = positional_count(&kind, &checked);
    if positional > 0 && !value_flags(&kind).is_empty() {
        return Err(format!(
            "the launch command has {positional} positional argument(s) (such as an initial prompt), which a resumed session would replay or misread"
        ));
    }
    match kind.as_str() {
        "claude" => {
            let mut out = vec![exe.clone()];
            out.extend(strip(
                rest,
                &["--model", "--effort", "--resume", "-r", "--session-id"],
                &["--continue", "-c", "--fork-session"],
            ));
            out.extend(["--resume".into(), session.into()]);
            if let Some(model) = model {
                out.extend(["--model".into(), model.into()]);
            }
            if let Some(effort) = effort {
                out.extend(["--effort".into(), effort.into()]);
            }
            Ok(out)
        }
        "codex" => {
            // `codex resume [OPTIONS] SESSION`; a lane switched before already starts so.
            let rest: Vec<String> = match rest.first().map(String::as_str) {
                Some("resume") => rest[1..].iter().filter(|t| *t != session).cloned().collect(),
                _ => rest.to_vec(),
            };
            let mut kept = strip(&rest, &["--model", "-m"], &["--last"]);
            // `-c model_reasoning_effort=...` is the effort setting; other `-c` settings stay.
            let mut out_rest = Vec::with_capacity(kept.len());
            let mut iter = kept.drain(..).peekable();
            while let Some(token) = iter.next() {
                if (token == "-c" || token == "--config")
                    && iter
                        .peek()
                        .is_some_and(|value| value.starts_with("model_reasoning_effort="))
                {
                    iter.next();
                    continue;
                }
                out_rest.push(token);
            }
            let mut out = vec![exe.clone(), "resume".into()];
            out.extend(out_rest);
            if let Some(model) = model {
                out.extend(["-m".into(), model.into()]);
            }
            if let Some(effort) = effort {
                out.extend(["-c".into(), format!("model_reasoning_effort={effort}")]);
            }
            out.push(session.into());
            Ok(out)
        }
        "pi" => {
            let mut out = vec![exe.clone()];
            out.extend(strip(
                rest,
                &["--model", "--session", "--session-id", "--thinking", "--fork"],
                &["--continue", "-c", "--resume", "-r"],
            ));
            out.extend(["--session".into(), session.into()]);
            match (model, effort) {
                (Some(model), Some(effort)) => out.extend(["--model".into(), format!("{model}:{effort}")]),
                (Some(model), None) => out.extend(["--model".into(), model.into()]),
                // pi takes the effort only as part of the model.
                (None, Some(effort)) => out.extend(["--thinking".into(), effort.into()]),
                (None, None) => {}
            }
            Ok(out)
        }
        other => Err(format!(
            "{other} has no known way to resume a session with another model; a fresh session is never started instead"
        )),
    }
}

/// Short plain text: a model or effort value goes onto a command line.
fn plain(key: &str, value: Option<&str>) -> Result<Option<String>, DunkError> {
    let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    // A value that starts with '-' would be read as another flag.
    let ok = value.len() <= 128
        && !value.starts_with('-')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-:/@".contains(c));
    if !ok {
        return Err(refusal(format!(
            "{key} must be at most 128 characters of letters, digits and . _ - : / @, not starting with -"
        )));
    }
    Ok(Some(value.to_string()))
}

impl Flock {
    /// `lanes action=switch_model`: restart `pane` with `model` (and `effort`), resuming its
    /// session, through the guarded respawn. Owner role only (the respawn checks it).
    pub fn switch_model(
        &self,
        services: &RespawnServices<'_>,
        pane: &str,
        model: Option<&str>,
        effort: Option<&str>,
        self_pane_id: Option<&str>,
    ) -> Result<Value, DunkError> {
        // Who may switch: the owner role for any lane; a planner only for a lane whose explicit
        // project is the one its project grant names (owner sign-off).
        let owner = self.is_superuser(self_pane_id, services.delivery)?;
        let planner = !owner
            && self_pane_id.is_some_and(|caller| {
                self.planner_grant_covers_lane(services.delivery, caller, pane)
            });
        if !owner && !planner {
            return Err(refusal(format!(
                "lane switch-model requires the owner role, or a planner grant for the project of {pane}"
            )));
        }
        let model = plain("model", model)?
            .ok_or_else(|| refusal("switch model needs the model (--model)"))?;
        let effort = plain("effort", effort)?;
        let launch = self.launch(pane).ok_or_else(|| {
            refusal(format!(
                "switch model refused: Dunking Sheep has no launch record for {pane}, so its command and session are not known"
            ))
        })?;
        let argv: Vec<String> = launch
            .get("argv")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|i| i.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if argv.is_empty() {
            return Err(refusal(format!(
                "switch model refused: the launch record of {pane} has no command"
            )));
        }
        let session = launch
            .get("session")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| {
                refusal(format!(
                    "switch model refused: the session of {pane} is not recorded, so it cannot be resumed; a fresh session is never started instead. No pane was changed. Use the harness's in-session model selection, or ask the owner to verify the existing session ID and perform a guarded explicit same-session relaunch once idle. Launch argv settings alone do not prove the active model."
                ))
            })?
            .to_string();
        let new_argv = switch_argv(&argv, &session, Some(&model), effort.as_deref())
            .map_err(|why| refusal(format!("switch model refused: {why}")))?;
        // A failed start is rolled back by RESUMING the same session with the old model, never
        // by the launch command, which would start a fresh session and abandon this one.
        let old_text = |key: &str| {
            launch
                .get(key)
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        let old_model =
            old_text("model").or_else(|| crate::state::flag(&argv, &crate::state::MODEL_FLAGS));
        let old_effort =
            old_text("effort").or_else(|| crate::state::flag(&argv, &crate::state::THINKING_FLAGS));
        let rollback_argv =
            switch_argv(&argv, &session, old_model.as_deref(), old_effort.as_deref())
                .map_err(|why| refusal(format!("switch model refused: no safe rollback: {why}")))?;
        let pane_pid = services
            .terminal
            .lock()
            .expect("terminal lock")
            .get_pane(pane)
            .and_then(|p| p.pane_pid)
            .ok_or_else(|| {
                refusal(format!(
                    "switch model refused: pane {pane} has no live process"
                ))
            })?;
        let start = crate::proc::process_start(pane_pid).ok_or_else(|| {
            refusal(format!(
                "switch model refused: the process of {pane} is not readable"
            ))
        })?;
        let previous = json!(old_model);
        let reason = json!(format!("switch model to {model}"));
        let (pane_value, pid_value, start_value) = (json!(pane), json!(pane_pid), json!(start));
        // The idle evidence is the respawn's own: no active turn, no queued outbox message, no
        // unread mail and an input box confirmed ready, all read inside it.
        let mut result = self.respawn_pane(
            services,
            RespawnRequest {
                pane_id: &pane_value,
                argv: &new_argv,
                rollback_argv: Some(&rollback_argv),
                expected_pid: Some(&pid_value),
                expected_start: Some(&start_value),
                confirm_idle: true,
                reason: Some(&reason),
                self_pane_id,
                keep_leases: true,
                preauthorized: true,
            },
        )?;
        result["authorized_as"] = json!(if owner {
            "owner role"
        } else {
            "project planner"
        });
        result["switched"] = json!({
            "from_model": previous,
            "model": model,
            "effort": effort,
            "session": session,
        });
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn claude_resumes_the_session_and_replaces_model_and_effort() {
        let argv = v(&[
            "/bin/claude",
            "--model",
            "old",
            "--effort=low",
            "--dangerously-skip-permissions",
            "-c",
        ]);
        assert_eq!(
            switch_argv(&argv, "abc-1", Some("opus-5.5"), Some("medium")).unwrap(),
            v(&[
                "/bin/claude",
                "--dangerously-skip-permissions",
                "--resume",
                "abc-1",
                "--model",
                "opus-5.5",
                "--effort",
                "medium"
            ])
        );
        // Switching again replaces the earlier resume, it does not stack.
        let once = switch_argv(&argv, "abc-1", Some("a"), None).unwrap();
        assert_eq!(
            switch_argv(&once, "abc-1", Some("b"), None).unwrap(),
            v(&[
                "/bin/claude",
                "--dangerously-skip-permissions",
                "--resume",
                "abc-1",
                "--model",
                "b"
            ])
        );
    }

    #[test]
    fn an_env_prefixed_launch_keeps_its_prefix_and_switches_the_program() {
        // t3912: start-agent records `env TMPDIR=<dir> <program> ...`.
        let switched = switch_argv(
            &v(&[
                "env",
                "TMPDIR=/w/.dunk-scratch/a-1f2e3d",
                "codex",
                "--no-daemon",
                "-m",
                "old",
            ]),
            "s-9",
            Some("new"),
            None,
        )
        .unwrap();
        assert_eq!(
            switched,
            v(&[
                "env",
                "TMPDIR=/w/.dunk-scratch/a-1f2e3d",
                "codex",
                "resume",
                "--no-daemon",
                "-m",
                "new",
                "s-9"
            ])
        );
        let claude = switch_argv(
            &v(&["env", "TMPDIR=/t", "claude", "--model", "a"]),
            "s",
            Some("b"),
            None,
        )
        .unwrap();
        assert_eq!(&claude[..3], &v(&["env", "TMPDIR=/t", "claude"])[..]);
        assert!(claude.contains(&"--resume".to_string()));
        // The prefix is not read as a positional argument of the agent.
        assert!(switch_argv(&v(&["env", "TMPDIR=/t", "A=b", "codex"]), "s", None, None).is_ok());
        // Only an env prefix and no program: still refused.
        assert!(switch_argv(&v(&["env", "TMPDIR=/t"]), "s", None, None).is_err());
    }

    #[test]
    fn codex_uses_the_resume_subcommand_and_keeps_other_settings() {
        let argv = v(&[
            "codex",
            "-m",
            "old",
            "-c",
            "model_reasoning_effort=low",
            "-c",
            "sandbox=x",
            "--yolo",
        ]);
        assert_eq!(
            switch_argv(&argv, "s-9", Some("gpt-sol"), Some("high")).unwrap(),
            v(&[
                "codex",
                "resume",
                "-c",
                "sandbox=x",
                "--yolo",
                "-m",
                "gpt-sol",
                "-c",
                "model_reasoning_effort=high",
                "s-9"
            ])
        );
        let switched = switch_argv(&argv, "s-9", Some("gpt-sol"), None).unwrap();
        assert_eq!(
            switch_argv(&switched, "s-9", Some("gpt-luna"), None).unwrap(),
            v(&[
                "codex",
                "resume",
                "-c",
                "sandbox=x",
                "--yolo",
                "-m",
                "gpt-luna",
                "s-9"
            ]),
            "a lane switched before is switched again without a second resume"
        );
    }

    #[test]
    fn pi_takes_the_session_and_effort_in_its_own_forms() {
        let argv = v(&[
            "pi",
            "--model",
            "old:low",
            "--session",
            "old-s",
            "--thinking",
            "low",
            "--extension",
            "x",
        ]);
        assert_eq!(
            switch_argv(&argv, "p-2", Some("sol"), Some("high")).unwrap(),
            v(&[
                "pi",
                "--extension",
                "x",
                "--session",
                "p-2",
                "--model",
                "sol:high"
            ])
        );
    }

    #[test]
    fn the_rollback_resumes_the_same_session_with_the_old_model_never_a_fresh_one() {
        // A lane launched normally: its launch command has no resume at all.
        let launch = v(&[
            "claude",
            "--model",
            "opus-4",
            "--effort",
            "high",
            "--dangerously-skip-permissions",
        ]);
        let rollback = switch_argv(&launch, "abc-1", Some("opus-4"), Some("high")).unwrap();
        assert_eq!(
            rollback,
            v(&[
                "claude",
                "--dangerously-skip-permissions",
                "--resume",
                "abc-1",
                "--model",
                "opus-4",
                "--effort",
                "high"
            ])
        );
        assert_ne!(
            rollback, launch,
            "the launch command would start a fresh session"
        );
        // With no old model known, the session is still resumed, with the harness default.
        assert_eq!(
            switch_argv(&v(&["codex", "-a", "never"]), "s-9", None, None).unwrap(),
            v(&["codex", "resume", "-a", "never", "s-9"])
        );
        assert_eq!(
            switch_argv(&v(&["pi"]), "p-2", None, Some("low")).unwrap(),
            v(&["pi", "--session", "p-2", "--thinking", "low"])
        );
    }

    #[test]
    fn a_launch_command_with_positional_arguments_is_refused() {
        let refused = |argv: &[&str]| switch_argv(&v(argv), "s", Some("m"), None).unwrap_err();
        // An initial prompt after a flag that takes no value.
        assert!(
            refused(&["claude", "--dangerously-skip-permissions", "start the task"])
                .contains("1 positional argument")
        );
        assert!(refused(&["codex", "--no-daemon", "a prompt"]).contains("positional"));
        assert!(refused(&["pi", "--offline", "hello"]).contains("positional"));
        // Everything after `--` is positional.
        assert!(refused(&["pi", "--model", "x", "--", "a", "b"]).contains("2 positional"));
        // An unknown flag followed by a word is not trusted to be flag and value.
        assert!(refused(&["claude", "--unknown-flag", "word"]).contains("positional"));
        // Known value flags, `=` forms and bare flags with no word after them are fine.
        assert!(switch_argv(
            &v(&[
                "claude",
                "--permission-mode",
                "plan",
                "--settings=x.json",
                "--verbose"
            ]),
            "s",
            Some("m"),
            None
        )
        .is_ok());
        // t4553: a model switch keeps --no-daemon.
        let switched = switch_argv(
            &v(&["codex", "-c", "k=v", "-s", "workspace-write", "--no-daemon"]),
            "s",
            Some("m"),
            None,
        )
        .unwrap();
        assert!(
            switched.contains(&"--no-daemon".to_string()),
            "{switched:?}"
        );
        let again = switch_argv(&switched, "s", Some("n"), None).unwrap();
        assert!(again.contains(&"--no-daemon".to_string()), "{again:?}");
    }

    #[test]
    fn a_harness_without_a_resume_form_is_refused() {
        for argv in [v(&["opencode", "--model", "x"]), v(&["bash"]), Vec::new()] {
            assert!(
                switch_argv(&argv, "s", Some("m"), None).is_err(),
                "{argv:?}"
            );
        }
        assert!(switch_argv(&v(&["opencode"]), "s", Some("m"), None)
            .unwrap_err()
            .contains("a fresh session is never started instead"));
    }

    #[test]
    fn model_and_effort_must_be_plain_values() {
        assert_eq!(
            plain("model", Some(" opus-5.5 ")).unwrap().as_deref(),
            Some("opus-5.5")
        );
        assert_eq!(
            plain("model", Some("anthropic/claude:high"))
                .unwrap()
                .as_deref(),
            Some("anthropic/claude:high")
        );
        assert_eq!(plain("model", Some("  ")).unwrap(), None);
        let long = "x".repeat(129);
        for bad in ["a b", "$(x)", "a;b", long.as_str(), "--yolo", "-m"] {
            assert!(plain("model", Some(bad)).is_err(), "{bad}");
        }
    }
}
