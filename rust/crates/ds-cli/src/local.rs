//! Local-only CLI commands (T44), slice 1: `git-credential` and the `gh-token` printer.
//!
//! The pieces that need the daemon or the secrets scanner take them as closures, so the
//! protocol logic is testable without either. NOT here yet: the secrets scanner itself
//! (`dunk_secrets.refusal`, unported), the client wiring, and the rest of T44 (context
//! reads, github protect/wrappers, install, guard install, doctor).

use std::io::{BufRead, Write};

use serde_json::Value;

/// `dunk_secrets.refusal()`: `(message, error)`; a message refuses the push.
pub type Refusal = (Option<String>, Option<String>);

/// `_git_credential(client, operation)`: the git credential protocol on stdin/stdout.
///
/// Only `get` for `https` + `github.com` answers, and only when the daemon hands this lane
/// an App token; every other path prints nothing so git falls through to the owner's own
/// helpers (an empty answer is not an error).
pub fn git_credential(
    operation: &str,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    err: &mut dyn Write,
    secrets_refusal: &dyn Fn() -> Refusal,
    gh_token: &dyn Fn() -> Result<Value, String>,
) -> std::io::Result<()> {
    let mut attrs = std::collections::BTreeMap::new();
    for line in input.lines() {
        let line = line?;
        // Python: `line.rstrip("\n")`, then a blank line ends the request.
        if line.is_empty() {
            break;
        }
        let (key, value) = line.split_once('=').unwrap_or((line.as_str(), ""));
        attrs.insert(key.to_string(), value.to_string());
    }
    if operation != "get" {
        return Ok(());
    }
    let attr = |k: &str| attrs.get(k).map(String::as_str);
    if attr("protocol") != Some("https") || attr("host") != Some("github.com") {
        return Ok(());
    }
    // t756: the credential handshake is the last moment before a push leaves the host, so
    // it is where the desk's own secrets are refused. LIMIT, stated rather than implied:
    // this blocks the lane's App token, but a helper configured behind it (the owner's
    // `gh auth git-credential` fallback) can still authenticate the push, which is why
    // `gh pr create` is checked too and why `.gitignore` carries the same names.
    let (message, error) = secrets_refusal();
    if let Some(message) = message.filter(|m| !m.is_empty()) {
        writeln!(err, "{message}")?;
        writeln!(
            err,
            "REFUSED by the dunkingsheep git credential helper (t756)."
        )?;
        return Ok(());
    }
    if let Some(error) = error.filter(|e| !e.is_empty()) {
        writeln!(
            err,
            "secrets-check: WARNING could not fully check this push: {error}"
        )?;
    }
    // No daemon means no token, not a broken git.
    let Ok(result) = gh_token() else {
        return Ok(());
    };
    let token = match result.get("token") {
        Some(Value::String(token)) if !token.is_empty() => token,
        _ => return Ok(()),
    };
    write!(out, "username=x-access-token\npassword={token}\n")?;
    out.flush()
}

/// The `gh-token` printer: the token on stdout, or (unless `quiet`) why there is none.
pub fn print_gh_token(
    result: &Value,
    quiet: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> std::io::Result<()> {
    if let Some(Value::String(token)) = result.get("token").filter(|t| t.as_str() != Some("")) {
        return writeln!(out, "{token}");
    }
    if !quiet {
        // Python's f-string prints a missing or null field as `None`.
        let show = |key: &str| match result.get(key) {
            None | Some(Value::Null) => "None".to_string(),
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
        };
        writeln!(
            err,
            "no App token for role {}: {}",
            show("role"),
            show("note")
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(
        op: &str,
        stdin: &str,
        refusal: Refusal,
        token: Result<Value, String>,
    ) -> (String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        git_credential(
            op,
            &mut stdin.as_bytes(),
            &mut out,
            &mut err,
            &|| refusal.clone(),
            &|| token.clone(),
        )
        .unwrap();
        (
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    const GITHUB: &str = "protocol=https\nhost=github.com\n\nignored=after-blank\n";

    #[test]
    fn a_token_is_answered_in_the_credential_format() {
        let (out, err) = run("get", GITHUB, (None, None), Ok(json!({"token": "ghs_x"})));
        assert_eq!(out, "username=x-access-token\npassword=ghs_x\n");
        assert_eq!(err, "");
    }

    #[test]
    fn only_get_for_https_github_com_answers() {
        let tok = Ok(json!({"token": "t"}));
        for (op, input) in [
            ("store", GITHUB),
            ("erase", GITHUB),
            ("get", "protocol=http\nhost=github.com\n\n"),
            ("get", "protocol=https\nhost=gitlab.com\n\n"),
            ("get", "host=github.com\n\n"),
        ] {
            assert_eq!(
                run(op, input, (None, None), tok.clone()),
                (String::new(), String::new()),
                "{op} {input:?}"
            );
        }
    }

    #[test]
    fn a_secrets_refusal_prints_both_lines_to_stderr_and_no_credential() {
        let (out, err) = run(
            "get",
            GITHUB,
            (Some("data/ would leave".into()), None),
            Ok(json!({"token": "t"})),
        );
        assert_eq!(out, "");
        assert_eq!(
            err,
            "data/ would leave\nREFUSED by the dunkingsheep git credential helper (t756).\n"
        );
    }

    #[test]
    fn a_scanner_error_warns_but_still_answers() {
        let (out, err) = run(
            "get",
            GITHUB,
            (None, Some("git failed".into())),
            Ok(json!({"token": "t"})),
        );
        assert_eq!(out, "username=x-access-token\npassword=t\n");
        assert_eq!(
            err,
            "secrets-check: WARNING could not fully check this push: git failed\n"
        );
    }

    #[test]
    fn no_daemon_or_no_token_prints_nothing() {
        for token in [
            Err("down".to_string()),
            Ok(json!({})),
            Ok(json!({"token": ""})),
            Ok(json!({"token": null})),
        ] {
            assert_eq!(
                run("get", GITHUB, (None, None), token),
                (String::new(), String::new())
            );
        }
    }

    #[test]
    fn the_gh_token_printer_prints_the_token_or_the_reason() {
        let print = |result: Value, quiet: bool| {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            print_gh_token(&result, quiet, &mut out, &mut err).unwrap();
            (
                String::from_utf8(out).unwrap(),
                String::from_utf8(err).unwrap(),
            )
        };
        assert_eq!(
            print(json!({"token": "abc"}), false),
            ("abc\n".into(), String::new())
        );
        assert_eq!(
            print(
                json!({"role": "owner", "note": "owner uses own login"}),
                false
            ),
            (
                String::new(),
                "no App token for role owner: owner uses own login\n".into()
            )
        );
        assert_eq!(
            print(json!({"role": "owner"}), true),
            (String::new(), String::new())
        );
        assert_eq!(
            print(json!({}), false).1,
            "no App token for role None: None\n"
        );
    }
}
