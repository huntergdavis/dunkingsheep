//! Owned Poolrad game-session backend tests (t3211).
//!
//! Every external program is a fixture script inside a temp directory. The real
//! DOSBox-X, the real game, the network and live files are never reached.
//! Routes are driven through the real `game_lifecycle`, `game_input`,
//! `game_state` and `game_media` routers with the backend mounted behind them.

#[path = "../src/game_backend.rs"]
mod game_backend;

// The module reaches the route types through `crate::routes`. Mirror the
// library's public route tree at the test crate root so those paths resolve.
mod routes {
    pub use ds_web::routes::*;
}

use std::{
    fs,
    os::unix::fs::{symlink, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    Extension, Router,
};
use ds_web::routes::{
    auth::{AuthBackend, AuthError, AuthFuture, Identity, RequestScheme},
    game_input::{self, GameInputBackend},
    game_lifecycle::{self, GameLifecycleBackend, GameLifecycleState},
    game_media::{self, GameMediaBackend, GameStreamKind},
    game_state::{self, GameStateBackend},
};
use game_backend::{OwnedGameSession, PlayBuild, SessionConfig, Timeouts, Tools};
use serde_json::{json, Value};
use tower::ServiceExt as _;

const TMUX_SCRIPT: &str = r#"#!/bin/sh
STATE='@STATE@'
if [ "$1" = "-L" ]; then shift 2; fi
cmd="$1"
shift
case "$cmd" in
  new-session)
    name=""
    while [ $# -gt 0 ]; do
      case "$1" in
        -s) name="$2"; shift 2 ;;
        -x|-y) shift 2 ;;
        -d) shift ;;
        *) break ;;
      esac
    done
    printf '%s' "$name" > "$STATE/tmux.session"
    echo "new-session $name" >> "$STATE/calls.log"
    sh -c "$1" > "$STATE/pane.out" 2>&1 &
    echo $! > "$STATE/tmux.pid"
    exit 0 ;;
  has-session)
    [ -f "$STATE/tmux.pid" ] || exit 1
    kill -0 "$(cat "$STATE/tmux.pid")" 2>/dev/null || exit 1
    exit 0 ;;
  kill-session)
    echo "kill-session" >> "$STATE/calls.log"
    if [ -f "$STATE/tmux.pid" ]; then
      kill -TERM "$(cat "$STATE/tmux.pid")" 2>/dev/null
      rm -f "$STATE/tmux.pid"
    fi
    exit 0 ;;
  kill-server)
    echo "kill-server" >> "$STATE/calls.log"
    if [ -f "$STATE/tmux.pid" ]; then
      kill -TERM "$(cat "$STATE/tmux.pid")" 2>/dev/null
      rm -f "$STATE/tmux.pid"
    fi
    exit 0 ;;
  pipe-pane)
    echo "pipe-pane" >> "$STATE/calls.log"
    exit 0 ;;
esac
exit 0
"#;

const XVFB_SCRIPT: &str = r#"#!/bin/sh
echo $$ > '@STATE@/xvfb.pid'
echo "xvfb $*" >> '@STATE@/calls.log'
exec sleep 600
"#;

const XAUTH_SCRIPT: &str = "#!/bin/sh\nexit 0\n";

const XDOTOOL_SCRIPT: &str = r#"#!/bin/sh
STATE='@STATE@'
echo "$*" >> "$STATE/xdotool.log"
run_dir=$(dirname "$XAUTHORITY")
log="$run_dir/vp-dosbox-terminal.log"
case "$1" in
  getdisplaygeometry)
    echo "800 600"
    exit 0 ;;
  search)
    [ -f "$STATE/no-window" ] && exit 1
    echo 4242
    exit 0 ;;
  windowfocus)
    exit 0 ;;
  getwindowgeometry)
    printf 'WINDOW=4242\nX=10\nY=20\nWIDTH=640\nHEIGHT=480\nSCREEN=0\n'
    exit 0 ;;
  key)
    for key; do :; done
    slot=$(cat "$STATE/slot" 2>/dev/null || echo 1)
    case "$key" in
      F12+s)
        printf 'Saved. (Slot %s)\n' "$slot" >> "$log"
        mkdir -p "$run_dir/.dosbox-saves"
        printf 'STATE-%s\n' "$slot" > "$run_dir/.dosbox-saves/$slot.sav" ;;
      F12+l)
        if [ -f "$run_dir/.dosbox-saves/$slot.sav" ]; then
          if [ -f "$STATE/mismatch" ]; then
            printf 'Aborted. Check your program name\n' >> "$log"
          else
            printf 'Loaded. (Slot %s)\n' "$slot" >> "$log"
          fi
        fi ;;
      F12+period) echo $(( slot % 99 + 1 )) > "$STATE/slot" ;;
      F12+comma) echo $(( (slot + 97) % 99 + 1 )) > "$STATE/slot" ;;
    esac
    exit 0 ;;
