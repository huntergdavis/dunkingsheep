//! Dunking Sheep core: the Rust port of the Python daemon.
//!
//! This crate is created EMPTY by T00 (docs/rust/TASKS.md) so that the workspace,
//! the toolchain and `cargo build`/`cargo clippy -D warnings`/`cargo fmt` are proven
//! before any module arrives. Every module is added by the task that names it, ported
//! from the Python file 1:1 (docs/rust/RUST_PORT.md section 0), with the same names:
//! `dunk_tasks.py::Tasks.claim_next` becomes `tasks::Tasks::claim_next`.
//!
//! Nothing is re-exported here yet. A re-export added before its module exists is a
//! claim about code that does not exist, which is the defect the port's own rules
//! (section 0.8) name.

/// The port's error type arrives with T02 (`error.rs`, RUST_PORT.md section 4.3).
///
/// It is not defined here on purpose: a placeholder enum would be a second definition
/// of the error surface, and the port must have exactly one.
pub const PORT_PHASE: u32 = 0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_workspace_builds_and_the_crate_is_reachable() {
        assert_eq!(0, PORT_PHASE);
    }
}
