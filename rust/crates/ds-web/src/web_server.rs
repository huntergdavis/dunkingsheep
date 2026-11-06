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
//! Routing follows Starlette. The path is decoded before routing, so `/%61pi/health` is
//! `/api/health`. A request that a registered Python route claims for its method goes to that
//! route. Every other request goes to the static mount, because Starlette's `/` mount takes any
//! path the routes do not fully match. The mount answers GET and HEAD from the built UI directory:
//! `..` normalises inside it, a directory without a trailing slash redirects, and a missing file
//! serves the bundle's `404.html` or `{"detail": "Not Found"}`. Any other method gets a
//! `{"detail": "Method Not Allowed"}` 405 whose `Allow` is `GET, HEAD`. The legacy shell middleware
//! runs first, so a path the desk does not keep gets a 307 to `/dash/<view>`.
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
//! - Conditional GET and ranges are not implemented: no 304 for If-None-Match or If-Modified-Since,
//!   no Accept-Ranges header, and no Range requests (206). Starlette's FileResponse answers them.
//! - The 405 `Allow` is `GET, HEAD`, the value Starlette 1.10.0 sets in its StaticFiles method check,
//!   the version in the release venv. `bridges/requirements.txt` pins ranges only, and the reviewer's
//!   note that Starlette 0.46.x sent no Allow is not verified in this tree.
//! - The P1 game routes are not in the route table. Python answers them, but here a POST gets the
//!   mount's 405 (Allow `GET, HEAD`) and a GET gets a 404. The 405 wrongly implies the path takes only
//!   GET and HEAD. This is documented rather than fixed.
//! - `POST /api/ops-board/question` is registered by `routes::ops_board`, but web/app.py has no such
//!   route, and the gate sends it to the mount. It is unreachable through the composition. Remove it
//!   or document it in the producer module.
//! - `PYTHON_ROUTES` is a hand-maintained copy of the P0 routes in web/app.py. No test checks it
//!   against the mounted axum routes yet.
//! - `GET /api/poolrad-dashboard/progress` is a P0 route (web/app.py:1953) mounted through
//!   `routes::activity_poolrad`. The poolrad dashboard page, its reports, and its refresh are P1 and
//!   not mounted here.

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
            .fallback(move |request: Request| {
                let root = ui_dist.clone();
                async move { serve_static(&root, request).await }
            })
            .layer(middleware::from_fn(retire_the_desk_ui))
    }
}

/// Rewrites the request path to its decoded form before routing, as uvicorn decodes the path before
/// Starlette routes it. `Router::layer` wraps matched endpoints, so the rewrite has to sit in front
/// of the router itself: `/%61pi/health` must match the `/api/health` route.
#[derive(Clone)]
struct DecodedPathRouting<S> {
    inner: S,
}

impl<S, B> tower::Service<Request<B>> for DecodedPathRouting<S>
where
    S: tower::Service<Request<B>>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = S::Future;

    fn poll_ready(
        &mut self,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, mut request: Request<B>) -> Self::Future {
        let decoded = percent_decode(request.uri().path());
        if is_python_route(&decoded, request.method()) {
            if let Some(routed) = routed_uri(request.uri(), &decoded) {
                *request.uri_mut() = routed;
            }
        } else {
            let query = request.uri().query().map(str::to_owned);
            request.extensions_mut().insert(ForcedStatic {
                path: decoded,
                query,
            });
            *request.uri_mut() = Uri::from_static(FORCED_STATIC_PATH);
        }
        self.inner.call(request)
    }
}

/// A request no Python route claims. It goes to the static mount with its decoded path and query,
/// because Starlette's `/` mount answers every path the routes do not fully match.
#[derive(Clone)]
struct ForcedStatic {
    path: String,
    query: Option<String>,
}

/// A path no route matches, so the router's fallback answers it with the static mount.
const FORCED_STATIC_PATH: &str = "/__desk_static__";

