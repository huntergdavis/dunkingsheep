//! Install a native Codex hook into an explicitly selected, unconfigured home.
//! Existing hooks and trusted configuration are never overwritten.
use serde_json::json;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const WRAPPER: &str = include_str!("../../../../../integrations/codex/dunkingsheep-turn.sh");

fn quote(path: &Path) -> io::Result<String> {
    let text = path
        .to_str()
        .ok_or_else(|| io::Error::other("home path is not UTF-8"))?;
    Ok(format!("'{}'", text.replace('\'', "'\\''")))
}

fn write_new(path: &Path, bytes: &[u8], mode: u32) -> io::Result<(u64, u64)> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    let identity = file.metadata().map(|m| (m.dev(), m.ino()))?;
    if let Err(error) = file.write_all(bytes).and_then(|()| file.sync_all()) {
        if fs::metadata(path).is_ok_and(|m| (m.dev(), m.ino()) == identity) {
            let _ = fs::remove_file(path);
        }
        return Err(error);
    }
    Ok(identity)
}

fn install_with<F>(home: &Path, binary: &Path, mut writer: F) -> io::Result<()>
where
    F: FnMut(&Path, &[u8], u32) -> io::Result<(u64, u64)>,
{
    if !home.is_absolute() || fs::canonicalize(home)? != home || !home.is_dir() {
        return Err(io::Error::other(
            "home must be an existing canonical absolute directory",
        ));
    }
    // Linux /proc/self is owned by the effective UID of this process.
    if fs::metadata(home)?.uid() != fs::metadata("/proc/self")?.uid() {
        return Err(io::Error::other("home must be owned by the caller"));
    }
    if fs::metadata(home)?.permissions().mode() & 0o022 != 0 {
        return Err(io::Error::other(
            "home must not be writable by group or others",
        ));
    }
    let meta = fs::symlink_metadata(binary)?;
    if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
        return Err(io::Error::other(
            "native binary must be a regular executable",
        ));
    }
    let bytes = fs::read(binary)?;
    let wrapper = home.join("dunkingsheep-turn.sh");
    let native = home.join("dunk-codex-turn");
    let hooks = home.join("hooks.json");
    for path in [&wrapper, &native, &hooks] {
        if path.symlink_metadata().is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("refusing existing {}", path.display()),
            ));
        }
    }
    let script = quote(&wrapper)?;
    let doc = json!({"hooks": {
        "UserPromptSubmit": [{"hooks":[{"type":"command","command":format!("bash {script} start"),"timeout":15}]}],
        "Stop": [{"hooks":[{"type":"command","command":format!("bash {script} end"),"timeout":15}]}]
    }});
    let mut hook_json = serde_json::to_vec_pretty(&doc)?;
    hook_json.push(b'\n');
    let mut created: Vec<(PathBuf, (u64, u64))> = Vec::new();
    let result = (|| {
        for (path, content, mode) in [
            (&native, bytes.as_slice(), 0o700),
            (&wrapper, WRAPPER.as_bytes(), 0o700),
            (&hooks, hook_json.as_slice(), 0o600),
        ] {
            let identity = writer(path, content, mode)?;
            created.push((path.clone(), identity));
        }
        Ok(())
    })();
    if result.is_err() {
        for (path, id) in created.iter().rev() {
            if fs::symlink_metadata(path).is_ok_and(|m| (m.dev(), m.ino()) == *id) {
                let _ = fs::remove_file(path);
            }
        }
    }
    result
}
fn install(home: &Path, binary: &Path) -> io::Result<()> {
    install_with(home, binary, write_new)
}

fn main() {
    let mut args = std::env::args_os().skip(1);
    let (Some(home), Some(binary), None) = (args.next(), args.next(), args.next()) else {
        eprintln!("usage: dunk-codex-install ABSOLUTE_HOME NATIVE_BINARY");
        std::process::exit(2);
    };
    if let Err(error) = install(Path::new(&home), Path::new(&binary)) {
        eprintln!("installation refused: {error}");
        std::process::exit(1);
    }
    println!("Installed native Codex hook in {}; trust the new hooks through Codex and launch with --no-daemon.",Path::new(&home).display());
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failure_cleans_only_created_files() {
        let d = tempfile::tempdir().unwrap();
        let home = d.path().join("home");
        fs::create_dir(&home).unwrap();
        let binary = d.path().join("source");
        fs::write(&binary, b"native").unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(home.join("sentinel"), b"keep").unwrap();
        let mut calls = 0;
        let result = install_with(&home, &binary, |path, bytes, mode| {
            calls += 1;
            if calls == 2 {
                return Err(io::Error::other("injected failure"));
            }
            write_new(path, bytes, mode)
        });
        assert!(result.is_err());
        assert!(!home.join("dunk-codex-turn").exists());
        assert!(!home.join("dunkingsheep-turn.sh").exists());
        assert!(!home.join("hooks.json").exists());
        assert_eq!(fs::read(home.join("sentinel")).unwrap(), b"keep");
    }
}
