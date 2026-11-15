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

/// The command line that resumes `session` with `model` (and `effort`), built from the lane's
/// launch command. The old model, effort and session flags are replaced, not repeated.
pub fn switch_argv(
    argv: &[String],
    session: &str,
    model: &str,
    effort: Option<&str>,
) -> Result<Vec<String>, String> {
    let Some(exe) = argv.first() else {
        return Err("the launch command is empty".into());
    };
    let kind = exe.rsplit('/').next().unwrap_or(exe).to_lowercase();
    let rest = &argv[1..];
    match kind.as_str() {
        "claude" => {
            let mut out = vec![exe.clone()];
            out.extend(strip(
                rest,
                &["--model", "--effort", "--resume", "-r", "--session-id"],
                &["--continue", "-c", "--fork-session"],
            ));
            out.extend(["--resume".into(), session.into(), "--model".into(), model.into()]);
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
            out.extend(["-m".into(), model.into()]);
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
            let model = match effort {
                Some(effort) => format!("{model}:{effort}"),
                None => model.to_string(),
            };
            out.extend(["--session".into(), session.into(), "--model".into(), model]);
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
                    "switch model refused: the session of {pane} is not recorded, so it cannot be resumed; a fresh session is never started instead"
                ))
            })?
            .to_string();
        let new_argv = switch_argv(&argv, &session, &model, effort.as_deref())
            .map_err(|why| refusal(format!("switch model refused: {why}")))?;
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
        let previous = launch.get("model").cloned().unwrap_or(Value::Null);
        let reason = json!(format!("switch model to {model}"));
        let (pane_value, pid_value, start_value) = (json!(pane), json!(pane_pid), json!(start));
        // The idle evidence is the respawn's own: no active turn, no queued outbox message, no
        // unread mail and an input box confirmed ready, all read inside it.
        let mut result = self.respawn_pane(
            services,
            RespawnRequest {
                pane_id: &pane_value,
                argv: &new_argv,
                rollback_argv: Some(&argv),
                expected_pid: Some(&pid_value),
                expected_start: Some(&start_value),
                confirm_idle: true,
                reason: Some(&reason),
                self_pane_id,
                keep_leases: true,
            },
        )?;
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
            switch_argv(&argv, "abc-1", "opus-5.5", Some("medium")).unwrap(),
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
        let again = switch_argv(
            &switch_argv(&argv, "abc-1", "a", None).unwrap(),
            "abc-1",
            "b",
            None,
        )
        .unwrap();
        assert_eq!(
            again,
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
            switch_argv(&argv, "s-9", "gpt-sol", Some("high")).unwrap(),
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
        let switched = switch_argv(&argv, "s-9", "gpt-sol", None).unwrap();
        assert_eq!(
            switch_argv(&switched, "s-9", "gpt-luna", None).unwrap(),
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
            switch_argv(&argv, "p-2", "sol", Some("high")).unwrap(),
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
    fn a_harness_without_a_resume_form_is_refused() {
        for argv in [v(&["opencode", "--model", "x"]), v(&["bash"]), Vec::new()] {
            assert!(switch_argv(&argv, "s", "m", None).is_err(), "{argv:?}");
        }
        assert!(switch_argv(&v(&["opencode"]), "s", "m", None)
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