/// The routes Python registers in `web/app.py`, as (method, path pattern). A `{name}` segment
/// matches one non-empty segment, and `{path:path}` matches the rest of the path. A method that a
/// path does not list is not a route for that request.
const PYTHON_ROUTES: &[(&str, &str)] = &[
    ("POST", "/api/login"),
    ("POST", "/api/logout"),
    ("POST", "/api/password"),
    ("GET", "/api/panes/activity"),
    ("GET", "/api/activity"),
    ("GET", "/api/poolrad-dashboard/progress"),
    ("GET", "/dash"),
    ("GET", "/dash/"),
    ("GET", "/dash/{path:path}"),
    ("POST", "/api/pane-send"),
    ("GET", "/api/fleet"),
    ("GET", "/api/ops-metrics"),
    ("GET", "/api/messages"),
    ("GET", "/api/deliveries"),
    ("GET", "/api/messages/{message_id}/context"),
    ("POST", "/api/messages"),
    ("POST", "/api/messages/{message_id}/send-now"),
    ("POST", "/api/brief"),
    ("POST", "/api/ops-board/not-now"),
    ("POST", "/api/ops-board/answer"),
    ("POST", "/api/ops-board/refresh"),
    ("GET", "/api/ops-board"),
    ("GET", "/api/ops-metrics/history"),
    ("POST", "/api/ops-metrics/refresh"),
    ("GET", "/api/capability"),
    ("POST", "/api/owner-send"),
    ("GET", "/api/owner-thread"),
    ("POST", "/api/viewport"),
    ("GET", "/api/channels"),
    ("GET", "/api/pane-text"),
    ("GET", "/api/search"),
    ("POST", "/api/uploads"),
    ("GET", "/api/uploads/{attachment_id}"),
    ("GET", "/api/herdr-webui"),
    ("GET", "/api/health"),
    ("GET", "/api/version"),
];

/// Whether a Python route claims this decoded path for this method.
fn is_python_route(decoded: &str, method: &Method) -> bool {
    PYTHON_ROUTES.iter().any(|(route_method, pattern)| {
        *route_method == method.as_str() && pattern_matches(pattern, decoded)
    })
}