esac
exit 0
"#;

const PACTL_SCRIPT: &str = r#"#!/bin/sh
echo "pactl $*" >> '@STATE@/calls.log'
case "$1" in
  load-module)
    echo 7
    exit 0 ;;
  unload-module)
    echo "unload $2" >> '@STATE@/calls.log'
    exit 0 ;;
  list)
    if [ -f '@STATE@/stale-sinks' ]; then
      printf '3\tmodule-null-sink\tsink_name=vp_poolrad_999 rate=48000\n'
    fi
    exit 0 ;;
esac
exit 0
"#;

const BWRAP_SCRIPT: &str = r#"#!/bin/sh
DOSBOX='@DOSBOX@'
while [ $# -gt 0 ]; do
  if [ "$1" = "$DOSBOX" ]; then exec "$@"; fi
  shift
done
exit 97
"#;

const FFMPEG_SCRIPT: &str = r#"#!/bin/sh
echo "ffmpeg $*" >> '@STATE@/calls.log'
cat '@STATE@/frame.rgb'
"#;

const CONVERT_SCRIPT: &str = r#"#!/bin/sh
base=$(basename "$1")
f='@STATE@/templates/'"$base"'.ppm'
[ -f "$f" ] || exit 1
cat "$f"
"#;

const DOSBOX_SCRIPT: &str = r#"#!/bin/sh
savedir=""
while [ $# -gt 0 ]; do
  if [ "$1" = "-savedir" ]; then savedir="$2"; shift 2; else shift; fi
done
run_dir=$(dirname "$savedir")
echo 1 > '@STATE@/slot'
if [ ! -f '@STATE@/no-start-marker' ]; then
  printf 'Packed file is corrupt\n' >> "$run_dir/vp-dosbox-terminal.log"
fi
exec sleep 600
"#;

const PREPARE_SCRIPT: &str = r#"#!/bin/sh
run='@REPO@/resources/oracle-runs/'"$1"
mkdir -p "$run/POOLRAD"
printf 'input\n' > "$run/.oracle-input-sha256"
printf 'exe\n' > "$run/POOLRAD/START.EXE"
echo "prepare $1" >> '@STATE@/calls.log'
"#;

struct AllowAuth;

impl AuthBackend for AllowAuth {
    fn login<'a>(
        &'a self,
        _address: &'a str,
        _password: &'a str,
    ) -> AuthFuture<'a, Result<String, AuthError>> {
        Box::pin(async { Err(AuthError::Internal("unexpected login".into())) })
    }

    fn identity<'a>(
        &'a self,
        _bearer: Option<&'a str>,
        _cookie: Option<&'a str>,
    ) -> AuthFuture<'a, Result<Identity, AuthError>> {
        Box::pin(async { Ok(Identity::Owner) })
    }

    fn logout<'a>(&'a self, _cookie: Option<&'a str>) -> AuthFuture<'a, Result<(), AuthError>> {
        Box::pin(async { Err(AuthError::Internal("unexpected logout".into())) })
    }

    fn change_password<'a>(
        &'a self,
        _current: &'a str,
        _new: &'a str,
        _keep_cookie: Option<&'a str>,
    ) -> AuthFuture<'a, Result<(), AuthError>> {
        Box::pin(async { Err(AuthError::Internal("unexpected password change".into())) })
    }
}

fn auth() -> Arc<dyn AuthBackend> {
    Arc::new(AllowAuth)
}

fn quick_timeouts() -> Timeouts {
    Timeouts {
        command: Duration::from_secs(10),
        game_start: Duration::from_millis(600),
        slot_wait: Duration::from_secs(3),
        xvfb_ready_polls: 30,
        post_launch: Duration::from_secs(1),
        terminate: Duration::from_secs(1),
    }
}

/// A private fixture world: fake tools, a fake repo and fake data directories.
struct Harness {
    _dir: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
    data: PathBuf,
    codewheel: PathBuf,
    state: PathBuf,
    bin: PathBuf,
    dosbox: PathBuf,
    manifest: PathBuf,
}

fn write_script(path: &Path, body: &str) {
    fs::write(path, body).expect("write fixture script");
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod fixture script");
}

