//! Default-off owner-authenticated terminal stream endpoint.
use crate::{
    routes::auth::{AuthBackend, Identity, RequestScheme},
    terminal_control_registry::{ControlIdentity, ControlLease, ControlRegistry, ResizeSlots},
    terminal_stream::{self, ControlLines, ControlReply},
};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        DefaultBodyLimit, Path, State,
    },
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use ds_core::{config::Env, roles::Roles, terminal::tmux::TmuxBackend};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    sync::Semaphore,
    time::{timeout, Instant},
};

type Binding = (String, i64, String);
struct StreamState {
    auth: Arc<dyn AuthBackend>,
    scheme: RequestScheme,
    hosts: Vec<String>,
    backend: TmuxBackend,
    roles: Mutex<Roles>,
    socket: Option<String>,
    workers: Arc<Semaphore>,
    sessions: Arc<Semaphore>,
    controls: Arc<ControlRegistry>,
    resize_slots: Arc<ResizeSlots>,
    resize_epoch: String,
}

/// Disabled unless explicitly enabled and supplied with trusted hosts.
pub fn configured_router(auth: Arc<dyn AuthBackend>, scheme: RequestScheme) -> Router {
    let env = Env::from_process();
    router_from_env(auth, scheme, &env)
}

fn router_from_env(auth: Arc<dyn AuthBackend>, scheme: RequestScheme, env: &Env) -> Router {
    if env.get("DUNKINGSHEEP_TERMINAL_STREAM") != Some("1") {
        return Router::new();
    }
    let hosts: Vec<String> = env
        .get("DUNKINGSHEEP_TERMINAL_STREAM_HOSTS")
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
        .collect();
    if hosts.is_empty() {
        return Router::new();
    }
    let Ok(backend) = ds_core::server_runtime::configured_backend() else {
        return Router::new();
    };
    let Ok(roles) = Roles::new(env, None, None, None) else {
        return Router::new();
    };
    let Some(resize_epoch) = new_resize_epoch() else {
        return Router::new();
    };
    let socket = env
        .get("DUNKINGSHEEP_TMUX_SOCKET")
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let state = Arc::new(StreamState {
        auth,
        scheme,
        hosts,
        backend,
        roles: Mutex::new(roles),
        socket,
        workers: Arc::new(Semaphore::new(4)),
        sessions: Arc::new(Semaphore::new(4)),
        controls: Arc::new(ControlRegistry::default()),
        resize_slots: Arc::new(ResizeSlots::default()),
        resize_epoch,
    });
    Router::new()
        .route("/api/terminals", get(list_terminals))
        .route(
            "/api/terminals/{name}/resize",
            post(resize_terminal)
                .get(resize_status)
                .layer(DefaultBodyLimit::max(4096)),
        )
        .route("/api/terminals/{name}/stream", get(upgrade))
        .with_state(state)
}

fn new_resize_epoch() -> Option<String> {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 16];
    ring::rand::SystemRandom::new().fill(&mut bytes).ok()?;
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[derive(serde::Deserialize)]
struct ResizeRequest {
    cols: i64,
    rows: i64,
}

enum ResizeResult {
    Applied(u16, u16),
    Refused,
    Unknown,
}