/// Matches one route pattern against a decoded path, as Starlette's path converters do.
fn pattern_matches(pattern: &str, path: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix("{path:path}") {
        return path.starts_with(prefix);
    }
    let mut wanted = pattern.split('/');
    let mut actual = path.split('/');
    loop {
        match (wanted.next(), actual.next()) {
            (None, None) => return true,
            (Some(segment), Some(value)) => {
                let matches = if segment.starts_with('{') && segment.ends_with('}') {
                    !value.is_empty()
                } else {
                    segment == value
                };
                if !matches {
                    return false;
                }
            }
            _ => return false,
        }
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
        axum::ServiceExt::<Request>::into_make_service_with_connect_info::<SocketAddr>(
            DecodedPathRouting { inner: app },
        ),
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
                    "VP_CHANNELS_FILE does not exist: {path}. The channel registry is the \
                     owner's channels.json, which the units point at inside the desk data \
                     dir; copy it there (cp <checkout>/channels.json {path}) or unset \
                     VP_CHANNELS_FILE to fall back to bridges/channels.example.json.",
                    path = path.display()
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
                "{shown}: invalid channel id {}",
                py_repr(id)
            )));
        }
        let kind = map.get("kind").and_then(Value::as_str).unwrap_or_default();
        if !matches!(kind, "active" | "standby" | "monitor") {
            return Err(DeskError::Config(format!(
                "{shown}: invalid kind for channel {}",
                py_repr(id)
            )));
        }
        if !ids.insert(id.to_owned()) {
            return Err(DeskError::Config(format!(
                "{shown}: duplicate channel id {}",
                py_repr(id)
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

/// Python's `repr()` of a `str`, which the channel loader's messages use to quote ids. The quote is
/// `'` unless the text holds a `'` and no `"`. Backslashes, the quote, and control or non-printable
/// characters are escaped the way CPython escapes them.
fn py_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for character in text.chars() {
        let code = u32::from(character);
        match character {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            _ if code < 0x20 || code == 0x7f => out.push_str(&format!("\\x{code:02x}")),
            _ if !character.is_ascii() && (character.is_control() || character.is_whitespace()) => {
                if code <= 0xff {
                    out.push_str(&format!("\\x{code:02x}"));
                } else if code <= 0xffff {
                    out.push_str(&format!("\\u{code:04x}"));
                } else {
                    out.push_str(&format!("\\U{code:08x}"));
                }
            }
            _ => out.push(character),
        }
    }
    out.push(quote);
    out
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

/// Python `retire_the_desk_ui`: runs before routing. The path is decoded first, as uvicorn decodes
/// it before Starlette routes, so `/%61pi/health` reaches `/api/health`. Paths the desk keeps pass
/// through, and every other path gets a 307 to the dashboard shell. The query string is dropped.
async fn retire_the_desk_ui(request: Request, next: Next) -> Response {
    let path = match request.extensions().get::<ForcedStatic>() {
        Some(forced) => forced.path.clone(),
        None => percent_decode(request.uri().path()),
    };
    if path == "/" || desk_keeps(&path) {
        return next.run(request).await;
    }
    Redirect::temporary(&format!("/dash/{}", legacy_view(&path))).into_response()
}

/// The request URI with its path re-encoded from the decoded path, so the router matches the same
/// text Python's router sees. The query string is kept as sent.
fn routed_uri(uri: &Uri, decoded: &str) -> Option<Uri> {
    let query = uri
        .query()
        .map(|query| format!("?{query}"))
        .unwrap_or_default();
    format!("{}{}", encode_path(decoded), query)
        .parse::<Uri>()
        .ok()
}

/// Percent-encodes everything except the characters that need no escape in a path. `/` is kept,
/// so the segments of the decoded path stay the segments the router sees.
fn encode_path(decoded: &str) -> String {
    let mut out = String::with_capacity(decoded.len());
    for byte in decoded.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/!$&'()*+,;=:@".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Starlette's `StaticFiles(html=True)` at `/`, as the app mounts it. GET and HEAD only. Any other
/// method gets Starlette's 405 with `Allow: GET, HEAD`, rendered as FastAPI renders an
/// HTTPException. A directory serves its `index.html`, and a directory URL without a trailing
/// slash redirects to one. `..` segments normalise first, and a path that climbs above the bundle
/// is a 404. A missing file serves the bundle's `404.html` when there is one.
async fn serve_static(root: &Path, request: Request) -> Response {
    let method = request.method().clone();
    if method != Method::GET && method != Method::HEAD {
        return method_not_allowed();
    }
    let (decoded, query) = match request.extensions().get::<ForcedStatic>() {
        Some(forced) => (forced.path.clone(), forced.query.clone()),
        None => (
            percent_decode(request.uri().path()),
            request.uri().query().map(str::to_owned),
        ),
    };
    let headers = request.headers().clone();
    let root_owned = root.to_path_buf();
    let head = method == Method::HEAD;
    let lookup_path = decoded.clone();
    let looked = tokio::task::spawn_blocking(move || lookup(&root_owned, &lookup_path, head)).await;
    match looked {
        Ok(Lookup::File(file)) => file_response(file, StatusCode::OK),
        Ok(Lookup::DirectoryWithoutSlash) => {
            let location = absolute_location(&headers, &decoded, query.as_deref());
            Redirect::temporary(&location).into_response()
        }
        Ok(Lookup::NotFoundPage(Some(page))) => file_response(page, StatusCode::NOT_FOUND),
        Ok(Lookup::NotFoundPage(None)) => not_found(),
        Ok(Lookup::Denied) => unauthorized(),
        Err(_) => not_found(),
    }
}

/// What a static lookup resolved to, before any HTTP shape is applied.
enum Lookup {
    File(StaticFile),
    DirectoryWithoutSlash,
    NotFoundPage(Option<StaticFile>),
    Denied,
}

/// A file read for the response: its body (empty for HEAD), its length, and its media type.
struct StaticFile {
    body: Vec<u8>,
    length: u64,
    media: &'static str,
}

/// Starlette's `lookup_path` for one request path under `root`, on the blocking pool.
fn lookup(root: &Path, decoded: &str, head: bool) -> Lookup {
    let route = decoded.trim_start_matches('/');
    if route.contains('\0') {
        return Lookup::NotFoundPage(None);
    }
    let Some(segments) = normalized_segments(route) else {
        return not_found_page(root, head);
    };
    let mut target = root.to_path_buf();
    for segment in &segments {
        target.push(segment);
    }
    if !within(root, &target) {
        return not_found_page(root, head);
    }
    let metadata = match fs::metadata(&target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            return Lookup::Denied;
        }
        Err(_) => return not_found_page(root, head),
    };
    if metadata.is_file() {
        return read_static_file(&target, head);
    }
    if metadata.is_dir() {
        let index = target.join("index.html");
        if fs::metadata(&index).is_ok_and(|found| found.is_file()) {
            if !decoded.ends_with('/') {
                return Lookup::DirectoryWithoutSlash;
            }
            return read_static_file(&index, head);
        }
    }
    not_found_page(root, head)
}

fn read_static_file(path: &Path, head: bool) -> Lookup {
    match read_file(path, head) {
        Ok(file) => Lookup::File(file),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Lookup::Denied,
        Err(_) => Lookup::NotFoundPage(None),
    }
}

/// The bundle's `404.html`, when it exists, as Starlette's HTML mode serves it with a 404.
fn not_found_page(root: &Path, head: bool) -> Lookup {
    let page = root.join("404.html");
    if !fs::metadata(&page).is_ok_and(|found| found.is_file()) {
        return Lookup::NotFoundPage(None);
    }
    Lookup::NotFoundPage(read_file(&page, head).ok())
}

fn read_file(path: &Path, head: bool) -> std::io::Result<StaticFile> {
    let length = fs::metadata(path)?.len();
    let body = if head { Vec::new() } else { fs::read(path)? };
    Ok(StaticFile {
        body,
        length,
        media: media_type(path),
    })
}

fn file_response(file: StaticFile, status: StatusCode) -> Response {
    let mut response = Response::new(file.body.into());
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(file.media));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(file.length));
    response
}

/// Starlette's `os.path.normpath` over the route's `/` segments. `None` when `..` climbs above the
/// bundle, which Starlette answers with a 404.
fn normalized_segments(route: &str) -> Option<Vec<&str>> {
    let mut stack: Vec<&str> = Vec::new();
    for segment in route.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                stack.pop()?;
            }
            other => stack.push(other),
        }
    }
    Some(stack)
}

