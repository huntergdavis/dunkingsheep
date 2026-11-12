//! Helpers for a future opt-in terminal stream. This module does not expose a route.

use serde_json::{Map, Value};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// Validate an external Host against explicitly configured authorities.
pub fn valid_terminal_host(headers: &axum::http::HeaderMap, allowed_hosts: &[String]) -> bool {
    let mut hosts = headers.get_all(axum::http::header::HOST).iter();
    let Some(host) = hosts.next().and_then(|v| v.to_str().ok()) else {
        return false;
    };
    hosts.next().is_none()
        && !host.is_empty()
        && !host.contains(['/', '\\', '@', ' ', '\t', ','])
        && allowed_hosts
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(host))
}

/// Browser WebSocket handshakes require exactly one explicit Origin and Host.
/// The scheme is server configuration, never a client forwarding header.
pub fn valid_websocket_origin(
    headers: &axum::http::HeaderMap,
    scheme: crate::routes::auth::RequestScheme,
    allowed_hosts: &[String],
) -> bool {
    use axum::http::header::{HOST, ORIGIN};
    let mut origins = headers.get_all(ORIGIN).iter();
    let mut hosts = headers.get_all(HOST).iter();
    let (Some(origin), Some(host)) = (origins.next(), hosts.next()) else {
        return false;
    };
    if origins.next().is_some() || hosts.next().is_some() {
        return false;
    }
    let (Ok(origin), Ok(host)) = (origin.to_str(), host.to_str()) else {
        return false;
    };
    if !valid_terminal_host(headers, allowed_hosts) {
        return false;
    }
    let protocol = match scheme {
        crate::routes::auth::RequestScheme::Http => "http",
        crate::routes::auth::RequestScheme::Https => "https",
    };
    origin == format!("{protocol}://{host}")
}

/// A tmux control client. The caller must establish and continually recheck
/// authorization; this transport alone grants no access and is not a route.
pub struct ControlClient {
    child: Child,
    input: ChildStdin,
    output: Option<ControlOutput>,
    pane: String,
}

pub struct ControlOutput {
    output: BufReader<ChildStdout>,
    pane: String,
}

impl ControlClient {
    pub fn attach(
        tmux: &std::path::Path,
        socket: Option<&str>,
        session: &str,
        pane: &str,
    ) -> io::Result<Self> {
        fn valid_id(id: &str, prefix: char) -> bool {
            id.strip_prefix(prefix)
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        }
        if !valid_id(session, '$') || !valid_id(pane, '%') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid tmux identity",
            ));
        }
        let mut command = Command::new(tmux);
        if let Some(socket) = socket {
            command.args(["-L", socket]);
        }
        let mut child = command
            .args([
                "-C",
                "attach-session",
                "-E",
                "-f",
                "ignore-size,active-pane",
                "-t",
                session,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let input = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("missing tmux stdin"))?;
        let output = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| io::Error::other("missing tmux stdout"))?,
        );
        Ok(Self {
            child,
            input,
            output: Some(ControlOutput {
                output,
                pane: pane.to_owned(),
            }),
            pane: pane.to_owned(),
        })
    }

    /// Write a command to the control pipe; this does not establish that tmux accepted it.
    pub fn send(&mut self, bytes: &[u8]) -> io::Result<()> {
        let command = input_command(&self.pane, bytes)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid input frame"))?;
        self.input.write_all(&command)?;
        self.input.flush()
    }

    /// Move the reader to a supervised worker so input and cancellation do not
    /// wait for terminal output. Dropping the client kills the control process.
    pub fn take_output(&mut self) -> Option<ControlOutput> {
        self.output.take()
    }
}

