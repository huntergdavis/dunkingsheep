//! Production composition of the desk's Rust route services (t3216).
//!
//! This module is the one place that wires the P0 route modules to their real backends. It takes
//! every input explicitly: nothing here reads the environment or the home directory, so the
//! executable step captures the configuration and tests pass temporary paths. The startup order
//! follows `web/app.py`: the owner auth store first, then the daemon-backed panes store (which
//! needs the auth store's session columns), the messages store, the owner/fleet adapter (which
//! needs the messages database), the activity and dashboard readers, and last the operations
//! board runtime. Startup warms the provider usage cache and starts the board timer. Shutdown
//! stops the timer and joins the usage workers.
//!
//! Routing order matches `web/app.py`. The legacy redirect middleware runs before routing, so it
//! sees every path. A path the desk keeps (`/`, `/api...`, `/dash...`, `/assets...`) falls through
//! to the routes. Every other path gets a 307 to `/dash/<view>`. Unmatched kept paths (the bundle
//! at `/` and `/assets/*`) are served from the built UI directory, and unknown `/api/*` paths
//! answer 404 with `{"detail": "Not Found"}`, as Starlette's static mount does.
//!
//! Known gaps, reported rather than widened here:
//! - The Python `vp-delivery-watch` loop (reconcile pending owner messages every 5 seconds) has no
//!   public Rust entry point. `MessagesBackend::reconcile` is private and runs only on message
//!   paths. The composition does not start a watcher, so this is a parity exception until a public
//!   reconcile hook exists.
//! - The operations database must be opened with the schema initialised. `ops_history::connect`
//!   is private, so the caller supplies the opener through `DeskConfig::open_operations_db`.
//! - Static responses carry Content-Type and Content-Length. Starlette's ETag and Last-Modified
//!   headers are not replicated, and neither is the exact MIME table of Python's `mimetypes`.
//! - Game routes and the poolrad dashboard refresh are P1 and not mounted here.

