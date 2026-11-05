//! Dedicated HTTP facade entry point. The generated `dunkingsheep` parser is
//! intentionally unchanged.

use std::{path::PathBuf, process::ExitCode};

use clap::{value_parser, Arg, Command};
use ds_web::http_adapter::{api_bind_config, bind_listener, serve_configured, system_router};

fn main() -> ExitCode {
    let defaults = match api_bind_config() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::from(2);
        }
    };
    let matches = Command::new("ds-http")
        .about("Dunking Sheep HTTP facade (GET only, plus POST resize)")
        .arg(
            Arg::new("host")
                .long("host")
                .value_name("HOST")
                .help(format!("Host to bind (default: {})", defaults.host)),
        )
        .arg(
            Arg::new("port")
                .long("port")
                .value_name("PORT")
                .value_parser(value_parser!(u16))
                .help(format!("Port to bind (default: {})", defaults.port)),
        )
        .get_matches();

    let host = matches
        .get_one::<String>("host")
        .map(String::as_str)
        .unwrap_or(&defaults.host);
    let port = matches
        .get_one::<u16>("port")
        .copied()
        .unwrap_or(defaults.port);
    let app = match system_router(daemon_executable()) {
        Ok(app) => app,
        Err(error) => {
            eprintln!("error: {error}");
            return ExitCode::FAILURE;
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: could not create HTTP runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    let result = runtime.block_on(async {
        let listener = bind_listener(host, port).await?;
        serve_configured(listener, app, host, port).await
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn daemon_executable() -> PathBuf {
    if let Some(path) = std::env::var_os("DUNKINGSHEEP_BIN").filter(|path| !path.is_empty()) {
        return PathBuf::from(path);
    }
    let current = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("dunkingsheep"));
    current
        .parent()
        .map(|directory| directory.join("dunkingsheep"))
        .filter(|path| path.is_file())
        .unwrap_or_else(|| PathBuf::from("dunkingsheep"))
}