impl ControlOutput {
    /// Read a bounded control line, including command replies and errors.
    pub fn read_line(&mut self) -> io::Result<Vec<u8>> {
        let mut line = Vec::new();
        let n = (&mut self.output)
            .take((ControlLines::MAX_LINE + 1) as u64)
            .read_until(b'\n', &mut line)?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "tmux control closed",
            ));
        }
        if line.len() > ControlLines::MAX_LINE || !line.ends_with(b"\n") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid tmux control line",
            ));
        }
        Ok(line)
    }

    /// Extract this pane's output from a control line. Callers must separately
    /// process command replies and errors instead of treating a pipe write as success.
    pub fn output_from_line(&self, line: &[u8]) -> io::Result<Option<Vec<u8>>> {
        pane_output(line, &self.pane).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

impl Drop for ControlClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Incrementally frame control-mode lines without allowing an unbounded line.
/// Close the connection on an error or incomplete EOF.
#[derive(Default)]
pub struct ControlLines {
    pending: Vec<u8>,
}

/// Correlates one serialized command with tmux's begin/end or begin/error
/// markers. The caller must drain startup replies before starting a command.
#[derive(Default)]
pub struct ControlReply {
    marker: Option<Vec<u8>>,
}

impl ControlReply {
    /// Returns `Some(true)` for an accepted command and `Some(false)` for a
    /// tmux error. Output notifications and command text do not complete it.
    pub fn observe(&mut self, line: &[u8]) -> Result<Option<bool>, &'static str> {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        let (kind, marker) = if let Some(marker) = line.strip_prefix(b"%begin ") {
            (0, marker)
        } else if let Some(marker) = line.strip_prefix(b"%end ") {
            (1, marker)
        } else if let Some(marker) = line.strip_prefix(b"%error ") {
            (2, marker)
        } else {
            return Ok(None);
        };
        let fields: Vec<_> = marker.split(|byte| *byte == b' ').collect();
        if fields.len() != 3
            || fields
                .iter()
                .any(|field| field.is_empty() || !field.iter().all(u8::is_ascii_digit))
        {
            return Err("invalid tmux reply marker");
        }
        match kind {
            0 if self.marker.is_none() => {
                self.marker = Some(marker.to_vec());
                Ok(None)
            }
            0 => Err("nested tmux reply"),
            _ if self.marker.as_deref() == Some(marker) => {
                self.marker = None;
                Ok(Some(kind == 1))
            }
            _ => Err("unmatched tmux reply"),
        }
    }
}

impl ControlLines {
    pub const MAX_LINE: usize = 256 * 1024;

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<Vec<u8>>, &'static str> {
        if chunk.len() > Self::MAX_LINE {
            return Err("tmux control chunk too long");
        }
        let mut lines = Vec::new();
        for part in chunk.split_inclusive(|byte| *byte == b'\n') {
            if part.len() > Self::MAX_LINE.saturating_sub(self.pending.len()) {
                return Err("tmux control line too long");
            }
            self.pending.extend_from_slice(part);
            if part.last() == Some(&b'\n') {
                lines.push(std::mem::take(&mut self.pending));
            }
        }
        Ok(lines)
    }

    pub fn finish(&self) -> Result<(), &'static str> {
        if self.pending.is_empty() {
            Ok(())
        } else {
            Err("incomplete tmux control line")
        }
    }
}

/// Read fresh managed, role, tmux identity and process-start snapshots. A
/// missing or malformed source denies access. This is a point-in-time check,
/// not an atomic lock over registry changes or tmux process replacement.
pub fn resolve_live_agent(
    name: &str,
    backend: &ds_core::terminal::tmux::TmuxBackend,
    roles: &mut ds_core::roles::Roles,
) -> Option<(String, i64, String)> {
    let registry = backend.registry().load().ok()?;
    let panes = registry.get("panes")?.as_object()?;
    roles.refresh(true).ok()?;
    let result = backend
        .run(&[
            "list-panes",
            "-a",
            "-F",
            ds_core::terminal::identity::identity_format(),
        ])
        .ok()?;
    if result.code != 0 {
        return None;
    }
    let identities = ds_core::terminal::identity::parse_pane_identities(&result.stdout).ok()?;
    if !unique_pane_ids(&identities) {
        return None;
    }
    let live = identities
        .into_iter()
        .filter_map(|(id, pid)| {
            let pid = pid?;
            Some((id, pid, ds_core::proc::process_start(pid)?))
        })
        .collect::<Vec<_>>();
    if name == OWNER_TERMINAL_NAME {
        if owner_namespace_collision(panes) {
            return None;
        }
        eligible_owner(&roles.lanes, &live)
    } else {
        eligible_agent(name, panes, &roles.lanes, &live)
    }
}

