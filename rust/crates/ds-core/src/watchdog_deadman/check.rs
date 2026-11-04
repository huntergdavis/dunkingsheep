//! Deadman check: actual state persistence and verified inbox fallback, with a supplied sender.
use super::*;
use crate::client::ClientError;
use crate::watchdog::{minutes, py_number};

pub struct CheckOptions<'a> {
    pub heartbeat: &'a Path,
    pub cadence_minutes: Value,
    pub multiplier: Value,
    pub target: &'a str,
    pub state_path: &'a Path,
    pub cooldown_minutes: f64,
    pub now: f64,
    pub dry_run: bool,
    pub error_file: Option<&'a Path>,
    pub status_path: Option<&'a Path>,
    pub inbox_path: Option<&'a Path>,
    pub daemon_path: Option<&'a str>,
    pub expected_dir: Option<&'a Path>,
}

/// Preserve transport kind and optional filename for the existing conservative classifier.
#[derive(Debug)]
pub struct CheckSendError {
    pub name: String,
    pub message: String,
    pub io_error: Option<io::Error>,
    pub filename: Option<PathBuf>,
}
impl From<ClientError> for CheckSendError {
    fn from(error: ClientError) -> Self {
        match error {
            ClientError::Io(error) => error.into(),
            other => Self {
                name: match &other {
                    ClientError::Dunk(_) => "DunkError",
                    ClientError::Unavailable(_) => "DaemonUnavailable",
                    _ => "ValueError",
                }
                .into(),
                message: other.to_string(),
                io_error: None,
                filename: None,
            },
        }
    }
}
impl From<io::Error> for CheckSendError {
    fn from(error: io::Error) -> Self {
        let name = match error.kind() {
            io::ErrorKind::ConnectionRefused => "ConnectionRefusedError",
            io::ErrorKind::ConnectionReset => "ConnectionResetError",
            io::ErrorKind::BrokenPipe => "BrokenPipeError",
            io::ErrorKind::WouldBlock => "BlockingIOError",
            io::ErrorKind::NotFound => "FileNotFoundError",
            io::ErrorKind::NotADirectory => "NotADirectoryError",
            io::ErrorKind::TimedOut => "TimeoutError",
            io::ErrorKind::PermissionDenied => "PermissionError",
            io::ErrorKind::IsADirectory => "IsADirectoryError",
            _ => "OSError",
        };
        Self {
            name: name.into(),
            message: match error.raw_os_error() {
                Some(code) => {
                    let text = error.to_string();
                    let suffix = format!(" (os error {code})");
                    format!(
                        "[Errno {code}] {}",
                        text.strip_suffix(&suffix).unwrap_or(&text)
                    )
                }
                None => error.to_string(),
            },
            io_error: Some(error),
            filename: None,
        }
    }
}
fn refuse(text: &str) -> DunkError {
    DunkError::Refused(text.into())
}
fn fail(
    options: &CheckOptions<'_>,
    mut report: Value,
    error: String,
    attempted: &str,
    cause: &str,
) -> Result<Value, DunkError> {
    report["delivery_error"] = json!(error);
    report["cause"] = json!(cause);
    if let Some(path) = options.status_path {
        save(
            path,
            &json!({"rule":"watchdog.deadman","at":options.now,"error":error,"cause":cause,"heartbeat":options.heartbeat,"target":options.target,"daemon_path":options.daemon_path,"attempted":attempted,"config_error":report["config_error"]}),
        )?;
    }
    eprintln!(
        "watchdog.deadman: VP alert NOT delivered [{cause}] ({error}); status written to {}",
        options
            .status_path
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "None".into())
    );
    Ok(report)
}

