//! Install the bundled OpenCode plugin without replacing existing files.
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const PLUGIN: &[u8] = include_bytes!("../../../../../integrations/opencode/dunkingsheep-turn.js");

fn private_dir(path: &Path, uid: u32) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.uid() != uid || meta.permissions().mode() & 0o022 != 0 {
        return Err(io::Error::other(format!(
            "{} must be a caller-owned directory not writable by group or others",
            path.display()
        )));
    }
    Ok(())
}

fn install(root: &Path) -> io::Result<()> {
    if !root.is_absolute() || fs::canonicalize(root)? != root {
        return Err(io::Error::other(
            "config root must be an existing canonical absolute directory",
        ));
    }
    let uid = fs::metadata("/proc/self")?.uid();
    private_dir(root, uid)?;
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
            private_dir(&dir, uid)?;
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
    let (Some(root), None) = (args.next(), args.next()) else {
        eprintln!("usage: dunk-opencode-install ABSOLUTE_CONFIG_ROOT");
        std::process::exit(2);
    };
    if let Err(error) = install(Path::new(&root)) {
        eprintln!("installation refused: {error}");
        std::process::exit(1);
    }
    println!(
        "Installed OpenCode plugin under {}; OpenCode loads plugins on startup.",
        Path::new(&root).display()
    );
}
