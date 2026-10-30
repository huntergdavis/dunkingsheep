//! Dunking Sheep, in Rust: the library the daemon, the CLI and the MCP server share.
//!
//! Ported module by module from `docs/rust/MANIFEST.md` ("Inventory"), each with
//! the Python tests it names. T02 delivers the foundation every later task stands
//! on: [`error`], [`config`], [`clock`], [`atomic_json`], [`lock`], [`retry`].
//!
//! Nothing here may be a stub that pretends a port is done - the differential
//! harness (T01) is the judge, and a green build is not evidence of behaviour.
//!
//! The Python tree stays beside this one until the phase-3 cutover
//! (docs/rust/RUST_PORT.md sections 2 and 8).

#![forbid(unsafe_code)]

pub mod atomic_json;
pub mod clock;
pub mod config;
pub mod db;
pub mod error;
pub mod inbox;
pub mod lock;
pub mod memory;
pub mod proc;
pub mod retry;
pub mod stuck;
pub mod tasks;
pub mod terminal;

pub use error::{refusal, DunkError};

/// The port manifest this crate is written against (docs/rust/MANIFEST.md).
///
/// T09 keeps this in step with the tree; a build that cannot name its manifest is
/// a port with no reference point.
pub const MANIFEST: &str = "docs/rust/MANIFEST.md";
