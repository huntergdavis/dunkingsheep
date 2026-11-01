//! T44, slice 8: the daemon-backed command path of `main()`: connect, call, and map failures
//! to the exit codes the Python CLI uses (`DunkError` -> "error: ..." exit 2; the socket
//! deadline -> exit 3; `DaemonUnavailable` / `OSError` -> "error: ..." exit 3), plus the two
//! commands that need a daemon answer but print locally: `gh-token` and `git-credential`.
//!
//! DIFFERENCE, on purpose: the Python CLI starts the daemon when it is absent. The Rust one
//! never does (`start = false`): starting runs `<this binary> serve`, which is not ported, and
//! the owner's rule is that the Python daemon (systemd) stays the one that runs. A missing
//! daemon is therefore "error: no dunkingsheep daemon on <socket>", exit 3.
//!
//! NOT identical: an `OSError` prints Rust's io text, not Python's `[Errno N] ...`.

use std::io::BufRead;
use std::path::PathBuf;

use ds_core::client::{connect_or_start_system, ClientError, FlockClient, UnixTransport};
use ds_core::error::DunkError;
use serde_json::{Map, Value};

use crate::usage_cmd::{sorted_json, Outcome};

fn fail(text: String, code: i32) -> Outcome {
    Outcome {
        stdout: String::new(),
        stderr: format!("error: {text}\n"),
        code,
    }
}

fn connect_failure(error: DunkError) -> Outcome {
    match error {
        // Python's DaemonUnavailable prints its message as is.
        DunkError::Unavailable(message) => fail(message, 3),
        other => fail(other.to_string(), other.exit_code().max(2)),
    }
}

fn call_failure(error: ClientError, timeout_s: f64) -> Outcome {
    match error {
        ClientError::Dunk(error) => fail(error.to_string(), 2),
        ClientError::Io(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
            ) =>
        {
            fail(
                format!("no answer after {}s from the daemon", timeout_s as i64),
                3,
            )
        }
        ClientError::Io(error) => fail(error.to_string(), 3),
        ClientError::Unavailable(message) | ClientError::Protocol(message) => fail(message, 3),
    }
}

/// `FlockClient.connect_or_start(socket_path=args.socket, self_pane_id=args.self_pane,
/// start=False)`.
pub fn connect(ns: &Map<String, Value>) -> Result<FlockClient<UnixTransport>, Outcome> {
    let socket = ns
        .get("socket")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from);
    let pane = ns
        .get("self_pane")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("dunkingsheep"));
    connect_or_start_system(
        socket.as_deref(),
        pane,
        false,
        std::time::Duration::from_secs(10),
        &exe,
    )
    .map_err(connect_failure)
}

/// One daemon call with the CLI's error mapping.
pub fn call(
    client: &mut FlockClient<UnixTransport>,
    cmd: &str,
    args: Map<String, Value>,
) -> Result<Value, Outcome> {
    let timeout = client.timeout.as_secs_f64();
    client.call(cmd, args).map_err(|e| call_failure(e, timeout))
}

/// `gh-token [--kind author|reviewer] [--quiet]`: the token on stdout, or (unless `--quiet`) why
/// there is none on stderr; exit 0 either way.
pub fn gh_token(ns: &Map<String, Value>) -> Outcome {
    let mut client = match connect(ns) {
        Ok(client) => client,
        Err(outcome) => return outcome,
    };
    let kind = ns
        .get("kind")
        .filter(|k| k.is_string())
        .cloned()
        .unwrap_or(Value::Null);
    let mut args = Map::new();
    args.insert("kind".into(), kind);
    let result = match call(&mut client, "gh_token", args) {
        Ok(result) => result,
        Err(outcome) => return outcome,
    };
    if matches!(ns.get("json"), Some(Value::Bool(true))) {
        return Outcome {
            stdout: format!("{}\n", sorted_json(&result)),
            stderr: String::new(),
            code: 0,
        };
    }
    let quiet = matches!(ns.get("quiet"), Some(Value::Bool(true)));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let _ = crate::local::print_gh_token(&result, quiet, &mut out, &mut err);
    Outcome {
        stdout: String::from_utf8_lossy(&out).into_owned(),
        stderr: String::from_utf8_lossy(&err).into_owned(),
        code: 0,
    }
}

/// `git-credential [get|store|erase]`: the git credential protocol on `stdin`, with the REAL
/// secrets guard and the daemon's `gh_token`. A daemon that cannot be reached for the TOKEN is
/// silence ("no daemon means no token, not a broken git"); one that cannot be reached at
/// CONNECT time is the CLI's own exit-3 error, as in the Python.
pub fn git_credential(ns: &Map<String, Value>, stdin: &mut dyn BufRead) -> Outcome {
    let mut client = match connect(ns) {
        Ok(client) => client,
        Err(outcome) => return outcome,
    };
    let operation = ns.get("operation").and_then(Value::as_str).unwrap_or("get");
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let client = std::cell::RefCell::new(&mut client);
    let token = || -> Result<Value, String> {
        client
            .borrow_mut()
            .call("gh_token", Map::new())
            .map_err(|e| e.to_string())
    };
    let refusal = || crate::secrets_cmd::guard_refusal(&cwd);
    let _ = crate::local::git_credential(operation, stdin, &mut out, &mut err, &refusal, &token);
    Outcome {
        stdout: String::from_utf8_lossy(&out).into_owned(),
        stderr: String::from_utf8_lossy(&err).into_owned(),
        code: 0,
    }
}
