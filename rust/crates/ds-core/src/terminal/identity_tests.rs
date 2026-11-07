//! t3368 tests: the caller-binding listing carries no free-text field, and a malformed listing
//! is refused rather than read.

use super::*;

#[test]
fn the_identity_format_carries_only_the_pane_id_and_the_root_pid() {
    let format = identity_format();
    let separator = pane_separator();
    assert_eq!(
        format,
        format!("#{{pane_id}}{separator}#{{pane_pid}}"),
        "exactly the pane id and the root pid, in that order"
    );
    for free_text in [
        "window_name",
        "pane_title",
        "pane_current_path",
        "pane_current_command",
        "session_name",
    ] {
        assert!(
            !format.contains(free_text),
            "{free_text} is free text a same-user process can set"
        );
    }
}

#[test]
fn a_well_formed_listing_reads_each_pane_id_and_pid() {
    let separator = pane_separator();
    let stdout = format!("%1{separator}4321\n%2{separator}\n");
    assert_eq!(
        parse_pane_identities(&stdout).unwrap(),
        vec![("%1".to_string(), Some(4321)), ("%2".to_string(), None)]
    );
}

#[test]
fn a_line_with_extra_fields_refuses_the_listing() {
    let separator = pane_separator();
    // A name carrying the separator would add fields: the listing is refused, not misread.
    let stdout = format!("%1{separator}window{separator}4321\n");
    let error = parse_pane_identities(&stdout).unwrap_err();
    assert!(
        error.to_string().contains("3 fields"),
        "the refusal names the field count: {error}"
    );
}

#[test]
fn a_pid_that_is_not_a_number_refuses_the_listing() {
    let separator = pane_separator();
    let stdout = format!("%1{separator}not-a-pid\n");
    assert!(parse_pane_identities(&stdout).is_err());
}
