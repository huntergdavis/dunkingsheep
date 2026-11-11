//! Default-off owner-authenticated terminal stream endpoint.
use crate::{
    routes::auth::{AuthBackend, Identity, RequestScheme},
    terminal_stream::{self, ControlLines, ControlReply},
};
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
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
    });
    Router::new()
        .route("/api/terminals/{name}/stream", get(upgrade))
        .with_state(state)
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

    #[tokio::test]
    async fn route_is_absent_without_explicit_enable_and_hosts() {
        use tower::ServiceExt;
        let auth = Arc::new(FakeAuth(AtomicBool::new(true)));
        for env in [
            Env::from_pairs(&[]),
            Env::from_pairs(&[("DUNKINGSHEEP_TERMINAL_STREAM", "1")]),
        ] {
            let response = router_from_env(auth.clone(), RequestScheme::Http, &env)
                .oneshot(
                    axum::http::Request::builder()
                        .uri("/api/terminals/example/stream")
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
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
        let backend = TmuxBackend::from_env("tmux", env.clone());
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
        let state = Arc::new(StreamState {
            auth: auth.clone(),
            scheme: RequestScheme::Http,
            hosts: vec![addr.to_string(), client_addr.to_string()],
            backend,
            roles: Mutex::new(roles),
            socket: Some(socket),
            workers: Arc::new(Semaphore::new(4)),
            sessions: Arc::new(Semaphore::new(4)),
        });
        let app = Router::new()
            .route("/api/terminals/{name}/stream", get(upgrade))
            .with_state(state);
        let serving = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
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
