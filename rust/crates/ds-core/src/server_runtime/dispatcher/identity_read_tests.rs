//! t3368 (review of #810): a failed identity read refuses the claim, except in the deliberate
//! no-backend trial. Decided on the pure function, so no test touches the environment.

use super::{failed_read_root, DunkError};

#[test]
fn a_failed_read_refuses_outside_the_trial() {
    let failed = DunkError::Refused("tmux listing refused".into());
    assert_eq!(
        failed_read_root(failed, false).unwrap_err().to_string(),
        "tmux listing refused",
        "the binding is refused on the read error"
    );
}

#[test]
fn a_failed_read_stands_only_in_the_no_backend_trial() {
    let failed = DunkError::Refused("no tmux binary".into());
    assert_eq!(
        failed_read_root(failed, true).unwrap(),
        Some(None),
        "the trial lists the pane with no pid, so the claim stands"
    );
}
