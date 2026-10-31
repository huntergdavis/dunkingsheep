//! `dunkingsheep` (Rust port). T42: parse only. A parsed command is not executed yet; the
//! exit code says so instead of pretending (command bodies are T43 onwards).

use std::process::ExitCode;

fn main() -> ExitCode {
    // `--help` / `--version` exit 0 and a usage error exits 2, as argparse does.
    let namespace = ds_cli::args::parse_or_exit(std::env::args_os());
    let command = namespace
        .get("command")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    eprintln!("dunkingsheep (rust port): `{command}` is parsed but not ported yet");
    ExitCode::from(1)
}