/// List uniquely named managed agents and the single unambiguous live owner.
/// Every source is a point-in-time snapshot; a later request revalidates it.
pub fn list_live_agents(
    backend: &ds_core::terminal::tmux::TmuxBackend,
    roles: &mut ds_core::roles::Roles,
) -> Option<Vec<Value>> {
    let registry = backend.registry().load().ok()?;
    let managed = registry.get("panes")?.as_object()?;
    roles.refresh(true).ok()?;
    let geometry = backend
        .run(&[
            "list-panes",
            "-a",
            "-F",
            "#{pane_id}|#{pane_pid}|#{pane_width}|#{pane_height}",
        ])
        .ok()?;
    if geometry.code != 0 {
        return None;
    }
    let mut live = Vec::new();
    let mut sizes = std::collections::BTreeMap::new();
    let mut duplicate_ids = std::collections::BTreeSet::new();
    for line in geometry.stdout.lines() {
        let fields: Vec<_> = line.split('|').collect();
        let [id, pid, cols, rows] = fields.as_slice() else {
            return None;
        };
        let digits = id.strip_prefix('%')?;
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let (pid, cols, rows) = (
            pid.parse::<i64>().ok()?,
            cols.parse::<u16>().ok()?,
            rows.parse::<u16>().ok()?,
        );
        if pid <= 0 || cols == 0 || rows == 0 {
            return None;
        }
        if let Some(start) = ds_core::proc::process_start(pid) {
            live.push(((*id).to_owned(), pid, start));
        }
        if sizes.insert(*id, (cols, rows)).is_some() {
            duplicate_ids.insert(*id);
        }
    }
    let mut result = Vec::new();
    for record in managed.values() {
        let Some(name) = record.get("name").and_then(Value::as_str) else {
            continue;
        };
        if name.contains('/')
            || name
                .strip_prefix('%')
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        let Some((pane, _, _)) = eligible_agent(name, managed, &roles.lanes, &live) else {
            continue;
        };
        if duplicate_ids.contains(pane.as_str()) {
            continue;
        }
        let Some((cols, rows)) = sizes.get(pane.as_str()) else {
            continue;
        };
        let agent = record
            .get("agent_kind")
            .and_then(Value::as_str)
            .map(Value::from)
            .unwrap_or(Value::Null);
        result.push(serde_json::json!({"name":name,"pane_id":pane,"agent":agent,"state":null,"state_reason":"no activity source", "cols":cols,"rows":rows}));
    }
    if !owner_namespace_collision(managed) {
        if let Some((pane, _, _)) = eligible_owner(&roles.lanes, &live) {
            if !duplicate_ids.contains(pane.as_str()) {
                if let Some((cols, rows)) = sizes.get(pane.as_str()) {
                    result.push(serde_json::json!({"name": OWNER_TERMINAL_NAME, "pane_id": pane, "agent": "owner", "state": null, "state_reason": "no activity source", "cols": cols, "rows": rows}));
                }
            }
        }
    }
    result.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Some(result)
}

fn unique_pane_ids(identities: &[(String, Option<i64>)]) -> bool {
    let mut seen = std::collections::BTreeSet::new();
    identities.iter().all(|(id, _)| seen.insert(id))
}

const OWNER_TERMINAL_NAME: &str = "__owner_terminal__";

fn owner_namespace_collision(managed: &Map<String, Value>) -> bool {
    managed.values().any(|record| {
        ["name", "agent"]
            .iter()
            .any(|field| record.get(*field).and_then(Value::as_str) == Some(OWNER_TERMINAL_NAME))
    })
}

