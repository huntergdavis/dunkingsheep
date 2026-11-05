//! Owned Poolrad game-session backend (t3211) for the four game route groups:
//! lifecycle (`status`, `start`), input (`stop`, `key`, `type`), state
//! (`states`, `savestate`, `loadstate`) and media (`autocodeword`, `video`,
//! `audio`). It ports `GameSession` from `web/gameplay.py` and the route
//! semantics in `web/app.py` lines 1467-1564, plus `web/codeword.py`.
//!
//! Status, stated narrowly:
//! - Implemented: lifecycle (`status`, `start`, `stop`), key and text input,
//!   save-state list, save and load, codeword recognition, emulator and archive
//!   evidence, orphan recovery.
//! - NOT implemented: live video and audio. See `STREAM_BLOCKER`. Once the Python
//!   preconditions pass, `video` and `audio` return 500, and no ffmpeg stream
//!   process is started. The stream-process lifecycle is also not implemented:
//!   one stream per kind, replacement of the previous stream, termination on
//!   client disconnect, and termination on shutdown.
//!
//! Ownership: every external program runs from an absolute path captured in
//! [`Tools`] at construction. The Xvfb child is owned by the session and is
//! terminated by process group. DOSBox-X lives in the private tmux server and
//! is owned by its session name. There is no shell, no Python, no live game and
//! no network access.
//!
//! Blockers, reported rather than stubbed:
//! - `GameMediaBackend::stream` (live video/audio) cannot build an axum `Body`.
//!   The exact constraint is recorded in `STREAM_BLOCKER`. `stream` runs the
//!   Python preconditions and then refuses. It never fabricates a stream.
//! - The `GameInputBackend` and `GameLifecycleBackend` signatures cannot express
//!   Python's HTTP 500 for internal failures. Those surface as 400 or 503 (see
//!   `input_error` and the lifecycle impl).
//! - Lifecycle `status` and `start` are synchronous trait methods that block for
//!   seconds while the launch waits. The composition task must call them off the
//!   async executor.
//!
//! The module is compiled by `tests/game_backend.rs` through `#[path]` until the
//! separate composition task adds `pub mod game_backend;` to `lib.rs`.

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        process::CommandExt,
    },
    path::{Component, Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::{Arc, Mutex, MutexGuard},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::body::Body;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chrono::{DateTime, FixedOffset, Utc};
use ring::{
    digest,
    rand::{SecureRandom, SystemRandom},
};
use serde_json::{json, Map, Value};

use crate::routes::{
    game_input::{GameInputBackend, GameInputError, GameInputFuture},
    game_lifecycle::GameLifecycleBackend,
    game_media::{
        GameMediaBackend, GameMediaError, GameMediaFuture, GameMediaJson, GameStreamKind,
    },
    game_state::{GameStateBackend, GameStateError, GameStateFuture, PythonString},
};

const PINNED_BUILD_ID: &str = "dosbox-x-5a9c7c53-parity-o1";
const PINNED_SHA256: &str = "fc66e91e8e8d48cc0b176bc350febab753bff297f2d93fc6c1e9d1f21cb6efae";
const PINNED_RELEASE: &str = "2026.01.02";
const SAVE_SLOT_DEFAULT: i64 = 1;
const SAVE_SLOT_MAX: i64 = 99;
const FRAME_INSET: i64 = 16;
const TMUX_SOCKET: &str = "vp-poolrad";
const TMUX_SESSION: &str = "oracle";
const SINK_PREFIX: &str = "vp_poolrad_";
const GAME_START_MARKER: &[u8] = b"Packed file is corrupt";
/// The exact constraint behind the video and audio refusal. Keep this text in
/// sync with the task note and the handoff.
const STREAM_BLOCKER: &str = concat!(
    "BLOCKER t3211-stream: GameMediaBackend::stream must return axum::body::Body, ",
    "a streaming body for the video (multipart/x-mixed-replace) and audio (audio/ogg) routes. ",
    "axum 0.8.9 builds a streaming Body through Body::from_stream, which needs futures_core::TryStream, ",
    "or through Body::new, which needs an http_body::Body with Data = Bytes whose poll_frame returns http_body::Frame. ",
    "axum re-exports only HttpBody (= http_body::Body), Body and BodyDataStream; it does not re-export Frame or Stream. ",
    "ds-web Cargo.toml has no futures-core, futures-util or http-body dependency. ",
    "Unblock: add http-body = \"1\" (1.1.0 is already in Cargo.lock) to ds-web. That is a Cargo change outside t3211's owned files. ",
    "Then wrap a tokio mpsc receiver fed by the ffmpeg stdout reader. ",
    "Not implemented: the ffmpeg stream-process lifecycle. No process is started and no stream is returned."
);

/// `gameplay.GameError` (`Game`) versus any other exception (`Failure`).
/// The route layer maps `Game` to the Python 4xx or 503 detail and `Failure` to
/// the generic 500 where the trait can express it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Fault {
    Game(String),
    Failure(String),
}

type Res<T> = Result<T, Fault>;

impl Fault {
    fn into_text(self) -> String {
        match self {
            Fault::Game(text) | Fault::Failure(text) => text,
        }
    }
}

fn game<T>(message: impl Into<String>) -> Res<T> {
    Err(Fault::Game(message.into()))
}

fn failure<T>(message: impl Into<String>) -> Res<T> {
    Err(Fault::Failure(message.into()))
}

fn io_res<T>(result: io::Result<T>) -> Res<T> {
    result.map_err(|error| Fault::Failure(error.to_string()))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Runs pure or blocking work on the blocking pool and maps a join failure.
async fn blocking<T, F>(operation: F) -> Res<T>
where
    T: Send + 'static,
    F: FnOnce() -> Res<T> + Send + 'static,
{
    match tokio::task::spawn_blocking(operation).await {
        Ok(result) => result,
        Err(error) => failure(error.to_string()),
    }
}

/// Runs one step under the session lock on the blocking pool, as Python's
/// `with self.lock:` does, without holding a std mutex across an await.
async fn on_session<T, F>(shared: Arc<Shared>, operation: F) -> Res<T>
where
    T: Send + 'static,
    F: FnOnce(&Ctx, &mut Session) -> Res<T> + Send + 'static,
{
    blocking(move || {
        let mut session = lock(&shared.session);
        operation(&shared.ctx, &mut session)
    })
    .await
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayBuild {
    Original,
    Replacement,
}

impl PlayBuild {
    fn as_str(self) -> &'static str {
        match self {
            PlayBuild::Original => "original",
            PlayBuild::Replacement => "replacement",
        }
    }
}

/// Absolute executables captured once, so no child process is resolved through
/// PATH at launch time.
#[derive(Clone, Debug)]
pub struct Tools {
    pub tmux: PathBuf,
    pub xvfb: PathBuf,
    pub xauth: PathBuf,
    pub xdotool: PathBuf,
    pub pactl: PathBuf,
    pub ffmpeg: PathBuf,
    pub bwrap: PathBuf,
    pub convert: PathBuf,
    pub kill: PathBuf,
}

impl Tools {
    /// Resolves each tool against `path_var`. A missing tool keeps its bare
    /// name, so the launch fails with a precise spawn error.
    pub fn capture(path_var: &str) -> Self {
        let find = |name: &str| which(name, path_var).unwrap_or_else(|| PathBuf::from(name));
        Self {
            tmux: find("tmux"),
            xvfb: find("Xvfb"),
            xauth: find("xauth"),
            xdotool: find("xdotool"),
            pactl: find("pactl"),
            ffmpeg: find("ffmpeg"),
            bwrap: find("bwrap"),
            convert: find("convert"),
            kill: find("kill"),
        }
    }
}

/// Python-side timeouts. Tests shorten the waits, production keeps these values.
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    pub command: Duration,
    pub game_start: Duration,
    pub slot_wait: Duration,
    pub xvfb_ready_polls: u32,
    pub post_launch: Duration,
    pub terminate: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            command: Duration::from_secs(70),
            game_start: Duration::from_secs(20),
            slot_wait: Duration::from_secs(8),
            xvfb_ready_polls: 30,
            post_launch: Duration::from_secs(1),
            terminate: Duration::from_secs(4),
        }
    }
}

/// Everything `GameSession.__init__` reads, made explicit.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub repo: PathBuf,
    pub data_dir: PathBuf,
    pub archive_manifest: PathBuf,
    pub codewheel_dir: PathBuf,
    pub play_build: PlayBuild,
    pub dosbox_configured: String,
    pub replacement_registry: PathBuf,
    pub replacement_dispatch: PathBuf,
    pub path_var: String,
    /// Directory probed for `.X<n>-lock` and `.X11-unix/X<n>`. Python uses a
    /// fixed `/tmp`, which stays the production default. Tests point it at a
    /// private directory, so host X displays cannot change their results.
    pub probe_dir: PathBuf,
    pub tools: Tools,
    pub timeouts: Timeouts,
}

impl SessionConfig {
    /// Mirrors the Python environment reads. `lookup` is the process environment
    /// in production and an explicit map in tests.
    pub fn from_lookup(
        repo: PathBuf,
        data_dir: PathBuf,
        path_var: String,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let play_build = match lookup("VP_PLAY_BUILD")
            .unwrap_or_else(|| "original".to_owned())
            .as_str()
        {
            "original" => PlayBuild::Original,
            "replacement" => PlayBuild::Replacement,
            _ => return Err("VP_PLAY_BUILD must be original or replacement".to_owned()),
        };
        let home = lookup("HOME").unwrap_or_default();
        let dosbox_configured = match play_build {
            PlayBuild::Replacement => {
                lookup("VP_REPLACEMENT_DOSBOX_X").unwrap_or_else(|| pinned_binary(&home))
            }
            PlayBuild::Original => {
                lookup("VP_DOSBOX_X").unwrap_or_else(|| "/usr/bin/dosbox-x".to_owned())
            }
        };
        let archive_manifest = lookup("VP_STATE_ARCHIVE_MANIFEST")
            .map(PathBuf::from)
            .unwrap_or_else(|| repo.join("resources/state-archives/manifest.json"));
        let replacement_registry = lookup("VP_REPLACEMENT_REGISTRY")
            .map(PathBuf::from)
            .unwrap_or_else(|| repo.join("resources/hybrid/registry_all.tsv"));
        let replacement_dispatch = lookup("VP_REPLACEMENT_DISPATCH")
            .map(PathBuf::from)
            .unwrap_or_else(|| repo.join("resources/hybrid/play-dispatch.txt"));
        let codewheel_dir = lookup("VP_CODEWHEEL_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| data_dir.join("codewheel"));
        let codewheel_dir = fs::canonicalize(&codewheel_dir).unwrap_or(codewheel_dir);
        let tools = Tools::capture(&path_var);
        Ok(Self {
            repo,
            data_dir,
            archive_manifest,
            codewheel_dir,
            play_build,
            dosbox_configured,
            replacement_registry,
            replacement_dispatch,
            path_var,
            tools,
            probe_dir: PathBuf::from("/tmp"),
            timeouts: Timeouts::default(),
        })
    }
}

fn pinned_binary(home: &str) -> String {
    format!("{home}/.local/share/vp-of-agentry/dosbox-x-5a9c7c53/dosbox-x")
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// `shutil.which` for both bare names and explicit paths.
fn which(command: &str, path_var: &str) -> Option<PathBuf> {
    if command.is_empty() {
        return None;
    }
    if command.contains('/') {
        let candidate = PathBuf::from(command);
        return is_executable(&candidate).then_some(candidate);
    }
    path_var
        .split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| Path::new(dir).join(command))
        .find(|candidate| is_executable(candidate))
}

/// Mirrors `GameSession.__init__`: a configured DOSBox-X must resolve on PATH or
/// be an absolute path. An absolute path may be missing, and status reports it.
fn resolve_dosbox(config: &SessionConfig) -> Result<String, String> {
    if let Some(found) = which(&config.dosbox_configured, &config.path_var) {
        let resolved = fs::canonicalize(&found).unwrap_or(found);
        return Ok(resolved.to_string_lossy().into_owned());
    }
    if Path::new(&config.dosbox_configured).is_absolute() {
        return Ok(config.dosbox_configured.clone());
    }
    Err("Configured DOSBox-X executable is unavailable on the server PATH".to_owned())
}

/// Captured output of one child process.
#[derive(Debug)]
struct Ran {
    code: Option<i32>,
    stdout: Vec<u8>,
}

impl Ran {
    fn success(&self) -> bool {
        self.code == Some(0)
    }

    fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    fn code_text(&self) -> String {
        self.code
            .map_or_else(|| "signal".to_owned(), |code| code.to_string())
    }
}

/// Runs an absolute program with a hard timeout, environment overrides and no
/// stdin. A spawn error or a timeout is an `Err`; a non-zero exit is an `Ok`.
fn run<I, S>(
    program: &Path,
    args: I,
    env: &[(String, String)],
    cwd: Option<&Path>,
    timeout: Duration,
) -> Result<Ran, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("{}: {error}", program.display()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "stdout pipe missing".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "stderr pipe missing".to_owned())?;
    let out_reader = thread::spawn(move || read_all(stdout));
    let _err_reader = thread::spawn(move || read_all(stderr));
    let deadline = Instant::now() + timeout;
    let status: ExitStatus = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => return Err(error.to_string()),
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{} timed out", program.display()));
        }
        thread::sleep(Duration::from_millis(5));
    };
    let stdout = out_reader.join().unwrap_or_default();
    Ok(Ran {
        code: status.code(),
        stdout,
    })
}

fn read_all<R: Read>(mut reader: R) -> Vec<u8> {
    let mut buffer = Vec::new();
    let _ = reader.read_to_end(&mut buffer);
    buffer
}

/// `subprocess.run(check=True)`: a non-zero exit becomes a failure.
fn checked(result: Result<Ran, String>, what: &str) -> Res<Ran> {
    match result {
        Ok(ran) if ran.success() => Ok(ran),
        Ok(ran) => failure(format!("{what} exited with status {}", ran.code_text())),
        Err(error) => failure(format!("{what} failed: {error}")),
    }
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(None) => {}
            Ok(Some(_)) | Err(_) => return true,
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn signal_group(ctx: &Ctx, pid: u32, signal: &str) {
    let group = format!("-{pid}");
    let _ = run(
        &ctx.cfg.tools.kill,
        ["-s", signal, "--", group.as_str()],
        &[],
        None,
        Duration::from_secs(2),
    );
}

/// `_terminate`: SIGTERM the process group, wait, then SIGKILL it if needed.
fn terminate(ctx: &Ctx, child: &mut Child) {
    if !matches!(child.try_wait(), Ok(None)) {
        return;
    }
    signal_group(ctx, child.id(), "TERM");
    if wait_for_exit(child, ctx.cfg.timeouts.terminate) {
        return;
    }
    signal_group(ctx, child.id(), "KILL");
    if !wait_for_exit(child, Duration::from_secs(2)) {
        let _ = child.kill();
        let _ = child.wait();
    }
}

fn py_isspace(ch: char) -> bool {
    ch.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&ch)
}

fn py_strip(text: &str) -> String {
    text.trim_matches(py_isspace).to_owned()
}

struct Ctx {
    cfg: SessionConfig,
    dosbox_x: String,
    hashes: Mutex<HashMap<PathBuf, (Fingerprint, String)>>,
    templates: Mutex<TemplateCache>,
}

/// The mutable part of `GameSession`, guarded by one lock as in Python.
#[derive(Debug)]
struct Session {
    tmux: Option<String>,
    run_dir: Option<PathBuf>,
    xvfb: Option<Child>,
    display: Option<String>,
    sink_module: Option<String>,
    sink_name: Option<String>,
    active_slot: i64,
    session_started: Option<f64>,
    launched: Option<Value>,
}

impl Session {
    fn new() -> Self {
        Self {
            tmux: None,
            run_dir: None,
            xvfb: None,
            display: None,
            sink_module: None,
            sink_name: None,
            active_slot: SAVE_SLOT_DEFAULT,
            session_started: None,
            launched: None,
        }
    }
}

struct Shared {
    ctx: Ctx,
    session: Mutex<Session>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        // Last reference gone: stop the owned display and tmux session.
        let session = self
            .session
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        stop_session(&self.ctx, session);
    }
}

// ---- Hashing, emulator identity and archive evidence ------------------------

type Fingerprint = (u64, u64, u64, i64, i64, i64, i64);

fn fingerprint(path: &Path) -> io::Result<Fingerprint> {
    let meta = fs::metadata(path)?;
    Ok((
        meta.dev(),
        meta.ino(),
        meta.size(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec(),
    ))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hex_sha256(bytes: &[u8]) -> String {
    hex(digest::digest(&digest::SHA256, bytes).as_ref())
}

fn hash_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut context = digest::Context::new(&digest::SHA256);
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        context.update(&buffer[..read]);
    }
    Ok(hex(context.finish().as_ref()))
}

/// `file_sha256`: hashes the resolved file and refuses if it changed meanwhile.
fn file_sha256(ctx: &Ctx, path: &Path) -> io::Result<String> {
    let target = fs::canonicalize(path)?;
    let before = fingerprint(&target)?;
    if let Some((cached, digest)) = lock(&ctx.hashes).get(&target) {
        if *cached == before {
            return Ok(digest.clone());
        }
    }
    let digest = hash_file(&target)?;
    if fingerprint(&target)? != before {
        return Err(io::Error::other("File changed while hashing"));
    }
    lock(&ctx.hashes).insert(target, (before, digest.clone()));
    Ok(digest)
}

fn other_identity() -> Value {
    json!({"kind": "other", "build_id": null, "sha256": null, "release": null})
}

/// `describe_emulator`: identifies the binary without executing it.
fn describe_emulator(ctx: &Ctx, command: &str) -> Value {
    let mut result = other_identity();
    let Some(executable) = which(command, &ctx.cfg.path_var) else {
        return result;
    };
    let Ok(sha256) = file_sha256(ctx, &executable) else {
        return result;
    };
    if sha256 == PINNED_SHA256 {
        result["kind"] = json!("checkpoint");
        result["build_id"] = json!(PINNED_BUILD_ID);
        result["release"] = json!(PINNED_RELEASE);
    }
    result["sha256"] = json!(sha256);
    result
}

fn parse_offset_datetime(text: &str) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .or_else(|| DateTime::parse_from_rfc3339(&text.replacen(' ', "T", 1)).ok())
}

fn is_lower_hex_64(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// `archive_status`: reports evidence, never creates or changes saved states.
fn archive_status(ctx: &Ctx) -> Value {
    let manifest = &ctx.cfg.archive_manifest;
    if !manifest.exists() {
        return json!({"status": "pending", "verified_count": 0, "archived_at": null});
    }
    match verify_archive(ctx, manifest) {
        Ok((count, archived_at)) => {
            json!({"status": "verified", "verified_count": count, "archived_at": archived_at})
        }
        Err(_) => json!({"status": "unavailable", "verified_count": 0, "archived_at": null}),
    }
}

fn verify_archive(ctx: &Ctx, manifest: &Path) -> Result<(usize, String), String> {
    let text = fs::read_to_string(manifest).map_err(|error| error.to_string())?;
    let data: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
    let created = data
        .get("created")
        .and_then(Value::as_str)
        .ok_or("manifest has no created stamp")?
        .to_owned();
    parse_offset_datetime(&created).ok_or("created stamp lacks a timezone")?;
    let schema = data.get("schema").and_then(Value::as_i64);
    let entries = data
        .get("entries")
        .and_then(Value::as_array)
        .ok_or("entries is not a list")?;
    let expected = data.get("expected_count").and_then(Value::as_i64);
    if schema != Some(1) || entries.is_empty() || expected != i64::try_from(entries.len()).ok() {
        return Err("Incomplete archive manifest".to_owned());
    }
    let root = fs::canonicalize(manifest.parent().unwrap_or_else(|| Path::new(".")))
        .map_err(|error| error.to_string())?;
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for entry in entries {
        let relative_text = entry
            .get("path")
            .and_then(Value::as_str)
            .ok_or("archive entry has no path")?;
        let relative = Path::new(relative_text);
        if relative.is_absolute()
            || relative
                .components()
                .any(|part| part == Component::ParentDir)
        {
            return Err("Archive entry escapes archive directory".to_owned());
        }
        let target = fs::canonicalize(root.join(relative)).map_err(|error| error.to_string())?;
        if !target.starts_with(&root) || seen.contains(&target) || !target.is_file() {
            return Err("Duplicate or non-file archive entry".to_owned());
        }
        let size = entry
            .get("size")
            .and_then(Value::as_i64)
            .ok_or("archive entry has no size")?;
        let expected_sha = entry
            .get("sha256")
            .and_then(Value::as_str)
            .ok_or("archive entry has no sha256")?;
        let actual_size = fs::metadata(&target)
            .map_err(|error| error.to_string())?
            .len();
        let actual_sha = file_sha256(ctx, &target).map_err(|error| error.to_string())?;
        if size <= 0
            || i64::try_from(actual_size).ok() != Some(size)
            || !is_lower_hex_64(expected_sha)
            || actual_sha != expected_sha
        {
            return Err("Archive entry failed integrity check".to_owned());
        }
        seen.insert(target);
    }
    Ok((seen.len(), created))
}

// ---- Time, randomness and the Python-compatible state file ------------------

/// `datetime.isoformat()` for an aware UTC stamp: microseconds only when non-zero.
fn py_isoformat(stamp: DateTime<Utc>) -> String {
    let micros = stamp.timestamp_subsec_micros();
    let base = stamp.format("%Y-%m-%dT%H:%M:%S").to_string();
    if micros == 0 {
        format!("{base}+00:00")
    } else {
        format!("{base}.{micros:06}+00:00")
    }
}

fn now_iso() -> String {
    py_isoformat(Utc::now())
}

fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or(0.0)
}

fn mtime_iso(meta: &fs::Metadata) -> String {
    u32::try_from(meta.mtime_nsec())
        .ok()
        .and_then(|nanos| DateTime::<Utc>::from_timestamp(meta.mtime(), nanos))
        .map(py_isoformat)
        .unwrap_or_else(now_iso)
}

fn random_hex(bytes: usize) -> Res<String> {
    let mut buffer = vec![0u8; bytes];
    SystemRandom::new()
        .fill(&mut buffer)
        .map_err(|_| Fault::Failure("random source unavailable".to_owned()))?;
    Ok(hex(&buffer))
}

/// `json.dumps(value, indent=2)` with `ensure_ascii=True`, so the saved-state
/// file keeps the byte layout the Python oracle reads.
fn py_dumps_indent2(value: &Value) -> String {
    let mut out = String::new();
    write_py_json(value, 0, &mut out);
    out
}

fn push_indent(out: &mut String, depth: usize) {
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn write_py_json(value: &Value, depth: usize, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => out.push_str(&number.to_string()),
        Value::String(text) => write_py_string(text, out),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push_str("[\n");
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(",\n");
                }
                push_indent(out, depth + 1);
                write_py_json(item, depth + 1, out);
            }
            out.push('\n');
            push_indent(out, depth);
            out.push(']');
        }
        Value::Object(fields) => {
            if fields.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push_str("{\n");
            for (index, (key, item)) in fields.iter().enumerate() {
                if index > 0 {
                    out.push_str(",\n");
                }
                push_indent(out, depth + 1);
                write_py_string(key, out);
                out.push_str(": ");
                write_py_json(item, depth + 1, out);
            }
            out.push('\n');
            push_indent(out, depth);
            out.push('}');
        }
    }
}

