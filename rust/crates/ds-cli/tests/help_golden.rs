//! `--help` parity with argparse (t1220): the renderer against `tests/help/*.txt`, which
//! `tools/gen_cli_rs.py --write` produces from `format_help()` at `COLUMNS=80`.

use std::fs;
use std::path::PathBuf;

use ds_cli::cli_table::ROOT;
use ds_cli::render::help_for;

#[test]
fn every_help_matches_argparse_byte_for_byte() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/help");
    let mut checked = 0;
    let mut failures = Vec::new();
    let mut names: Vec<_> = fs::read_dir(&dir)
        .expect("tests/help")
        .map(|e| e.expect("entry").path())
        .collect();
    names.sort();
    for path in names {
        let stem = path
            .file_stem()
            .expect("stem")
            .to_string_lossy()
            .to_string();
        let words: Vec<&str> = if stem == "root" {
            vec![]
        } else {
            stem.split("__").collect()
        };
        let want = fs::read_to_string(&path).expect("golden");
        let got =
            help_for(&ROOT, &words, 80).unwrap_or_else(|| format!("<no such command {stem}>"));
        checked += 1;
        if got != want {
            failures.push(stem);
        }
    }
    assert!(checked >= 174, "only {checked} goldens");
    assert!(
        failures.is_empty(),
        "{} of {checked} differ: {:?}",
        failures.len(),
        &failures[..failures.len().min(25)]
    );
}

/// `--help` on the real parser prints argparse's text: the command path is recovered from
/// clap's own usage line, including through nested sub-commands and an alias.
#[test]
fn help_flag_resolves_the_command_path_from_clap() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/help");
    let cases: [(&[&str], &str); 3] = [
        (&["dunkingsheep", "--help"], "root"),
        (&["dunkingsheep", "update", "--help"], "update"),
        (&["dunkingsheep", "set", "-h"], "update"), // `set` is update's alias
    ];
    for (argv, golden) in cases {
        let error = ds_cli::args::try_parse(argv.iter().copied()).expect_err("help is an Err");
        let got = ds_cli::args::argparse_help(&error.to_string()).expect("path recovered");
        let want = fs::read_to_string(dir.join(format!("{golden}.txt"))).expect("golden");
        assert_eq!(want, got, "{argv:?}");
    }
}
