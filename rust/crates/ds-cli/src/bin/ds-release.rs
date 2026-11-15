//! Operator helper for tools/rust_rollout.sh --packet. Not a daemon/API verb.
use clap::{Arg, ArgAction, Command};
use std::{path::PathBuf, process::ExitCode};

fn main() -> ExitCode {
    let args = Command::new("ds-release")
        .about("Reviewed three-unit release packet; dry-run by default")
        .arg(
            Arg::new("action")
                .required(true)
                .value_parser(["plan", "activate", "verify", "rollback"]),
        )
        .arg(
            Arg::new("packet")
                .long("packet")
                .required(true)
                .value_parser(clap::value_parser!(PathBuf)),
        )
        .arg(Arg::new("digest").long("packet-sha").required(true))
        .arg(Arg::new("apply").long("apply").action(ArgAction::SetTrue))
        .get_matches();
    match ds_core::release_packet::execute(
        args.get_one::<PathBuf>("packet").unwrap(),
        args.get_one::<String>("digest").unwrap(),
        args.get_one::<String>("action").unwrap(),
        args.get_flag("apply"),
    ) {
        Ok(value) => {
            println!("{value}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("release packet refused/failed: {error}");
            ExitCode::FAILURE
        }
    }
}
