//! Helpers for a future opt-in terminal stream. This module does not expose a route.

use serde_json::{Map, Value};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// Match an authority against the current local names/addresses at the listener port.
/// Numeric addresses are compared by value, including alternate IPv6 spellings.
pub fn valid_terminal_host(headers: &axum::http::HeaderMap, allowed_hosts: &[String]) -> bool {
    let mut hosts = headers.get_all(axum::http::header::HOST).iter();
    let Some(host) = hosts.next().and_then(|v| v.to_str().ok()) else {
        return false;
    };
    if hosts.next().is_some() {
        return false;
    }
    let Some(host) = terminal_authority(host) else {
        return false;
    };
    allowed_hosts
        .iter()
        .filter_map(|allowed| terminal_authority(allowed))
        .any(|allowed| allowed == host)
}

fn terminal_authority(value: &str) -> Option<(String, Option<u16>)> {
    if value.is_empty() || value.contains(['/', '\\', '@', ' ', '\t', ',', '?', '#', '%']) {
        return None;
    }
    let authority = value.parse::<axum::http::uri::Authority>().ok()?;
    let port_text = if value.starts_with('[') {
        let end = value.find(']')?;
        let tail = &value[end + 1..];
        if tail.is_empty() {
            None
        } else {
            Some(tail.strip_prefix(':')?)
        }
    } else {
        if value.bytes().filter(|byte| *byte == b':').count() > 1 {
            return None;
        }
        value.rsplit_once(':').map(|(_, port)| port)
    };
    let port = match port_text {
        Some(port) => {
            if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            Some(port.parse::<u16>().ok()?)
        }
        None => None,
    };
    let name = authority.host();
    let name = name
        .strip_prefix('[')
        .and_then(|name| name.strip_suffix(']'))
        .unwrap_or(name);
    let name = match name.parse::<std::net::IpAddr>() {
        Ok(address) => address.to_string(),
        Err(_) => name.strip_suffix('.').unwrap_or(name).to_ascii_lowercase(),
    };
    if name.is_empty() {
        return None;
    }
    Some((name, port))
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

    /// Ask tmux for the pane's initial screen; the reply is read with [`Snapshot`].
    pub fn request_snapshot(&mut self) -> io::Result<()> {
        let command = snapshot_command(&self.pane)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid tmux identity"))?;
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

/// The control-mode command for a pane's initial screen (t4601): one line of cursor, screen mode
/// and size, then the visible rows with their colours. It must be sent on the SAME control
/// connection that carries the pane's `%output`, so that tmux orders the reply against the output.
pub fn snapshot_command(pane: &str) -> Option<Vec<u8>> {
    let id = pane.strip_prefix('%')?;
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(
        format!(
            "display-message -p -t %{id} '#{{cursor_x}} #{{cursor_y}} #{{alternate_on}} \
             #{{pane_width}} #{{pane_height}}' ; capture-pane -p -e -t %{id}\n"
        )
        .into_bytes(),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SnapshotStage {
    AwaitBegin,
    Meta,
    AfterMeta,
    AwaitSecondBegin,
    Rows,
    AwaitEnd,
    Done,
}

/// Reads the reply to [`snapshot_command`] from the control stream and builds the first frame a
/// terminal client receives.
///
/// Ordering: tmux writes pane output, notifications and command replies to a control client in
/// one ordered stream. Every `%output` line before the reply's `%begin` is already in the
/// captured screen, so the caller drops it (this reader ignores it); every `%output` after the
/// final `%end` is newer than the capture, so the caller forwards it. Nothing is lost, repeated
/// or drawn over by an older screen.
///
/// Framing: captured rows are unescaped screen text and can spell `%end ...`. The rows are
/// therefore taken by COUNT (`pane_height` from the metadata line), never by content. Any
/// mismatch is an error and the caller must close the stream without forwarding anything.
pub struct Snapshot {
    stage: SnapshotStage,
    marker: Vec<u8>,
    cursor: (usize, usize),
    alternate: bool,
    size: (usize, usize),
    rows: Vec<Vec<u8>>,
    bytes: usize,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            stage: SnapshotStage::AwaitBegin,
            marker: Vec::new(),
            cursor: (0, 0),
            alternate: false,
            size: (0, 0),
            rows: Vec::new(),
            bytes: 0,
        }
    }
}

impl Snapshot {
    pub const MAX_ROWS: usize = 500;
    pub const MAX_COLS: usize = 2000;
    pub const MAX_BYTES: usize = 1024 * 1024;

    fn reply_marker(line: &[u8], prefix: &[u8]) -> Option<Vec<u8>> {
        let marker = line.strip_prefix(prefix)?;
        let fields: Vec<_> = marker.split(|byte| *byte == b' ').collect();
        (fields.len() == 3
            && fields
                .iter()
                .all(|field| !field.is_empty() && field.iter().all(u8::is_ascii_digit)))
        .then(|| marker.to_vec())
    }

    fn is_end(&self, line: &[u8]) -> bool {
        line.strip_prefix(b"%end ") == Some(self.marker.as_slice())
    }

    fn metadata(&mut self, line: &[u8]) -> Result<(), &'static str> {
        let text = std::str::from_utf8(line).map_err(|_| "invalid snapshot metadata")?;
        let fields: Vec<usize> = text
            .split(' ')
            .map(|field| {
                (!field.is_empty() && field.len() <= 5 && field.bytes().all(|b| b.is_ascii_digit()))
                    .then(|| field.parse().ok())
                    .flatten()
            })
            .collect::<Option<_>>()
            .ok_or("invalid snapshot metadata")?;
        let [x, y, alternate, cols, rows] = fields[..] else {
            return Err("invalid snapshot metadata");
        };
        if alternate > 1
            || !(1..=Self::MAX_COLS).contains(&cols)
            || !(1..=Self::MAX_ROWS).contains(&rows)
            || x > cols
            || y >= rows
        {
            return Err("snapshot metadata out of range");
        }
        self.cursor = (x, y);
        self.alternate = alternate == 1;
        self.size = (cols, rows);
        Ok(())
    }

    fn row(&mut self, line: &[u8]) -> Result<(), &'static str> {
        self.bytes += line.len() + 2;
        if self.bytes > Self::MAX_BYTES {
            return Err("snapshot too large");
        }
        self.rows.push(line.to_vec());
        if self.rows.len() == self.size.1 {
            self.stage = SnapshotStage::AwaitEnd;
        }
        Ok(())
    }

    /// Feed one control line. `Ok(Some(frame))` is the complete first frame; after it every
    /// later line belongs to the live stream. `Err` means the stream must be closed.
    pub fn observe(&mut self, line: &[u8]) -> Result<Option<Vec<u8>>, &'static str> {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        match self.stage {
            SnapshotStage::AwaitBegin | SnapshotStage::AwaitSecondBegin => {
                if let Some(marker) = Self::reply_marker(line, b"%begin ") {
                    self.marker = marker;
                    self.stage = if self.stage == SnapshotStage::AwaitBegin {
                        SnapshotStage::Meta
                    } else {
                        SnapshotStage::Rows
                    };
                    Ok(None)
                } else if line.starts_with(b"%end ")
                    || line.starts_with(b"%error ")
                    || !line.starts_with(b"%")
                {
                    Err("unexpected tmux line before the snapshot reply")
                } else {
                    // A notification, including `%output` that the capture already contains.
                    Ok(None)
                }
            }
            SnapshotStage::Meta => {
                self.metadata(line)?;
                self.stage = SnapshotStage::AfterMeta;
                Ok(None)
            }
            SnapshotStage::AfterMeta => {
                // tmux answers the two commands as two reply blocks or as one; accept both.
                if self.is_end(line) {
                    self.stage = SnapshotStage::AwaitSecondBegin;
                    Ok(None)
                } else {
                    self.stage = SnapshotStage::Rows;
                    self.row(line).map(|()| None)
                }
            }
            SnapshotStage::Rows => self.row(line).map(|()| None),
            SnapshotStage::AwaitEnd => {
                if !self.is_end(line) {
                    return Err("snapshot row count does not match the reply");
                }
                self.stage = SnapshotStage::Done;
                Ok(Some(self.frame()))
            }
            SnapshotStage::Done => Err("snapshot already complete"),
        }
    }

    /// Screen mode set explicitly, attributes reset, screen cleared (scrollback kept), rows with
    /// no newline after the last, attributes reset, cursor placed.
    fn frame(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.bytes + 64);
        out.extend_from_slice(if self.alternate {
            b"\x1b[?1049h"
        } else {
            b"\x1b[?1049l"
        });
        out.extend_from_slice(b"\x1b[0m\x1b[H\x1b[2J");
        for (index, row) in self.rows.iter().enumerate() {
            if index > 0 {
                out.extend_from_slice(b"\r\n");
            }
            out.extend_from_slice(row);
        }
        let column = (self.cursor.0 + 1).min(self.size.0);
        out.extend_from_slice(format!("\x1b[0m\x1b[{};{column}H", self.cursor.1 + 1).as_bytes());
        out
    }
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
    use super::{snapshot_command, ControlClient, ControlLines, ControlReply, Snapshot};
    use serde_json::json;

    #[test]
    fn terminal_authorities_normalize_ip_spelling_but_never_widen_ports() {
        use axum::http::{header, HeaderMap};
        let allowed = vec!["[::1]:8787".to_owned(), "cell:8787".to_owned()];
        for (host, expected) in [
            ("[0:0:0:0:0:0:0:1]:8787", true),
            ("CELL:8787", true),
            ("cell.:8787", true),
            ("cell", false),
            ("cell:80", false),
            ("cell:65536", false),
            ("user@cell:8787", false),
            ("cell:8787/path", false),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, host.parse().unwrap());
            assert_eq!(
                super::valid_terminal_host(&headers, &allowed),
                expected,
                "{host}"
            );
        }
        let defaults = vec!["cell".to_owned(), "cell:80".to_owned()];
        for host in ["cell:", "cell:65536", "cell:+80", "cell:invalid"] {
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, host.parse().unwrap());
            assert!(!super::valid_terminal_host(&headers, &defaults), "{host}");
        }
        let mut headers = HeaderMap::new();
        headers.append(header::HOST, "cell:8787".parse().unwrap());
        headers.append(header::HOST, "cell:8787".parse().unwrap());
        assert!(!super::valid_terminal_host(&headers, &allowed));
    }

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

    fn feed(lines: &[&[u8]]) -> Result<Option<Vec<u8>>, &'static str> {
        let mut snapshot = Snapshot::default();
        let mut frame = None;
        for line in lines {
            assert!(
                frame.is_none(),
                "lines after the frame belong to the live stream"
            );
            frame = snapshot.observe(line)?;
        }
        Ok(frame)
    }

    /// t4601: rows are taken by count, so screen text that spells a reply marker is data; the
    /// frame sets the screen mode explicitly, resets attributes and ends without a newline.
    #[test]
    fn snapshot_reads_rows_by_count_and_fails_closed_on_any_mismatch() {
        assert_eq!(
            snapshot_command("%7").unwrap(),
            b"display-message -p -t %7 '#{cursor_x} #{cursor_y} #{alternate_on} #{pane_width} \
              #{pane_height}' ; capture-pane -p -e -t %7\n"
        );
        assert!(snapshot_command("%7;kill-server").is_none());
        let two_blocks: &[&[u8]] = &[
            b"%output %7 already-on-screen\n",
            b"%begin 10 5 1\n",
            b"2 1 0 80 3\n",
            b"%end 10 5 1\n",
            b"%begin 10 6 1\n",
            b"%end 10 6 1\n",
            b"\x1b[31mred\n",
            b"\n",
            b"%end 10 6 1\n",
        ];
        assert_eq!(
            feed(two_blocks).unwrap().unwrap(),
            b"\x1b[?1049l\x1b[0m\x1b[H\x1b[2J%end 10 6 1\r\n\x1b[31mred\r\n\x1b[0m\x1b[2;3H"
        );
        let one_block: &[&[u8]] = &[
            b"%begin 10 5 1\n",
            b"0 0 1 80 1\n",
            b"%output %7 text\n",
            b"%end 10 5 1\n",
        ];
        assert_eq!(
            feed(one_block).unwrap().unwrap(),
            b"\x1b[?1049h\x1b[0m\x1b[H\x1b[2J%output %7 text\x1b[0m\x1b[1;1H"
        );
        for bad in [
            "%begin 1 2 1|0 0 0 80",
            "%begin 1 2 1|0 0 2 80 1",
            "%begin 1 2 1|0 5 0 80 1",
            "%begin 1 2 1|0 0 0 80 501",
            "%begin 1 2 1|0 0 0 80 1|row|extra row",
            "%begin 1 2 1|0 0 0 80 1|row|%end 1 3 1",
            "%error 1 2 1",
            "stray text",
        ] {
            let lines: Vec<Vec<u8>> = bad
                .split('|')
                .map(|line| format!("{line}\n").into_bytes())
                .collect();
            let lines: Vec<&[u8]> = lines.iter().map(Vec::as_slice).collect();
            assert!(feed(&lines).is_err(), "{bad}");
        }
    }

    fn strip_escapes(bytes: &[u8]) -> String {
        let mut out = String::new();
        let mut rest = bytes;
        while let Some((&byte, tail)) = rest.split_first() {
            rest = tail;
            if byte == 0x1b && rest.first() == Some(&b'[') {
                let end = rest[1..]
                    .iter()
                    .position(|b| (0x40..=0x7e).contains(b))
                    .map_or(rest.len(), |at| at + 2);
                rest = &rest[end..];
            } else {
                out.push(char::from(byte));
            }
        }
        out
    }

    /// t4601 against a real private tmux server: with a pane printing a counter non-stop, the
    /// snapshot frame followed by the forwarded live output shows every number once and in
    /// order (nothing lost, nothing repeated), a screen holding reply-marker text still frames
    /// correctly, and a second connection gets its own snapshot. This is the proof of the tmux
    /// behaviour the design assumes: reply shape, one line per row, and output ordering.
    #[test]
    fn snapshot_then_live_output_is_gapless_on_a_private_tmux_server() {
        struct PrivateServer(String);
        impl Drop for PrivateServer {
            fn drop(&mut self) {
                let _ = std::process::Command::new("tmux")
                    .args(["-L", &self.0, "kill-server"])
                    .output();
            }
        }
        let socket = format!(
            "ds-t4601-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let server = PrivateServer(socket.clone());
        let tmux = |args: &[&str]| {
            let output = std::process::Command::new("tmux")
                .args(["-L", &server.0])
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "{args:?}");
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        };
        tmux(&[
            "new-session",
            "-d",
            "-s",
            "fixture",
            "-x",
            "80",
            "-y",
            "24",
            "i=0; while :; do echo L$i; i=$((i+1)); sleep 0.005; done",
        ]);
        let hostile = tmux(&[
            "new-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-t",
            "fixture",
            "printf '%%end 1 2 3\\n%%begin 1 2 3\\n%%output %%0 x\\n'; exec cat",
        ]);
        let session = tmux(&["display-message", "-p", "-t", "fixture", "#{session_id}"]);
        let counter = tmux(&["list-panes", "-t", "fixture:0", "-F", "#{pane_id}"]);
        let height: usize = tmux(&["display-message", "-p", "-t", &counter, "#{pane_height}"])
            .parse()
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(400));

        let connect = |pane: &str| {
            let mut control =
                ControlClient::attach(std::path::Path::new("tmux"), Some(&socket), &session, pane)
                    .unwrap();
            let mut output = control.take_output().unwrap();
            let mut startup = ControlReply::default();
            while startup
                .observe(&output.read_line().unwrap())
                .unwrap()
                .is_none()
            {}
            control.request_snapshot().unwrap();
            let mut snapshot = Snapshot::default();
            let frame = loop {
                if let Some(frame) = snapshot.observe(&output.read_line().unwrap()).unwrap() {
                    break frame;
                }
            };
            (control, output, frame)
        };

        for _connection in 0..2 {
            let (_control, mut output, frame) = connect(&counter);
            let text = strip_escapes(&frame);
            let rows: Vec<&str> = text.split("\r\n").collect();
            assert_eq!(rows.len(), height, "one row per pane line: {text:?}");
            // The frame ends with the cursor position; rows above the cursor are complete.
            let cursor = std::str::from_utf8(&frame[frame.len() - 12..]).unwrap();
            let (y, x) = cursor[cursor.rfind('[').unwrap() + 1..cursor.len() - 1]
                .split_once(';')
                .unwrap();
            let (y, x): (usize, usize) = (y.parse().unwrap(), x.parse().unwrap());
            assert!(y >= 2, "the counter has printed at least one full line");
            let number = |line: &str| line.strip_prefix('L').and_then(|n| n.parse::<u64>().ok());
            let last_complete = number(rows[y - 2]).unwrap();
            let mut stream = rows[y - 1][..x - 1].to_owned();
            let mut seen = Vec::new();
            while seen.len() < 40 {
                let line = output.read_line().unwrap();
                if let Some(bytes) = output.output_from_line(&line).unwrap() {
                    stream.push_str(&String::from_utf8(bytes).unwrap());
                }
                let complete: Vec<&str> = stream.split("\r\n").collect();
                seen = complete[..complete.len() - 1]
                    .iter()
                    .map(|line| number(line).unwrap_or_else(|| panic!("garbled line {line:?}")))
                    .collect();
            }
            let expected: Vec<u64> = (last_complete + 1..).take(seen.len()).collect();
            assert_eq!(seen, expected, "no gap and no repeat after the snapshot");
        }

        let (_control, _output, frame) = connect(&hostile);
        let text = strip_escapes(&frame);
        assert_eq!(text.split("\r\n").count(), height);
        assert!(
            text.starts_with("%end 1 2 3\r\n%begin 1 2 3\r\n%output %0 x\r\n"),
            "{text:?}"
        );
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
