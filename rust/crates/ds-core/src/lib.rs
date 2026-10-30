//! Dunking Sheep, in Rust: the library the daemon, the CLI and the MCP server share.
//!
//! Each module is added by the task that names it (docs/rust/TASKS.md) and ports the
//! Python file 1:1 (docs/rust/RUST_PORT.md section 0). Nothing here may be a stub that
//! pretends a port is done - the differential harness (T01) is the judge, and a green
//! build is not evidence of behaviour.
//!
//! The Python tree stays beside this one until the phase-3 cutover.

#![forbid(unsafe_code)]

/// The port manifest this crate is written against (docs/rust/MANIFEST.md).
pub const MANIFEST: &str = "docs/rust/MANIFEST.md";

pub mod atomic_json;
pub mod clock;
pub mod config;
pub mod error;
pub mod lock;
pub mod retry;
pub mod roles;

pub use error::DunkError;