fn write_py_string(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(ch),
            other => {
                let mut units = [0u16; 2];
                for unit in other.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}

/// `_record`: one JSON line in `vp-input.jsonl`, with `time` first.
fn journal(run_dir: &Path, fields: Vec<(&str, Value)>) -> Res<()> {
    let mut entry = Map::new();
    entry.insert("time".to_owned(), json!(now_iso()));
    for (key, value) in fields {
        entry.insert(key.to_owned(), value);
    }
    let mut line = serde_json::to_string(&Value::Object(entry))
        .map_err(|error| Fault::Failure(error.to_string()))?;
    line.push('\n');
    let mut file = io_res(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(run_dir.join("vp-input.jsonl")),
    )?;
    io_res(file.write_all(line.as_bytes()))
}

// ---- Command construction (shell quoting, env, DOSBox command) ---------------

/// `shlex.quote`, byte for byte: bare when every byte is in the safe set.
fn shell_quote(text: &str) -> String {
    if text.is_empty() {
        return "''".to_owned();
    }
    if text
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
    {
        return text.to_owned();
    }
    format!("'{}'", text.replace('\'', "'\"'\"'"))
}

fn shell_join(parts: &[String]) -> String {
    parts
        .iter()
        .map(|part| shell_quote(part))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `_launch_env_args`: clears the replacement and parity knobs, then sets the
/// display, audio and XDG variables explicitly.
fn launch_env_args(env: &BTreeMap<String, String>, play_env: &[(String, String)]) -> Vec<String> {
    const CLEAR: [&str; 7] = [
        "PR_HYBRID_REGISTRY",
        "PR_HYBRID_LOG",
        "PR_HYBRID_DISPATCH",
        "PR_HYBRID_BUS_MODE",
        "PR_HYBRID_SEED_PIN",
        "PR_PARITY_TARGET",
        "PR_PARITY_PREFIX",
    ];
    let mut clear: BTreeSet<String> = CLEAR.iter().map(|key| (*key).to_owned()).collect();
    clear.extend(
        env.keys()
            .filter(|key| key.starts_with("PR_HYBRID_") || key.starts_with("PR_PARITY_"))
            .cloned(),
    );
    let mut args = Vec::new();
    for key in &clear {
        args.push("-u".to_owned());
        args.push(key.clone());
    }
    for key in [
        "DISPLAY",
        "XAUTHORITY",
        "SDL_AUDIODRIVER",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_DATA_HOME",
    ] {
        let value = env.get(key).cloned().unwrap_or_default();
        args.push(format!("{key}={value}"));
    }
    for (key, value) in play_env {
        args.push(format!("{key}={value}"));
    }
    args
}

/// `_dosbox_command`: both builds launch START.EXE inside a bwrap sandbox.
fn dosbox_command(ctx: &Ctx, run_dir: &Path, play_build: PlayBuild) -> Vec<String> {
    let run = run_dir.to_string_lossy().into_owned();
    let saves = run_dir.join(".dosbox-saves").to_string_lossy().into_owned();
    let mut command = vec![ctx.cfg.tools.bwrap.to_string_lossy().into_owned()];
    command.extend(
        [
            "--unshare-net",
            "--ro-bind",
            "/",
            "/",
            "--dev-bind",
            "/dev",
            "/dev",
            "--bind",
        ]
        .map(str::to_owned),
    );
    command.push(run.clone());
    command.push(run.clone());
    command.push("--chdir".to_owned());
    command.push(run.clone());
    command.push(ctx.dosbox_x.clone());
    command.extend(
        [
            "-defaultconf",
            "-defaultmapper",
            "-nogui",
            "-nomenu",
            "-fastlaunch",
        ]
        .map(str::to_owned),
    );
    if play_build == PlayBuild::Replacement {
        command.extend(["-set", "cpu core=normal"].map(str::to_owned));
    }
    command.extend(
        [
            "-set",
            "sdl windowresolution=640x480",
            "-set",
            "dosbox saveslot=1",
            "-set",
            "dosbox saveremark=false",
            "-savedir",
        ]
        .map(str::to_owned),
    );
    command.push(saves);
    command.push("-c".to_owned());
    command.push(format!("mount c {run}"));
    command.extend(["-c", "c:", "-c"].map(str::to_owned));
    command.push("cd \\POOLRAD".to_owned());
    command.extend(["-c", "START"].map(str::to_owned));
    command
}

// ---- Session lifecycle: tmux, status, start, stop ----------------------------

fn tmux_run<I, S>(ctx: &Ctx, args: I, timeout: Duration) -> Result<Ran, String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut full = vec!["-L".to_owned(), TMUX_SOCKET.to_owned()];
    full.extend(args.into_iter().map(|arg| arg.as_ref().to_owned()));
    run(&ctx.cfg.tools.tmux, full, &[], None, timeout)
}

/// `_game_running`: the owned tmux session still exists.
fn game_running(ctx: &Ctx, st: &Session) -> bool {
    match &st.tmux {
        Some(session) => matches!(
            tmux_run(
                ctx,
                ["has-session", "-t", session.as_str()],
                Duration::from_secs(2)
            ),
            Ok(ran) if ran.success()
        ),
        None => false,
    }
}

/// `_stop_locked`: kill tmux, terminate the Xvfb group, unload the sink, and
/// clear every per-launch field. Python keeps `_session_started` across stops,
/// and so does this.
fn stop_session(ctx: &Ctx, st: &mut Session) {
    if let Some(session) = st.tmux.clone() {
        let _ = tmux_run(
            ctx,
            ["kill-session", "-t", session.as_str()],
            Duration::from_secs(5),
        );
    }
    if let Some(mut xvfb) = st.xvfb.take() {
        terminate(ctx, &mut xvfb);
    }
    if let Some(module) = st.sink_module.take() {
        let _ = run(
            &ctx.cfg.tools.pactl,
            ["unload-module", module.as_str()],
            &[],
            None,
            Duration::from_secs(5),
        );
    }
    st.tmux = None;
    st.launched = None;
    st.display = None;
    st.sink_name = None;
    st.run_dir = None;
}

fn stop_op(ctx: &Ctx, st: &mut Session) -> Res<Value> {
    stop_session(ctx, st);
    status_session(ctx, st)
}

fn last_file(ctx: &Ctx) -> PathBuf {
    ctx.cfg.data_dir.join("poolrad-last-run.txt")
}

fn read_last_run(ctx: &Ctx) -> Result<Option<String>, String> {
    let path = last_file(ctx);
    if !path.exists() {
        return Ok(None);
    }
    fs::read_to_string(&path)
        .map(|text| Some(py_strip(&text)))
        .map_err(|error| error.to_string())
}

/// `status()`: a dead tmux session is cleaned up first, as in Python.
fn status_session(ctx: &Ctx, st: &mut Session) -> Res<Value> {
    if st.tmux.is_some() && !game_running(ctx, st) {
        stop_session(ctx, st);
    }
    let last = read_last_run(ctx).map_err(Fault::Failure)?;
    let running = st.tmux.is_some();
    let identity = if running {
        st.launched.clone().unwrap_or_else(other_identity)
    } else {
        describe_emulator(ctx, &ctx.dosbox_x)
    };
    let mut emulator = identity;
    if let Value::Object(fields) = &mut emulator {
        let scope = if running {
            "running_session"
        } else {
            "next_start"
        };
        fields.insert("scope".to_owned(), json!(scope));
    }
    let run_name = match &st.run_dir {
        Some(run_dir) => run_dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned()),
        None => last,
    };
    Ok(json!({
        "running": running,
        "audio": st.sink_name.is_some(),
        "build": ctx.cfg.play_build.as_str(),
        "emulator": emulator,
        "state_archive": archive_status(ctx),
        "run_name": run_name,
    }))
}

fn is_run_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphanumeric() => {
            chars.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
        }
        _ => false,
    }
}

/// `_valid_run`: a prepared run needs the input-hash marker and START.EXE.
fn valid_run(ctx: &Ctx, name: &str) -> Res<PathBuf> {
    if !is_run_name(name) {
        return game("Invalid run name");
    }
    let path = ctx.cfg.repo.join("resources/oracle-runs").join(name);
    if !path.join(".oracle-input-sha256").is_file() || !path.join("POOLRAD/START.EXE").is_file() {
        return game("Prepared Poolrad run is unavailable");
    }
    Ok(path)
}

/// `start(fresh)`: a running session is returned as is. Otherwise any stale
/// state is cleared, a run is prepared or reused, and the game is launched.
fn start_session(ctx: &Ctx, st: &mut Session, fresh: bool) -> Res<Value> {
    if game_running(ctx, st) {
        return status_session(ctx, st);
    }
    stop_session(ctx, st);
    if !ctx.cfg.repo.is_dir() {
        return game("Poolrad checkout is unavailable");
    }
    let name = if fresh || !last_file(ctx).exists() {
        let name = format!(
            "vp-{}-{}",
            Utc::now().format("%Y%m%d-%H%M%S"),
            random_hex(2)?
        );
        let prepare = ctx.cfg.repo.join("tools/oracle/prepare_run.sh");
        match run(
            &prepare,
            [name.as_str()],
            &[],
            Some(&ctx.cfg.repo),
            ctx.cfg.timeouts.command,
        ) {
            Ok(ran) if ran.success() => {}
            Ok(ran) => {
                return game(format!(
                    "Could not prepare verified game copy: exit status {}",
                    ran.code_text()
                ))
            }
            Err(error) => return game(format!("Could not prepare verified game copy: {error}")),
        }
        name
    } else {
        read_last_run(ctx)
            .map_err(Fault::Failure)?
            .unwrap_or_default()
    };
    let run_dir = valid_run(ctx, &name)?;
    st.run_dir = Some(run_dir.clone());
    io_res(fs::write(last_file(ctx), format!("{name}\n")))?;
    io_res(fs::set_permissions(
        last_file(ctx),
        fs::Permissions::from_mode(0o600),
    ))?;
    match launch_session(ctx, st, &run_dir) {
        Ok(identity) => {
            st.launched = Some(identity);
            status_session(ctx, st)
        }
        Err(fault) => {
            stop_session(ctx, st);
            match fault {
                Fault::Game(text) => game(text),
                Fault::Failure(text) => game(format!("Could not start Poolrad: {text}")),
            }
        }
    }
}

/// The body of the `try:` in `GameSession.start`. Any error here stops the
/// partial launch and is wrapped by the caller.
fn launch_session(ctx: &Ctx, st: &mut Session, run_dir: &Path) -> Res<Value> {
    let play_env = play_environment(ctx, run_dir)?;
    let identity = describe_emulator(ctx, &ctx.dosbox_x);
    if identity["sha256"].is_null() {
        return game("Configured DOSBox-X binary is unavailable");
    }
    let (display, auth) = start_display(ctx, st, run_dir)?;
    let sink = format!("{SINK_PREFIX}{}", std::process::id());
    let sink_arg = format!("sink_name={sink}");
    if let Ok(ran) = run(
        &ctx.cfg.tools.pactl,
        [
            "load-module",
            "module-null-sink",
            sink_arg.as_str(),
            "rate=48000",
            "channels=2",
        ],
        &[],
        None,
        ctx.cfg.timeouts.command,
    ) {
        if ran.success() {
            st.sink_module = Some(py_strip(&ran.stdout_text()));
            st.sink_name = Some(sink.clone());
        }
    }
    let mut env: BTreeMap<String, String> = std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .collect();
    for (key, value) in &play_env {
        env.insert(key.clone(), value.clone());
    }
    env.insert("DISPLAY".to_owned(), display.clone());
    env.insert("XAUTHORITY".to_owned(), auth.to_string_lossy().into_owned());
    let audio_driver = if st.sink_name.is_some() {
        "pulse"
    } else {
        "dummy"
    };
    env.insert("SDL_AUDIODRIVER".to_owned(), audio_driver.to_owned());
    env.insert(
        "XDG_CONFIG_HOME".to_owned(),
        run_dir.join(".config").to_string_lossy().into_owned(),
    );
    env.insert(
        "XDG_CACHE_HOME".to_owned(),
        run_dir.join(".cache").to_string_lossy().into_owned(),
    );
    env.insert(
        "XDG_DATA_HOME".to_owned(),
        run_dir.join(".local/share").to_string_lossy().into_owned(),
    );
    if let Some(sink_name) = &st.sink_name {
        env.insert("PULSE_SINK".to_owned(), sink_name.clone());
    }
    for dir in [".config", ".cache", ".local/share", ".dosbox-saves"] {
        io_res(fs::create_dir_all(run_dir.join(dir)))?;
    }
    let mut env_args = launch_env_args(&env, &play_env);
    if let Some(sink_name) = &st.sink_name {
        env_args.push(format!("PULSE_SINK={sink_name}"));
    }
    let mut parts = vec!["env".to_owned()];
    parts.extend(env_args);
    parts.extend(dosbox_command(ctx, run_dir, ctx.cfg.play_build));
    let command = format!("exec {}", shell_join(&parts));
    checked(
        tmux_run(
            ctx,
            [
                "new-session",
                "-d",
                "-s",
                TMUX_SESSION,
                "-x",
                "140",
                "-y",
                "40",
                command.as_str(),
            ],
            ctx.cfg.timeouts.command,
        ),
        "tmux new-session",
    )?;
    st.tmux = Some(TMUX_SESSION.to_owned());
    st.active_slot = SAVE_SLOT_DEFAULT;
    st.session_started = Some(now_epoch());
    let log = run_dir.join("vp-dosbox-terminal.log");
    let pipe = format!("cat >> {}", shell_quote(&log.to_string_lossy()));
    checked(
        tmux_run(
            ctx,
            ["pipe-pane", "-o", "-t", TMUX_SESSION, pipe.as_str()],
            ctx.cfg.timeouts.command,
        ),
        "tmux pipe-pane",
    )?;
    thread::sleep(ctx.cfg.timeouts.post_launch);
    if !game_running(ctx, st) {
        return game("DOSBox-X exited during startup; inspect private vp-dosbox-terminal.log");
    }
    Ok(identity)
}

