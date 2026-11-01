//! `session_of` (`dunk_state.session_of`): the session a live pane is in, read from its agent
//! PROCESS, with a reason wherever it cannot say.
//!
//! Why not launches.json alone: the flip left launch rows that carry another lane's session (two
//! dead rows carried the VP's), so a recorded id is only trusted when its file exists, and the
//! process in this pane wins over any row. Why not the cwd: several lanes share a workspace, so a
//! directory can hold someone else's session.
//!
//! Every read is bound to the process identity (pid + start ticks) BEFORE and AFTER, and the order
//! and count of those checks mirror the Python line for line: the tests stage a pid recycle at the
//! Nth `proc_start_ticks` call.

use serde_json::Value;

use super::proc::{
    cwd_slug, descendants, read_bound, session_id_from_path, still_the_same_process, Kids,
    ProcReader, Starts,
};
use super::{flag, AGENT_EXES, SESSION_FLAGS};

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_none_or(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// The first `count` characters (`sess_id[:8]`).
fn head(text: &str, count: usize) -> String {
    text.chars().take(count).collect()
}

fn base(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// `session_of(pane_pid, launch, kids, cwd, starts)`: `(session_id, reason)`.
///
/// `pane_pid` and `launch` are JSON because the Python accepts whatever the registry holds (a blank
/// tmux field, a string pid). `kids: None` walks /proc once; `starts: None` is the documented
/// legacy path (no snapshot evidence), while a supplied-but-empty snapshot fails closed.
pub fn session_of(
    reader: &dyn ProcReader,
    pane_pid: Option<&Value>,
    launch: Option<&Value>,
    kids: Option<&Kids>,
    cwd: Option<&str>,
    starts: Option<&Starts>,
) -> (Option<String>, Option<String>) {
    let recorded: Option<String> = launch
        .and_then(|l| {
            l.get("session")
                .filter(|v| truthy(v))
                .or_else(|| l.get("session_id").filter(|v| truthy(v)))
        })
        .and_then(Value::as_str)
        .map(str::to_string);
    let Some(pane_pid) = pane_pid.filter(|v| truthy(v)) else {
        return (
            recorded,
            Some("the pane carries no pid, so its session cannot be read from the process; using the launch row".into()),
        );
    };
    let pid: i64 = match pane_pid {
        Value::Number(n) => match n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)) {
            Some(pid) => pid,
            None => {
                return (
                    recorded,
                    Some("the pane pid is not a number, so its session cannot be read".into()),
                )
            }
        },
        Value::String(text) => match text.trim().parse() {
            Ok(pid) => pid,
            Err(_) => {
                return (
                    recorded,
                    Some("the pane pid is not a number, so its session cannot be read".into()),
                )
            }
        },
        Value::Bool(b) => i64::from(*b),
        _ => {
            return (
                recorded,
                Some("the pane pid is not a number, so its session cannot be read".into()),
            )
        }
    };
    let walked;
    let kids = match kids {
        Some(kids) => kids,
        None => {
            walked = reader.proc_children();
            &walked
        }
    };
    let mut pids = vec![pid];
    pids.extend(descendants(pid, kids));
    let mut agent_pids: Vec<(i64, Vec<String>)> = Vec::new();
    let mut replaced = false;
    for candidate in &pids {
        if !still_the_same_process(reader, *candidate, starts) {
            // t505: the pid in the snapshot is no longer that process, so its argv belongs to
            // somebody else. Never adopt it.
            replaced = true;
            continue;
        }
        let (argv, held) = read_bound(reader, *candidate, starts, |p| reader.cmdline(p));
        let Some(argv) = argv.filter(|_| held) else {
            replaced = true;
            continue;
        };
        if argv.is_empty() {
            continue;
        }
        let is_agent = AGENT_EXES.contains(&base(&argv[0]))
            || argv
                .iter()
                .take(3)
                .any(|tok| AGENT_EXES.contains(&base(tok)));
        if is_agent {
            agent_pids.push((*candidate, argv));
        }
    }
    if agent_pids.is_empty() {
        if replaced {
            return (
                None,
                Some("a descendant pid from the capture snapshot no longer belongs to that process (pid reuse), so its argv cannot name this pane's session".into()),
            );
        }
        return (
            None,
            Some("no agent process under this pane (it is a shell), so there is no session".into()),
        );
    }
    for (_, argv) in &agent_pids {
        let mut value = flag(argv, &SESSION_FLAGS).filter(|v| !v.is_empty());
        if value.is_none() {
            for index in 0..argv.len().saturating_sub(1) {
                if argv[index] == "resume" {
                    value = Some(argv[index + 1].clone()).filter(|v| !v.is_empty());
                    break;
                }
            }
        }
        if let Some(value) = value {
            if value.ends_with(".jsonl") || value.contains('/') {
                return (Some(session_id_from_path(&value)), None);
            }
            return (Some(value), None);
        }
    }
    // Environment next: a harness that exports its session (pi does) is authoritative for the
    // PROCESS - but only when the exported session belongs to this pane's workspace. A session
    // file being written for this workspace right now outranks an environment claim: the
    // environment can be INHERITED from a pane created before this one.
    //
    // The start-dependent derivation is a READ too, and it is the one that can leak worst:
    // `started` decides which session files count as "written since this pane started", so a
    // foreign start time hands us a foreign cutoff. Bind it the same way as every other read, and
    // if the identity moves, derive NOTHING.
    let agent_pid = agent_pids[0].0;
    if !still_the_same_process(reader, agent_pid, starts) {
        return (
            None,
            Some("the agent process behind this pane was replaced while its session was being read, so no process-derived session can be attributed to it".into()),
        );
    }
    let (started, held) = read_bound(reader, agent_pid, starts, |p| reader.proc_start(p));
    let started = started.flatten();
    let (mut active, mut candidates): (Option<String>, usize) = (None, 0);
    if held {
        let (derived, held) = read_bound(reader, agent_pid, starts, |_| {
            reader.active_session_file(cwd, started)
        });
        match derived.filter(|_| held) {
            Some((file, count)) => {
                active = file;
                candidates = count;
            }
            None => replaced = true,
        }
    } else {
        replaced = true;
    }
    let slug = cwd_slug(cwd);
    for (candidate, _) in &agent_pids {
        let (env, held) = read_bound(reader, *candidate, starts, |p| reader.env_of(p));
        let Some(env) = env.filter(|_| held) else {
            // t500 addendum: the process behind this cached pid is no longer the one we
            // validated, so its environment cannot name this pane's session.
            replaced = true;
            continue;
        };
        let mut sess_id = env.get("PI_SESSION_ID").filter(|v| !v.is_empty()).cloned();
        let sess_file = env.get("PI_SESSION_FILE").cloned().unwrap_or_default();
        if sess_id.is_none() && !sess_file.is_empty() {
            sess_id = Some(session_id_from_path(&sess_file));
        }
        let Some(sess_id) = sess_id.filter(|s| !s.is_empty()) else {
            continue;
        };
        if let Some(slug) = &slug {
            if !sess_file.is_empty() && !sess_file.contains(slug.as_str()) {
                if let Some(active) = &active {
                    let active_id = session_id_from_path(active);
                    return (
                        Some(active_id.clone()),
                        Some(format!(
                            "the environment names session {} from another workspace - an inherited claim, not this pane's session; recording {}, the live file for this workspace",
                            head(&sess_id, 8),
                            head(&active_id, 8)
                        )),
                    );
                }
                let parent = sess_file.rsplit_once('/').map_or("", |(dir, _)| dir);
                return (
                    None,
                    Some(format!(
                        "the process environment names session {} from another workspace ({}) - an inherited claim, not this pane's session",
                        head(&sess_id, 8),
                        base(parent)
                    )),
                );
            }
        }
        if let Some(active) = &active {
            let active_id = session_id_from_path(active);
            if active_id.split('-').next() != sess_id.split('-').next() {
                return (
                    Some(active_id.clone()),
                    Some(format!(
                        "the environment claims session {}, but the only session file in this workspace written since the pane started is {} - recording the live file and flagging the environment claim as inherited/stale",
                        head(&sess_id, 8),
                        head(&active_id, 8)
                    )),
                );
            }
        }
        return (Some(sess_id), None);
    }
    for (candidate, _) in &agent_pids {
        let (path, held) = read_bound(reader, *candidate, starts, |p| reader.open_session_file(p));
        let Some(path) = path.filter(|_| held) else {
            replaced = true;
            continue;
        };
        if let Some(path) = path {
            return (Some(session_id_from_path(&path)), None);
        }
    }
    if replaced {
        // A read was refused because the process behind the pid changed, so NO process-derived
        // answer is admissible - not even the start-dependent fallback, whose cutoff came from that
        // same process. This refusal has to come BEFORE the `active` returns (reviewer t500
        // addendum, third shape).
        return (
            None,
            Some("a process under this pane was replaced while its session was being read (pid reuse), so no process-derived session can be attributed to it".into()),
        );
    }
    if let Some(active) = active {
        let active_id = session_id_from_path(&active);
        if candidates > 1 {
            return (
                Some(active_id.clone()),
                Some(format!(
                    "no session in argv, environment or open files; recording the newest of {candidates} session files written since this pane started - {} - which is the weakest admissible source",
                    head(&active_id, 8)
                )),
            );
        }
        return (
            Some(active_id.clone()),
            Some(format!(
                "no session in argv, environment or open files; recording the only session file written in this workspace since the pane started ({})",
                head(&active_id, 8)
            )),
        );
    }
    if let Some(recorded) = recorded {
        return (
            Some(recorded),
            Some(
                "no session in the agent's argv and none open; keeping the recorded launch session"
                    .into(),
            ),
        );
    }
    (
        None,
        Some("the agent's argv carries no --session/--resume and it has no session file open, so this capture cannot say which conversation is in this pane".into()),
    )
}
