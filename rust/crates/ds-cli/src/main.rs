//! `dunkingsheep` (Rust port). Parse with the generated argparse tree, then run the commands
//! whose bodies exist (`dispatch::run`); the rest answer "parsed but not ported yet" with
//! exit 1 instead of pretending.

use std::io::Write;
use std::process::ExitCode;

fn main() -> ExitCode {
    // `--help` / `--version` exit 0 and a usage error exits 2, as argparse does.
    let namespace = ds_cli::args::parse_or_exit(std::env::args_os());
    let outcome = ds_cli::dispatch::run(&namespace);
    let _ = std::io::stdout().write_all(outcome.stdout.as_bytes());
    let _ = std::io::stderr().write_all(outcome.stderr.as_bytes());
    ExitCode::from(outcome.code.clamp(0, 255) as u8)
}
