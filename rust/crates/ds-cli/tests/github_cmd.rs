//! T44: `github wrappers` against a sandbox: a fake real `gh` on PATH, a throwaway global git
//! config (`GIT_CONFIG_GLOBAL`), a temp bin dir. One test fn: the environment is process-wide.
//! Expected behaviour was recorded from the real Python `dunk_github_wrappers_install` run in
//! the same kind of sandbox (result dict, file mode, resulting git config, idempotence).

use std::os::unix::fs::PermissionsExt;

use ds_cli::github_cmd::{render_wrappers, wrappers_install, GH_WRAPPER};

#[test]
fn the_wrapper_install_matches_the_pythons_behaviour_in_a_sandbox() {
    let t = tempfile::tempdir().unwrap();
    let real_dir = t.path().join("realbin");
    std::fs::create_dir_all(&real_dir).unwrap();
    let real = real_dir.join("gh");
    std::fs::write(&real, "#!/bin/sh\necho real\n").unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
    let bin_dir = t.path().join("bin");
    let cfg = t.path().join("gitconfig");
    std::fs::write(
        &cfg,
        "[credential \"https://github.com\"]\n\thelper = !gh auth git-credential\n",
    )
    .unwrap();
    std::env::set_var("GIT_CONFIG_GLOBAL", &cfg);
    let path = format!("{}:/usr/bin:/bin", real_dir.display());

    let result = wrappers_install(bin_dir.to_str().unwrap(), &path).expect("install");
    assert_eq!(bin_dir.join("gh").display().to_string(), result.gh_wrapper);
    assert_eq!(real.display().to_string(), result.real_gh);
    assert_eq!("!dunkingsheep git-credential", result.git_credential_helper);
    assert_eq!(
        vec!["!dunkingsheep git-credential", "!gh auth git-credential"],
        result.helpers
    );

    // The script is the Python's GH_WRAPPER with the real gh substituted, and executable.
    let script = std::fs::read_to_string(bin_dir.join("gh")).unwrap();
    assert_eq!(
        GH_WRAPPER.replace("__REAL_GH__", &real.display().to_string()),
        script
    );
    assert!(GH_WRAPPER.starts_with("#!/bin/sh\n# dunkingsheep gh wrapper:"));
    assert!(!script.contains("__REAL_GH__"));
    assert_ne!(
        0,
        std::fs::metadata(bin_dir.join("gh"))
            .unwrap()
            .permissions()
            .mode()
            & 0o111
    );
    let syntax = std::process::Command::new("sh")
        .arg("-n")
        .arg(bin_dir.join("gh"))
        .output()
        .unwrap();
    assert!(
        syntax.status.success(),
        "{}",
        String::from_utf8_lossy(&syntax.stderr)
    );

    // Ours first, theirs kept behind it (t599), as in the Python's resulting config.
    let config = std::fs::read_to_string(&cfg).unwrap();
    assert_eq!(
        "[credential \"https://github.com\"]\n\thelper = !dunkingsheep git-credential\n\thelper = !gh auth git-credential\n",
        config
    );

    // Idempotent: a second install keeps the same helpers, and skips OUR OWN wrapper when
    // looking for the real gh (bin_dir is first on PATH here).
    let again_path = format!("{}:{path}", bin_dir.display());
    let again = wrappers_install(bin_dir.to_str().unwrap(), &again_path).expect("second install");
    assert_eq!(result.helpers, again.helpers);
    assert_eq!(real.display().to_string(), again.real_gh);

    // The printer: `{key:<20} {value}`, helpers as a Python list.
    let text = render_wrappers(&again);
    assert!(
        text.contains(
            "helpers              ['!dunkingsheep git-credential', '!gh auth git-credential']\n"
        ),
        "{text}"
    );
    assert!(text.starts_with("gh_wrapper           "), "{text}");

    // No real gh at all: the Python's SystemExit text.
    let error =
        wrappers_install(t.path().join("bin2").to_str().unwrap(), "/nonexistent-dir").unwrap_err();
    assert_eq!("no real `gh` found on PATH to wrap", error);
}