/// Resolve the owner only through one unambiguous owner-role registration and
/// its live pane generation. A pane id provided by a client is never authority.
pub fn eligible_owner(
    roles: &std::collections::BTreeMap<String, Map<String, Value>>,
    live: &[(String, i64, String)],
) -> Option<(String, i64, String)> {
    let owners: Vec<_> = roles
        .iter()
        .filter(|(_, role)| role.get("role").and_then(Value::as_str) == Some("owner"))
        .collect();
    let [(pane, role)] = owners.as_slice() else {
        return None;
    };
    let digits = pane.strip_prefix('%')?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let pid = role.get("pane_pid")?.as_i64().filter(|pid| *pid > 0)?;
    let start = role
        .get("start")?
        .as_str()
        .filter(|start| !start.is_empty())?;
    let identities: Vec<_> = live.iter().filter(|(id, _, _)| id == *pane).collect();
    let [(id, current_pid, current_start)] = identities.as_slice() else {
        return None;
    };
    if *current_pid != pid || current_start != start {
        return None;
    }
    Some((id.clone(), pid, start.to_owned()))
}

/// Resolve a named managed agent only when its role registration binds the live
/// pane process and process start. Re-run this check before each input operation.
/// The caller must supply a fresh, unambiguous tmux identity listing and fresh
/// registry snapshots; labels or pane ids alone are not authority.
pub fn eligible_agent(
    name: &str,
    managed: &Map<String, Value>,
    roles: &std::collections::BTreeMap<String, Map<String, Value>>,
    live: &[(String, i64, String)],
) -> Option<(String, i64, String)> {
    if name == OWNER_TERMINAL_NAME || name.is_empty() || name.trim() != name {
        return None;
    }
    let matches: Vec<_> = managed
        .iter()
        .filter(|(_, record)| {
            record.get("name").and_then(Value::as_str) == Some(name)
                && record.get("agent").and_then(Value::as_str) == Some(name)
        })
        .collect();
    let [(pane, _)] = matches.as_slice() else {
        return None;
    };
    let role = roles.get(*pane)?;
    if !matches!(
        role.get("role").and_then(Value::as_str),
        Some("desk" | "planner" | "worker" | "reviewer")
    ) {
        return None;
    }
    let pid = role.get("pane_pid")?.as_i64().filter(|pid| *pid > 0)?;
    let start = role
        .get("start")?
        .as_str()
        .filter(|start| !start.is_empty())?;
    let identities: Vec<_> = live.iter().filter(|(id, _, _)| id == *pane).collect();
    let [(id, current_pid, current_start)] = identities.as_slice() else {
        return None;
    };
    if *current_pid != pid || current_start != start {
        return None;
    }
    Some((id.clone(), pid, start.to_owned()))
}

/// Decode the octal escapes used by tmux control mode for pane output.
/// Reject malformed escapes rather than silently changing terminal bytes.
pub fn decode_tmux_output(encoded: &str) -> Result<Vec<u8>, &'static str> {
    decode_tmux_output_bytes(encoded.as_bytes())
}

/// Control mode may contain arbitrary non-UTF-8 bytes as well as octal escapes.
pub fn decode_tmux_output_bytes(bytes: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'\\' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        if i + 3 >= bytes.len()
            || !bytes[i + 1..i + 4]
                .iter()
                .all(|b| (b'0'..=b'7').contains(b))
        {
            return Err("invalid tmux output escape");
        }
        let value = u16::from(bytes[i + 1] - b'0') * 64
            + u16::from(bytes[i + 2] - b'0') * 8
            + u16::from(bytes[i + 3] - b'0');
        out.push(u8::try_from(value).map_err(|_| "invalid tmux output escape")?);
        i += 4;
    }
    Ok(out)
}

/// Format a bounded binary input frame as a tmux control-mode command.
/// Pane ids and byte values are encoded, never interpolated as command text.
pub fn input_command(pane: &str, input: &[u8]) -> Option<Vec<u8>> {
    use std::fmt::Write;
    const MAX_FRAME: usize = 4096;
    let id = pane.strip_prefix('%')?;
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) || input.len() > MAX_FRAME {
        return None;
    }
    if input.is_empty() {
        return Some(Vec::new());
    }
    let mut command = format!("send-keys -t %{id} -H");
    for byte in input {
        write!(&mut command, " {byte:02x}").ok()?;
    }
    command.push('\n');
    Some(command.into_bytes())
}