impl Harness {
    fn new() -> Self {
        let target_tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
        fs::create_dir_all(target_tmp).expect("target tmp directory");
        let dir = tempfile::Builder::new()
            .prefix("t3211-game-backend-")
            .tempdir_in(target_tmp)
            .expect("tempdir under the worktree target directory");
        let root = dir.path().to_path_buf();
        let repo = root.join("repo");
        let data = root.join("data");
        let codewheel = root.join("codewheel");
        let state = root.join("state");
        let bin = root.join("bin");
        let probe = root.join("x11-probe");
        for path in [&repo, &data, &codewheel, &state, &bin, &probe] {
            fs::create_dir_all(path).expect("create fixture directory");
        }
        fs::create_dir_all(state.join("templates")).expect("templates directory");
        let harness = Self {
            _dir: dir,
            dosbox: bin.join("dosbox-x"),
            manifest: root.join("state-archives/manifest.json"),
            root,
            repo,
            data,
            codewheel,
            state,
            bin,
        };
        harness.write_fixtures();
        harness
    }

    fn substitute(&self, template: &str) -> String {
        template
            .replace("@STATE@", &self.state.to_string_lossy())
            .replace("@REPO@", &self.repo.to_string_lossy())
            .replace("@DOSBOX@", &self.dosbox.to_string_lossy())
    }

    fn write_fixtures(&self) {
        let scripts = [
            ("tmux", TMUX_SCRIPT),
            ("Xvfb", XVFB_SCRIPT),
            ("xauth", XAUTH_SCRIPT),
            ("xdotool", XDOTOOL_SCRIPT),
            ("pactl", PACTL_SCRIPT),
            ("bwrap", BWRAP_SCRIPT),
            ("ffmpeg", FFMPEG_SCRIPT),
            ("convert", CONVERT_SCRIPT),
        ];
        for (name, template) in scripts {
            write_script(&self.bin.join(name), &self.substitute(template));
        }
        write_script(&self.dosbox, &self.substitute(DOSBOX_SCRIPT));
        let tools = self.repo.join("tools/oracle");
        fs::create_dir_all(&tools).expect("tools directory");
        write_script(
            &tools.join("prepare_run.sh"),
            &self.substitute(PREPARE_SCRIPT),
        );
        fs::create_dir_all(self.repo.join("resources/oracle-runs")).expect("runs directory");
    }

    fn config(&self, timeouts: Timeouts) -> SessionConfig {
        let inherited = std::env::var("PATH").unwrap_or_default();
        let path_var = format!("{}:{inherited}", self.bin.display());
        SessionConfig {
            repo: self.repo.clone(),
            data_dir: self.data.clone(),
            archive_manifest: self.manifest.clone(),
            codewheel_dir: self.codewheel.clone(),
            play_build: PlayBuild::Original,
            dosbox_configured: self.dosbox.to_string_lossy().into_owned(),
            replacement_registry: self.repo.join("resources/hybrid/registry_all.tsv"),
            replacement_dispatch: self.repo.join("resources/hybrid/play-dispatch.txt"),
            tools: Tools::capture(&path_var),
            path_var,
            probe_dir: self.root.join("x11-probe"),
            timeouts,
        }
    }

    fn session(&self) -> Arc<OwnedGameSession> {
        Arc::new(OwnedGameSession::new(self.config(quick_timeouts())).expect("session"))
    }

    /// The single prepared run directory created by the fake prepare script.
    fn run_dir(&self) -> PathBuf {
        let mut entries: Vec<PathBuf> = fs::read_dir(self.repo.join("resources/oracle-runs"))
            .expect("runs directory")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        entries.sort();
        entries.pop().expect("a prepared run")
    }

    fn read_state_file(&self, name: &str) -> String {
        fs::read_to_string(self.state.join(name)).unwrap_or_default()
    }
}

fn lifecycle_app(session: &Arc<OwnedGameSession>, codewheel: &Path) -> Router {
    let backend: Arc<dyn GameLifecycleBackend> = session.clone();
    game_lifecycle::router(
        GameLifecycleState::new(codewheel, backend),
        auth(),
        RequestScheme::Http,
    )
}

fn input_app(session: &Arc<OwnedGameSession>) -> Router {
    let backend: Arc<dyn GameInputBackend> = session.clone();
    game_input::router(backend, auth(), RequestScheme::Http)
}

fn state_app(session: &Arc<OwnedGameSession>) -> Router {
    let backend: Arc<dyn GameStateBackend> = session.clone();
    game_state::router_with_scheme(RequestScheme::Http)
        .layer(Extension(auth()))
        .layer(Extension(backend))
}

fn media_app(session: &Arc<OwnedGameSession>) -> Router {
    let backend: Arc<dyn GameMediaBackend> = session.clone();
    game_media::router(backend, auth(), RequestScheme::Http)
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Runtime::new().expect("tokio runtime")
}