async fn resize_status(
    State(state): State<Arc<StreamState>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !terminal_stream::valid_terminal_host(&headers, &state.hosts)
        || (headers.contains_key(header::ORIGIN)
            && !terminal_stream::valid_websocket_origin(&headers, state.scheme, &state.hosts))
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(token) = cookie(&headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if !is_owner(&state, &token).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(busy_name) = state.resize_slots.busy_name(&name) else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let in_flight = if busy_name {
        true
    } else {
        // A different name can refer to a pane with an outstanding operation.
        // If the current target cannot be resolved, do not claim it is idle.
        let Some((binding, _)) = lookup(state.clone(), name.clone(), false).await else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        let Some(busy) = state.resize_slots.busy_target(&name, &binding.0) else {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        };
        busy
    };
    if !is_owner(&state, &token).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mut response =
        Json(serde_json::json!({"in_flight":in_flight,"epoch":state.resize_epoch.as_str()}))
            .into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

fn resize_in_worker(
    state: &StreamState,
    name: &str,
    binding: &Binding,
    client: &ControlIdentity,
    cols: i64,
    rows: i64,
    deadline: std::time::Instant,
) -> ResizeResult {
    let Some(pane_digits) = binding.0.strip_prefix('%') else {
        return ResizeResult::Refused;
    };
    if pane_digits.is_empty() || !pane_digits.bytes().all(|b| b.is_ascii_digit()) {
        return ResizeResult::Refused;
    }
    let Ok(mut roles) = state.roles.lock() else {
        return ResizeResult::Refused;
    };
    if terminal_stream::resolve_live_agent(name, &state.backend, &mut roles).as_ref()
        != Some(binding)
    {
        return ResizeResult::Refused;
    }
    drop(roles);
    if state.controls.unique(&binding.0).as_ref() != Some(client)
        || ds_core::proc::process_start(client.pid).as_deref() != Some(client.start.as_str())
        || std::time::Instant::now() >= deadline
    {
        return ResizeResult::Refused;
    }
    let pid_match = format!("#{{==:#{{pane_pid}},{}}}", binding.1);
    let client_match = format!(
        "#{{==:#{{window_active_clients_list}},client-{}}}",
        client.pid
    );
    // The queue and format semantics used here have been inspected for tmux
    // 3.4 and 3.6. Unknown server versions fail closed.
    let version_ok = "#{||:#{==:#{version},3.4},#{==:#{version},3.6}}";
    let condition = format!("#{{&&:{version_ok},#{{&&:#{{==:#{{window_panes}},1}},#{{&&:#{{==:#{{window_linked}},0}},#{{&&:{pid_match},#{{&&:#{{==:#{{window_active_clients}},1}},{client_match}}}}}}}}}}}");
    let command = format!("resize-window -t {} -x {cols} -y {rows}", binding.0);
    let Ok(result) = state.backend.run(&[
        "if-shell",
        "-F",
        "-t",
        &binding.0,
        &condition,
        &command,
        "display-message -p t62-resize-refused",
    ]) else {
        return ResizeResult::Unknown;
    };
    if result.code != 0 {
        return ResizeResult::Unknown;
    }
    if result.stdout.trim() == "t62-resize-refused" {
        return ResizeResult::Refused;
    }
    if !result.stdout.trim().is_empty() {
        return ResizeResult::Unknown;
    }
    let Ok(measured) = state.backend.run(&[
        "display-message",
        "-p",
        "-t",
        &binding.0,
        "#{pane_pid}|#{pane_width}|#{pane_height}",
    ]) else {
        return ResizeResult::Unknown;
    };
    let fields: Vec<_> = measured.stdout.trim().split('|').collect();
    let [pid, width, height] = fields.as_slice() else {
        return ResizeResult::Unknown;
    };
    let (Ok(pid), Ok(width), Ok(height)) = (
        pid.parse::<i64>(),
        width.parse::<u16>(),
        height.parse::<u16>(),
    ) else {
        return ResizeResult::Unknown;
    };
    if measured.code != 0 || pid != binding.1 || width == 0 || height == 0 {
        return ResizeResult::Unknown;
    }
    ResizeResult::Applied(width, height)
}

async fn resize_terminal(
    State(state): State<Arc<StreamState>>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Json(request): Json<ResizeRequest>,
) -> Response {
    if !terminal_stream::valid_websocket_origin(&headers, state.scheme, &state.hosts) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(token) = cookie(&headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if !is_owner(&state, &token).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if !(1..=1000).contains(&request.cols) || !(1..=1000).contains(&request.rows) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"detail":"cols and rows must be between 1 and 1000"})),
        )
            .into_response();
    }
    let Some((binding, _)) = lookup(state.clone(), name.clone(), false).await else {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"detail":"target unavailable or ambiguous; resize refused"})),
        )
            .into_response();
    };
    let Some(client) = state.controls.unique(&binding.0) else {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"detail":"exactly one active managed stream is required"})),
        )
            .into_response();
    };
    let Some(lease) = state.resize_slots.acquire(&name, &binding.0) else {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"detail":"resize still in flight or target is busy"})),
        )
            .into_response();
    };
    if !is_owner(&state, &token).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(permit) = state.workers.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let worker_state = state.clone();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let result = timeout(
        Duration::from_secs(2),
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _lease = lease;
            resize_in_worker(
                &worker_state,
                &name,
                &binding,
                &client,
                request.cols,
                request.rows,
                deadline,
            )
        }),
    )
    .await;
    match result {
        Ok(Ok(ResizeResult::Applied(cols, rows))) => Json(serde_json::json!({"cols":cols,"rows":rows})).into_response(),
        Ok(Ok(ResizeResult::Refused)) => (StatusCode::CONFLICT, Json(serde_json::json!({"detail":"resize refused: target or client is not isolated"}))).into_response(),
        _ => (StatusCode::GATEWAY_TIMEOUT, Json(serde_json::json!({"detail":"resize outcome unknown; only if the resize status epoch matches, wait for no operation in flight, then inspect dimensions before any manual retry. If epochs differ, independently reconcile before any retry.","epoch":state.resize_epoch.as_str()}))).into_response(),
    }
}

