//! Session registration for a successful Claude launch/respawn (t5427).
//! No process mutation, history search, picker selection or model-switch bypass.
use serde_json::Value;

/// Only a single explicit resume UUID is an unambiguous session. Parse known
/// value-taking flags so a flag's value cannot masquerade as --resume. Unknown
/// options refuse registration; launch argv itself is still recorded unchanged.
fn claude_argv(value: &Value) -> Option<Vec<String>> {
    let argv: Vec<String> = value
        .as_array()?
        .iter()
        .map(|v| v.as_str().map(str::to_owned))
        .collect::<Option<_>>()?;
    if super::launched_program(&argv)? != "claude" {
        return None;
    }
    Some(argv)
}

pub(super) fn is_claude(value: &Value) -> bool {
    claude_argv(value).is_some()
}

pub(super) fn claude_resume(value: &Value) -> Option<String> {
    let argv = claude_argv(value)?;
    let start = super::launched_program_index(&argv)?;
    let mut rest = argv[start + 1..].iter();
    let mut session = None;
    while let Some(token) = rest.next() {
        let (flag, inline) = token
            .split_once('=')
            .map_or((token.as_str(), None), |(k, v)| (k, Some(v)));
        match flag {
            "--resume" | "-r" => {
                let value = inline.or_else(|| rest.next().map(String::as_str))?;
                if session.is_some() || !uuid(value) {
                    return None;
                }
                session = Some(value.to_owned());
            }
            // These can select/create another session or suppress its history.
            "--continue"
            | "-c"
            | "--fork-session"
            | "--session-id"
            | "--no-session-persistence" => return None,
            flag if super::switch_model::value_flags("claude").contains(&flag) => {
                if inline.is_none() {
                    rest.next()?;
                }
            }
            "--dangerously-skip-permissions"
            | "--allow-dangerously-skip-permissions"
            | "--verbose"
            | "--strict-mcp-config"
            | "--ide"
            | "--chrome"
            | "--no-chrome"
                if inline.is_none() => {}
            _ => return None,
        }
    }
    session
}

fn uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    const SESSION: &str = "11111111-2222-4333-8444-555555555555";

    #[test]
    fn explicit_resume_uuid_survives_known_flags_and_wrappers() {
        for argv in [
            json!([
                "claude",
                "--model",
                "claude-opus-5-5",
                "--effort",
                "medium",
                "--dangerously-skip-permissions",
                "--resume",
                SESSION
            ]),
            json!([
                "/bin/env",
                "TMPDIR=/work/scratch",
                "/bin/claude",
                "-r",
                SESSION
            ]),
            json!(["claude", format!("--resume={SESSION}"), "--model=opus"]),
        ] {
            assert_eq!(claude_resume(&argv).as_deref(), Some(SESSION), "{argv}");
        }
    }

    #[test]
    fn picker_fork_prompt_conflict_and_unknown_forms_never_register_a_session() {
        for argv in [
            json!(["claude"]),
            json!(["claude", "--resume"]),
            json!(["claude", "--resume", "search term"]),
            json!(["claude", "--resume", "--model", "opus"]),
            json!(["claude", "--continue"]),
            json!(["claude", "--resume", SESSION, "--fork-session"]),
            json!(["claude", "--resume", SESSION, "--session-id", SESSION]),
            json!(["claude", "--resume", SESSION, "--no-session-persistence"]),
            json!(["claude", "--resume", SESSION, "-r", SESSION]),
            json!(["claude", "--append-system-prompt", "--resume", SESSION]),
            json!(["claude", "--unknown", "--resume", SESSION]),
            json!(["claude", "--resume", SESSION, "initial prompt"]),
            json!(["claude", "--", "--resume", SESSION]),
            json!(["codex", "resume", SESSION]),
            json!(["claude", "--resume", 42]),
        ] {
            assert_eq!(claude_resume(&argv), None, "{argv}");
        }
    }
}