use std::{
    collections::{HashMap, HashSet},
    fmt, fs,
    future::Future,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use axum::{
    extract::{Extension, Request},
    http::{header, HeaderValue, Method, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    Json, Router,
};
use ds_core::client::{FlockClient, UnixTransport};
use serde_json::{json, Value};
use tokio::net::TcpListener;

use crate::{
    auth_backend::PersistentAuth,
    dashboard_activity_backend::{
        ActivityPoolradBackend as DashboardActivity, DashBackend as DashboardShell, DashboardPaths,
        SystemClock as DashboardClock,
    },
    messages_backend::{
        ChannelConfig, MessagesBackend, MessagesBackendConfig, UnixDaemonConnector,
    },
    ops_board_runtime::{
        owner_board_backend, BoardRuntime, BoardRuntimeConfig, CoreUsageReader, Dispatch, OpenDb,
        SharedDaemon, SystemClock as BoardClock,
    },
    ops_collector::{CollectorConfig, GhCli, COMMAND_TIMEOUT},
    owner_fleet_backend::{OwnerFleetBackend, OwnerFleetConfig},
    panes_backend::DaemonPanesBackend,
    routes::{
        activity_poolrad::{self, ActivityPoolradBackend as ActivityRoutes},
        auth::{AuthBackend, RequestScheme},
        dash::{self, DashBackend as DashRoutes},
        fleet::{self, FleetSources, FleetState},
        messages_read, messages_write,
        ops_board::{self, OpsBoardBackend},
        ops_metrics::{self, OpsMetricsBackend},
        owner::{self, OwnerWebBackend, OwnerWebState},
        panes::{self, PanesWebBackend, PanesWebState},
        search_attachments,
        system::{self, SystemConfig},
    },
};

/// Every input the desk needs. The executable fills it from the captured environment; tests pass
/// temporary paths. Nothing is read from the environment here.
#[derive(Clone)]
pub struct DeskConfig {
    /// Desk data directory (`VP_DATA_DIR`): `messages.sqlite3`, `auth.json`, `uploads/`.
    pub data_dir: PathBuf,
    /// Config root (`DUNKINGSHEEP_DIR`): the owner's `lanes.json` lives here.
    pub config_dir: PathBuf,
    /// Home directory: the fleet session roots (`.codex`, `.claude`, `.pi`) are under it.
    pub home: PathBuf,
    /// Daemon socket (`DUNKINGSHEEP_SOCKET`).
    pub daemon_socket: PathBuf,
    /// Service token (`DUNKINGSHEEP_SERVICE_TOKEN` or its file). Every daemon write needs it,
    /// because the pane-less web desk has no pane identity of its own.
    pub service_token: Option<String>,
    /// Daemon call timeout (`DUNKINGSHEEP_CLIENT_TIMEOUT`).
    pub client_timeout: Duration,
    /// The validated channel registry, as returned by [`load_channel_registry`].
    pub channels: Vec<Value>,
    /// The built desk bundle (`ROOT/ui/dist`).
    pub ui_dist: PathBuf,
    /// Poolrad dashboard bridge directory (`POOLRAD_DASHBOARD_DIR`).
    pub dashboard_dir: PathBuf,
    /// Checkout the dashboard progress view reads (`POOLRAD_DASHBOARD_REPO`, else `POOLRAD_REPO`).
    pub dashboard_repo: PathBuf,
    /// Checkout the metrics collector reads through `gh` (`POOLRAD_REPO`).
    pub poolrad_repo: PathBuf,
    /// Operations database (`VP_OPS_DB`).
    pub operations_db: PathBuf,
    /// Host watch state (`VP_HOST_WATCH_STATE`).
    pub host_watch_state: PathBuf,
    /// OpenCode usage database (`OPENCODE_DB`).
    pub opencode_db: PathBuf,
    /// Dunking Sheep metrics database (`DUNK_EVENTS_DB`).
    pub dunk_events_db: PathBuf,
    /// Provider usage configuration (`~/.config/dunkingsheep/usage.json`).
    pub usage_config: PathBuf,
    /// Provider reading cadence (`VP_USAGE_REFRESH_S`).
    pub usage_refresh_s: i64,
    /// Provider failure backoff sequence (`VP_USAGE_BACKOFF_S`).
    pub usage_backoff_s: Vec<i64>,
    /// Maximum provider reading age before it is `stale` (`2 * VP_USAGE_REFRESH_S`).
    pub usage_max_age_s: i64,
    /// Operations board refresh cadence in seconds (`VP_OPS_BOARD_INTERVAL_S`).
    pub board_interval_s: u64,
    /// The `gh` executable used by the metrics collector.
    pub gh_program: PathBuf,
    /// The `tmux` executable used by the fleet lane scan.
    pub tmux_program: PathBuf,
    /// The process table root used by the fleet scan (`/proc`).
    pub proc_root: PathBuf,
    /// Herdr web UI URL (`VP_HERDR_WEBUI_URL`); absent means unavailable.
    pub herdr_webui_url: Option<String>,
    /// Herdr web UI label (`VP_HERDR_WEBUI_LABEL`).
    pub herdr_webui_label: Option<String>,
    /// Initial owner password (`VP_PASSWORD`), used only when `auth.json` does not exist yet.
    pub initial_password: Option<String>,
    /// The externally visible request scheme, for same-origin checks.
    pub scheme: RequestScheme,
    /// Opens the operations database with the schema initialised.
    pub open_operations_db: OpenDb,
}

/// Failure to validate the configuration or to start or serve the desk.
#[derive(Debug)]
pub enum DeskError {
    /// The channel registry or another input is invalid, with Python's message text.
    Config(String),
    /// A backend could not be opened at startup, named by its component.
    Startup(String),
    /// The listener stopped with an I/O error.
    Serve(String),
}

impl fmt::Display for DeskError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DeskError::Config(text) | DeskError::Startup(text) | DeskError::Serve(text) => {
                formatter.write_str(text)
            }
        }
    }
}