/// Sends one request through a router and returns the status and JSON body.
fn send(app: Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let runtime = runtime();
    let request = match body {
        Some(json) => Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from(json.to_string())),
        None => Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty()),
    }
    .expect("request");
    let response = runtime
        .block_on(app.oneshot(request))
        .expect("infallible service");
    let status = response.status();
    let bytes = runtime
        .block_on(to_bytes(response.into_body(), usize::MAX))
        .expect("response body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Sends one GET and returns the status, content type and raw body.
fn send_raw(app: Router, uri: &str) -> (StatusCode, String, Vec<u8>) {
    let runtime = runtime();
    let request = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .expect("request");
    let response = runtime
        .block_on(app.oneshot(request))
        .expect("infallible service");
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let bytes = runtime
        .block_on(to_bytes(response.into_body(), usize::MAX))
        .expect("response body");
    (status, content_type, bytes.to_vec())
}

fn start(session: &Arc<OwnedGameSession>, codewheel: &Path, fresh: bool) -> (StatusCode, Value) {
    send(
        lifecycle_app(session, codewheel),
        "POST",
        "/api/game/start",
        Some(json!({ "fresh": fresh })),
    )
}

fn stop(session: &Arc<OwnedGameSession>) -> (StatusCode, Value) {
    send(input_app(session), "POST", "/api/game/stop", None)
}

fn sha256_hex(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn read_pid(path: &Path) -> u32 {
    fs::read_to_string(path)
        .expect("pid file")
        .trim()
        .parse()
        .expect("numeric pid")
}

/// A process is alive unless it is gone or a zombie.
fn pid_alive(pid: u32) -> bool {
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat
            .rsplit(") ")
            .next()
            .and_then(|rest| rest.chars().next())
            .is_some_and(|state| state != 'Z'),
        Err(_) => false,
    }
}

fn detail(body: &Value) -> &str {
    body["detail"].as_str().unwrap_or_default()
}

// ---- Lifecycle, status and launch --------------------------------------------

#[test]
fn status_before_start_reports_next_start_and_pending_archive() {
    let harness = Harness::new();
    let session = harness.session();
    let (status, body) = send(
        lifecycle_app(&session, &harness.codewheel),
        "GET",
        "/api/game/status",
        None,
    );
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["running"], json!(false));
    assert_eq!(body["audio"], json!(false));
    assert_eq!(body["build"], json!("original"));
    assert_eq!(body["emulator"]["scope"], json!("next_start"));
    assert_eq!(body["emulator"]["kind"], json!("other"));
    assert_eq!(
        body["emulator"]["sha256"],
        json!(sha256_hex(&fs::read(&harness.dosbox).unwrap()))
    );
    assert_eq!(body["state_archive"]["status"], json!("pending"));
    assert_eq!(body["run_name"], Value::Null);
}

#[test]
fn start_launches_the_owned_session_and_status_reports_running() {
    let harness = Harness::new();
    let session = harness.session();
    let (status, body) = start(&session, &harness.codewheel, true);
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["running"], json!(true));
    assert_eq!(body["audio"], json!(true));
    assert_eq!(body["emulator"]["scope"], json!("running_session"));
    let run_dir = harness.run_dir();
    assert_eq!(
        body["run_name"],
        json!(run_dir.file_name().unwrap().to_string_lossy())
    );
    assert!(harness.state.join("xvfb.pid").exists(), "Xvfb was launched");
    assert!(harness
        .read_state_file("calls.log")
        .contains("new-session oracle"));
    assert_eq!(
        fs::read_to_string(harness.data.join("poolrad-last-run.txt")).unwrap(),
        format!("{}\n", run_dir.file_name().unwrap().to_string_lossy())
    );
    let _ = stop(&session);
}

#[test]
fn stop_terminates_the_owned_display_and_session() {
    let harness = Harness::new();
    let session = harness.session();
    let (status, _) = start(&session, &harness.codewheel, true);
    assert_eq!(status, StatusCode::OK);
    let xvfb_pid = read_pid(&harness.state.join("xvfb.pid"));
    assert!(pid_alive(xvfb_pid));
    let (status, body) = stop(&session);
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["running"], json!(false));
    assert_eq!(body["audio"], json!(false));
    assert!(!harness.state.join("tmux.pid").exists());
    assert!(!pid_alive(xvfb_pid), "the Xvfb process group is gone");
}

#[test]
fn restart_reuses_the_last_run_without_preparing_a_new_copy() {
    let harness = Harness::new();
    let session = harness.session();
    let (first_status, first) = start(&session, &harness.codewheel, true);
    assert_eq!(first_status, StatusCode::OK, "{first}");
    let _ = stop(&session);
    let (status, second) = start(&session, &harness.codewheel, false);
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(second["run_name"], first["run_name"]);
    let prepares = harness
        .read_state_file("calls.log")
        .lines()
        .filter(|line| line.starts_with("prepare "))
        .count();
    assert_eq!(prepares, 1, "fresh=false reuses the recorded run");
    let _ = stop(&session);
}

