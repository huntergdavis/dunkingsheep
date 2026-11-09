//! Install the bundled OpenCode plugin without replacing existing files.
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const PLUGIN: &[u8] = include_bytes!("../../../../../integrations/opencode/dunkingsheep-turn.js");

fn checked_dir(path: &Path, uid: u32, trust_group: bool) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    let disallowed = if trust_group { 0o002 } else { 0o022 };
    if !meta.is_dir() || meta.uid() != uid || meta.permissions().mode() & disallowed != 0 {
        return Err(io::Error::other(format!(
            "{} must be caller-owned, not a symlink, and not writable by untrusted users",
            path.display()
        )));
    }
    Ok(())
}

fn install(root: &Path, trust_group: bool) -> io::Result<()> {
    if !root.is_absolute() || fs::canonicalize(root)? != root {
        return Err(io::Error::other(
            "config root must be an existing canonical absolute directory",
        ));
    }
    let uid = fs::metadata("/proc/self")?.uid();
    checked_dir(root, uid, trust_group)?;
    let mut created: Vec<(PathBuf, (u64, u64))> = Vec::new();
    let result = (|| {
        let mut dir = root.to_path_buf();
        for component in ["opencode", "plugins"] {
            dir.push(component);
            match DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => {
                    let meta = fs::symlink_metadata(&dir)?;
                    created.push((dir.clone(), (meta.dev(), meta.ino())));
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
                Err(e) => return Err(e),
            }
            checked_dir(&dir, uid, trust_group)?;
        }
        let target = dir.join("dunkingsheep-turn.js");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&target)?;
        let meta = file.metadata()?;
        let id = (meta.dev(), meta.ino());
        if let Err(error) = file.write_all(PLUGIN).and_then(|()| file.sync_all()) {
            if fs::symlink_metadata(&target).is_ok_and(|m| (m.dev(), m.ino()) == id) {
                let _ = fs::remove_file(&target);
            }
            return Err(error);
        }
        Ok(())
    })();
    if result.is_err() {
        for (path, id) in created.iter().rev() {
            if fs::symlink_metadata(path).is_ok_and(|m| (m.dev(), m.ino()) == *id) {
                let _ = fs::remove_dir(path);
            }
        }
    }
    result
}

fn main() {
    let mut args = std::env::args_os().skip(1);
    let first = args.next();
    let trust_group = first.as_deref() == Some(std::ffi::OsStr::new("--trust-group-writable"));
    let root = if trust_group { args.next() } else { first };
    let (Some(root), None) = (root, args.next()) else {
        eprintln!("usage: dunk-opencode-install [--trust-group-writable] ABSOLUTE_CONFIG_ROOT");
        std::process::exit(2);
    };
    if let Err(error) = install(Path::new(&root), trust_group) {
        eprintln!("installation refused: {error}");
        std::process::exit(1);
    }
    println!(
        "Installed OpenCode plugin under {}; OpenCode loads plugins on startup.",
        Path::new(&root).display()
    );
}
