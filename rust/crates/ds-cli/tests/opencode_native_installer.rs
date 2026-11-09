use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::process::Command;

fn run(root: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_dunk-opencode-install"))
        .arg(root)
        .output()
        .unwrap()
}

#[test]
fn installs_once_and_preserves_other_plugins() {
    let dir = tempfile::tempdir().unwrap();
    let plugins = dir.path().join("opencode/plugins");
    fs::create_dir_all(&plugins).unwrap();
    fs::set_permissions(
        dir.path().join("opencode"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    fs::set_permissions(&plugins, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(plugins.join("other.js"), b"keep").unwrap();
    let output = run(dir.path());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let target = plugins.join("dunkingsheep-turn.js");
    assert_eq!(
        fs::read(&target).unwrap(),
        include_bytes!("../../../../integrations/opencode/dunkingsheep-turn.js")
    );
    assert_eq!(fs::read(plugins.join("other.js")).unwrap(), b"keep");
    assert!(!run(dir.path()).status.success());
    assert_eq!(
        fs::read(&target).unwrap(),
        include_bytes!("../../../../integrations/opencode/dunkingsheep-turn.js")
    );
}

#[test]
fn creates_private_parents_and_refuses_symlinked_directories() {
    let dir = tempfile::tempdir().unwrap();
    assert!(run(dir.path()).status.success());
    let plugins = dir.path().join("opencode/plugins");
    assert_eq!(
        fs::metadata(&plugins).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let other = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    symlink(external.path(), other.path().join("opencode")).unwrap();
    assert!(!run(other.path()).status.success());
    assert!(!external.path().join("plugins").exists());
}
