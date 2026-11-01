# Rust CI gates (t1370)

`.github/workflows/rust.yml` runs on every pull request (no `paths:` filter on `pull_request`) and on pushes that touch `rust/**`.

| check name | blocking on main | what it does |
|---|---|---|
| `cargo build + test` | required | `cargo build` and `cargo test` of the whole workspace |
| `cargo fmt --check` | required | formatting |
| `cargo clippy (non-blocking until verified green)` | NOT yet | `cargo clippy --workspace --all-targets -- -D warnings`; blocking after t1384 fixes the existing lints |
| `scan` (Public-repo secret scan) | existing | the secret scan |

Why there is no `paths:` filter: a required check that never reports (a PR that touches no `rust/` file) blocks that PR forever when admins are enforced.

Before pushing a Rust change, run what CI runs, with the same flag, on every target:

    RUSTFLAGS="-D warnings" cargo build --manifest-path rust/Cargo.toml --workspace --all-targets
    cargo fmt --manifest-path rust/Cargo.toml --all -- --check

Main went red for 20+ runs in 2026-10/11 because six warnings-as-errors accumulated; the three that only appear when test targets compile (unused imports/variables in tests) are the ones a plain `cargo build` misses.