/// Ignore notifications for other panes, and reject malformed escapes.
pub fn pane_output(line: &[u8], pane: &str) -> Result<Option<Vec<u8>>, &'static str> {
    let Some(rest) = line.strip_prefix(b"%output ") else {
        return Ok(None);
    };
    let Some(separator) = rest.iter().position(|byte| *byte == b' ') else {
        return Err("malformed tmux output");
    };
    if &rest[..separator] != pane.as_bytes() {
        return Ok(None);
    }
    let payload = rest[separator + 1..]
        .strip_suffix(b"\n")
        .unwrap_or(&rest[separator + 1..]);
    decode_tmux_output_bytes(payload).map(Some)
}

#[cfg(test)]
mod tests {
    use super::{
        decode_tmux_output, eligible_agent, input_command, pane_output, resolve_live_agent,
    };
    use super::{ControlClient, ControlLines, ControlReply};
    use serde_json::json;

    #[test]
    fn websocket_requires_single_same_origin_for_configured_scheme() {
        use crate::routes::auth::RequestScheme;
        use axum::http::{header, HeaderMap, HeaderValue};
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("desk.test:8787"));
        let allowed = vec!["desk.test:8787".to_owned()];
        assert!(!super::valid_websocket_origin(
            &headers,
            RequestScheme::Http,
            &allowed
        ));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://desk.test:8787"),
        );
        assert!(super::valid_websocket_origin(
            &headers,
            RequestScheme::Http,
            &allowed
        ));
        assert!(!super::valid_websocket_origin(
            &headers,
            RequestScheme::Http,
            &[]
        ));
        assert!(!super::valid_websocket_origin(
            &headers,
            RequestScheme::Https,
            &allowed
        ));
        headers.append(header::ORIGIN, HeaderValue::from_static("http://evil.test"));
        assert!(!super::valid_websocket_origin(
            &headers,
            RequestScheme::Http,
            &allowed
        ));
        headers.remove(header::ORIGIN);
        headers.insert(header::ORIGIN, HeaderValue::from_static("http://evil.test"));
        assert!(!super::valid_websocket_origin(
            &headers,
            RequestScheme::Http,
            &allowed
        ));
    }

    #[test]
    fn command_reply_requires_matching_begin_and_end_or_error() {
        let mut reply = ControlReply::default();
        assert_eq!(reply.observe(b"%begin 1 2 1\n"), Ok(None));
        assert_eq!(reply.observe(b"%output %1 hello\n"), Ok(None));
        assert_eq!(reply.observe(b"%end 1 2 1\n"), Ok(Some(true)));
        assert_eq!(reply.observe(b"%begin 3 4 1\n"), Ok(None));
        assert_eq!(reply.observe(b"%error 3 4 1\n"), Ok(Some(false)));
        assert!(reply.observe(b"%end 3 4 1\n").is_err());
        assert!(reply.observe(b"%begin bad 4 1\n").is_err());
    }

    #[test]
    fn control_lines_handle_fragments_and_reject_oversize_or_truncated_eof() {
        let mut lines = ControlLines::default();
        assert!(lines.push(b"%out").unwrap().is_empty());
        assert_eq!(
            lines.push(b"put %1 a\n%exit\npartial").unwrap(),
            vec![b"%output %1 a\n".to_vec(), b"%exit\n".to_vec()]
        );
        assert!(lines.finish().is_err());
        let mut oversized = ControlLines::default();
        assert!(oversized
            .push(&vec![b'x'; ControlLines::MAX_LINE + 1])
            .is_err());
    }

    #[test]
    fn preserves_control_mode_bytes_and_rejects_invalid_escapes() {
        assert_eq!(
            decode_tmux_output(r"hello\033[31m\000\134").unwrap(),
            b"hello\x1b[31m\0\\"
        );
        assert!(decode_tmux_output(r"bad\12").is_err());
        assert!(decode_tmux_output(r"bad\777").is_err());
        assert_eq!(
            pane_output(b"%output %1 \xff\\033\n", "%1").unwrap(),
            Some(vec![0xff, 0x1b])
        );
        assert_eq!(pane_output(b"%output %2 ignored\n", "%1").unwrap(), None);
        assert!(pane_output(b"%output %1 bad\\12\n", "%1").is_err());
        assert_eq!(
            input_command("%1", b"a\n\xff").unwrap(),
            b"send-keys -t %1 -H 61 0a ff\n"
        );
        assert!(input_command("%1;kill-server", b"x").is_none());
        assert!(input_command("%1", &vec![0; 4097]).is_none());
    }

    #[test]
    fn duplicate_pane_id_is_rejected_even_when_one_pid_is_unreadable() {
        assert!(!super::unique_pane_ids(&[
            ("%1".to_owned(), None),
            ("%1".to_owned(), Some(42))
        ]));
        assert!(super::unique_pane_ids(&[
            ("%1".to_owned(), None),
            ("%2".to_owned(), Some(42))
        ]));
    }

    #[test]
    fn owner_terminal_namespace_rejects_managed_aliases() {
        let clean = json!({"%1": {"name": "worker", "agent": "worker"}});
        assert!(!super::owner_namespace_collision(
            clean.as_object().unwrap()
        ));
        let collision = json!({"%1": {"name": "__owner_terminal__", "agent": "worker"}});
        assert!(super::owner_namespace_collision(
            collision.as_object().unwrap()
        ));
        let agent_collision = json!({"%1": {"name": "worker", "agent": "__owner_terminal__"}});
        assert!(super::owner_namespace_collision(
            agent_collision.as_object().unwrap()
        ));
    }

    #[test]
    fn owner_eligibility_fails_closed_for_ambiguity_and_stale_generations() {
        let owner = json!({"role":"owner","pane_pid":42,"start":"123"})
            .as_object()
            .unwrap()
            .clone();
        let mut roles = std::collections::BTreeMap::new();
        roles.insert("%1".to_owned(), owner.clone());
        let live = vec![("%1".to_owned(), 42, "123".to_owned())];
        assert!(super::eligible_owner(&roles, &live).is_some());
        assert!(
            super::eligible_owner(&roles, &[("%1".to_owned(), 42, "changed".to_owned())]).is_none()
        );
        assert!(super::eligible_owner(&roles, &[live[0].clone(), live[0].clone()]).is_none());
        roles.insert("%2".to_owned(), owner);
        assert!(super::eligible_owner(&roles, &live).is_none());
        roles.remove("%2");
        roles
            .get_mut("%1")
            .unwrap()
            .insert("role".to_owned(), json!("worker"));
        assert!(super::eligible_owner(&roles, &live).is_none());
    }

    #[test]
    fn eligibility_requires_unique_managed_name_and_matching_non_owner_generation() {
        let managed = json!({"%1":{"name":"worker","agent":"worker"}})
            .as_object()
            .unwrap()
            .clone();
        let mut roles = std::collections::BTreeMap::new();
        roles.insert(
            "%1".to_owned(),
            json!({"role":"worker","pane_pid":42,"start":"123"})
                .as_object()
                .unwrap()
                .clone(),
        );
        let live = vec![("%1".to_owned(), 42, "123".to_owned())];
        assert!(eligible_agent("worker", &managed, &roles, &live).is_some());
        assert!(eligible_agent("%1", &managed, &roles, &live).is_none());
        assert!(eligible_agent(
            "worker",
            &managed,
            &roles,
            &[("%1".into(), 42, "124".into())]
        )
        .is_none());
        assert!(eligible_agent(
            "worker",
            &managed,
            &roles,
            &[live[0].clone(), live[0].clone()]
        )
        .is_none());
        roles
            .get_mut("%1")
            .unwrap()
            .insert("role".into(), json!("owner"));
        assert!(eligible_agent("worker", &managed, &roles, &live).is_none());
        roles
            .get_mut("%1")
            .unwrap()
            .insert("role".into(), json!("worker"));
        let mut ambiguous = managed.clone();
        ambiguous.insert("%2".into(), json!({"name":"worker","agent":"worker"}));
        assert!(eligible_agent("worker", &ambiguous, &roles, &live).is_none());
    }

    #[test]
    fn fresh_private_tmux_identity_and_revocation() {
        struct PrivateServer(String);
        impl Drop for PrivateServer {
            fn drop(&mut self) {
                let _ = std::process::Command::new("tmux")
                    .args(["-L", &self.0, "kill-server"])
                    .output();
            }
        }
        let dir = tempfile::Builder::new()
            .prefix("t62-private-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        let socket = format!(
            "ds-t62-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let server = PrivateServer(socket.clone());
        let started = std::process::Command::new("tmux")
            .args(["-L", &server.0, "new-session", "-d", "-s", "fixture", "cat"])
            .output()
            .unwrap();
        assert!(started.status.success());
        let registry_path = dir.path().join("tmux-panes.json");
        let registry_string = registry_path.to_str().unwrap();
        let env = ds_core::config::Env::from_pairs(&[
            ("DUNKINGSHEEP_TMUX_SOCKET", &socket),
            ("DUNKINGSHEEP_TMUX_REGISTRY", registry_string),
        ]);
        let backend = ds_core::terminal::tmux::TmuxBackend::from_env("tmux", env.clone());
        let listing = backend
            .run(&[
                "list-panes",
                "-a",
                "-F",
                ds_core::terminal::identity::identity_format(),
            ])
            .unwrap();
        assert_eq!(listing.code, 0);
        let identities =
            ds_core::terminal::identity::parse_pane_identities(&listing.stdout).unwrap();
        let [(pane, Some(pid))] = identities.as_slice() else {
            panic!("expected one private pane")
        };
        let start = ds_core::proc::process_start(*pid).unwrap();
        backend
            .registry()
            .remember(
                "pane",
                pane,
                json!({"name":"fixture-worker","agent":"fixture-worker"}),
            )
            .unwrap();
        let mut roles = ds_core::roles::Roles::new(
            &env,
            Some(dir.path().join("roles.json")),
            Some(dir.path().join("tokens.json")),
            None,
        )
        .unwrap();
        roles
            .register(
                pane,
                "worker",
                Some(*pid),
                Some(&start),
                None,
                None,
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            resolve_live_agent("fixture-worker", &backend, &mut roles),
            Some((pane.clone(), *pid, start))
        );
        let session = backend
            .run(&["display-message", "-p", "-t", pane, "#{session_id}"])
            .unwrap();
        let session = session.stdout.trim();
        let mut control =
            ControlClient::attach(std::path::Path::new("tmux"), Some(&socket), session, pane)
                .unwrap();
        let mut output = control.take_output().unwrap();
        let mut startup = ControlReply::default();
        loop {
            if startup
                .observe(&output.read_line().unwrap())
                .unwrap()
                .is_some()
            {
                break;
            }
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut reply = ControlReply::default();
            let mut saw_output = false;
            let mut accepted = false;
            loop {
                let Ok(line) = output.read_line() else {
                    let _ = tx.send(false);
                    break;
                };
                match reply.observe(&line) {
                    Ok(Some(true)) => accepted = true,
                    Ok(Some(false)) | Err(_) => {
                        let _ = tx.send(false);
                        break;
                    }
                    Ok(None) => {}
                }
                if let Ok(Some(bytes)) = output.output_from_line(&line) {
                    saw_output |= bytes
                        .windows(b"t62-control-test".len())
                        .any(|w| w == b"t62-control-test");
                }
                if accepted && saw_output {
                    let _ = tx.send(true);
                    break;
                }
            }
            output
        });
        control.send(b"t62-control-test\n").unwrap();
        assert!(rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap());
        let mut output = reader.join().unwrap();
        let (closed_tx, closed_rx) = std::sync::mpsc::channel();
        let closing = std::thread::spawn(move || {
            while output.read_line().is_ok() {}
            let _ = closed_tx.send(());
        });
        drop(control);
        closed_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        closing.join().unwrap();
        roles.unregister(pane).unwrap();
        assert!(resolve_live_agent("fixture-worker", &backend, &mut roles).is_none());
    }
}
