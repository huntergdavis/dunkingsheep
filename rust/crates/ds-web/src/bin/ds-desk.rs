//! Production desk entrypoint. The service supplies the release working directory and data paths.
use ds_web::{
    ops_history,
    web_server::{self, DeskConfig},
    RequestScheme,
};
use std::{
    env,
    error::Error,
    path::{Path, PathBuf},
};

fn value(name: &str) -> Option<String> {
    env::var(name).ok().filter(|v| !v.is_empty())
}
fn path(name: &str, default: impl Into<PathBuf>) -> PathBuf {
    value(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| default.into())
}
fn positive(name: &str, default: i64, minimum: i64) -> Result<i64, Box<dyn Error>> {
    Ok(value(name)
        .map(|v| v.parse::<i64>())
        .transpose()?
        .unwrap_or(default)
        .max(minimum))
}
fn make_config(root: &Path) -> Result<DeskConfig, Box<dyn Error>> {
    let home = PathBuf::from(value("HOME").ok_or("HOME is required")?);
    let config_dir = path("DUNKINGSHEEP_DIR", home.join(".config/dunkingsheep"));
    let data_dir = path("VP_DATA_DIR", config_dir.join("desk"));
    let channels_path = value("VP_CHANNELS_FILE").map(PathBuf::from);
    let channels = web_server::load_channel_registry(channels_path.as_deref(), root)?;
    let token = ds_core::client::service_token(&|name| env::var(name).ok(), &|p| {
        std::fs::read_to_string(p)
    });
    if token.is_none() {
        return Err("web service token is missing or unreadable".into());
    }
    let socket = path("DUNKINGSHEEP_SOCKET", config_dir.join("dunkingsheep.sock"));
    let timeout = ds_core::client::default_timeout(&|name| env::var(name).ok());
    let poolrad_repo = path(
        "POOLRAD_REPO",
        root.parent().unwrap_or(root).join("poolrad-decomp"),
    );
    let refresh = positive("VP_USAGE_REFRESH_S", 1800, 1)?;
    let backoff = value("VP_USAGE_BACKOFF_S")
        .unwrap_or_else(|| "300,600,1800".into())
        .split(',')
        .map(|v| v.parse::<i64>().map(|n| n.max(1)))
        .collect::<Result<Vec<_>, _>>()?;
    let scheme = match value("DS_DESK_SCHEME").as_deref().unwrap_or("http") {
        "http" => RequestScheme::Http,
        "https" => RequestScheme::Https,
        _ => return Err("DS_DESK_SCHEME must be http or https".into()),
    };
    Ok(DeskConfig {
        data_dir: data_dir.clone(),
        config_dir: config_dir.clone(),
        home: home.clone(),
        daemon_socket: socket,
        service_token: token,
        client_timeout: timeout,
        channels,
        ui_dist: root.join("ui/dist"),
        dashboard_dir: path(
            "POOLRAD_DASHBOARD_DIR",
            root.join("bridges/integrations/poolrad-dashboard"),
        ),
        dashboard_repo: path("POOLRAD_DASHBOARD_REPO", poolrad_repo.clone()),
        poolrad_repo,
        operations_db: path("VP_OPS_DB", data_dir.join("operations.sqlite3")),
        host_watch_state: path(
            "VP_HOST_WATCH_STATE",
            data_dir.join("host-watch-state.json"),
        ),
        opencode_db: path(
            "OPENCODE_DB",
            home.join(".local/share/opencode/opencode.db"),
        ),
        dunk_events_db: path(
            "DUNK_EVENTS_DB",
            home.join(".config/dunkingsheep/metrics.db"),
        ),
        usage_config: home.join(".config/dunkingsheep/usage.json"),
        usage_refresh_s: refresh,
        usage_backoff_s: backoff,
        usage_max_age_s: refresh.saturating_mul(2),
        board_interval_s: positive("VP_OPS_BOARD_INTERVAL_S", 30, 10)? as u64,
        gh_program: PathBuf::from("gh"),
        tmux_program: PathBuf::from("tmux"),
        proc_root: PathBuf::from("/proc"),
        herdr_webui_url: value("VP_HERDR_WEBUI_URL"),
        herdr_webui_label: value("VP_HERDR_WEBUI_LABEL").or_else(|| Some("Herdr web UI".into())),
        initial_password: value("VP_PASSWORD"),
        scheme,
        open_operations_db: ops_history::connect,
    })
}
async fn shutdown() {
    #[cfg(unix)]
    {
        if let Ok(mut term) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = term.recv() => {} }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
fn main() -> Result<(), Box<dyn Error>> {
    let root = env::current_dir()?;
    let config = make_config(&root)?;
    if !config.ui_dist.join("index.html").is_file() {
        return Err("built UI index is missing".into());
    }
    let host = value("VP_BIND").unwrap_or_else(|| "0.0.0.0".into());
    let port = value("VP_PORT")
        .unwrap_or_else(|| "8787".into())
        .parse::<u16>()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let listener = tokio::net::TcpListener::bind((host.as_str(), port)).await?;
        eprintln!("desk listening on {host}:{port}");
        web_server::serve(config, listener, shutdown())
            .await
            .map_err(|e| -> Box<dyn Error> { e.to_string().into() })
    })
}