fn x_ready(ctx: &Ctx, display: &str, auth: &Path) -> bool {
    let env = [
        ("DISPLAY".to_owned(), display.to_owned()),
        ("XAUTHORITY".to_owned(), auth.to_string_lossy().into_owned()),
    ];
    matches!(
        run(
            &ctx.cfg.tools.xdotool,
            ["getdisplaygeometry"],
            &env,
            None,
            Duration::from_secs(1)
        ),
        Ok(ran) if ran.success()
    )
}

/// Creates the xauth file with mode 0600 when it is missing, as `touch` does.
fn touch_private(path: &Path) -> Res<()> {
    io_res(
        OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .mode(0o600)
            .open(path),
    )?;
    Ok(())
}

/// The display loop: the first free `:90`..`:129` whose Xvfb answers
/// `getdisplaygeometry` is owned by the session.
fn start_display(ctx: &Ctx, st: &mut Session, run_dir: &Path) -> Res<(String, PathBuf)> {
    let auth = run_dir.join(".vp-xauthority");
    for number in 90..130 {
        let lock = ctx.cfg.probe_dir.join(format!(".X{number}-lock"));
        let socket = ctx
            .cfg
            .probe_dir
            .join(".X11-unix")
            .join(format!("X{number}"));
        if lock.exists() || socket.exists() {
            continue;
        }
        let display = format!(":{number}");
        touch_private(&auth)?;
        let cookie = random_hex(16)?;
        let auth_arg = auth.to_string_lossy().into_owned();
        checked(
            run(
                &ctx.cfg.tools.xauth,
                [
                    "-f",
                    auth_arg.as_str(),
                    "add",
                    display.as_str(),
                    ".",
                    cookie.as_str(),
                ],
                &[],
                None,
                ctx.cfg.timeouts.command,
            ),
            "xauth add",
        )?;
        let log = io_res(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(run_dir.join("vp-xvfb.log")),
        )?;
        let log_err = io_res(log.try_clone())?;
        let mut xvfb = io_res(
            Command::new(&ctx.cfg.tools.xvfb)
                .arg(&display)
                .args(["-screen", "0", "800x600x24", "-nolisten", "tcp", "-auth"])
                .arg(&auth)
                .stdin(Stdio::null())
                .stdout(Stdio::from(log))
                .stderr(Stdio::from(log_err))
                .process_group(0)
                .spawn(),
        )?;
        let mut ready = false;
        for _ in 0..ctx.cfg.timeouts.xvfb_ready_polls {
            if !matches!(xvfb.try_wait(), Ok(None)) {
                break;
            }
            if x_ready(ctx, &display, &auth) {
                ready = true;
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        if ready {
            st.display = Some(display.clone());
            st.xvfb = Some(xvfb);
            return Ok((display, auth));
        }
        terminate(ctx, &mut xvfb);
    }
    game("Could not start private display")
}

fn is_registry_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == 13
        && (id.starts_with("RES_") || id.starts_with("OVL_"))
        && bytes[4..8]
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_lowercase())
        && bytes[8] == b'_'
        && bytes[9..13]
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_lowercase())
}

/// `_play_environment`: the replacement build copies the registry into the run
/// after validating it. The original build has no extra environment.
fn play_environment(ctx: &Ctx, run_dir: &Path) -> Res<Vec<(String, String)>> {
    if ctx.cfg.play_build == PlayBuild::Original {
        return Ok(Vec::new());
    }
    if which(&ctx.dosbox_x, &ctx.cfg.path_var).is_none() {
        return game("Replacement DOSBox-X binary is unavailable");
    }
    let unavailable = "Replacement registry or dispatch list is unavailable";
    let registry =
        fs::read(&ctx.cfg.replacement_registry).map_err(|_| Fault::Game(unavailable.to_owned()))?;
    let dispatch = fs::read_to_string(&ctx.cfg.replacement_dispatch)
        .map(|text| py_strip(&text))
        .map_err(|_| Fault::Game(unavailable.to_owned()))?;
    let ids: Vec<&str> = dispatch.split(',').collect();
    let unique: HashSet<&str> = ids.iter().copied().collect();
    if !ids.iter().all(|id| is_registry_id(id)) || unique.len() != ids.len() {
        return game("Replacement dispatch list must name unique registry entries");
    }
    validate_replacement_registry(&registry, &ids)?;
    let copied = run_dir.join("vp-replacement-registry.tsv");
    io_res(fs::write(&copied, &registry))?;
    io_res(fs::write(
        run_dir.join("vp-replacement-dispatch.txt"),
        format!("{dispatch}\n"),
    ))?;
    Ok(vec![
        (
            "PR_HYBRID_REGISTRY".to_owned(),
            copied.to_string_lossy().into_owned(),
        ),
        (
            "PR_HYBRID_LOG".to_owned(),
            run_dir
                .join("vp-replacement")
                .to_string_lossy()
                .into_owned(),
        ),
        ("PR_HYBRID_DISPATCH".to_owned(), dispatch.clone()),
        ("PR_HYBRID_BUS_MODE".to_owned(), "route".to_owned()),
    ])
}

fn is_registry_data_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'=' | b',' | b'-' | b'\t')
}

/// `_validate_replacement_registry`: byte-level whitelist, LF framing, one unique
/// row per selected entry, and a well-formed signature and mask.
fn validate_replacement_registry(data: &[u8], selected: &[&str]) -> Res<()> {
    let mut rows: Vec<(&str, Vec<Vec<&str>>)> =
        selected.iter().map(|name| (*name, Vec::new())).collect();
    for line in data.split(|byte| *byte == b'\n') {
        if line.len() > 4094 {
            return game("Invalid replacement registry physical line length");
        }
        let comment = line.starts_with(b"#");
        let allowed = if comment {
            line.iter()
                .all(|byte| (32..=126).contains(byte) || *byte == 9)
        } else {
            line.iter().all(|byte| is_registry_data_byte(*byte))
        };
        if !allowed {
            return game("Invalid replacement registry byte outside format whitelist");
        }
        if comment || line.is_empty() {
            continue;
        }
        let text = match std::str::from_utf8(line) {
            Ok(text) => text,
            Err(_) => return game("Invalid replacement registry byte outside format whitelist"),
        };
        let row: Vec<&str> = text.split('\t').collect();
        if let Some(entry) = rows.iter_mut().find(|entry| entry.0 == row[0]) {
            entry.1.push(row);
        }
    }
    for (name, matches) in &rows {
        let name: &str = name;
        if matches.len() != 1 {
            return game(format!(
                "Replacement requires exactly one unique registry row for {name}"
            ));
        }
        let row = &matches[0];
        if !(row.len() == 6 || row.len() == 7) || row[5] != "unique" {
            return game(format!(
                "Invalid replacement registry columns or uniqueness for {name}"
            ));
        }
        let (ip, length, signature, mask) = (row[1], row[2], row[3], row[4]);
        let suffix = &name[name.len() - 4..];
        let ip_ok = ip.len() == 4 && ip.bytes().all(|byte| byte.is_ascii_hexdigit());
        if !ip_ok || u32::from_str_radix(ip, 16).ok() != u32::from_str_radix(suffix, 16).ok() {
            return game(format!("Invalid replacement registry entry IP for {name}"));
        }
        let length_ok = (1..=3).contains(&length.len())
            && length.as_bytes()[0] != b'0'
            && length.bytes().all(|byte| byte.is_ascii_digit());
        let size: usize = match length.parse::<usize>() {
            Ok(value) if length_ok && (1..=256).contains(&value) => value,
            _ => {
                return game(format!(
                    "Invalid replacement registry signature length for {name}"
                ))
            }
        };
        for value in [signature, mask] {
            if value.len() != size * 2 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return game(format!(
                    "Invalid replacement registry signature or mask encoding for {name}"
                ));
            }
        }
        let mask_bytes: Vec<u8> = (0..mask.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&mask[index..index + 2], 16).unwrap_or(0))
            .collect();
        if !mask_bytes.iter().any(|byte| *byte != 0)
            || !mask_bytes.iter().all(|byte| *byte == 0 || *byte == 255)
        {
            return game(format!("Invalid replacement registry byte mask for {name}"));
        }
    }
    Ok(())
}

// ---- Input: keys and typed text ----------------------------------------------

fn is_supported_key(key: &str) -> bool {
    const NAMED: [&str; 14] = [
        "Up",
        "Down",
        "Left",
        "Right",
        "Return",
        "Escape",
        "space",
        "BackSpace",
        "Tab",
        "Delete",
        "Home",
        "End",
        "Page_Up",
        "Page_Down",
    ];
    NAMED.contains(&key)
        || (key.len() == 1
            && key
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()))
        || (1..=12).any(|number| key == format!("F{number}"))
}

/// `_require_running`: tmux alive, a display, and a run directory.
fn require_running(ctx: &Ctx, st: &Session) -> Res<(String, PathBuf)> {
    match (&st.display, &st.run_dir) {
        (Some(display), Some(run_dir)) if game_running(ctx, st) => {
            Ok((display.clone(), run_dir.clone()))
        }
        _ => game("Poolrad is not running"),
    }
}

fn game_env(display: &str, run_dir: &Path) -> Vec<(String, String)> {
    vec![
        ("DISPLAY".to_owned(), display.to_owned()),
        (
            "XAUTHORITY".to_owned(),
            run_dir
                .join(".vp-xauthority")
                .to_string_lossy()
                .into_owned(),
        ),
    ]
}

/// A checked `xdotool` call; returns its stdout.
fn xdotool<I, S>(ctx: &Ctx, env: &[(String, String)], args: I) -> Res<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    checked(
        run(
            &ctx.cfg.tools.xdotool,
            args,
            env,
            None,
            ctx.cfg.timeouts.command,
        ),
        "xdotool",
    )
    .map(|ran| ran.stdout_text())
}

fn chord(ctx: &Ctx, env: &[(String, String)], combo: &str) -> Res<()> {
    xdotool(
        ctx,
        env,
        ["key", "--clearmodifiers", "--delay", "70", combo],
    )?;
    Ok(())
}

fn key_op(ctx: &Ctx, st: &mut Session, key: &str) -> Res<()> {
    if !is_supported_key(key) {
        return game("Unsupported game key");
    }
    let (display, run_dir) = require_running(ctx, st)?;
    let env = game_env(&display, &run_dir);
    xdotool(ctx, &env, ["key", "--clearmodifiers", "--delay", "70", key])?;
    journal(&run_dir, vec![("key", json!(key))])
}

fn type_op(ctx: &Ctx, st: &mut Session, text: &str, submit: bool) -> Res<()> {
    if text.is_empty() || text.chars().count() > 64 || text.chars().any(|ch| (ch as u32) < 32) {
        return game("Unsupported text");
    }
    let (display, run_dir) = require_running(ctx, st)?;
    let env = game_env(&display, &run_dir);
    xdotool(
        ctx,
        &env,
        ["type", "--clearmodifiers", "--delay", "60", text],
    )?;
    if submit {
        xdotool(
            ctx,
            &env,
            ["key", "--clearmodifiers", "--delay", "70", "Return"],
        )?;
    }
    journal(
        &run_dir,
        vec![("type", json!(text)), ("submit", json!(submit))],
    )
}

/// `_focus_game`: focus the first visible DOSBox window, if any.
fn focus_game(ctx: &Ctx, env: &[(String, String)]) -> Res<()> {
    let found = xdotool(ctx, env, ["search", "--onlyvisible", "--name", "DOSBox"])?;
    if let Some(window) = found.split_whitespace().next() {
        xdotool(ctx, env, ["windowfocus", "--sync", window])?;
    }
    Ok(())
}

// ---- Save states --------------------------------------------------------------

fn states_path(run_dir: &Path) -> PathBuf {
    run_dir.join("vp-savestates.json")
}

fn saves_dir(run_dir: &Path) -> PathBuf {
    run_dir.join(".dosbox-saves")
}

