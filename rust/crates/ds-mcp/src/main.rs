//! `dunkingsheep-mcp`: the MCP server over stdio (also reachable as `dunkingsheep mcp`).

use std::io::{stdin, stdout};
use std::time::Duration;

use ds_core::client::{connect_or_start_system, ClientError};
use ds_mcp::{Caller, McpServer, SERVER_VERSION};

/// `dunk_client.START_TIMEOUT_S`.
const START_TIMEOUT: Duration = Duration::from_secs(8);

fn main() {
    eprintln!(
        "dunkingsheep MCP server {SERVER_VERSION} starting (self pane {})",
        std::env::var("HERDR_PANE_ID").unwrap_or_else(|_| "None".to_string())
    );
    // The daemon is started by the `dunkingsheep` launcher (`dunkingsheep serve`), never by this
    // binary: it would spawn another MCP server. No launcher found means no auto-start.
    let launcher = find_launcher();
    let factory = Box::new(move || -> Result<Box<dyn Caller>, ClientError> {
        let (exe, start) = match &launcher {
            Some(path) => (path.clone(), true),
            None => (std::path::PathBuf::from("dunkingsheep"), false),
        };
        let client = connect_or_start_system(None, None, start, START_TIMEOUT, &exe)
            .map_err(ClientError::Dunk)?;
        Ok(Box::new(client))
    });
    let mut server = McpServer::new(factory);
    if let Err(error) = server.serve_forever(&mut stdin().lock(), &mut stdout().lock()) {
        eprintln!("dunkingsheep MCP server: {error}");
        std::process::exit(1);
    }
}

/// `DUNKINGSHEEP_BIN`, else `dunkingsheep` beside this binary, else on `PATH`.
fn find_launcher() -> Option<std::path::PathBuf> {
    if let Some(explicit) = std::env::var_os("DUNKINGSHEEP_BIN").filter(|v| !v.is_empty()) {
        return Some(explicit.into());
    }
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("dunkingsheep")))
        .filter(|path| path.is_file());
    beside.or_else(|| {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|dir| dir.join("dunkingsheep"))
                .find(|path| path.is_file())
        })
    })
}