/// The desk's owned backends. [`Desk::open`] performs the startup order; [`Desk::router`] mounts
/// every P0 route against them; [`Desk::start`] and [`Desk::stop`] bracket the serving period.
pub struct Desk {
    auth: Arc<PersistentAuth>,
    panes: Arc<DaemonPanesBackend<UnixTransport>>,
    messages: MessagesBackend,
    fleet: Arc<OwnerFleetBackend>,
    activity: Arc<DashboardActivity<UnixTransport>>,
    dash: Arc<DashboardShell>,
    board: BoardRuntime,
    system: SystemConfig,
    ui_dist: PathBuf,
    scheme: RequestScheme,
}

impl Desk {
    /// Opens every backend in the startup order of `web/app.py`. No daemon call is made here, so
    /// a daemon that is absent at startup surfaces later as a 503 on the routes that need it.
    pub fn open(config: DeskConfig) -> Result<Self, DeskError> {
        let auth = PersistentAuth::open_with_password(
            &config.data_dir,
            config.initial_password.as_deref(),
        )
        .map_err(|error| DeskError::Startup(format!("auth: {error:?}")))?;
        let auth = Arc::new(auth);
        let token = config.service_token.clone();
        let panes = DaemonPanesBackend::open(
            daemon_client(&config, "web-desk-panes", token.clone()),
            &config.data_dir,
            config.channels.clone(),
        )
        .map_err(|error| DeskError::Startup(format!("panes: {}", error.0)))?;
        let panes = Arc::new(panes);
        let messages = MessagesBackend::open(MessagesBackendConfig {
            data_dir: config.data_dir.clone(),
            channels: channel_configs(&config.channels),
            daemon: Arc::new(UnixDaemonConnector::new(
                config.daemon_socket.clone(),
                token.clone(),
                config.client_timeout,
                "web-desk-messages",
            )),
        })
        .map_err(|error| DeskError::Startup(format!("messages: {error}")))?;
        let fleet = OwnerFleetBackend::open(panes.clone(), fleet_config(&config))
            .map_err(|error| DeskError::Startup(format!("owner/fleet: {error:?}")))?;
        let fleet = Arc::new(fleet);
        let readers = (0..4)
            .map(|index| {
                daemon_client(
                    &config,
                    &format!("web-desk-pane-reader-{index}"),
                    token.clone(),
                )
            })
            .collect();
        let activity = Arc::new(DashboardActivity::new(
            daemon_client(&config, "web-desk-activity", token.clone()),
            readers,
            config.channels.clone(),
            DashboardPaths {
                ui_dist: config.ui_dist.clone(),
                dashboard_dir: config.dashboard_dir.clone(),
                dashboard_repo: config.dashboard_repo.clone(),
                operations_db: config.operations_db.clone(),
            },
            Arc::new(DashboardClock::new()),
        ));
        let dash = Arc::new(DashboardShell::new(config.ui_dist.clone()));
        let board = BoardRuntime::new(
            BoardRuntimeConfig {
                interval_s: config.board_interval_s,
                ops_db: config.operations_db.clone(),
                host_watch_state: config.host_watch_state.clone(),
                opencode_db: config.opencode_db.clone(),
                collector: CollectorConfig {
                    message_db: config.data_dir.join("messages.sqlite3"),
                    dunk_events_db: config.dunk_events_db.clone(),
                },
                github: GhCli {
                    program: config.gh_program.clone(),
                    repo_dir: config.poolrad_repo.clone(),
                    timeout: COMMAND_TIMEOUT,
                },
                usage_refresh_s: config.usage_refresh_s,
                usage_backoff_s: config.usage_backoff_s.clone(),
                usage_max_age_s: config.usage_max_age_s,
                timer_period: Duration::from_secs(config.board_interval_s),
            },
            SharedDaemon::new(daemon_client(&config, "web-desk-board", token.clone())),
            config.open_operations_db,
            Arc::new(BoardClock),
            Arc::new(CoreUsageReader {
                config_path: config.usage_config.clone(),
                max_age_s: config.usage_max_age_s,
            }),
            Dispatch::Threads,
        );
        let system = SystemConfig::new(
            config.herdr_webui_url.clone(),
            config.herdr_webui_label.clone(),
            config.ui_dist.clone(),
        );
        Ok(Self {
            auth,
            panes,
            messages,
            fleet,
            activity,
            dash,
            board,
            system,
            ui_dist: config.ui_dist,
            scheme: config.scheme,
        })
    }