#[test]
fn start_refuses_an_invalid_recorded_run_name_with_503() {
    let harness = Harness::new();
    fs::write(harness.data.join("poolrad-last-run.txt"), "../escape\n").unwrap();
    let session = harness.session();
    let (status, body) = start(&session, &harness.codewheel, false);
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(detail(&body), "Invalid run name");
}

#[test]
fn start_reports_an_unavailable_checkout() {
    let harness = Harness::new();
    let mut config = harness.config(quick_timeouts());
    config.repo = harness.root.join("missing-checkout");
    let session = Arc::new(OwnedGameSession::new(config).expect("session"));
    let (status, body) = start(&session, &harness.codewheel, true);
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(detail(&body), "Poolrad checkout is unavailable");
}

#[test]
fn start_refuses_a_missing_absolute_dosbox_binary() {
    let harness = Harness::new();
    let mut config = harness.config(quick_timeouts());
    config.dosbox_configured = harness
        .root
        .join("absent/dosbox-x")
        .to_string_lossy()
        .into_owned();
    let session = Arc::new(OwnedGameSession::new(config).expect("absolute path is accepted"));
    let (status, body) = start(&session, &harness.codewheel, true);
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(detail(&body), "Configured DOSBox-X binary is unavailable");
}

// ---- Code-wheel allowlist ------------------------------------------------------

#[test]
fn codewheel_serves_only_allowlisted_gifs_and_refuses_symlinks() {
    let harness = Harness::new();
    fs::write(harness.codewheel.join("esp01.gif"), b"GIF89a-fixture").unwrap();
    symlink(
        harness.codewheel.join("esp01.gif"),
        harness.codewheel.join("esp02.gif"),
    )
    .unwrap();
    let session = harness.session();
    let app = lifecycle_app(&session, &harness.codewheel);

    let (status, content_type, bytes) = send_raw(app.clone(), "/api/game/codewheel/esp01.gif");
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type, "image/gif");
    assert_eq!(bytes, b"GIF89a-fixture");

    let (status, _, body) = send_raw(app.clone(), "/api/game/codewheel/esp37.gif");
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(String::from_utf8_lossy(&body).contains("Unknown rune template"));

    let (status, _, body) = send_raw(app, "/api/game/codewheel/esp02.gif");
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(String::from_utf8_lossy(&body).contains("Private rune template unavailable"));
}

// ---- Input: keys and text -------------------------------------------------------

#[test]
fn key_and_type_refuse_bad_input_and_forward_valid_input_when_running() {
    let harness = Harness::new();
    let session = harness.session();

    let (status, body) = send(
        input_app(&session),
        "POST",
        "/api/game/key",
        Some(json!({"key": "a"})),
    );
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(detail(&body), "Poolrad is not running");

    let (status, _) = start(&session, &harness.codewheel, true);
    assert_eq!(status, StatusCode::OK);

    let (status, body) = send(
        input_app(&session),
        "POST",
        "/api/game/key",
        Some(json!({"key": "Shift_L"})),
    );
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(detail(&body), "Unsupported game key");

    let (status, body) = send(
        input_app(&session),
        "POST",
        "/api/game/key",
        Some(json!({"key": "F12"})),
    );
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"ok": true}));

    let (status, body) = send(
        input_app(&session),
        "POST",
        "/api/game/type",
        Some(json!({"text": "a\u{1}b"})),
    );
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(detail(&body), "Unsupported text");

    let (status, body) = send(
        input_app(&session),
        "POST",
        "/api/game/type",
        Some(json!({"text": "hello"})),
    );
    assert_eq!(status, StatusCode::OK, "{body}");

    let xdotool_log = harness.read_state_file("xdotool.log");
    assert!(xdotool_log.contains("key --clearmodifiers --delay 70 F12"));
    assert!(xdotool_log.contains("type --clearmodifiers --delay 60 hello"));
    assert!(xdotool_log.contains("key --clearmodifiers --delay 70 Return"));

    let journal = fs::read_to_string(harness.run_dir().join("vp-input.jsonl")).unwrap();
    assert!(journal.contains("\"key\":\"F12\""));
    assert!(journal.contains("\"type\":\"hello\""));
    assert!(journal.contains("\"submit\":true"));
    let _ = stop(&session);
}

// ---- Save states -------------------------------------------------------------------

