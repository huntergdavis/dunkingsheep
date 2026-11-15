//! Operator helper for tools/rust_rollout.sh. Inventory is always read-only.
use clap::{Arg, ArgAction, ArgMatches, Command};
use std::{path::PathBuf, process::ExitCode};
fn command() -> Command {
    Command::new("ds-release")
        .about("Reviewed release packets and read-only inventory")
        .arg(Arg::new("action").required(true).value_parser([
            "plan",
            "activate",
            "verify",
            "rollback",
            "inventory",
        ]))
        .arg(
            Arg::new("packet")
                .long("packet")
                .required_if_eq_any([
                    ("action", "plan"),
                    ("action", "activate"),
                    ("action", "verify"),
                    ("action", "rollback"),
                ])
                .value_parser(clap::value_parser!(PathBuf)),
        )
        .arg(Arg::new("digest").long("packet-sha").required_if_eq_any([
            ("action", "plan"),
            ("action", "activate"),
            ("action", "verify"),
            ("action", "rollback"),
        ]))
        .arg(Arg::new("apply").long("apply").action(ArgAction::SetTrue))
        .arg(
            Arg::new("releases")
                .long("releases")
                .value_parser(clap::value_parser!(PathBuf)),
        )
        .arg(
            Arg::new("record-root")
                .long("record-root")
                .action(ArgAction::Append)
                .value_parser(clap::value_parser!(PathBuf)),
        )
        .arg(
            Arg::new("record")
                .long("record")
                .action(ArgAction::Append)
                .value_parser(clap::value_parser!(PathBuf)),
        )
        .arg(
            Arg::new("launcher")
                .long("launcher")
                .action(ArgAction::Append)
                .value_parser(clap::value_parser!(PathBuf)),
        )
        .arg(
            Arg::new("constraints")
                .long("constraints")
                .value_parser(clap::value_parser!(PathBuf)),
        )
        .arg(Arg::new("json").long("json").action(ArgAction::SetTrue))
}
fn validate(args: &ArgMatches) -> Result<(), String> {
    if args.get_one::<String>("action").unwrap() == "inventory" {
        if args.get_flag("apply") || args.contains_id("packet") || args.contains_id("digest") {
            return Err("inventory refuses --apply and packet operation flags".into());
        }
    } else {
        if !args.contains_id("packet") || !args.contains_id("digest") {
            return Err("packet operations require --packet and --packet-sha".into());
        }
        if [
            "releases",
            "record-root",
            "record",
            "launcher",
            "constraints",
        ]
        .iter()
        .any(|key| args.contains_id(key))
            || args.get_flag("json")
        {
            return Err("inventory flags cannot mix with packet operations".into());
        }
    }
    Ok(())
}
fn main() -> ExitCode {
    let args = command().get_matches();
    let result = (|| -> Result<String, String> {
        validate(&args)?;
        if args.get_one::<String>("action").unwrap() == "inventory" {
            let home = std::env::var_os("HOME").map(PathBuf::from);
            let releases = args
                .get_one::<PathBuf>("releases")
                .cloned()
                .or_else(|| std::env::var_os("RUST_RELEASES").map(PathBuf::from))
                .or_else(|| {
                    home.as_ref()
                        .map(|p| p.join(".local/share/dunkingsheep/releases"))
                })
                .ok_or("--releases or HOME is required")?;
            let paths = |name: &str| {
                args.get_many::<PathBuf>(name)
                    .map(|v| v.cloned().collect())
                    .unwrap_or_default()
            };
            let mut launchers: Vec<PathBuf> = home
                .as_ref()
                .map(|p| {
                    vec![
                        p.join(".local/bin/dunkingsheep"),
                        p.join(".local/bin/dunk-poll"),
                    ]
                })
                .unwrap_or_default();
            launchers.extend(paths("launcher"));
            launchers.sort();
            launchers.dedup();
            let report =
                ds_core::release_inventory::collect(&ds_core::release_inventory::Options {
                    releases,
                    record_roots: paths("record-root"),
                    records: paths("record"),
                    launchers,
                    constraints: args.get_one::<PathBuf>("constraints").cloned(),
                })?;
            return if args.get_flag("json") {
                serde_json::to_string_pretty(&report).map_err(|e| e.to_string())
            } else {
                Ok(ds_core::release_inventory::text(&report))
            };
        }
        ds_core::release_packet::execute(
            args.get_one::<PathBuf>("packet").unwrap(),
            args.get_one::<String>("digest").unwrap(),
            args.get_one::<String>("action").unwrap(),
            args.get_flag("apply"),
        )
    })();
    match result {
        Ok(value) => {
            println!("{value}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("release inventory/packet refused: {error}");
            ExitCode::FAILURE
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn release_inventory_refuses_apply_and_keeps_packet_arguments_required() {
        for words in [
            vec!["ds-release", "inventory", "--apply"],
            vec!["ds-release", "inventory", "--packet", "/x"],
            vec!["ds-release", "activate"],
            vec![
                "ds-release",
                "plan",
                "--packet",
                "/x",
                "--packet-sha",
                "x",
                "--record",
                "/y",
            ],
        ] {
            if let Ok(args) = command().try_get_matches_from(words) {
                assert!(validate(&args).is_err());
            }
        }
        assert!(validate(
            &command()
                .try_get_matches_from(["ds-release", "inventory", "--json", "--record-root", "/x"])
                .unwrap()
        )
        .is_ok());
        assert!(validate(
            &command()
                .try_get_matches_from(["ds-release", "plan", "--packet", "/x", "--packet-sha", "x"])
                .unwrap()
        )
        .is_ok());
    }
}