    /// Starts the background work that Python starts at lifespan startup: the provider usage
    /// warm-up, then the board timer.
    pub fn start(&self) -> std::io::Result<()> {
        self.board.start()
    }

    /// Stops the board timer and joins the usage workers, as Python's shutdown does.
    pub fn stop(&self) {
        self.board.stop();
    }

    /// Mounts every P0 route against the owned backends, then the legacy redirect middleware
    /// (outermost, so it sees every path) and the static fallback.
    pub fn router(&self) -> Router {
        let auth: Arc<dyn AuthBackend> = self.auth.clone();
        let scheme = self.scheme;
        let (messages_read_backend, messages_write_backend, search_backend) =
            self.messages.trait_objects();
        let panes_state = Arc::new(PanesWebState::new(
            self.panes.clone() as Arc<dyn PanesWebBackend>
        ));
        let owner_state = Arc::new(OwnerWebState::new(
            self.fleet.clone() as Arc<dyn OwnerWebBackend>
        ));
        let fleet_state = Arc::new(FleetState::new(
            self.panes.clone() as Arc<dyn PanesWebBackend>,
            self.fleet.clone() as Arc<dyn FleetSources>,
        ));
        let metrics: Arc<dyn OpsMetricsBackend> = Arc::new(self.board.clone());
        let board: Arc<dyn OpsBoardBackend> = Arc::new(owner_board_backend(&self.board));
        let activity: Arc<dyn ActivityRoutes> = self.activity.clone();
        let dash: Arc<dyn DashRoutes> = self.dash.clone();
        let ui_dist = self.ui_dist.clone();

        Router::new()
            .merge(system::router(self.system.clone(), auth.clone(), scheme))
            .merge(crate::routes::auth::router_with_scheme(scheme).layer(Extension(auth.clone())))
            .merge(
                panes::router()
                    .layer(Extension(panes_state))
                    .layer(Extension(auth.clone()))
                    .layer(Extension(scheme)),
            )
            .merge(
                owner::router_with_scheme(scheme)
                    .layer(Extension(owner_state))
                    .layer(Extension(auth.clone())),
            )
            .merge(fleet::router(fleet_state, auth.clone(), scheme))
            .merge(
                messages_read::router_with_scheme(scheme)
                    .layer(Extension(messages_read_backend))
                    .layer(Extension(auth.clone())),
            )
            .merge(messages_write::router(
                messages_write_backend,
                auth.clone(),
                scheme,
            ))
            .merge(search_attachments::router(
                search_backend,
                auth.clone(),
                scheme,
            ))
            .merge(activity_poolrad::router(activity, auth.clone(), scheme))
            .merge(
                ops_metrics::router_with_scheme(scheme)
                    .layer(Extension(metrics))
                    .layer(Extension(auth.clone())),
            )
            .merge(ops_board::router(board, auth.clone(), scheme))
            .merge(dash::router(dash, auth, scheme))
            .fallback(move |method: Method, uri: Uri| {
                let root = ui_dist.clone();
                async move { serve_static(&root, &method, &uri).await }
            })
            .layer(middleware::from_fn(retire_the_desk_ui))
    }
}