fn slot_of(entry: &Value) -> Option<i64> {
    entry.get("slot").and_then(Value::as_i64)
}

fn run_name_of(run_dir: &Path) -> String {
    run_dir
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|number| number != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
    }
}

/// `_read_states`: a missing, unreadable or malformed file reads as no states.
fn read_states(run_dir: &Path) -> Vec<Value> {
    let path = states_path(run_dir);
    if !path.is_file() {
        return Vec::new();
    }
    let Ok(text) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(payload) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let Some(states) = payload.get("states").and_then(Value::as_array) else {
        return Vec::new();
    };
    states
        .iter()
        .filter(|entry| entry.is_object() && slot_of(entry).is_some())
        .cloned()
        .collect()
}

/// `_write_states`: the Python layout, written through a temporary file.
fn write_states(run_dir: &Path, states: &[Value]) -> Res<()> {
    let payload = json!({
        "version": 1,
        "run": run_name_of(run_dir),
        "states": states,
    });
    let text = format!("{}\n", py_dumps_indent2(&payload));
    let tmp = run_dir.join("vp-savestates.json.tmp");
    io_res(fs::write(&tmp, text))?;
    io_res(fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600)))?;
    io_res(fs::rename(&tmp, states_path(run_dir)))
}

fn file_name_starts_with_dot(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.to_string_lossy().starts_with('.'))
}

/// `glob("*.sav")` in sorted order, with each file's stem.
fn sav_files(saves: &Path) -> Vec<(PathBuf, String)> {
    if !saves.is_dir() {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(saves) else {
        return Vec::new();
    };
    let mut files: Vec<(PathBuf, String)> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            !file_name_starts_with_dot(path) && path.extension().is_some_and(|ext| ext == "sav")
        })
        .map(|path| {
            let stem = path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            (path, stem)
        })
        .collect();
    files.sort();
    files
}

/// `int(stem)` when the stem is all ASCII digits (Python `isdigit`).
fn digit_slot(stem: &str) -> Option<i64> {
    if stem.is_empty() || !stem.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    stem.parse::<i64>().ok()
}

fn last_run_dir(ctx: &Ctx, st: &Session) -> Option<PathBuf> {
    if let Some(run_dir) = &st.run_dir {
        return Some(run_dir.clone());
    }
    match read_last_run(ctx) {
        Ok(Some(name)) => valid_run(ctx, &name).ok(),
        _ => None,
    }
}

/// `list_states`: recorded states plus any `.sav` file the record lacks, and
/// only the slots whose `.sav` is still present.
fn list_states_op(ctx: &Ctx, st: &mut Session) -> Res<Value> {
    let Some(run_dir) = last_run_dir(ctx, st) else {
        return Ok(json!({"run_name": null, "states": []}));
    };
    let mut states = read_states(&run_dir);
    let mut known: HashSet<i64> = states.iter().filter_map(slot_of).collect();
    let saves = saves_dir(&run_dir);
    for (path, stem) in sav_files(&saves) {
        let Some(slot) = digit_slot(&stem) else {
            continue;
        };
        if known.contains(&slot) {
            continue;
        }
        let meta = io_res(fs::metadata(&path))?;
        states.push(json!({
            "slot": slot,
            "tag": format!("slot {stem}"),
            "created": mtime_iso(&meta),
            "size": meta.len(),
        }));
        known.insert(slot);
    }
    let mut present: Vec<Value> = states
        .into_iter()
        .filter(|entry| {
            slot_of(entry).is_some_and(|slot| saves.join(format!("{slot}.sav")).is_file())
        })
        .collect();
    present.sort_by_key(|entry| slot_of(entry).unwrap_or(i64::MIN));
    Ok(json!({"run_name": run_name_of(&run_dir), "states": present}))
}

/// `goto_slot`: moves DOSBox-X's selected slot with the F12 +/- chords, taking
/// the shorter direction.
fn goto_slot(ctx: &Ctx, st: &mut Session, env: &[(String, String)], target: i64) -> Res<()> {
    let target = target.clamp(1, SAVE_SLOT_MAX);
    if target == st.active_slot {
        return Ok(());
    }
    let forward = (target - st.active_slot).rem_euclid(SAVE_SLOT_MAX);
    let backward = (st.active_slot - target).rem_euclid(SAVE_SLOT_MAX);
    let combo = if forward <= backward {
        "F12+period"
    } else {
        "F12+comma"
    };
    for _ in 0..forward.min(backward) {
        chord(ctx, env, combo)?;
        thread::sleep(Duration::from_millis(80));
    }
    st.active_slot = target;
    Ok(())
}

fn hash_bytes_of(path: &Path) -> Res<String> {
    io_res(fs::read(path)).map(|bytes| hex_sha256(&bytes))
}