async fn list_terminals(State(state): State<Arc<StreamState>>, headers: HeaderMap) -> Response {
    if !terminal_stream::valid_terminal_host(&headers, &state.hosts)
        || (headers.contains_key(header::ORIGIN)
            && !terminal_stream::valid_websocket_origin(&headers, state.scheme, &state.hosts))
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(token) = cookie(&headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if !is_owner(&state, &token).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(permit) = state.workers.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let worker_state = state.clone();
    let found = timeout(
        Duration::from_secs(2),
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut roles = worker_state.roles.lock().ok()?;
            terminal_stream::list_live_agents(&worker_state.backend, &mut roles)
        }),
    )
    .await;
    let Ok(Ok(Some(found))) = found else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    if !is_owner(&state, &token).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let mut response = Json(found).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}

fn cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|p| p.trim().split_once('='))
        .filter(|(k, _)| *k == "vp_session")
        .map(|(_, v)| v.to_owned())
        .next_back()
        .filter(|v| !v.is_empty())
}
async fn is_owner(state: &StreamState, token: &str) -> bool {
    matches!(
        timeout(
            Duration::from_secs(2),
            state.auth.identity(None, Some(token))
        )
        .await,
        Ok(Ok(Identity::Owner))
    )
}
/// Blocking registry and tmux work is bounded even if a subprocess gets stuck.
async fn lookup(
    state: Arc<StreamState>,
    name: String,
    with_session: bool,
) -> Option<(Binding, Option<String>)> {
    let permit = state.workers.clone().try_acquire_owned().ok()?;
    timeout(
        Duration::from_secs(2),
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let mut roles = state.roles.lock().ok()?;
            let binding = terminal_stream::resolve_live_agent(&name, &state.backend, &mut roles)?;
            let session = if with_session {
                let result = state
                    .backend
                    .run(&["display-message", "-p", "-t", &binding.0, "#{session_id}"])
                    .ok()?;
                let id = result.stdout.trim();
                if result.code != 0
                    || !id
                        .strip_prefix('$')
                        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                {
                    return None;
                }
                Some(id.to_owned())
            } else {
                None
            };
            Some((binding, session))
        }),
    )
    .await
    .ok()?
    .ok()?
}
async fn authorized(state: &Arc<StreamState>, name: &str, token: &str, binding: &Binding) -> bool {
    is_owner(state, token).await
        && lookup(state.clone(), name.to_owned(), false)
            .await
            .is_some_and(|(b, _)| &b == binding)
}