/// Serves the desk until `shutdown` resolves, then stops the background work. The startup order
/// matches Python's lifespan: open and warm everything, then accept requests.
pub async fn serve(
    config: DeskConfig,
    listener: TcpListener,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<(), DeskError> {
    let desk = Desk::open(config)?;
    desk.start()
        .map_err(|error| DeskError::Startup(format!("board: {error}")))?;
    let app = desk.router();
    let served = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await;
    desk.stop();
    served.map_err(|error| DeskError::Serve(error.to_string()))
}

/// The channel registry loader of `web/app.py` (`_load_channels`), with the same checks and
/// messages. `override_path` is `VP_CHANNELS_FILE`. Without it the owner's `channels.json` under
/// `root` is used, and the checked-in example is the fallback.
pub fn load_channel_registry(
    override_path: Option<&Path>,
    root: &Path,
) -> Result<Vec<Value>, DeskError> {
    let path = match override_path {
        Some(path) => {
            if !path.exists() {
                return Err(DeskError::Config(format!(
                    "VP_CHANNELS_FILE does not exist: {}. The channel registry is the owner's \
                     channels.json, which the units point at inside the desk data dir; copy it \
                     there or unset VP_CHANNELS_FILE to fall back to bridges/channels.example.json.",
                    path.display()
                )));
            }
            path.to_path_buf()
        }
        None => {
            let default = root.join("channels.json");
            if default.exists() {
                default
            } else {
                root.join("bridges").join("channels.example.json")
            }
        }
    };
    let shown = path.display().to_string();
    let text = fs::read_to_string(&path)
        .map_err(|error| DeskError::Config(format!("{shown}: {error}")))?;
    let payload: Value = serde_json::from_str(&text)
        .map_err(|error| DeskError::Config(format!("{shown}: {error}")))?;
    let items = match payload {
        Value::Array(items) if !items.is_empty() => items,
        _ => {
            return Err(DeskError::Config(format!(
                "{shown}: expected a nonempty list of channels"
            )))
        }
    };
    let mut ids: HashSet<String> = HashSet::new();
    for item in &items {
        let Some(map) = item.as_object() else {
            return Err(channel_shape_error(&shown));
        };
        for key in ["id", "name", "match", "kind"] {
            let valid = map
                .get(key)
                .and_then(Value::as_str)
                .is_some_and(|text| !text.trim().is_empty());
            if !valid {
                return Err(channel_shape_error(&shown));
            }
        }
        let id = map.get("id").and_then(Value::as_str).unwrap_or_default();
        if !valid_channel_id(id) {
            return Err(DeskError::Config(format!(
                "{shown}: invalid channel id {id:?}"
            )));
        }
        let kind = map.get("kind").and_then(Value::as_str).unwrap_or_default();
        if !matches!(kind, "active" | "standby" | "monitor") {
            return Err(DeskError::Config(format!(
                "{shown}: invalid kind for channel {id:?}"
            )));
        }
        if !ids.insert(id.to_owned()) {
            return Err(DeskError::Config(format!(
                "{shown}: duplicate channel id {id:?}"
            )));
        }
        if let Some(dashboard) = map.get("dashboard") {
            if !dashboard.is_boolean() {
                return Err(DeskError::Config(format!(
                    "{shown}: dashboard must be true or false"
                )));
            }
        }
    }
    Ok(items)
}

fn channel_shape_error(shown: &str) -> DeskError {
    DeskError::Config(format!(
        "{shown}: each channel needs nonempty id, name, match, and kind"
    ))
}

/// Python's `re.fullmatch(r"[a-z][a-z0-9_-]*", id)`.
fn valid_channel_id(id: &str) -> bool {
    let mut characters = id.chars();
    matches!(characters.next(), Some(first) if first.is_ascii_lowercase())
        && characters.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn daemon_client(
    config: &DeskConfig,
    identity: &str,
    token: Option<String>,
) -> FlockClient<UnixTransport> {
    FlockClient::new(
        UnixTransport {
            path: config.daemon_socket.clone(),
        },
        None,
        identity.to_owned(),
        config.client_timeout,
        token,
    )
}

fn channel_configs(channels: &[Value]) -> Vec<ChannelConfig> {
    channels
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?.to_owned();
            Some(ChannelConfig {
                id,
                config: entry.clone(),
            })
        })
        .collect()
}

fn fleet_config(config: &DeskConfig) -> OwnerFleetConfig {
    let channels: HashMap<String, Value> = config
        .channels
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?.to_owned();
            Some((id, entry.clone()))
        })
        .collect();
    OwnerFleetConfig::new(
        config.data_dir.join("messages.sqlite3"),
        config.operations_db.clone(),
    )
    .with_summary_sources(config.host_watch_state.clone(), config.opencode_db.clone())
    .with_local_sources(
        config.config_dir.join("lanes.json"),
        vec![
            config.home.join(".codex/sessions"),
            config.home.join(".claude/projects"),
            config.home.join(".pi/agent/sessions"),
        ],
        config.proc_root.clone(),
        config.home.clone(),
        config.tmux_program.clone(),
        channels,
    )
}