#[test]
fn savestate_records_identity_and_load_verifies_the_recorded_file() {
    let harness = Harness::new();
    let session = harness.session();
    let (status, _) = start(&session, &harness.codewheel, true);
    assert_eq!(status, StatusCode::OK);
    let run_dir = harness.run_dir();

    let (status, saved) = send(
        state_app(&session),
        "POST",
        "/api/game/savestate",
        Some(json!({"tag": "  quick  "})),
    );
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["slot"], json!(1));
    assert_eq!(saved["tag"], json!("quick"));
    let bytes = fs::read(run_dir.join(".dosbox-saves/1.sav")).unwrap();
    assert_eq!(saved["size"], json!(bytes.len()));
    assert_eq!(saved["sha256"], json!(sha256_hex(&bytes)));

    let (status, listed) = send(state_app(&session), "GET", "/api/game/states", None);
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed["states"][0]["slot"], json!(1));
    assert_eq!(listed["states"][0]["tag"], json!("quick"));

    let state_file = fs::read_to_string(run_dir.join("vp-savestates.json")).unwrap();
    assert!(
        state_file.starts_with("{\n  \"version\": 1,\n  \"run\": "),
        "{state_file}"
    );

    let (status, loaded) = send(
        state_app(&session),
        "POST",
        "/api/game/loadstate",
        Some(json!({"slot": 1})),
    );
    assert_eq!(status, StatusCode::OK, "{loaded}");
    assert_eq!(loaded["loaded"], json!(true));
    assert_eq!(loaded["verified"], json!(true));
    let _ = stop(&session);
}

#[test]
fn savestate_refuses_when_every_slot_is_in_use() {
    let harness = Harness::new();
    let session = harness.session();
    let (status, _) = start(&session, &harness.codewheel, true);
    assert_eq!(status, StatusCode::OK);
    let saves = harness.run_dir().join(".dosbox-saves");
    for slot in 1..=99 {
        fs::write(saves.join(format!("{slot}.sav")), b"x").unwrap();
    }
    let (status, body) = send(
        state_app(&session),
        "POST",
        "/api/game/savestate",
        Some(json!({"tag": "full"})),
    );
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(detail(&body), "All save-state slots are in use");
    let _ = stop(&session);
}

#[test]
fn loadstate_refuses_unknown_and_altered_slots() {
    let harness = Harness::new();
    let session = harness.session();
    let (status, _) = start(&session, &harness.codewheel, true);
    assert_eq!(status, StatusCode::OK);

    let (status, body) = send(
        state_app(&session),
        "POST",
        "/api/game/loadstate",
        Some(json!({"slot": 5})),
    );
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(detail(&body), "That save state is not available");

    let (status, _) = send(
        state_app(&session),
        "POST",
        "/api/game/savestate",
        Some(json!({"tag": "before"})),
    );
    assert_eq!(status, StatusCode::OK);
    fs::write(harness.run_dir().join(".dosbox-saves/1.sav"), b"tampered").unwrap();
    let (status, body) = send(
        state_app(&session),
        "POST",
        "/api/game/loadstate",
        Some(json!({"slot": 1})),
    );
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        detail(&body),
        "The save state file changed since it was recorded"
    );
    let _ = stop(&session);
}

#[test]
fn loadstate_refuses_the_program_name_dialog_and_journals_it() {
    let harness = Harness::new();
    let session = harness.session();
    let (status, _) = start(&session, &harness.codewheel, true);
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(
        state_app(&session),
        "POST",
        "/api/game/savestate",
        Some(json!({"tag": "dialog"})),
    );
    assert_eq!(status, StatusCode::OK);
    fs::write(harness.state.join("mismatch"), b"").unwrap();
    let (status, body) = send(
        state_app(&session),
        "POST",
        "/api/game/loadstate",
        Some(json!({"slot": 1})),
    );
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(detail(&body).starts_with("DOSBox-X is still at its startup prompt"));
    let journal = fs::read_to_string(harness.run_dir().join("vp-input.jsonl")).unwrap();
    assert!(journal.contains("\"reason\":\"program-name dialog\""));
    let _ = stop(&session);
}

#[test]
fn loadstate_waits_for_the_title_screen_before_loading() {
    let harness = Harness::new();
    fs::write(harness.state.join("no-start-marker"), b"").unwrap();
    let session = harness.session();
    let (status, _) = start(&session, &harness.codewheel, true);
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(
        state_app(&session),
        "POST",
        "/api/game/savestate",
        Some(json!({"tag": "early"})),
    );
    assert_eq!(status, StatusCode::OK);
    let (status, body) = send(
        state_app(&session),
        "POST",
        "/api/game/loadstate",
        Some(json!({"slot": 1})),
    );
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(detail(&body).starts_with("The game is still starting"));
    let _ = stop(&session);
}

// ---- Media ---------------------------------------------------------------------------

#[test]
fn video_and_audio_report_not_running_before_start() {
    let harness = Harness::new();
    let session = harness.session();
    let (status, _) = send(media_app(&session), "GET", "/api/game/video", None);
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(media_app(&session), "GET", "/api/game/audio", None);
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[test]
fn live_video_and_audio_fail_explicitly_while_streaming_is_blocked() {
    let harness = Harness::new();
    let session = harness.session();
    let (status, _) = start(&session, &harness.codewheel, true);
    assert_eq!(status, StatusCode::OK);
    // Live streaming is NOT implemented: see STREAM_BLOCKER in game_backend.rs.
    // Until the body adapter exists, the routes refuse with a 500.
    let (status, _) = send(media_app(&session), "GET", "/api/game/video", None);
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let (status, _) = send(media_app(&session), "GET", "/api/game/audio", None);
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        !harness.read_state_file("calls.log").contains("ffmpeg"),
        "a refused stream must not start an ffmpeg process"
    );
    let _ = stop(&session);
}

