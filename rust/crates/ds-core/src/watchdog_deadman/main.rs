//! `dunk_watchdog_deadman.main`: invoke check, print its report, map its cause to an exit.
use super::*;
use crate::client::{FlockClient, Transport};
use crate::config::Env;
use std::io::Write;

/// Parsed CLI values. Socket/default/credential resolution belongs to the calling CLI.
/// The configured client is used only if check actually attempts daemon delivery.
pub struct MainArgs<'a> {
    pub heartbeat: &'a str,
    pub cadence_minutes: Value,
    pub multiplier: Value,
    pub to: &'a str,
    pub state: &'a str,
    pub cooldown_minutes: f64,
    pub now: Option<f64>,
    pub dry_run: bool,
    pub error_file: &'a str,
    pub status: &'a str,
    pub inbox: &'a str,
    pub daemon_path: Option<&'a str>,
    pub expected_dir: Option<&'a Path>,
    pub json: bool,
}

/// Emit one stdout line after check succeeds, then return Python's 0/1/2 mapping.
/// Check/output failures propagate; they are not converted into a successful report.
pub fn main<T: Transport>(
    args: &MainArgs<'_>,
    client: FlockClient<T>,
    output: &mut impl Write,
) -> Result<i32, DunkError> {
    main_with_env(args, client, output, &Env::from_process())
}
fn main_with_env<T: Transport>(
    args: &MainArgs<'_>,
    client: FlockClient<T>,
    output: &mut impl Write,
    env: &Env,
) -> Result<i32, DunkError> {
    let expand = |path| {
        crate::terminal::tmux_registry::expand_home(path, env)
            .map_err(|error| DunkError::Refused(error.to_string()))
    };
    let heartbeat = expand(args.heartbeat)?;
    let state = expand(args.state)?;
    let now = args.now.unwrap_or_else(crate::clock::now_s);
    let error_file = expand(args.error_file)?;
    let status = expand(args.status)?;
    let inbox = expand(args.inbox)?;
    let options = CheckOptions {
        heartbeat: &heartbeat,
        cadence_minutes: args.cadence_minutes.clone(),
        multiplier: args.multiplier.clone(),
        target: args.to,
        state_path: &state,
        cooldown_minutes: args.cooldown_minutes,
        now,
        dry_run: args.dry_run,
        error_file: Some(&error_file),
        status_path: Some(&status),
        inbox_path: Some(&inbox),
        daemon_path: args.daemon_path,
        expected_dir: args.expected_dir,
    };
    let mut send = daemon_sender(client);
    let report = check(&options, |to, subject, body| {
        send(to, subject, body).map_err(Into::into)
    })?;
    if args.json {
        writeln!(
            output,
            "{}",
            crate::usage::dumps_sorted(&report).replace('\u{7f}', "\\u007f")
        )?;
    } else {
        let status = if report["stale"] == true {
            "STALE"
        } else {
            "fresh"
        };
        let reason = report["reason"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(|reason| format!(" ({reason})"))
            .unwrap_or_default();
        writeln!(
            output,
            "watchdog.deadman {}: heartbeat {status} age={} min limit={} min alerted={}{}",
            report["mode"].as_str().unwrap(),
            python_repr(&report["age_minutes"]),
            python_repr(&report["limit_minutes"]),
            python_repr(&report["alerted"]),
            reason
        )?;
    }
    Ok(if report["cause"] == "config_bad" {
        2
    } else if report["delivery_error"]
        .as_str()
        .is_some_and(|s| !s.is_empty())
    {
        1
    } else {
        0
    })
}

#[cfg(test)]
#[path = "main_tests.rs"]
mod tests;
