//! The generated `--version` text and the generated release identity constants come from one
//! run of `tools/gen_cli_rs.py`; this pins them together so the daemon's `status` (which
//! reports the constants) can never disagree with what the CLI prints (t3463).

use ds_core::release_identity::{CODE_HASH, VERSION};

#[test]
fn cli_version_text_is_the_release_identity() {
    let text = ds_cli::cli_table::ROOT
        .args
        .iter()
        .find(|arg| arg.dest == "version")
        .and_then(|arg| arg.version)
        .expect("the generated CLI table carries the --version text");
    assert_eq!(format!("dunkingsheep {VERSION} (code {CODE_HASH})"), text);
    assert!(
        CODE_HASH == "unknown"
            || (CODE_HASH.len() == 12 && CODE_HASH.bytes().all(|b| b.is_ascii_hexdigit())),
        "{CODE_HASH}"
    );
}