/// `send` normally wraps `daemon_sender`; tests supply only fake transport. Direct fallback
/// always uses the real temporary/production store selected by the caller, never a success stub.
pub fn check(
    options: &CheckOptions<'_>,
    mut send: impl FnMut(&str, &str, &str) -> Result<(), CheckSendError>,
) -> Result<Value, DunkError> {
    let unit = options
        .cadence_minutes
        .as_f64()
        .ok_or_else(|| refuse("cadence_minutes must be numeric"))?;
    let multiplier = options
        .multiplier
        .as_f64()
        .ok_or_else(|| refuse("multiplier must be numeric"))?;
    if unit <= 0.0 || multiplier <= 0.0 || options.cooldown_minutes < 0.0 {
        return Err(refuse(
            "cadence_minutes and multiplier must be > 0, cooldown_minutes >= 0",
        ));
    }
    let age = heartbeat_age(options.heartbeat, options.now)?;
    let (recorded, problem) = if age.is_some() {
        recorded_cadence(options.heartbeat)
    } else {
        (None, None)
    };
    let (cadence, clamped) = effective_cadence(&options.cadence_minutes, recorded.as_ref());
    let limit = multiplier * cadence.as_f64().unwrap() * 60.0;
    let stale = age.is_none_or(|age| age > limit);
    let mut notes = Vec::new();
    if let Some(problem) = &problem {
        notes.push(format!(
            "heartbeat {}: {problem}; using the unit cadence {} min.",
            options.heartbeat.display(),
            py_number(&options.cadence_minutes)
        ));
    }
    if clamped {
        notes.push(format!("heartbeat {} records cadence {} min, more than {CLAMP_FACTOR} x the unit cadence {} min; clamped to {} min. Check the watchdog config and the dead-man unit agree.",options.heartbeat.display(),py_number(recorded.as_ref().unwrap()),py_number(&options.cadence_minutes),py_number(&cadence)));
    }
    let mut report = json!({"rule":"watchdog.deadman","heartbeat":options.heartbeat,"age_minutes":age.map(minutes),"limit_minutes":minutes(limit),"stale":stale,"alerted":false,"reason":null,"mode":if options.dry_run {"dry-run"} else {"live"},"unit_cadence_minutes":options.cadence_minutes,"recorded_cadence_minutes":recorded,"effective_cadence_minutes":cadence,"clamped":clamped,"heartbeat_problem":problem,"notes":notes});
    if !stale {
        for note in &notes {
            eprintln!("watchdog.deadman: {note}");
        }
        return Ok(report);
    }
    let mut state = load(options.state_path)?;
    if !state.is_object() {
        return Err(refuse("deadman state must be an object"));
    }
    if let Some(last) = state.get("last_alerted").filter(|v| !v.is_null()) {
        let last = last
            .as_f64()
            .ok_or_else(|| refuse("last_alerted must be numeric"))?;
        if options.now - last < options.cooldown_minutes * 60.0 {
            report["reason"] = json!("cooldown");
            return Ok(report);
        }
    }
    let what = match age {
        None => "is missing (the watchdog has never completed a live run)".into(),
        Some(age) => format!(
            "is {:?} min old (limit {:?} min = {} x cadence {} min)",
            minutes(age),
            minutes(limit),
            py_number(&options.multiplier),
            py_number(&cadence)
        ),
    };
    let config_error = options.error_file.map(read_error).transpose()?.flatten();
    report["config_error"] = json!(config_error);
    let mut body = format!("[critical] watchdog.deadman: the watchdog heartbeat {} {what}. Fleet exceptions are NOT being watched until it runs again. Check `systemctl --user status dunkingsheep-watchdog.timer` and `dunkingsheep watchdog --dry-run`.", options.heartbeat.display());
    if let Some(error) = config_error {
        body.push_str(&format!(
            "\nThe watchdog refused to start with this raw config error:\n{error}"
        ));
    }
    for note in notes {
        body.push_str(&format!("\n{note}"));
    }
    if options.dry_run {
        report["reason"] = json!("would alert (dry run)");
        return Ok(report);
    }
    let incident = age
        .map(|age| format!("mtime:{:.0}", options.now - age))
        .unwrap_or_else(|| "missing".into());
    if let Some(problem) = daemon_path_problem(options.daemon_path, options.expected_dir) {
        return fail(
            options,
            report,
            format!("CONFIG: {problem}"),
            "none",
            "config_bad",
        );
    }
    let subject = "watchdog.deadman: watchdog heartbeat stale";
    match send(options.target, subject, &body) {
        Ok(()) => report["delivered_via"] = json!("daemon"),
        Err(error) => {
            let text = format!("{}: {}", error.name, error.message);
            report["daemon_error"] = json!(text);
            let cause = error
                .io_error
                .as_ref()
                .map(|io| {
                    classify_daemon_error(
                        io,
                        error.filename.as_deref(),
                        options.daemon_path,
                        options.expected_dir,
                    )
                })
                .unwrap_or("other");
            if cause == "config_bad" {
                return fail(
                    options,
                    report,
                    format!(
                        "CONFIG: daemon path {}: {text}",
                        options.daemon_path.unwrap_or("None")
                    ),
                    "daemon",
                    cause,
                );
            }
            if cause != "daemon_down" {
                return fail(
                    options,
                    report,
                    format!("daemon send failed without proof the daemon is down: {text}"),
                    "daemon",
                    cause,
                );
            }
            if state.get("direct_incident") != Some(&json!(incident)) {
                let direct = options
                    .inbox_path
                    .ok_or_else(|| DirectWriteError::store(refuse("inbox path is missing")))
                    .and_then(|path| direct_write_context(path, options.target, subject, &body));
                match direct {
                    Ok(id) => report["direct_message_id"] = json!(id),
                    Err(error) => {
                        return fail(
                            options,
                            report,
                            format!(
                                "no daemon ({text}); direct write failed: {}: {}",
                                error.name, error.message
                            ),
                            "direct",
                            "daemon_down",
                        )
                    }
                }
                state["direct_incident"] = json!(incident);
            }
            report["delivered_via"] = json!("direct");
        }
    }
    state["last_alerted"] = json!(options.now);
    save(options.state_path, &state)?;
    report["alerted"] = json!(true);
    Ok(report)
}

#[cfg(test)]
#[path = "check_tests.rs"]
mod tests;
