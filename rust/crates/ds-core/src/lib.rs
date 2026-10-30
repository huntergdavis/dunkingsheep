//! Dunking Sheep, in Rust: the library the daemon, the CLI and the MCP server share.
//!
//! This crate starts empty on purpose (docs/rust/TASKS.md T00): the workspace and
//! the build are the deliverable of T00, and each later task adds one module from
//! `docs/rust/MANIFEST.md` ("Inventory") together with the Python tests it ports.
//! Nothing here may be a stub that pretends a port is done - the differential
//! harness (T01) is the judge, and a green build is not evidence of behaviour.
//!
//! The Python tree stays beside this one until the phase-3 cutover
//! (docs/rust/RUST_PORT.md sections 2 and 8).

#![forbid(unsafe_code)]

/// The port manifest this crate is written against (docs/rust/MANIFEST.md).
///
/// T09 keeps this in step with the tree; a build that cannot name its manifest is
/// a port with no reference point.
pub const MANIFEST: &str = "docs/rust/MANIFEST.md";