async fn register_control(state: &Arc<StreamState>, pane: &str, pid: u32) -> Option<ControlLease> {
    let permit = state.workers.clone().try_acquire_owned().ok()?;
    let start = timeout(
        Duration::from_secs(2),
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            ds_core::proc::process_start(i64::from(pid))
        }),
    )
    .await
    .ok()?
    .ok()??;
    state.controls.register(pane, i64::from(pid), start)
}
async fn upgrade(
    State(state): State<Arc<StreamState>>,
    Path(name): Path<String>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !terminal_stream::valid_websocket_origin(&headers, state.scheme, &state.hosts) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let Some(token) = cookie(&headers) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    if !is_owner(&state, &token).await {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Ok(permit) = state.sessions.clone().try_acquire_owned() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let Some((binding, Some(session))) = lookup(state.clone(), name.clone(), true).await else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let mut command = Command::new(&state.backend.tmux_bin);
    if let Some(socket) = state.socket.as_deref() {
        command.args(["-L", socket]);
    }
    command
        .args([
            "-C",
            "attach-session",
            "-E",
            "-f",
            "ignore-size,active-pane",
            "-t",
            &session,
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let Ok(mut child) = command.spawn() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let (Some(input), Some(output)) = (child.stdin.take(), child.stdout.take()) else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    ws.max_message_size(4096)
        .max_frame_size(4096)
        .on_upgrade(move |ws| async move {
            let _permit = permit;
            run(
                ws,
                state,
                name,
                token,
                binding,
                ControlPipes {
                    child,
                    input,
                    output,
                },
            )
            .await;
        })
        .into_response()
}
struct ControlPipes {
    child: tokio::process::Child,
    input: tokio::process::ChildStdin,
    output: tokio::process::ChildStdout,
}

async fn run(
    mut ws: WebSocket,
    state: Arc<StreamState>,
    name: String,
    token: String,
    binding: Binding,
    control: ControlPipes,
) {
    let ControlPipes {
        mut child,
        mut input,
        mut output,
    } = control;
    let Some(pid) = child.id() else { return };
    let Some(_control_lease) = register_control(&state, &binding.0, pid).await else {
        return;
    };
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut lines = ControlLines::default();
    let mut tracker = ControlReply::default();
    let mut ready = false;
    let mut pending = false;
    let mut deadline = Instant::now() + Duration::from_secs(5);
    let mut queue: VecDeque<Vec<u8>> = VecDeque::new();
    let mut control_queue: VecDeque<Vec<u8>> = VecDeque::new();
    let mut buf = [0u8; 8192];
    'stream: loop {
        tokio::select! {
            _ = tick.tick() => {
                if ((!ready || pending) && Instant::now() >= deadline) || !authorized(&state, &name, &token, &binding).await { break; }
            }
            incoming = ws.recv() => {
                let bytes = match incoming {
                    Some(Ok(Message::Binary(bytes))) => bytes.to_vec(),
                    Some(Ok(Message::Text(text))) => text.as_bytes().to_vec(),
                    Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => continue,
                    _ => break,
                };
                if queue.len() >= 8 || !authorized(&state, &name, &token, &binding).await { break; }
                if !bytes.is_empty() { queue.push_back(bytes); }
            }
            read = output.read(&mut buf), if control_queue.is_empty() => {
                let Ok(n) = read else { break }; if n == 0 { break; }
                let Ok(batch) = lines.push(&buf[..n]) else { break };
                if batch.len() > 32 { break; }
                control_queue.extend(batch);
            }
            _ = std::future::ready(()), if !control_queue.is_empty() => {
                if let Some(line) = control_queue.pop_front() {
                    match tracker.observe(&line) {
                        Ok(Some(true)) if !ready => ready = true,
                        Ok(Some(true)) if pending => pending = false,
                        Ok(Some(_)) | Err(_) => break 'stream,
                        Ok(None) => {}
                    }
                    match terminal_stream::pane_output(&line, &binding.0) {
                        Ok(Some(bytes)) if ready => {
                            if !authorized(&state, &name, &token, &binding).await || !matches!(timeout(Duration::from_secs(2), ws.send(Message::Binary(bytes.into()))).await, Ok(Ok(()))) { break 'stream; }
                        }
                        Ok(_) => {}, Err(_) => break 'stream,
                    }
                }
            }
        }
        if ready && !pending {
            if let Some(bytes) = queue.pop_front() {
                let Some(command) = terminal_stream::input_command(&binding.0, &bytes) else {
                    break;
                };
                if !authorized(&state, &name, &token, &binding).await
                    || !matches!(
                        timeout(Duration::from_secs(2), input.write_all(&command)).await,
                        Ok(Ok(()))
                    )
                {
                    break;
                }
                pending = true;
                deadline = Instant::now() + Duration::from_secs(5);
            }
        }
    }
    let _ = child.start_kill();
    let _ = timeout(Duration::from_secs(2), child.wait()).await;
    let _ = timeout(Duration::from_secs(2), ws.send(Message::Close(None))).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::auth::{AuthError, AuthFuture};
    use futures_util::{SinkExt, StreamExt};
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message as ClientMessage};

    struct FakeAuth(AtomicBool);
    impl AuthBackend for FakeAuth {
        fn login<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
        ) -> AuthFuture<'a, Result<String, AuthError>> {
            Box::pin(async { Err(AuthError::SignInRequired) })
        }
        fn identity<'a>(
            &'a self,
            _: Option<&'a str>,
            cookie: Option<&'a str>,
        ) -> AuthFuture<'a, Result<Identity, AuthError>> {
            Box::pin(async move {
                if self.0.load(Ordering::SeqCst) && cookie == Some("private-test") {
                    Ok(Identity::Owner)
                } else {
                    Err(AuthError::SignInRequired)
                }
            })
        }
        fn logout<'a>(&'a self, _: Option<&'a str>) -> AuthFuture<'a, Result<(), AuthError>> {
            Box::pin(async { Ok(()) })
        }
        fn change_password<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: Option<&'a str>,
        ) -> AuthFuture<'a, Result<(), AuthError>> {
            Box::pin(async { Err(AuthError::SignInRequired) })
        }
    }
    struct PrivateServer(String);
    impl Drop for PrivateServer {
        fn drop(&mut self) {
            let _ = std::process::Command::new("tmux")
                .args(["-L", &self.0, "kill-server"])
                .output();
        }
    }

    #[test]
    fn independently_created_router_epochs_are_distinct() {
        let first = new_resize_epoch().unwrap();
        let second = new_resize_epoch().unwrap();
        assert_eq!(first.len(), 32);
        assert_ne!(first, second);
    }

    #[tokio::test]
    async fn route_is_absent_without_explicit_enable_and_hosts() {
        use tower::ServiceExt;
        let auth = Arc::new(FakeAuth(AtomicBool::new(true)));
        for env in [
            Env::from_pairs(&[]),
            Env::from_pairs(&[("DUNKINGSHEEP_TERMINAL_STREAM", "1")]),
        ] {
            for (method, path) in [
                ("GET", "/api/terminals"),
                ("GET", "/api/terminals/example/stream"),
                ("POST", "/api/terminals/example/resize"),
            ] {
                let response = router_from_env(auth.clone(), RequestScheme::Http, &env)
                    .oneshot(
                        axum::http::Request::builder()
                            .method(method)
                            .uri(path)
                            .body(axum::body::Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::NOT_FOUND);
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn private_owner_websocket_stream_rejects_bad_origin_and_revocation() {
        let dir = tempfile::Builder::new()
            .prefix("t62-ws-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        let socket = format!(
            "ds-t62-ws-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let server = PrivateServer(socket.clone());
        assert!(std::process::Command::new("tmux")
            .args(["-L", &server.0, "new-session", "-d", "-s", "fixture", "cat"])
            .status()
            .unwrap()
            .success());
        let registry_path = dir.path().join("tmux-panes.json");
        let env = Env::from_pairs(&[
            ("DUNKINGSHEEP_TMUX_SOCKET", &socket),
            (
                "DUNKINGSHEEP_TMUX_REGISTRY",
                registry_path.to_str().unwrap(),
            ),
        ]);
        let cancel_resize = std::env::var_os("T62_CANCEL_RESIZE").is_some();
        let slow_resize = cancel_resize || std::env::var_os("T62_SLOW_RESIZE").is_some();
        let tmux_program = if slow_resize {
            use std::os::unix::fs::PermissionsExt;
            let wrapper = dir.path().join("private-tmux-wrapper");
            std::fs::write(&wrapper, b"#!/bin/sh\nmarker=${0%/*}/resize-slept\nfor arg in \"$@\"; do if [ \"$arg\" = if-shell ] && [ ! -e \"$marker\" ]; then : > \"$marker\"; sleep 5; break; fi; done\nexec /usr/bin/tmux \"$@\"\n").unwrap();
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
            wrapper
        } else {
            std::path::PathBuf::from("tmux")
        };
        let backend = TmuxBackend::from_env(tmux_program, env.clone());
        let listing = backend
            .run(&[
                "list-panes",
                "-a",
                "-F",
                ds_core::terminal::identity::identity_format(),
            ])
            .unwrap();
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
                serde_json::json!({"name":"fixture-worker","agent":"fixture-worker"}),
            )
            .unwrap();
        let mut roles = Roles::new(
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
        let owner = backend
            .run(&[
                "new-window",
                "-d",
                "-P",
                "-F",
                "#{pane_id}",
                "-t",
                "fixture",
                "cat",
            ])
            .unwrap();
        assert_eq!(owner.code, 0);
        let owner_pane = owner.stdout.trim();
        let identities = backend
            .run(&[
                "list-panes",
                "-a",
                "-F",
                ds_core::terminal::identity::identity_format(),
            ])
            .unwrap();
        let owner_pid = ds_core::terminal::identity::parse_pane_identities(&identities.stdout)
            .unwrap()
            .into_iter()
            .find(|(id, _)| id == owner_pane)
            .and_then(|(_, pid)| pid)
            .unwrap();
        let owner_start = ds_core::proc::process_start(owner_pid).unwrap();
        backend
            .registry()
            .remember(
                "pane",
                owner_pane,
                serde_json::json!({"name":"private-owner","agent":"private-owner"}),
            )
            .unwrap();
        roles
            .register(
                owner_pane,
                "owner",
                Some(owner_pid),
                Some(&owner_start),
                None,
                None,
                None,
                None,
            )
            .unwrap();
        let fake_auth = Arc::new(FakeAuth(AtomicBool::new(true)));
        let real_data = dir.path().join("real-auth");
        let real_auth = if std::env::var_os("T62_REAL_AUTH").is_some() {
            let path = real_data.clone();
            Some(Arc::new(
                tokio::task::spawn_blocking(move || {
                    crate::auth_backend::PersistentAuth::open_with_password(
                        path,
                        Some("private-fixture-password"),
                    )
                })
                .await
                .unwrap()
                .unwrap(),
            ))
        } else {
            None
        };
        let auth: Arc<dyn AuthBackend> = match &real_auth {
            Some(real) => real.clone(),
            None => fake_auth.clone(),
        };
        let session_token = if real_auth.is_some() {
            auth.login("127.0.0.1", "private-fixture-password")
                .await
                .unwrap()
        } else {
            "private-test".to_owned()
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Optional local integration against the actual gateway script. All
        // sockets and the session database belong to this private fixture.
        let mut proxy = None;
        let client_addr = if let Ok(script) = std::env::var("T62_GATEWAY_SCRIPT") {
            let reserved = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let proxy_addr = reserved.local_addr().unwrap();
            drop(reserved);
            let db = if real_auth.is_some() {
                real_data.join("messages.sqlite3")
            } else {
                let db = dir.path().join("proxy-auth.sqlite3");
                let connection = rusqlite::Connection::open(&db).unwrap();
                connection
                    .execute(
                        "CREATE TABLE sessions(token_hash TEXT, expires_at REAL)",
                        [],
                    )
                    .unwrap();
                let digest = ring::digest::digest(&ring::digest::SHA256, session_token.as_bytes());
                let hash = digest
                    .as_ref()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>();
                connection
                    .execute(
                        "INSERT INTO sessions VALUES (?1, ?2)",
                        rusqlite::params![hash, 9_999_999_999_f64],
                    )
                    .unwrap();
                db
            };
            let child = Command::new("python3")
                .arg(script)
                .env("HERDR_WEBUI_PROXY_BIND", proxy_addr.to_string())
                .env("HERDR_WEBUI_PROXY_BACKEND", addr.to_string())
                .env("VP_AUTH_DB", db)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            proxy = Some(child);
            timeout(Duration::from_secs(5), async {
                loop {
                    if tokio::net::TcpStream::connect(proxy_addr).await.is_ok() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            })
            .await
            .unwrap();
            proxy_addr
        } else {
            addr
        };
        let inspect_backend = backend.clone();
        let state = Arc::new(StreamState {
            auth: auth.clone(),
            scheme: RequestScheme::Http,
            hosts: vec![addr.to_string(), client_addr.to_string()],
            backend,
            roles: Mutex::new(roles),
            socket: Some(socket),
            workers: Arc::new(Semaphore::new(4)),
            sessions: Arc::new(Semaphore::new(4)),
            controls: Arc::new(ControlRegistry::default()),
            resize_slots: Arc::new(ResizeSlots::default()),
            resize_epoch: "private-fixture-epoch".to_owned(),
        });
        let app = Router::new()
            .route("/api/terminals", get(list_terminals))
            .route(
                "/api/terminals/{name}/resize",
                post(resize_terminal)
                    .get(resize_status)
                    .layer(DefaultBodyLimit::max(4096)),
            )
            .route("/api/terminals/{name}/stream", get(upgrade))
            .with_state(state.clone());
        use tower::ServiceExt;
        let list_request = |authenticated: bool| {
            let mut request = axum::http::Request::builder()
                .uri("/api/terminals")
                .header(header::HOST, client_addr.to_string());
            if authenticated {
                request = request.header(header::COOKIE, format!("vp_session={session_token}"));
            }
            request.body(axum::body::Body::empty()).unwrap()
        };
        let denied = app.clone().oneshot(list_request(false)).await.unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
        let cross_origin = axum::http::Request::builder()
            .uri("/api/terminals")
            .header(header::HOST, client_addr.to_string())
            .header(header::ORIGIN, "http://evil.test")
            .header(header::COOKIE, format!("vp_session={session_token}"))
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(cross_origin).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        let listed = app.clone().oneshot(list_request(true)).await.unwrap();
        assert_eq!(listed.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(listed.into_body(), 4096)
            .await
            .unwrap();
        let items: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(items.as_array().unwrap().len(), 1);
        assert_eq!(items[0]["name"], "fixture-worker");
        assert!(items[0].get("pane_id").is_none());
        let resize_request = |name: &str, body: &'static str| {
            axum::http::Request::builder()
                .method("POST")
                .uri(format!("/api/terminals/{name}/resize"))
                .header(header::HOST, client_addr.to_string())
                .header(header::ORIGIN, format!("http://{client_addr}"))
                .header(header::COOKIE, format!("vp_session={session_token}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(body))
                .unwrap()
        };
        let mut cross_resize = resize_request("fixture-worker", r#"{"cols":95,"rows":30}"#);
        cross_resize
            .headers_mut()
            .insert(header::ORIGIN, "http://evil.test".parse().unwrap());
        assert_eq!(
            app.clone().oneshot(cross_resize).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            app.clone()
                .oneshot(resize_request("fixture-worker", r#"{"cols":95,"rows":30}"#))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            app.clone()
                .oneshot(resize_request("private-owner", r#"{"cols":95,"rows":30}"#))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            app.clone()
                .oneshot(resize_request("fixture-worker", r#"{"cols":0,"rows":30}"#))
                .await
                .unwrap()
                .status(),
            StatusCode::BAD_REQUEST
        );
        let test_app = app.clone();
        let serving = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut list_socket = tokio::net::TcpStream::connect(client_addr).await.unwrap();
        let list_http = format!("GET /api/terminals HTTP/1.1\r\nHost: {client_addr}\r\nCookie: vp_session={session_token}\r\nConnection: close\r\n\r\n");
        list_socket.write_all(list_http.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        timeout(
            Duration::from_secs(8),
            list_socket.read_to_end(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        let text = std::str::from_utf8(&response).unwrap();
        assert!(text.starts_with("HTTP/1.1 200"));
        let body = text.split_once("\r\n\r\n").unwrap().1;
        let items: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(items.as_array().unwrap().len(), 1);
        assert_eq!(items[0]["name"], "fixture-worker");
        let url = format!("ws://{client_addr}/api/terminals/fixture-worker/stream");
        let request = |origin: &str, with_cookie: bool| {
            let mut req = url.as_str().into_client_request().unwrap();
            req.headers_mut()
                .insert(header::ORIGIN, origin.parse().unwrap());
            if with_cookie {
                req.headers_mut().insert(
                    header::COOKIE,
                    format!("vp_session={session_token}").parse().unwrap(),
                );
            }
            req
        };
        let origin = format!("http://{client_addr}");
        assert!(tokio_tungstenite::connect_async(request(&origin, false))
            .await
            .is_err());
        assert!(
            tokio_tungstenite::connect_async(request("http://evil.test", true))
                .await
                .is_err()
        );
        let (mut ws, _) = tokio_tungstenite::connect_async(request(&origin, true))
            .await
            .unwrap();
        ws.send(ClientMessage::Binary(b"t62-ws-input\n".to_vec().into()))
            .await
            .unwrap();
        let received = timeout(Duration::from_secs(8), async {
            let mut bytes = Vec::new();
            while let Some(Ok(message)) = ws.next().await {
                if let ClientMessage::Binary(chunk) = message {
                    bytes.extend_from_slice(&chunk);
                    if bytes
                        .windows(b"t62-ws-input".len())
                        .any(|w| w == b"t62-ws-input")
                    {
                        return true;
                    }
                }
            }
            false
        })
        .await
        .unwrap();
        assert!(received);
        // Simulate an earlier operation whose pane was subsequently renamed.
        let held = state.resize_slots.acquire("prior-name", pane).unwrap();
        let status_request = || {
            axum::http::Request::builder()
                .uri("/api/terminals/fixture-worker/resize")
                .header(header::HOST, client_addr.to_string())
                .header(header::COOKIE, format!("vp_session={session_token}"))
                .body(axum::body::Body::empty())
                .unwrap()
        };
        let status = test_app.clone().oneshot(status_request()).await.unwrap();
        assert_eq!(status.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(status.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["in_flight"],
            true
        );
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["epoch"],
            "private-fixture-epoch"
        );
        assert_eq!(
            test_app
                .clone()
                .oneshot(resize_request("fixture-worker", r#"{"cols":95,"rows":30}"#))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        drop(held);
        let status = test_app.clone().oneshot(status_request()).await.unwrap();
        let bytes = axum::body::to_bytes(status.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["in_flight"],
            false
        );
        let body = r#"{"cols":95,"rows":30}"#;
        let mut resize_socket = tokio::net::TcpStream::connect(client_addr).await.unwrap();
        let resize_http = format!("POST /api/terminals/fixture-worker/resize HTTP/1.1\r\nHost: {client_addr}\r\nOrigin: http://{client_addr}\r\nCookie: vp_session={session_token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        resize_socket
            .write_all(resize_http.as_bytes())
            .await
            .unwrap();
        let mut response = Vec::new();
        if cancel_resize {
            timeout(Duration::from_secs(8), async {
                loop {
                    if state.resize_slots.busy_name("fixture-worker") == Some(true) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .unwrap();
            drop(resize_socket);
        } else {
            timeout(
                Duration::from_secs(8),
                resize_socket.read_to_end(&mut response),
            )
            .await
            .unwrap()
            .unwrap();
        }
        if slow_resize {
            if !cancel_resize {
                assert!(response.starts_with(b"HTTP/1.1 504"));
                let body = std::str::from_utf8(&response)
                    .unwrap()
                    .split_once("\r\n\r\n")
                    .unwrap()
                    .1;
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(body).unwrap()["epoch"],
                    "private-fixture-epoch"
                );
            }
            let status = test_app.clone().oneshot(status_request()).await.unwrap();
            let bytes = axum::body::to_bytes(status.into_body(), 1024)
                .await
                .unwrap();
            let status: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(status["in_flight"], true);
            assert_eq!(
                test_app
                    .clone()
                    .oneshot(resize_request("fixture-worker", r#"{"cols":95,"rows":30}"#))
                    .await
                    .unwrap()
                    .status(),
                StatusCode::CONFLICT
            );
            timeout(Duration::from_secs(10), async {
                loop {
                    let status = test_app.clone().oneshot(status_request()).await.unwrap();
                    let bytes = axum::body::to_bytes(status.into_body(), 1024)
                        .await
                        .unwrap();
                    if serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["in_flight"]
                        == false
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .unwrap();
        } else {
            assert!(response.starts_with(b"HTTP/1.1 200"));
        }
        let geometry = || {
            inspect_backend
                .run(&[
                    "display-message",
                    "-p",
                    "-t",
                    pane,
                    "#{window_width}x#{window_height}",
                ])
                .unwrap()
                .stdout
                .trim()
                .to_owned()
        };
        assert_eq!(geometry(), "95x30");
        let split = inspect_backend
            .run(&[
                "split-window",
                "-d",
                "-P",
                "-F",
                "#{pane_id}",
                "-t",
                pane,
                "cat",
            ])
            .unwrap();
        assert_eq!(split.code, 0);
        let split_pane = split.stdout.trim();
        let split_pid: i64 = inspect_backend
            .run(&["display-message", "-p", "-t", split_pane, "#{pane_pid}"])
            .unwrap()
            .stdout
            .trim()
            .parse()
            .unwrap();
        let split_start = ds_core::proc::process_start(split_pid).unwrap();
        inspect_backend
            .registry()
            .remember(
                "pane",
                split_pane,
                serde_json::json!({"name":"shared-owner","agent":"shared-owner"}),
            )
            .unwrap();
        state
            .roles
            .lock()
            .unwrap()
            .register(
                split_pane,
                "owner",
                Some(split_pid),
                Some(&split_start),
                None,
                None,
                None,
                None,
            )
            .unwrap();
        let before = geometry();
        assert_eq!(
            test_app
                .clone()
                .oneshot(resize_request("fixture-worker", r#"{"cols":90,"rows":28}"#))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        assert_eq!(geometry(), before);
        state.roles.lock().unwrap().unregister(split_pane).unwrap();
        inspect_backend
            .registry()
            .forget("pane", split_pane)
            .unwrap();
        assert_eq!(
            inspect_backend
                .run(&["kill-pane", "-t", split_pane])
                .unwrap()
                .code,
            0
        );
        assert_eq!(
            inspect_backend
                .run(&["new-session", "-d", "-s", "linked", "cat"])
                .unwrap()
                .code,
            0
        );
        assert_eq!(
            inspect_backend
                .run(&["link-window", "-s", pane, "-t", "linked:5"])
                .unwrap()
                .code,
            0
        );
        let before = geometry();
        assert_eq!(
            test_app
                .clone()
                .oneshot(resize_request("fixture-worker", r#"{"cols":90,"rows":28}"#))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        assert_eq!(geometry(), before);
        assert_eq!(
            inspect_backend
                .run(&["unlink-window", "-t", "linked:5"])
                .unwrap()
                .code,
            0
        );
        let (mut other, _) = tokio_tungstenite::connect_async(request(&origin, true))
            .await
            .unwrap();
        other
            .send(ClientMessage::Binary(b"second-control\n".to_vec().into()))
            .await
            .unwrap();
        assert!(timeout(Duration::from_secs(8), async {
            while let Some(Ok(message)) = other.next().await {
                if let ClientMessage::Binary(bytes) = message {
                    if bytes
                        .windows(b"second-control".len())
                        .any(|w| w == b"second-control")
                    {
                        return true;
                    }
                }
            }
            false
        })
        .await
        .unwrap());
        assert_eq!(
            test_app
                .clone()
                .oneshot(resize_request("fixture-worker", r#"{"cols":90,"rows":28}"#))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        other.close(None).await.unwrap();
        timeout(Duration::from_secs(6), async {
            loop {
                if state.controls.unique(pane).is_some() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        state.roles.lock().unwrap().unregister(pane).unwrap();
        assert_eq!(
            test_app
                .clone()
                .oneshot(resize_request("fixture-worker", r#"{"cols":90,"rows":28}"#))
                .await
                .unwrap()
                .status(),
            StatusCode::CONFLICT
        );
        assert!(timeout(Duration::from_secs(6), async {
            while let Some(Ok(message)) = ws.next().await {
                if matches!(message, ClientMessage::Close(_)) {
                    break;
                }
            }
        })
        .await
        .is_ok());
        timeout(Duration::from_secs(6), async {
            loop {
                if state.controls.unique(pane).is_none() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        state
            .roles
            .lock()
            .unwrap()
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
        let (mut ws, _) = tokio_tungstenite::connect_async(request(&origin, true))
            .await
            .unwrap();
        ws.send(ClientMessage::Binary(
            b"after-role-rebind\n".to_vec().into(),
        ))
        .await
        .unwrap();
        assert!(timeout(Duration::from_secs(8), async {
            while let Some(Ok(message)) = ws.next().await {
                if let ClientMessage::Binary(bytes) = message {
                    if bytes
                        .windows(b"after-role-rebind".len())
                        .any(|w| w == b"after-role-rebind")
                    {
                        return true;
                    }
                }
            }
            false
        })
        .await
        .unwrap());
        if real_auth.is_some() {
            auth.logout(Some(&session_token)).await.unwrap();
        } else {
            fake_auth.0.store(false, Ordering::SeqCst);
        }
        assert!(timeout(Duration::from_secs(6), async {
            while let Some(Ok(message)) = ws.next().await {
                if matches!(message, ClientMessage::Close(_)) {
                    break;
                }
            }
        })
        .await
        .is_ok());
        serving.abort();
        if let Some(mut child) = proxy {
            let _ = child.kill().await;
        }
    }
}
