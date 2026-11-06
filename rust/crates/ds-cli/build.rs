//! Build-time code identity (t3457).
//!
//! The daemon's `status.code_hash` used to be the sha256 of the running executable, read in
//! full at startup: ~95 s for a 275 MB debug build, before the socket was bound. The hash now
//! comes from here: a digest of the Rust source tree this binary is built from, in the same
//! construction as the Python `dunk_core.code_identity` (a sorted `{path: sha256(bytes)}` map,
//! digested again, first 12 hex), plus the toolchain, profile and target so that the same
//! sources built differently read as different code. It is exported as `DS_CODE_HASH`, read with
//! `option_env!` in `main.rs` and handed to ds-core (`flock::status::embed_code_hash`).
//!
//! Hashed: every crate's `src/` tree, `build.rs` and `Cargo.toml` under `rust/crates/`, plus the
//! workspace `Cargo.toml` and `Cargo.lock`. `target/` is never read. When the workspace is not
//! there (a build outside the repo), nothing is exported and the daemon falls back to hashing
//! its binary in the background.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn main() {
    let manifest_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR"));
    let workspace = manifest_dir
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| manifest_dir.clone());
    match code_hash(&workspace) {
        Ok(hash) => println!("cargo:rustc-env=DS_CODE_HASH={hash}"),
        Err(error) => println!("cargo:warning=DS_CODE_HASH not embedded ({error}); the daemon hashes its binary in the background instead"),
    }
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// Every regular file below `dir`, sorted, recursing into subdirectories.
fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|error| format!("{}: {error}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            walk(&path, out)?;
        } else if path.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

fn code_hash(workspace: &Path) -> Result<String, String> {
    let crates = workspace.join("crates");
    let mut crate_dirs: Vec<PathBuf> = std::fs::read_dir(&crates)
        .map_err(|error| format!("{}: {error}", crates.display()))?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_dir())
        .collect();
    crate_dirs.sort();
    let mut inputs = vec![workspace.join("Cargo.toml"), workspace.join("Cargo.lock")];
    for dir in crate_dirs {
        for name in ["Cargo.toml", "build.rs"] {
            let path = dir.join(name);
            if path.is_file() {
                inputs.push(path);
            }
        }
        let src = dir.join("src");
        if src.is_dir() {
            // A directory here makes cargo re-run this script when anything below it changes.
            println!("cargo:rerun-if-changed={}", src.display());
            walk(&src, &mut inputs)?;
        }
    }
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    for path in &inputs {
        println!("cargo:rerun-if-changed={}", path.display());
        let bytes = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
        let relative = path
            .strip_prefix(workspace)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        files.insert(relative, hex(Sha256::digest(&bytes)));
    }
    if files.is_empty() {
        return Err("no source files found".to_string());
    }
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());
    let rustc_version = std::process::Command::new(&rustc)
        .arg("-V")
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .unwrap_or_default();
    files.insert("<rustc>".to_string(), rustc_version);
    files.insert(
        "<profile>".to_string(),
        std::env::var("PROFILE").unwrap_or_default(),
    );
    files.insert(
        "<target>".to_string(),
        std::env::var("TARGET").unwrap_or_default(),
    );
    let mut hasher = Sha256::new();
    for (name, digest) in &files {
        hasher.update(name.as_bytes());
        hasher.update(b"\0");
        hasher.update(digest.as_bytes());
        hasher.update(b"\n");
    }
    Ok(hex(hasher.finalize())[..12].to_string())
}