/// Paths the desk keeps itself (Python `_desk_keeps`): the bare prefixes, and anything below
/// `/api/`, `/dash/` or `/assets/`.
fn desk_keeps(path: &str) -> bool {
    matches!(path, "/api" | "/dash" | "/assets")
        || ["/api/", "/dash/", "/assets/"]
            .iter()
            .any(|prefix| path.starts_with(prefix))
}

/// The dashboard view a retired bookmark lands on (Python `LEGACY_DASH_ALIASES`). Everything not
/// listed, including `/cos`, `/desk` and `/vp`, lands on the chief-of-staff conversation.
fn legacy_view(path: &str) -> &'static str {
    match path {
        "/poolrad" => "poolrad",
        "/operations" => "operations",
        "/play" => "play",
        "/fleet" => "fleet",
        _ => "cos",
    }
}

/// Python `retire_the_desk_ui`: runs before routing. Paths the desk keeps pass through, and every
/// other path gets a 307 to the dashboard shell. The query string is dropped.
async fn retire_the_desk_ui(request: Request, next: Next) -> Response {
    let path = percent_decode(request.uri().path());
    if path == "/" || desk_keeps(&path) {
        return next.run(request).await;
    }
    Redirect::temporary(&format!("/dash/{}", legacy_view(&path))).into_response()
}

/// Starlette's `StaticFiles(html=True)` at `/`: GET and HEAD only, directories serve their index,
/// `..` is refused, and a missing file is `404 {"detail": "Not Found"}`.
async fn serve_static(root: &Path, method: &Method, uri: &Uri) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            [(header::ALLOW, HeaderValue::from_static("GET, HEAD"))],
            "Method Not Allowed",
        )
            .into_response();
    }
    let Some(target) = static_target(root, &percent_decode(uri.path())) else {
        return not_found();
    };
    let head = method == Method::HEAD;
    let served = tokio::task::spawn_blocking(move || read_static(&target, head)).await;
    match served {
        Ok(Some((bytes, length, media))) => {
            let mut response = Response::new(bytes.into());
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static(media));
            response
                .headers_mut()
                .insert(header::CONTENT_LENGTH, HeaderValue::from(length));
            response
        }
        _ => not_found(),
    }
}

/// Resolves a decoded request path under `root`. Empty and `.` segments are ignored, and any
/// `..` or backslash is refused.
fn static_target(root: &Path, decoded: &str) -> Option<PathBuf> {
    let mut target = root.to_path_buf();
    for segment in decoded.split('/') {
        match segment {
            "" | "." => {}
            ".." => return None,
            other if other.contains(['\\', '\0']) => return None,
            other => target.push(other),
        }
    }
    Some(target)
}

/// Reads a static file (blocking, so it runs on the blocking pool). A directory serves its
/// `index.html`. Returns the body, its length and its media type.
fn read_static(target: &Path, head: bool) -> Option<(Vec<u8>, u64, &'static str)> {
    let metadata = fs::metadata(target).ok()?;
    let (file, metadata) = if metadata.is_dir() {
        let index = target.join("index.html");
        let metadata = fs::metadata(&index).ok()?;
        if !metadata.is_file() {
            return None;
        }
        (index, metadata)
    } else if metadata.is_file() {
        (target.to_path_buf(), metadata)
    } else {
        return None;
    };
    let body = if head {
        Vec::new()
    } else {
        fs::read(&file).ok()?
    };
    Some((body, metadata.len(), media_type(&file)))
}

fn media_type(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/vnd.microsoft.icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({"detail": "Not Found"}))).into_response()
}

/// Decodes `%XX` escapes the way the router sees a path. A malformed escape stays literal.
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                out.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