/// `savestate(tag)`: allocates the first free slot, saves through the F12
/// chord, and records the slot only after DOSBox-X reports and finishes it.
fn savestate_op(ctx: &Ctx, st: &mut Session, tag: &str) -> Res<Value> {
    let (display, run_dir) = require_running(ctx, st)?;
    let saves = saves_dir(&run_dir);
    io_res(fs::create_dir_all(&saves))?;
    let mut states = read_states(&run_dir);
    let mut used: HashSet<i64> = states.iter().filter_map(slot_of).collect();
    for (_, stem) in sav_files(&saves) {
        if let Some(slot) = digit_slot(&stem) {
            used.insert(slot);
        }
    }
    let slot = (1..=SAVE_SLOT_MAX)
        .find(|candidate| !used.contains(candidate))
        .ok_or_else(|| Fault::Game("All save-state slots are in use".to_owned()))?;
    let env = game_env(&display, &run_dir);
    focus_game(ctx, &env)?;
    goto_slot(ctx, st, &env, slot)?;
    let log_offset = log_size(&run_dir);
    chord(ctx, &env, "F12+s")?;
    let actual = wait_for_saved_slot(ctx, &run_dir, log_offset)
        .filter(|found| *found != 0)
        .unwrap_or(slot);
    st.active_slot = actual;
    let path = saves.join(format!("{actual}.sav"));
    if !wait_for_stable(ctx, &path) {
        return game("DOSBox-X did not finish writing the save state");
    }
    let bytes = io_res(fs::read(&path))?;
    let tag_text = if tag.is_empty() {
        format!("slot {actual}")
    } else {
        tag.to_owned()
    };
    let entry = json!({
        "slot": actual,
        "tag": tag_text,
        "created": now_iso(),
        "session": st.session_started,
        "size": bytes.len(),
        "sha256": hex_sha256(&bytes),
    });
    states.retain(|existing| slot_of(existing) != Some(actual));
    states.push(entry.clone());
    states.sort_by_key(|existing| slot_of(existing).unwrap_or(i64::MIN));
    write_states(&run_dir, &states)?;
    journal(
        &run_dir,
        vec![
            ("action", json!("savestate")),
            ("slot", json!(actual)),
            ("tag", entry["tag"].clone()),
        ],
    )?;
    Ok(entry)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoadStatus {
    Loaded,
    Mismatch,
}

/// `loadstate(slot)`: refuses a missing or altered file, waits for the game to
/// reach its title screen, then loads through the F12 chord.
fn loadstate_op(ctx: &Ctx, st: &mut Session, slot: i64) -> Res<Value> {
    let (display, run_dir) = require_running(ctx, st)?;
    let recorded = read_states(&run_dir)
        .into_iter()
        .find(|candidate| slot_of(candidate) == Some(slot));
    let path = saves_dir(&run_dir).join(format!("{slot}.sav"));
    let entry = match recorded {
        Some(entry) if path.is_file() => entry,
        _ => return game("That save state is not available"),
    };
    if let Some(expected) = entry.get("sha256").filter(|value| truthy(value)) {
        let actual = hash_bytes_of(&path)?;
        if expected.as_str() != Some(actual.as_str()) {
            return game("The save state file changed since it was recorded");
        }
    }
    if !wait_for_game_start(ctx, &run_dir) {
        return game("The game is still starting. Wait for the title screen, then load the state.");
    }
    let env = game_env(&display, &run_dir);
    focus_game(ctx, &env)?;
    goto_slot(ctx, st, &env, slot)?;
    let log_offset = log_size(&run_dir);
    chord(ctx, &env, "F12+l")?;
    let status = wait_for_load_status(ctx, &run_dir, slot, log_offset);
    let tag = entry.get("tag").cloned().unwrap_or_else(|| json!(""));
    if status == Some(LoadStatus::Mismatch) {
        journal(
            &run_dir,
            vec![
                ("action", json!("loadstate")),
                ("slot", json!(slot)),
                ("tag", tag),
                ("verified", json!(false)),
                ("reason", json!("program-name dialog")),
            ],
        )?;
        return game(
            "DOSBox-X is still at its startup prompt. Wait for the title screen, then load the state.",
        );
    }
    let verified = status == Some(LoadStatus::Loaded);
    journal(
        &run_dir,
        vec![
            ("action", json!("loadstate")),
            ("slot", json!(slot)),
            ("tag", tag),
            ("verified", json!(verified)),
        ],
    )?;
    let mut result = entry;
    if let Value::Object(fields) = &mut result {
        fields.insert("loaded".to_owned(), json!(true));
        fields.insert("verified".to_owned(), json!(verified));
    }
    Ok(result)
}

// ---- Log-driven waits (the DOSBox-X terminal log is the protocol) -------------

fn log_size(run_dir: &Path) -> u64 {
    fs::metadata(run_dir.join("vp-dosbox-terminal.log"))
        .map(|meta| if meta.is_file() { meta.len() } else { 0 })
        .unwrap_or(0)
}

fn log_bytes_from(run_dir: &Path, offset: u64) -> Vec<u8> {
    match fs::read(run_dir.join("vp-dosbox-terminal.log")) {
        Ok(bytes) => usize::try_from(offset)
            .ok()
            .and_then(|start| bytes.get(start..))
            .map(|slice| slice.to_vec())
            .unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Every `<prefix><digits>)` occurrence, in order, as numbers.
fn marker_numbers(bytes: &[u8], prefix: &[u8]) -> Vec<i64> {
    let mut found = Vec::new();
    let mut index = 0usize;
    while let Some(offset) = find_bytes(&bytes[index..], prefix) {
        let start = index + offset + prefix.len();
        let digits = bytes[start..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if digits > 0 && bytes.get(start + digits) == Some(&b')') {
            let number = std::str::from_utf8(&bytes[start..start + digits])
                .ok()
                .and_then(|text| text.parse::<i64>().ok());
            if let Some(number) = number {
                found.push(number);
            }
        }
        index = start;
    }
    found
}

fn saved_slots(bytes: &[u8]) -> Vec<i64> {
    marker_numbers(bytes, b"Saved. (Slot ")
}

fn loaded_slots(bytes: &[u8]) -> Vec<i64> {
    marker_numbers(bytes, b"Loaded. (Slot ")
}

/// `_wait_for_saved_slot`: the slot DOSBox-X reports it saved to, from the log.
fn wait_for_saved_slot(ctx: &Ctx, run_dir: &Path, offset: u64) -> Option<i64> {
    let deadline = Instant::now() + ctx.cfg.timeouts.slot_wait;
    loop {
        let fresh = log_bytes_from(run_dir, offset);
        if let Some(slot) = saved_slots(&fresh).last().copied() {
            return Some(slot);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// `_wait_for_load_status`: "loaded" for this slot, "mismatch" for the
/// program-name dialog, or no answer before the deadline.
fn wait_for_load_status(ctx: &Ctx, run_dir: &Path, slot: i64, offset: u64) -> Option<LoadStatus> {
    let deadline = Instant::now() + ctx.cfg.timeouts.slot_wait;
    loop {
        let fresh = log_bytes_from(run_dir, offset);
        if loaded_slots(&fresh).contains(&slot) {
            return Some(LoadStatus::Loaded);
        }
        if find_bytes(&fresh, b"Aborted. Check your program name").is_some() {
            return Some(LoadStatus::Mismatch);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// `_wait_for_stable`: the save file has a non-zero size that is unchanged
/// across two polls.
fn wait_for_stable(ctx: &Ctx, path: &Path) -> bool {
    let deadline = Instant::now() + ctx.cfg.timeouts.slot_wait;
    let mut stable: Option<u64> = None;
    while Instant::now() < deadline {
        let size = fs::metadata(path)
            .ok()
            .filter(|meta| meta.is_file())
            .map_or(0, |meta| meta.len());
        if size > 0 && stable == Some(size) {
            return true;
        }
        stable = Some(size);
        thread::sleep(Duration::from_millis(200));
    }
    false
}

fn game_started(run_dir: &Path) -> bool {
    find_bytes(&log_bytes_from(run_dir, 0), GAME_START_MARKER).is_some()
}

/// `_wait_for_game_start`: START.EXE has printed its startup screen.
fn wait_for_game_start(ctx: &Ctx, run_dir: &Path) -> bool {
    let deadline = Instant::now() + ctx.cfg.timeouts.game_start;
    loop {
        if game_started(run_dir) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(250));
    }
}

// ---- Code-wheel recognition: a port of web/codeword.py ------------------------

const WORDS: [&str; 36] = [
    "AXEIAX", "BEWARE", "COPPER", "DRAGON", "EFREET", "FRIEND", "GOOGLE", "HARASH", "IXYVSI",
    "JUNGLE", "KNIGHT", "LQLGMT", "MLSSXS", "NOTNOW", "OTTAWA", "POOLRD", "QUOHOG", "RHUDIA",
    "SAVIOR", "TEMPLE", "UICDRH", "VULCAN", "WYVERN", "XRSEHK", "YUFSTA", "ZOMBIE", "1GKKRY",
    "2IOLCD", "3MASAI", "4NINER", "5GUNGA", "6BROWN", "7GNATS", "80ASIS", "9TROUT", "0SOMAS",
];
const RINGS: [&str; 2] = ["esp", "det"];

/// `wheel_word(a, b, path)`: the code word for two rune numbers and a path id.
pub(crate) fn wheel_word(first: i64, second: i64, path: i64) -> Option<String> {
    if !(1..=36).contains(&first) || !(1..=36).contains(&second) || !(0..=2).contains(&path) {
        return None;
    }
    let index = usize::try_from((first + second - 1 + path * 12) % 36).ok()?;
    let word = WORDS[index];
    Some(if word.as_bytes()[0].is_ascii_digit() {
        word[1..].to_owned()
    } else {
        word.to_owned()
    })
}

#[derive(Clone, Debug)]
pub(crate) struct Mask {
    width: usize,
    height: usize,
    pixels: Vec<u8>,
}

/// `bitmap_rgb`: masks an RGB24 buffer. A template keeps dark pixels; a rune
/// frame keeps red pixels.
pub(crate) fn bitmap_rgb(rgb: &[u8], width: usize, height: usize, kind: &str) -> Mask {
    let mut pixels = vec![0u8; width * height];
    for (index, pixel) in pixels.iter_mut().enumerate() {
        let (red, green, blue) = match rgb.get(index * 3..index * 3 + 3) {
            Some(&[r, g, b]) => (f64::from(r), f64::from(g), f64::from(b)),
            _ => (0.0, 0.0, 0.0),
        };
        let hit = if kind == "template" {
            (red + green + blue) / 3.0 < 128.0
        } else {
            red > 130.0 && red > green * 1.6 && red > blue * 1.6
        };
        *pixel = u8::from(hit);
    }
    Mask {
        width,
        height,
        pixels,
    }
}

#[derive(Clone, Debug)]
struct Comp {
    x: i64,
    y: i64,
    width: i64,
    height: i64,
    points: Vec<usize>,
}

/// `_bounds`: the bounding box of a non-empty set of pixel indices.
fn bounds(points: Vec<usize>, width: usize) -> Comp {
    let (mut x0, mut y0, mut x1, mut y1) = (i64::MAX, i64::MAX, i64::MIN, i64::MIN);
    for &index in &points {
        let x = (index % width) as i64;
        let y = (index / width) as i64;
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    Comp {
        x: x0,
        y: y0,
        width: x1 - x0 + 1,
        height: y1 - y0 + 1,
        points,
    }
}

/// `_components`: 8-connected regions of at least three pixels.
fn components(mask: &Mask) -> Vec<Comp> {
    let (width, height) = (mask.width, mask.height);
    let mut seen = vec![false; mask.pixels.len()];
    let mut result = Vec::new();
    for start in 0..mask.pixels.len() {
        if mask.pixels[start] == 0 || seen[start] {
            continue;
        }
        seen[start] = true;
        let mut todo = vec![start];
        let mut points = Vec::new();
        while let Some(index) = todo.pop() {
            points.push(index);
            let x = (index % width) as i64;
            let y = (index / width) as i64;
            for dy in -1i64..=1 {
                for dx in -1i64..=1 {
                    let (nx, ny) = (x + dx, y + dy);
                    if nx < 0 || ny < 0 || nx >= width as i64 || ny >= height as i64 {
                        continue;
                    }
                    let next = ny as usize * width + nx as usize;
                    if mask.pixels[next] != 0 && !seen[next] {
                        seen[next] = true;
                        todo.push(next);
                    }
                }
            }
        }
        if points.len() >= 3 {
            result.push(bounds(points, width));
        }
    }
    result
}

/// `normalize`: an aspect-preserving 32x32 resample, with Python's round-half-even.
pub(crate) fn normalize(mask: &Mask) -> Vec<u8> {
    let mut out = vec![0u8; 32 * 32];
    let points: Vec<usize> = mask
        .pixels
        .iter()
        .enumerate()
        .filter(|(_, value)| **value != 0)
        .map(|(index, _)| index)
        .collect();
    if points.is_empty() {
        return out;
    }
    let bx = bounds(points, mask.width);
    let scale = 28.0 / (bx.width.max(bx.height) as f64);
    let w = ((bx.width as f64 * scale).round_ties_even() as i64).max(1);
    let h = ((bx.height as f64 * scale).round_ties_even() as i64).max(1);
    let left = (32 - w) / 2;
    let top = (32 - h) / 2;
    for y in 0..h {
        let offset = (((y as f64 + 0.5) * bx.height as f64) / h as f64) as i64;
        let sy = bx.y + (bx.height - 1).min(offset);
        for x in 0..w {
            let offset = (((x as f64 + 0.5) * bx.width as f64) / w as f64) as i64;
            let sx = bx.x + (bx.width - 1).min(offset);
            out[((top + y) * 32 + left + x) as usize] =
                mask.pixels[sy as usize * mask.width + sx as usize];
        }
    }
    out
}

/// `_overlap`: the best 2*shared/total score over a one-pixel shift.
fn overlap(sample: &[u8], template: &[u8]) -> f64 {
    let total: u32 = sample
        .iter()
        .chain(template.iter())
        .map(|value| u32::from(*value))
        .sum();
    if total == 0 {
        return 0.0;
    }
    let mut best = 0.0f64;
    for dy in -1i64..=1 {
        for dx in -1i64..=1 {
            let mut shared = 0u32;
            for y in 1..31usize {
                for x in 1..31usize {
                    let ty = (y as i64 + dy) as usize;
                    let tx = (x as i64 + dx) as usize;
                    if sample[y * 32 + x] != 0 && template[ty * 32 + tx] != 0 {
                        shared += 1;
                    }
                }
            }
            best = best.max(f64::from(2 * shared) / f64::from(total));
        }
    }
    best
}

#[derive(Clone, Copy, Debug)]
struct Ranked {
    number: i64,
    score: f64,
}

/// `rank_rune`: templates of one ring, best score first (stable on ties).
fn rank_rune(glyph: &Mask, templates: &[Template], ring: &str) -> Vec<Ranked> {
    let sample = normalize(glyph);
    let mut ranked: Vec<Ranked> = templates
        .iter()
        .filter(|template| template.ring == ring)
        .map(|template| Ranked {
            number: template.number,
            score: overlap(&sample, &template.normalized),
        })
        .collect();
    ranked.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
    ranked
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Dash,
    Dot,
    Unknown,
}

/// `_path_for`: classifies a row of dots or dashes as path 0, 1 or 2.
fn path_for(row: &[&Comp]) -> Option<i64> {
    if row.len() < 3 {
        return None;
    }
    let max_height = row.iter().map(|comp| comp.height).max()?;
    let min_height = row.iter().map(|comp| comp.height).min()?;
    if max_height as f64 > min_height as f64 * 1.8 {
        return None;
    }
    let kinds: Vec<Kind> = row
        .iter()
        .map(|comp| {
            let ratio = comp.width as f64 / comp.height as f64;
            if ratio >= 2.0 {
                Kind::Dash
            } else if ratio <= 1.5 {
                Kind::Dot
            } else {
                Kind::Unknown
            }
        })
        .collect();
    if kinds.contains(&Kind::Unknown) {
        return None;
    }
    let mut ordered: Vec<&Comp> = row.to_vec();
    ordered.sort_by_key(|comp| comp.x);
    if kinds.iter().all(|kind| *kind == kinds[0]) {
        let gaps: Vec<f64> = ordered
            .windows(2)
            .map(|pair| {
                (pair[1].x as f64 + pair[1].width as f64 / 2.0)
                    - pair[0].x as f64
                    - pair[0].width as f64 / 2.0
            })
            .collect();
        let max_gap = gaps.iter().copied().fold(f64::MIN, f64::max);
        let min_gap = gaps.iter().copied().fold(f64::MAX, f64::min);
        if max_gap > min_gap * 1.35 {
            return None;
        }
        return Some(if kinds[0] == Kind::Dot { 0 } else { 2 });
    }
    if ordered.len() < 6 {
        return None;
    }
    let dashes: Vec<bool> = ordered
        .iter()
        .map(|comp| comp.width as f64 / comp.height as f64 >= 2.0)
        .collect();
    for phase in 0..3usize {
        if dashes
            .iter()
            .enumerate()
            .all(|(index, dash)| *dash == ((index + phase) % 3 == 0))
        {
            return Some(1);
        }
    }
    None
}

struct Segment {
    glyphs: Vec<Mask>,
    path: Option<i64>,
}

/// `segment_frame`: finds the two rune glyphs and the path row.
fn segment_frame(mask: &Mask) -> Segment {
    let (width, height) = (mask.width as f64, mask.height as f64);
    let (width_i, height_i) = (mask.width as i64, mask.height as i64);
    let parts: Vec<Comp> = components(mask)
        .into_iter()
        .filter(|comp| {
            comp.x > 2
                && comp.y > 2
                && comp.x + comp.width < width_i - 2
                && comp.y + comp.height < height_i - 2
                && (comp.width as f64) < width * 0.18
                && (comp.height as f64) < height * 0.24
        })
        .collect();
    let mut rows: Vec<Vec<usize>> = Vec::new();
    for (index, comp) in parts.iter().enumerate() {
        if !((comp.height as f64) <= height * 0.035 && (comp.width as f64) <= width * 0.06) {
            continue;
        }
        let center = comp.y as f64 + comp.height as f64 / 2.0;
        let tolerance = 2.0f64.max(height * 0.006);
        let position = rows.iter().position(|row| {
            let first = &parts[row[0]];
            ((first.y as f64 + first.height as f64 / 2.0) - center).abs() <= tolerance
        });
        match position {
            Some(at) => rows[at].push(index),
            None => rows.push(vec![index]),
        }
    }
    let paths: Vec<(Vec<usize>, Option<i64>)> = rows
        .iter()
        .map(|row| {
            let members: Vec<&Comp> = row.iter().map(|&index| &parts[index]).collect();
            (row.clone(), path_for(&members))
        })
        .collect();
    let valid: Vec<&(Vec<usize>, Option<i64>)> =
        paths.iter().filter(|(_, path)| path.is_some()).collect();
    let path = if valid.len() == 1 { valid[0].1 } else { None };
    let path_members: HashSet<usize> = valid
        .iter()
        .flat_map(|(row, _)| row.iter().copied())
        .collect();
    let mut groups: Vec<Comp> = parts
        .iter()
        .enumerate()
        .filter(|(index, _)| !path_members.contains(index))
        .map(|(_, comp)| comp.clone())
        .collect();
    let gap = ((width / 80.0).round_ties_even() as i64).max(2);
    let mut merged = true;
    while merged {
        merged = false;
        'search: for i in 0..groups.len() {
            for j in (i + 1)..groups.len() {
                let (a, b) = (&groups[i], &groups[j]);
                if a.x > b.x + b.width + gap
                    || b.x > a.x + a.width + gap
                    || a.y > b.y + b.height + gap
                    || b.y > a.y + a.height + gap
                {
                    continue;
                }
                let mut points = a.points.clone();
                points.extend_from_slice(&b.points);
                let together = bounds(points, mask.width);
                if (together.width as f64) >= width * 0.18
                    || (together.height as f64) >= height * 0.24
                {
                    continue;
                }
                groups[i] = together;
                groups.remove(j);
                merged = true;
                break 'search;
            }
        }
    }
    let glyphs = groups
        .iter()
        .map(|comp| {
            let mut pixels = vec![0u8; (comp.width * comp.height) as usize];
            for &index in &comp.points {
                let x = (index % mask.width) as i64;
                let y = (index / mask.width) as i64;
                pixels[((y - comp.y) * comp.width + (x - comp.x)) as usize] = 1;
            }
            Mask {
                width: comp.width as usize,
                height: comp.height as usize,
                pixels,
            }
        })
        .collect();
    Segment { glyphs, path }
}

#[derive(Clone, Debug)]
struct Template {
    ring: String,
    number: i64,
    normalized: Vec<u8>,
}

#[derive(Clone, Debug)]
struct Recognition {
    confident: bool,
    a: i64,
    b: i64,
    path: Option<i64>,
    word: Option<String>,
    glyph_count: usize,
    signature: Option<String>,
    reason: String,
}

impl Recognition {
    fn abstain(reason: &str, path: Option<i64>, glyph_count: usize) -> Self {
        Self {
            confident: false,
            a: 0,
            b: 0,
            path,
            word: None,
            glyph_count,
            signature: None,
            reason: reason.to_owned(),
        }
    }

    /// The Python result dict, in the same key order.
    fn fields(&self) -> Vec<(String, Value)> {
        let mut fields = vec![("confident".to_owned(), json!(self.confident))];
        if self.confident {
            fields.push(("a".to_owned(), json!(self.a)));
            fields.push(("b".to_owned(), json!(self.b)));
            fields.push(("path".to_owned(), json!(self.path)));
            fields.push(("word".to_owned(), json!(self.word)));
            fields.push(("glyph_count".to_owned(), json!(self.glyph_count)));
            fields.push(("signature".to_owned(), json!(self.signature)));
        } else {
            fields.push(("reason".to_owned(), json!(self.reason)));
            fields.push(("path".to_owned(), json!(self.path)));
            fields.push(("glyph_count".to_owned(), json!(self.glyph_count)));
        }
        fields
    }
}

/// `recognize_wheel`: abstains unless a complete pair, a path, and a clear
/// assignment are all present.
fn recognize_wheel(mask: &Mask, templates: &[Template]) -> Recognition {
    let segment = segment_frame(mask);
    let glyph_count = segment.glyphs.len();
    if glyph_count != 2 {
        return Recognition::abstain(
            "A complete pair of wheel runes was not found.",
            segment.path,
            glyph_count,
        );
    }
    let Some(path) = segment.path else {
        return Recognition::abstain("The path is missing or ambiguous.", None, glyph_count);
    };
    for ring in RINGS {
        let numbers: HashSet<i64> = templates
            .iter()
            .filter(|template| template.ring == ring)
            .map(|template| template.number)
            .collect();
        if numbers.len() != 36 {
            return Recognition::abstain(
                "The private rune templates are incomplete.",
                Some(path),
                glyph_count,
            );
        }
    }
    let mut assignments: Vec<(Vec<Ranked>, Vec<Ranked>, f64)> = Vec::new();
    for (first, second) in [(0usize, 1usize), (1, 0)] {
        let esp = rank_rune(&segment.glyphs[first], templates, "esp");
        let det = rank_rune(&segment.glyphs[second], templates, "det");
        let score = esp[0].score + det[0].score;
        assignments.push((esp, det, score));
    }
    assignments.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(Ordering::Equal));
    let (best_esp, best_det, best_score) = (&assignments[0].0, &assignments[0].1, assignments[0].2);
    let runner_up = assignments[1].2;
    let weak = [best_esp, best_det]
        .iter()
        .any(|ranks| ranks[0].score < 0.90 || ranks[0].score - ranks[1].score < 0.08);
    if best_score - runner_up < 0.12 || weak {
        return Recognition::abstain(
            "The rune shapes do not match clearly enough.",
            Some(path),
            glyph_count,
        );
    }
    let a = best_esp[0].number;
    let b = best_det[0].number;
    Recognition {
        confident: true,
        a,
        b,
        path: Some(path),
        word: wheel_word(a, b, path),
        glyph_count: 2,
        signature: Some(format!("{a}:{b}:{path}")),
        reason: String::new(),
    }
}

fn recognize_frame(rgb: &[u8], width: usize, height: usize, templates: &[Template]) -> Recognition {
    recognize_wheel(&bitmap_rgb(rgb, width, height, "rune"), templates)
}

fn encode_template(template: &Template) -> Value {
    json!({
        "ring": template.ring,
        "number": template.number,
        "normalized": BASE64.encode(&template.normalized),
    })
}

fn decode_template(value: &Value) -> Option<Template> {
    let ring = value.get("ring")?.as_str()?.to_owned();
    let number = value.get("number")?.as_i64()?;
    let normalized = BASE64.decode(value.get("normalized")?.as_str()?).ok()?;
    if normalized.len() != 32 * 32 {
        return None;
    }
    Some(Template {
        ring,
        number,
        normalized,
    })
}

/// `_parse_ppm`: the P6 header (whitespace and `#` comments skipped), then the
/// raw RGB24 pixels.
pub(crate) fn parse_ppm(data: &[u8]) -> Result<(Vec<u8>, usize, usize), String> {
    let mut fields: Vec<String> = Vec::new();
    let mut pos = 0usize;
    while fields.len() < 4 {
        let Some(&byte) = data.get(pos) else {
            return Err("truncated PPM header".to_owned());
        };
        if byte == b'#' {
            while data.get(pos) != Some(&b'\n') {
                if pos >= data.len() {
                    return Err("unterminated PPM comment".to_owned());
                }
                pos += 1;
            }
            continue;
        }
        if byte <= 32 {
            pos += 1;
            continue;
        }
        let start = pos;
        while data.get(pos).is_some_and(|value| *value > 32) {
            pos += 1;
        }
        let field = std::str::from_utf8(&data[start..pos]).map_err(|error| error.to_string())?;
        fields.push(field.to_owned());
    }
    pos += 1;
    let width: usize = fields[1]
        .parse()
        .map_err(|_| "invalid PPM width".to_owned())?;
    let height: usize = fields[2]
        .parse()
        .map_err(|_| "invalid PPM height".to_owned())?;
    let length = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(3))
        .ok_or_else(|| "PPM size overflow".to_owned())?;
    let pixels = data
        .get(pos..pos + length)
        .ok_or_else(|| "truncated PPM pixel data".to_owned())?
        .to_vec();
    Ok((pixels, width, height))
}

/// `_gif_to_rgb`: decodes one GIF through the captured ImageMagick `convert`.
fn gif_to_rgb(ctx: &Ctx, path: &Path) -> Result<(Vec<u8>, usize, usize), String> {
    let path_text = path.to_string_lossy().into_owned();
    let ran = run(
        &ctx.cfg.tools.convert,
        [path_text.as_str(), "-depth", "8", "ppm:-"],
        &[],
        None,
        Duration::from_secs(30),
    )?;
    if !ran.success() {
        return Err(format!("convert exited with status {}", ran.code_text()));
    }
    parse_ppm(&ran.stdout)
}

type GifStamp = (String, i64, u64);

/// Decoded codeword templates keyed by the directory they were read from.
type TemplateCache = HashMap<PathBuf, (Vec<GifStamp>, Vec<Template>)>;

/// The `(name, mtime_ns, size)` stamps of the `*.gif` files, sorted.
fn gif_stamps(dir: &Path) -> Vec<GifStamp> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut stamps: Vec<GifStamp> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !name.ends_with(".gif") {
                return None;
            }
            let meta = fs::metadata(entry.path()).ok()?;
            let mtime_ns = meta.mtime() * 1_000_000_000 + meta.mtime_nsec();
            Some((name, mtime_ns, meta.len()))
        })
        .collect();
    stamps.sort();
    stamps
}

/// `load_templates`: decodes `esp01..esp36` and `det01..det36` into masks.
fn load_templates(ctx: &Ctx, dir: &Path) -> Result<Vec<Template>, String> {
    let mut templates = Vec::new();
    for ring in RINGS {
        for number in 1..=36i64 {
            let path = dir.join(format!("{ring}{number:02}.gif"));
            let (rgb, width, height) = gif_to_rgb(ctx, &path)?;
            if rgb.len() != width * height * 3 {
                return Err("PPM pixel data is short".to_owned());
            }
            let mask = bitmap_rgb(&rgb, width, height, "template");
            templates.push(Template {
                ring: ring.to_owned(),
                number,
                normalized: normalize(&mask),
            });
        }
    }
    Ok(templates)
}

/// `templates_cached`: reloads only when the GIF stamps change.
fn templates_cached(ctx: &Ctx) -> Result<Vec<Template>, String> {
    let dir = ctx.cfg.codewheel_dir.clone();
    let stamps = gif_stamps(&dir);
    if let Some((cached_stamps, templates)) = lock(&ctx.templates).get(&dir) {
        if *cached_stamps == stamps {
            return Ok(templates.clone());
        }
    }
    let templates = load_templates(ctx, &dir)?;
    lock(&ctx.templates).insert(dir, (stamps, templates.clone()));
    Ok(templates)
}

// ---- Media: frame capture, codeword flow, stream preflight -------------------

/// `capture_frame`: grabs the DOSBox window as RGB24 and drops the border.
fn capture_frame(ctx: &Ctx, st: &mut Session, inset: i64) -> Res<(Vec<u8>, usize, usize)> {
    let (display, run_dir) = require_running(ctx, st)?;
    let env = game_env(&display, &run_dir);
    let found = xdotool(ctx, &env, ["search", "--onlyvisible", "--name", "DOSBox"])?;
    let Some(window) = found.split_whitespace().next().map(str::to_owned) else {
        return game("The game window is unavailable");
    };
    let geometry = xdotool(ctx, &env, ["getwindowgeometry", "--shell", window.as_str()])?;
    let values: HashMap<&str, &str> = geometry
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect();
    let parse = |key: &str| {
        values
            .get(key)
            .and_then(|value| value.trim().parse::<i64>().ok())
    };
    let (Some(x), Some(y), Some(width), Some(height)) =
        (parse("X"), parse("Y"), parse("WIDTH"), parse("HEIGHT"))
    else {
        return game("Could not read the game window geometry");
    };
    let crop_width = (width - 2 * inset).max(1);
    let crop_height = (height - 2 * inset).max(1);
    let video_size = format!("{width}x{height}");
    let input = format!("{display}+{x},{y}");
    let filter = format!("crop={crop_width}:{crop_height}:{inset}:{inset}");
    let ffmpeg_args = [
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "x11grab",
        "-draw_mouse",
        "0",
        "-video_size",
        video_size.as_str(),
        "-i",
        input.as_str(),
        "-vf",
        filter.as_str(),
        "-frames:v",
        "1",
        "-pix_fmt",
        "rgb24",
        "-f",
        "rawvideo",
        "pipe:1",
    ];
    let ran = match run(
        &ctx.cfg.tools.ffmpeg,
        ffmpeg_args,
        &env,
        None,
        Duration::from_secs(30),
    ) {
        Ok(ran) => ran,
        Err(error) => return failure(format!("ffmpeg failed: {error}")),
    };
    let expected = (crop_width * crop_height * 3) as usize;
    if !ran.success() || ran.stdout.len() != expected {
        return game("Could not capture the game frame");
    }
    Ok((ran.stdout, crop_width as usize, crop_height as usize))
}

fn with_typed(mut fields: Vec<(String, Value)>, typed: bool) -> Vec<(String, Value)> {
    fields.push(("typed".to_owned(), json!(typed)));
    fields
}

/// One capture (under the lock) followed by the pure recognition (off the lock).
async fn recognize_once(shared: &Arc<Shared>, templates: Arc<Vec<Template>>) -> Res<Recognition> {
    let (rgb, width, height) = on_session(Arc::clone(shared), |ctx, st| {
        capture_frame(ctx, st, FRAME_INSET)
    })
    .await?;
    blocking(move || Ok(recognize_frame(&rgb, width, height, &templates))).await
}

/// `recognize_codeword`: types the answer only when two frames agree.
async fn recognize_codeword_flow(
    shared: Arc<Shared>,
    templates: Vec<Template>,
) -> Res<Vec<(String, Value)>> {
    let templates = Arc::new(templates);
    let first = recognize_once(&shared, Arc::clone(&templates)).await?;
    if !first.confident {
        return Ok(with_typed(first.fields(), false));
    }
    tokio::time::sleep(Duration::from_millis(180)).await;
    let second = recognize_once(&shared, Arc::clone(&templates)).await?;
    if !second.confident || second.signature != first.signature {
        return Ok(vec![
            ("confident".to_owned(), json!(false)),
            ("typed".to_owned(), json!(false)),
            ("path".to_owned(), json!(second.path)),
            ("glyph_count".to_owned(), json!(second.glyph_count)),
            (
                "reason".to_owned(),
                json!("The rune prompt changed during recognition."),
            ),
        ]);
    }
    let word = first.word.clone().unwrap_or_default();
    let typed_word = word.clone();
    on_session(Arc::clone(&shared), move |ctx, st| {
        type_op(ctx, st, &typed_word, true)
    })
    .await?;
    let journal_fields = vec![
        ("action", json!("autocodeword")),
        ("word", json!(word)),
        ("a", json!(first.a)),
        ("b", json!(first.b)),
        ("path", json!(first.path)),
    ];
    on_session(Arc::clone(&shared), move |_ctx, st| {
        journal_session(st, journal_fields)
    })
    .await?;
    Ok(with_typed(first.fields(), true))
}

fn journal_session(st: &Session, fields: Vec<(&str, Value)>) -> Res<()> {
    match &st.run_dir {
        Some(run_dir) => journal(run_dir, fields),
        None => failure("no active run directory for the input journal"),
    }
}

/// The Python preconditions of `GameSession.stream`, checked under the lock.
fn stream_preflight(ctx: &Ctx, st: &mut Session, kind: GameStreamKind) -> Res<()> {
    let _ = require_running(ctx, st)?;
    if kind == GameStreamKind::Audio && st.sink_name.is_none() {
        return game("Audio capture is unavailable");
    }
    Ok(())
}

/// The ffmpeg argv `GameSession.stream` runs for each kind. The process itself
/// is not started here: see `STREAM_BLOCKER`.
pub fn stream_command(
    ffmpeg: &Path,
    kind: GameStreamKind,
    display: &str,
    sink: &str,
) -> Vec<String> {
    let mut command = vec![ffmpeg.to_string_lossy().into_owned()];
    match kind {
        GameStreamKind::Video => command.extend(
            [
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "x11grab",
                "-draw_mouse",
                "0",
                "-framerate",
                "15",
                "-video_size",
                "800x600",
                "-i",
                display,
                "-vf",
                "scale=1600:1200:flags=neighbor",
                "-q:v",
                "6",
                "-fflags",
                "nobuffer",
                "-flush_packets",
                "1",
                "-f",
                "mpjpeg",
                "pipe:1",
            ]
            .map(str::to_owned),
        ),
        GameStreamKind::Audio => {
            let monitor = format!("{sink}.monitor");
            command.extend(
                [
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-f",
                    "pulse",
                    "-i",
                    monitor.as_str(),
                    "-ac",
                    "2",
                    "-ar",
                    "48000",
                    "-c:a",
                    "libopus",
                    "-b:a",
                    "64k",
                    "-page_duration",
                    "200000",
                    "-flush_packets",
                    "1",
                    "-f",
                    "ogg",
                    "pipe:1",
                ]
                .map(str::to_owned),
            );
        }
    }
    command
}

/// `has_poolrad_sink`: `\bsink_name=vp_poolrad_\d+\b`, matched by hand.
fn has_poolrad_sink(text: &str) -> bool {
    const NEEDLE: &str = "sink_name=vp_poolrad_";
    fn is_word(byte: u8) -> bool {
        byte.is_ascii_alphanumeric() || byte == b'_'
    }
    let bytes = text.as_bytes();
    let mut from = 0usize;
    while let Some(position) = text[from..].find(NEEDLE) {
        let start = from + position;
        let boundary_before = start == 0 || !is_word(bytes[start - 1]);
        let digits_start = start + NEEDLE.len();
        let mut end = digits_start;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        let boundary_after = end == bytes.len() || !is_word(bytes[end]);
        if boundary_before && end > digits_start && boundary_after {
            return true;
        }
        from = start + 1;
    }
    false
}

/// `recover_orphans`: clears only this game's tmux server and stale PoolRad sinks.
fn recover_orphans_op(ctx: &Ctx) {
    let _ = tmux_run(ctx, ["kill-server"], Duration::from_secs(5));
    let Ok(modules) = run(
        &ctx.cfg.tools.pactl,
        ["list", "short", "modules"],
        &[],
        None,
        Duration::from_secs(5),
    ) else {
        return;
    };
    for line in modules.stdout_text().lines() {
        let fields: Vec<&str> = line.splitn(4, '\t').collect();
        if fields.len() >= 3 && fields[1] == "module-null-sink" && has_poolrad_sink(fields[2]) {
            let _ = run(
                &ctx.cfg.tools.pactl,
                ["unload-module", fields[0]],
                &[],
                None,
                Duration::from_secs(5),
            );
        }
    }
}

// ---- Public surface and route-group adapters -----------------------------------

/// The owned game session. One instance can back all four route groups; clone the
/// `Arc` to mount it behind each router.
pub struct OwnedGameSession {
    shared: Arc<Shared>,
}

impl OwnedGameSession {
    /// Resolves the configured DOSBox-X as `GameSession.__init__` does.
    pub fn new(config: SessionConfig) -> Result<Self, String> {
        let dosbox_x = resolve_dosbox(&config)?;
        Ok(Self {
            shared: Arc::new(Shared {
                ctx: Ctx {
                    cfg: config,
                    dosbox_x,
                    hashes: Mutex::new(HashMap::new()),
                    templates: Mutex::new(HashMap::new()),
                },
                session: Mutex::new(Session::new()),
            }),
        })
    }

    /// Clears this game's tmux server and any stale PoolRad sinks after a crash.
    pub fn recover_orphans(&self) {
        recover_orphans_op(&self.shared.ctx);
    }

    /// Stops the owned session, as `GAME.stop()` does at application shutdown.
    pub fn shutdown(&self) -> Result<Value, String> {
        let mut session = lock(&self.shared.session);
        stop_op(&self.shared.ctx, &mut session).map_err(Fault::into_text)
    }
}

impl GameLifecycleBackend for OwnedGameSession {
    fn status(&self) -> Result<Value, String> {
        let mut session = lock(&self.shared.session);
        status_session(&self.shared.ctx, &mut session).map_err(Fault::into_text)
    }

    /// Start failures all surface as 503 with the message. The trait cannot
    /// express Python's 500 for non-`GameError` exceptions.
    fn start(&self, fresh: bool) -> Result<Value, String> {
        let mut session = lock(&self.shared.session);
        start_session(&self.shared.ctx, &mut session, fresh).map_err(Fault::into_text)
    }
}

/// The input route maps every `Err` to 400, so an internal failure cannot be a
/// 500 through this trait. It is reported as a generic 400.
fn input_error(fault: Fault) -> GameInputError {
    match fault {
        Fault::Game(text) => GameInputError(text),
        Fault::Failure(_) => GameInputError("Could not send game input".to_owned()),
    }
}

impl GameInputBackend for OwnedGameSession {
    fn stop<'a>(&'a self) -> GameInputFuture<'a, Result<Value, GameInputError>> {
        let shared = Arc::clone(&self.shared);
        Box::pin(async move { on_session(shared, stop_op).await.map_err(input_error) })
    }

    fn key<'a>(&'a self, key: &'a str) -> GameInputFuture<'a, Result<(), GameInputError>> {
        let shared = Arc::clone(&self.shared);
        let key = key.to_owned();
        Box::pin(async move {
            on_session(shared, move |ctx, st| key_op(ctx, st, &key))
                .await
                .map_err(input_error)
        })
    }

    fn type_text<'a>(
        &'a self,
        text: &'a str,
        submit: bool,
    ) -> GameInputFuture<'a, Result<(), GameInputError>> {
        let shared = Arc::clone(&self.shared);
        let text = text.to_owned();
        Box::pin(async move {
            on_session(shared, move |ctx, st| type_op(ctx, st, &text, submit))
                .await
                .map_err(input_error)
        })
    }
}