#[test]
fn autocodeword_refuses_when_stopped_and_abstains_on_a_blank_frame() {
    let harness = Harness::new();
    let session = harness.session();
    let (status, body) = send(media_app(&session), "POST", "/api/game/autocodeword", None);
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(detail(&body), "Poolrad is not running");

    let (status, _) = start(&session, &harness.codewheel, true);
    assert_eq!(status, StatusCode::OK);
    // Crop of a 640x480 window with a 16px inset: 608x448 RGB24.
    fs::write(harness.state.join("frame.rgb"), vec![0u8; 608 * 448 * 3]).unwrap();
    let (status, body) = send(media_app(&session), "POST", "/api/game/autocodeword", None);
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["confident"], json!(false));
    assert_eq!(body["typed"], json!(false));
    assert_eq!(body["glyph_count"], json!(0));
    assert!(!harness
        .read_state_file("xdotool.log")
        .contains("type --clearmodifiers"));
    let _ = stop(&session);
}

// ---- Concurrency and shutdown -----------------------------------------------------

#[test]
fn concurrent_control_calls_serialize_and_shutdown_leaves_no_processes() {
    let harness = Harness::new();
    let session = harness.session();
    let (status, _) = start(&session, &harness.codewheel, true);
    assert_eq!(status, StatusCode::OK);
    let xvfb_pid = read_pid(&harness.state.join("xvfb.pid"));

    let workers: Vec<_> = (0..4)
        .map(|worker| {
            let session = Arc::clone(&session);
            std::thread::spawn(move || {
                let runtime = runtime();
                for step in 0..5 {
                    match (worker + step) % 3 {
                        0 => {
                            let _ = runtime.block_on(GameInputBackend::key(&*session, "a"));
                        }
                        1 => {
                            let _ = GameLifecycleBackend::status(&*session);
                        }
                        _ => {
                            let _ = runtime.block_on(GameStateBackend::list_states(&*session));
                        }
                    }
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("worker thread completed");
    }

    let stopped = session.shutdown().expect("shutdown reports status");
    assert_eq!(stopped["running"], json!(false));
    assert!(!harness.state.join("tmux.pid").exists());
    assert!(!pid_alive(xvfb_pid), "no Xvfb process survives shutdown");
}

#[test]
fn recover_orphans_kills_the_tmux_server_and_unloads_only_stale_poolrad_sinks() {
    let harness = Harness::new();
    fs::write(harness.state.join("stale-sinks"), b"").unwrap();
    let session = harness.session();
    session.recover_orphans();
    let calls = harness.read_state_file("calls.log");
    assert!(calls.contains("kill-server"), "{calls}");
    assert!(calls.contains("unload 3"), "{calls}");
    assert!(!calls.contains("unload 999"), "{calls}");
}

// ---- Configuration, archive evidence and command shapes -------------------------------

#[test]
fn config_from_lookup_resolves_play_build_and_rejects_unknown_builds() {
    let rejected = SessionConfig::from_lookup(
        PathBuf::from("/r"),
        PathBuf::from("/d"),
        String::new(),
        |key: &str| (key == "VP_PLAY_BUILD").then(|| "bogus".to_owned()),
    );
    assert_eq!(
        rejected.unwrap_err(),
        "VP_PLAY_BUILD must be original or replacement"
    );

    let original = SessionConfig::from_lookup(
        PathBuf::from("/r"),
        PathBuf::from("/d"),
        String::new(),
        |key: &str| (key == "HOME").then(|| "/home/x".to_owned()),
    )
    .expect("original build");
    assert_eq!(original.dosbox_configured, "/usr/bin/dosbox-x");
    assert_eq!(
        original.archive_manifest,
        PathBuf::from("/r/resources/state-archives/manifest.json")
    );

    let replacement = SessionConfig::from_lookup(
        PathBuf::from("/r"),
        PathBuf::from("/d"),
        String::new(),
        |key: &str| match key {
            "VP_PLAY_BUILD" => Some("replacement".to_owned()),
            "HOME" => Some("/home/x".to_owned()),
            _ => None,
        },
    )
    .expect("replacement build");
    assert_eq!(
        replacement.dosbox_configured,
        "/home/x/.local/share/vp-of-agentry/dosbox-x-5a9c7c53/dosbox-x"
    );
}

#[test]
fn archive_status_verifies_manifest_entries_and_rejects_escapes() {
    let harness = Harness::new();
    let session = harness.session();
    let archive = harness.root.join("state-archives");
    fs::create_dir_all(archive.join("saves")).unwrap();
    let payload = b"archived-state";
    fs::write(archive.join("saves/a.sav"), payload).unwrap();
    let manifest = json!({
        "schema": 1,
        "created": "2026-11-05T10:00:00+00:00",
        "expected_count": 1,
        "entries": [{
            "path": "saves/a.sav",
            "size": payload.len(),
            "sha256": sha256_hex(payload),
        }],
    });
    fs::write(&harness.manifest, manifest.to_string()).unwrap();
    let (status, body) = send(
        lifecycle_app(&session, &harness.codewheel),
        "GET",
        "/api/game/status",
        None,
    );
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["state_archive"]["status"], json!("verified"));
    assert_eq!(body["state_archive"]["verified_count"], json!(1));
    assert_eq!(
        body["state_archive"]["archived_at"],
        json!("2026-11-05T10:00:00+00:00")
    );

    let escaping = json!({
        "schema": 1,
        "created": "2026-11-05T10:00:00+00:00",
        "expected_count": 1,
        "entries": [{"path": "../escape.sav", "size": 1, "sha256": sha256_hex(b"x")}],
    });
    fs::write(&harness.manifest, escaping.to_string()).unwrap();
    let (_, body) = send(
        lifecycle_app(&session, &harness.codewheel),
        "GET",
        "/api/game/status",
        None,
    );
    assert_eq!(body["state_archive"]["status"], json!("unavailable"));
}

/// Argument shapes only. This starts no stream, which stays blocked
/// (`STREAM_BLOCKER`).
#[test]
fn stream_commands_match_the_python_argv() {
    let video = game_backend::stream_command(
        Path::new("/usr/bin/ffmpeg"),
        GameStreamKind::Video,
        ":90",
        "",
    );
    assert_eq!(video.last().map(String::as_str), Some("pipe:1"));
    assert!(video
        .windows(2)
        .any(|pair| pair[0] == "-i" && pair[1] == ":90"));
    assert!(video.iter().any(|arg| arg == "mpjpeg"));

    let audio = game_backend::stream_command(
        Path::new("/usr/bin/ffmpeg"),
        GameStreamKind::Audio,
        ":90",
        "vp_poolrad_7",
    );
    assert!(audio
        .windows(2)
        .any(|pair| pair[0] == "-i" && pair[1] == "vp_poolrad_7.monitor"));
    assert!(audio.iter().any(|arg| arg == "libopus"));
    assert_eq!(audio.last().map(String::as_str), Some("pipe:1"));
}

// ---- Codeword parity with web/codeword.py ------------------------------------------------

#[test]
fn wheel_words_match_the_python_oracle() {
    use game_backend::wheel_word;
    assert_eq!(wheel_word(1, 1, 0).as_deref(), Some("BEWARE"));
    assert_eq!(wheel_word(1, 26, 0).as_deref(), Some("GKKRY"));
    assert_eq!(wheel_word(36, 36, 2).as_deref(), Some("XRSEHK"));
    assert_eq!(wheel_word(12, 25, 1).as_deref(), Some("MLSSXS"));
    assert_eq!(wheel_word(0, 1, 0), None);
    assert_eq!(wheel_word(1, 1, 3), None);
}

#[test]
fn rune_mask_normalization_matches_the_python_oracle() {
    use game_backend::{bitmap_rgb, normalize};
    let (width, height) = (40usize, 30usize);
    let mut rgb = vec![0u8; width * height * 3];
    for y in 0..height {
        for x in 0..width {
            if (5..=20).contains(&x) && (8..=22).contains(&y) {
                let index = (y * width + x) * 3;
                rgb[index..index + 3].copy_from_slice(&[200, 10, 10]);
            }
        }
    }
    let mask = bitmap_rgb(&rgb, width, height, "rune");
    let normalized = normalize(&mask);
    assert_eq!(normalized.len(), 32 * 32);
    assert_eq!(
        sha256_hex(&normalized),
        "4d22a3c92e022b3fc8228fcfb43eb599c76e6c388a9896451f8c45c767fe7d55"
    );
}

#[test]
fn ppm_parsing_reads_the_header_and_raw_pixels() {
    let mut ppm = b"P6\n# fixture\n2 1\n255\n".to_vec();
    ppm.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
    let (pixels, width, height) = game_backend::parse_ppm(&ppm).expect("valid PPM");
    assert_eq!((width, height), (2, 1));
    assert_eq!(pixels, vec![1, 2, 3, 4, 5, 6]);
    assert!(game_backend::parse_ppm(b"P6\n2 1\n255\n\x01").is_err());
}