/// Starlette's containment check: a target whose real path leaves the bundle is not served. A
/// target that does not exist is contained, because normalisation already removed any climb.
fn within(root: &Path, target: &Path) -> bool {
    match (fs::canonicalize(root), fs::canonicalize(target)) {
        (Ok(real_root), Ok(real_target)) => real_target.starts_with(real_root),
        _ => true,
    }
}

/// The absolute URL Starlette's directory redirect uses: `scheme://host` from the request, the
/// decoded path with a trailing slash, and the query string as sent.
fn absolute_location(headers: &axum::http::HeaderMap, path: &str, query: Option<&str>) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost");
    let mut location = format!("http://{host}{}/", quote_location(path));
    if let Some(query) = query {
        location.push('?');
        location.push_str(&quote_location(query));
    }
    location
}

/// Percent-encodes a redirect target the way Starlette's `RedirectResponse` quotes it: the
/// unreserved characters and Starlette's safe set stay, and every other byte, including non-ASCII,
/// becomes `%XX`. The result is always ASCII, so the `Location` header is always valid.
fn quote_location(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"_.-~:/%#?=@[]!$&'()*+,;".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// Starlette's `StaticFiles` 405 as FastAPI renders it: a JSON detail and the `Allow` header that
/// Starlette sets for the method check.
fn method_not_allowed() -> Response {
    let mut response = (
        StatusCode::METHOD_NOT_ALLOWED,
        Json(json!({"detail": "Method Not Allowed"})),
    )
        .into_response();
    response
        .headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static("GET, HEAD"));
    response
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"detail": "Unauthorized"})),
    )
        .into_response()
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
