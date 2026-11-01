//! Project context reads use the same absolute store key as the daemon's write path.

use std::path::Path;
use std::process::{Command, Output};

use ds_core::context::ledger::Ledger;
use ds_core::context::store::{AddArgs, Store};

fn add_ground_truth(root: &Path, global: bool, slug: &str, body: &str, ledger: &mut Ledger) {
    Store::new(Some(root), global)
        .add(
            body,
            &AddArgs {
                cls: "ground-truth",
                subject: "path-regression",
                type_: "rule",
                authority: Some("owner"),
                source: "isolated fixture",
                stated: Some("2026-11-01"),
                recheck_after: Some("365d"),
                slug: Some(slug),
                ..AddArgs::default()
            },
            Some(ledger),
        )
        .unwrap();
}

fn run_cli(project: &Path, config: &Path, home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dunkingsheep"))
        .arg("--no-start-daemon")
        .args(args)
        .current_dir(project)
        .env("DUNKINGSHEEP_DIR", config)
        .env(
            "DUNKINGSHEEP_CONTEXT_LEDGER",
            config.join("context-ledger.db"),
        )
        .env("HOME", home)
        .env_remove("DUNKINGSHEEP_SOCKET")
        .env_remove("DUNKINGSHEEP_SERVICE_TOKEN")
        .env_remove("DUNKINGSHEEP_SERVICE_TOKEN_FILE")
        .output()
        .expect("run isolated Rust CLI")
}

#[test]
fn project_relative_digest_and_check_use_the_absolute_ledger_key() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config");
    std::fs::create_dir_all(&config).unwrap();

    let project = temp.path().join("one");
    let home = temp.path().join("home");
    let store_root = project.join(".dunkingsheep");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let ledger_path = config.join("context-ledger.db");
    let mut ledger = Ledger::new(Some(&ledger_path));
    add_ground_truth(
        &store_root,
        false,
        "path-regression",
        "Original trusted rule.",
        &mut ledger,
    );

    let digest = run_cli(
        &project,
        &config,
        &home,
        &["context", "digest", "--project"],
    );
    assert!(
        digest.status.success(),
        "{}",
        String::from_utf8_lossy(&digest.stderr)
    );
    let digest_text = String::from_utf8(digest.stdout).unwrap();
    assert!(digest_text.contains("Ground truths (1)"), "{digest_text}");
    assert!(
        digest_text.contains("Original trusted rule."),
        "{digest_text}"
    );

    let check = run_cli(&project, &config, &home, &["context", "check", "--project"]);
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    let check_text = String::from_utf8(check.stdout).unwrap();
    assert!(check_text.contains("entries 1, refusals 0"), "{check_text}");

    let entry = store_root.join("ground-truth/path-regression.md");
    let original = std::fs::read_to_string(&entry).unwrap();
    std::fs::write(
        &entry,
        original.replace("Original trusted rule.", "Edited rule."),
    )
    .unwrap();
    let modified_digest = run_cli(
        &project,
        &config,
        &home,
        &["context", "digest", "--project"],
    );
    let modified_text = String::from_utf8(modified_digest.stdout).unwrap();
    assert!(modified_text.contains("path-regression"), "{modified_text}");
    assert!(modified_text.contains("NOT injected"), "{modified_text}");
    let modified_check = run_cli(&project, &config, &home, &["context", "check", "--project"]);
    let modified_check_text = String::from_utf8(modified_check.stdout).unwrap();
    assert!(
        modified_check_text.contains("entries 1, refusals 1"),
        "{modified_check_text}"
    );

    // A different project has the same store basename but cannot borrow the first ledger row.
    let other_project = temp.path().join("two");
    let other_store = other_project.join(".dunkingsheep");
    std::fs::create_dir_all(other_store.join("ground-truth")).unwrap();
    std::fs::write(
        other_store.join("ground-truth/path-regression.md"),
        original,
    )
    .unwrap();
    let other_digest = run_cli(
        &other_project,
        &config,
        &home,
        &["context", "digest", "--project"],
    );
    let other_text = String::from_utf8(other_digest.stdout).unwrap();
    assert!(other_text.contains("Ground truths (0)"), "{other_text}");
    assert!(other_text.contains("NOT injected"), "{other_text}");
    let other_check = run_cli(
        &other_project,
        &config,
        &home,
        &["context", "check", "--project"],
    );
    let other_check_text = String::from_utf8(other_check.stdout).unwrap();
    assert!(
        other_check_text.contains("entries 1, refusals 1"),
        "{other_check_text}"
    );

    // Absolute-path resolution for project reads must not redirect a global-store request.
    add_ground_truth(
        &config.join("context"),
        true,
        "global-path-regression",
        "Global trusted rule.",
        &mut ledger,
    );
    let global_digest = run_cli(&project, &config, &home, &["context", "digest", "--global"]);
    let global_text = String::from_utf8(global_digest.stdout).unwrap();
    assert!(global_text.contains("Ground truths (1)"), "{global_text}");
    assert!(
        global_text.contains("Global trusted rule."),
        "{global_text}"
    );
    let global_check = run_cli(&project, &config, &home, &["context", "check", "--global"]);
    let global_check_text = String::from_utf8(global_check.stdout).unwrap();
    assert!(
        global_check_text.contains("entries 1, refusals 0"),
        "{global_check_text}"
    );
}

#[test]
fn context_check_expands_tilde_in_project_path_output() {
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join("config");
    let project = temp.path().join("project");
    let home = temp.path().join("home");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(home.join("store/ground-truth")).unwrap();

    let output = run_cli(
        &project,
        &config,
        &home,
        &["context", "check", "--path", "~/store"],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(
        text.contains(&format!("project ({}):", home.join("store").display())),
        "{text}"
    );
    assert!(!text.contains("~/store"), "{text}");
}
