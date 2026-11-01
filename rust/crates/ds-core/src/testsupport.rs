//! Test fixtures that clean up after themselves (t1490).
//!
//! The tests used to build their directories with
//! `std::env::temp_dir().join(format!("ds-<tag>-{}", std::process::id()))` and, at best, removed
//! the directory at the START of the test. Nothing removed it at the end, and the pid differs on
//! every `cargo test` run, so each run left a fresh set behind: 31,557 directories and 3.8 GB
//! filled the host's tmpfs. A `tempfile::TempDir` removes itself on drop; [`Scratch`] keeps one
//! alive next to the value that lives in it, so a helper can return the store (or a path) and the
//! directory goes away when the test's last use of it does, without touching the call sites.

use std::ops::{Deref, DerefMut};
use std::path::Path;

/// A fresh directory named `ds-<tag>-XXXXXX` under the system temp dir, removed on drop.
pub fn dir(tag: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("ds-{tag}-"))
        .tempdir()
        .expect("create a scratch directory")
}

/// A value together with the directory it lives in. The value is dropped FIRST (a store's
/// connection closes before its files are removed), then the directory.
pub struct Scratch<T> {
    value: T,
    dir: tempfile::TempDir,
}

impl<T> Scratch<T> {
    pub fn new(value: T, dir: tempfile::TempDir) -> Scratch<T> {
        Scratch { value, dir }
    }

    /// The directory the value lives in.
    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    /// Replace the value but keep the directory alive with the new one: a helper that builds
    /// something in a `Scratch` directory and hands back THAT, not the directory's path.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Scratch<U> {
        Scratch {
            value: f(self.value),
            dir: self.dir,
        }
    }
}

impl<T> Deref for Scratch<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> DerefMut for Scratch<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scratch_dir_is_removed_when_it_drops() {
        let path = {
            let scratch = Scratch::new(7_u32, dir("testsupport"));
            assert_eq!(7, *scratch);
            assert!(scratch.dir().is_dir());
            assert!(scratch
                .dir()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("ds-testsupport-"));
            scratch.dir().to_path_buf()
        };
        assert!(!path.exists(), "the directory outlived its owner");
    }

    #[test]
    fn map_keeps_the_directory_alive_with_the_new_value() {
        // `staffed()` in roles_gate_tests built a Flock inside a Scratch directory and returned
        // only the Flock: the directory dropped at the return and the roles file was gone.
        let scratch = Scratch::new(5_u32, dir("testsupport-map"));
        let path = scratch.dir().to_path_buf();
        let mapped = scratch.map(|n| n * 2);
        assert_eq!(10, *mapped);
        assert!(path.is_dir(), "the directory must survive the map");
        drop(mapped);
        assert!(!path.exists());
    }

    #[test]
    fn the_value_drops_before_the_directory() {
        struct Probe(std::path::PathBuf, std::rc::Rc<std::cell::Cell<bool>>);
        impl Drop for Probe {
            fn drop(&mut self) {
                self.1.set(self.0.is_dir());
            }
        }
        let seen = std::rc::Rc::new(std::cell::Cell::new(false));
        let d = dir("testsupport-order");
        let probe = Probe(d.path().to_path_buf(), seen.clone());
        drop(Scratch::new(probe, d));
        assert!(
            seen.get(),
            "the directory was already gone when the value dropped"
        );
    }
}
