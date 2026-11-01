//! T44: the LOCAL context writes (`init`, `add|promote|supersede|retire --local`, `ledger adopt`)
//! through argv -> generated parser -> `dispatch::run`, replayed in order against a temp project and
//! compared with what the REAL Python CLI printed AND WROTE (`tools/gen_context_local_fixture.py`):
//! stdout / stderr / exit code per step, then every store file byte for byte. Today's date and the temp
//! dir are normalised to <TODAY> / <T> on both sides. One test fn: cwd and environment are
//! process-wide.

use ds_cli::args::try_parse;
use ds_cli::dispatch::run;
use serde_json::Value;

const FIXTURE: &str = include_str!("context_local.json");

fn norm(text: &str, base: &str, today: &str) -> String {
    text.replace(base, "<T>").replace(today, "<TODAY>")
}

#[test]
fn local_context_writes_print_and_write_what_the_python_did() {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture");
    let base = tempfile::tempdir().unwrap();
    let base_text = std::fs::canonicalize(base.path())
        .unwrap()
        .display()
        .to_string();
    let proj = base.path().join("proj");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(proj.join("body-c.md"), "Body C from a file.\n").unwrap();
    std::fs::write(proj.join("body-d.md"), "Body D supersedes B.\n").unwrap();
    std::env::set_var("DUNKINGSHEEP_DIR", base.path().join("cfg"));
    std::env::set_var("DUNKINGSHEEP_SELF", "fixture");
    std::env::remove_var("DUNKINGSHEEP_SUPERUSER");
    std::env::set_current_dir(&proj).unwrap();
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let base_for = |text: &str| {
        norm(
            &text.replace(&base.path().display().to_string(), &base_text),
            &base_text,
            &today,
        )
    };
    let mut bad = Vec::new();
    for step in fixture["steps"].as_array().unwrap() {
        match step["env"].as_object() {
            Some(env) if !env.is_empty() => {
                for (k, v) in env {
                    std::env::set_var(k, v.as_str().unwrap());
                }
            }
            _ => {}
        }
        let mut argv = vec!["dunkingsheep".to_string(), "context".to_string()];
        argv.extend(
            step["argv"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string()),
        );
        let ns = try_parse(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        let out = run(&ns);
        std::env::remove_var("DUNKINGSHEEP_SUPERUSER");
        let (got_out, got_err) = (base_for(&out.stdout), base_for(&out.stderr));
        let (want_out, want_err) = (
            step["stdout"].as_str().unwrap(),
            step["stderr"].as_str().unwrap(),
        );
        let want_code = step["code"].as_i64().unwrap() as i32;
        if out.code != want_code || got_out != want_out || got_err != want_err {
            bad.push(format!(
                "{argv:?}\n  got:  code {} out {got_out:?} err {got_err:?}\n  want: code {want_code} out {want_out:?} err {want_err:?}",
                out.code
            ));
        }
    }
    // The store the Rust wrote equals the one the Python wrote, file for file.
    let store = proj.join(".dunkingsheep");
    let mut got_files = std::collections::BTreeMap::new();
    fn walk(
        dir: &std::path::Path,
        root: &std::path::Path,
        out: &mut std::collections::BTreeMap<String, String>,
    ) {
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, root, out);
            } else if path.extension().is_some_and(|e| e == "md") {
                out.insert(
                    path.strip_prefix(root).unwrap().display().to_string(),
                    std::fs::read_to_string(&path).unwrap(),
                );
            }
        }
    }
    walk(&store, &store, &mut got_files);
    let want_files = fixture["files"].as_object().unwrap();
    let got_norm: std::collections::BTreeMap<String, String> = got_files
        .iter()
        .map(|(k, v)| (base_for(k), base_for(v)))
        .collect();
    let want_norm: std::collections::BTreeMap<String, String> = want_files
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
        .collect();
    if got_norm != want_norm {
        let keys: Vec<&String> = got_norm.keys().chain(want_norm.keys()).collect();
        for key in keys {
            if got_norm.get(key) != want_norm.get(key) {
                bad.push(format!(
                    "store file {key}:\n  got:  {:?}\n  want: {:?}",
                    got_norm.get(key),
                    want_norm.get(key)
                ));
            }
        }
    }
    bad.sort();
    bad.dedup();
    assert!(
        bad.is_empty(),
        "{} differences:\n{}",
        bad.len(),
        bad.join("\n")
    );
}