fn state_error(fault: Fault) -> GameStateError {
    match fault {
        Fault::Game(text) => GameStateError::Game(text),
        Fault::Failure(text) => GameStateError::Failure(text),
    }
}

/// `tag.strip()[:80]` on code points. A lone surrogate cannot round-trip through
/// this JSON reader, so such a tag is refused with a 400 instead of being written.
fn normalize_tag(code_points: &[u32]) -> Result<String, GameStateError> {
    let mut text = String::with_capacity(code_points.len());
    for &point in code_points {
        match char::from_u32(point) {
            Some(ch) => text.push(ch),
            None => {
                return Err(GameStateError::Game(
                    "Save-state tag must be valid text".to_owned(),
                ))
            }
        }
    }
    Ok(text.trim_matches(py_isspace).chars().take(80).collect())
}

impl GameStateBackend for OwnedGameSession {
    fn list_states(&self) -> GameStateFuture<'_> {
        let shared = Arc::clone(&self.shared);
        Box::pin(async move {
            on_session(shared, list_states_op)
                .await
                .map_err(state_error)
        })
    }

    fn savestate(&self, tag: PythonString) -> GameStateFuture<'_> {
        let shared = Arc::clone(&self.shared);
        Box::pin(async move {
            let tag = normalize_tag(tag.code_points())?;
            on_session(shared, move |ctx, st| savestate_op(ctx, st, &tag))
                .await
                .map_err(state_error)
        })
    }

    fn loadstate(&self, slot: i64) -> GameStateFuture<'_> {
        let shared = Arc::clone(&self.shared);
        Box::pin(async move {
            on_session(shared, move |ctx, st| loadstate_op(ctx, st, slot))
                .await
                .map_err(state_error)
        })
    }
}

fn media_error(fault: Fault) -> GameMediaError {
    match fault {
        Fault::Game(text) => GameMediaError::Game(text),
        Fault::Failure(text) => GameMediaError::Failure(text),
    }
}

impl GameMediaBackend for OwnedGameSession {
    fn templates_cached(&self) -> GameMediaFuture<'_, Result<Vec<Value>, GameMediaError>> {
        let shared = Arc::clone(&self.shared);
        Box::pin(async move {
            match blocking(move || Ok(templates_cached(&shared.ctx))).await {
                Ok(Ok(templates)) => Ok(templates.iter().map(encode_template).collect()),
                // Python swallows OSError, SubprocessError and ValueError here and
                // continues with an empty template list.
                Ok(Err(message)) => Err(GameMediaError::Template(message)),
                Err(fault) => Err(media_error(fault)),
            }
        })
    }

    fn recognize_codeword<'a>(
        &'a self,
        templates: Vec<Value>,
    ) -> GameMediaFuture<'a, Result<GameMediaJson, GameMediaError>> {
        let shared = Arc::clone(&self.shared);
        Box::pin(async move {
            let decoded = templates
                .iter()
                .map(decode_template)
                .collect::<Option<Vec<Template>>>();
            let Some(decoded) = decoded else {
                return Err(GameMediaError::Failure(
                    "malformed codeword template".to_owned(),
                ));
            };
            match recognize_codeword_flow(shared, decoded).await {
                Ok(fields) => Ok(GameMediaJson::new(fields)),
                Err(fault) => Err(media_error(fault)),
            }
        })
    }

    fn status(&self) -> GameMediaFuture<'_, Result<Value, GameMediaError>> {
        let shared = Arc::clone(&self.shared);
        Box::pin(async move {
            on_session(shared, status_session)
                .await
                .map_err(media_error)
        })
    }

    /// Runs the Python preconditions, then refuses with `STREAM_BLOCKER`. No
    /// ffmpeg process is started and no fake stream is returned. Live video and
    /// audio are NOT implemented.
    fn stream<'a>(
        &'a self,
        kind: GameStreamKind,
    ) -> GameMediaFuture<'a, Result<Body, GameMediaError>> {
        let shared = Arc::clone(&self.shared);
        Box::pin(async move {
            match on_session(shared, move |ctx, st| stream_preflight(ctx, st, kind)).await {
                Ok(()) => Err(GameMediaError::Failure(STREAM_BLOCKER.to_owned())),
                Err(fault) => Err(media_error(fault)),
            }
        })
    }
}
