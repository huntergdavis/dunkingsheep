//! Ports of `SuperuserStoreTests` (tests/test_dunk_superusers.py) by name, plus the t423
//! allowlist/locking behaviour the advance transaction relies on. The Python
//! `SuperuserGatingTests` need the daemon (Flock, T31+) and are NOT ported here.

use super::*;
use serde_json::json;

fn dir(tag: &str) -> crate::testsupport::Scratch<PathBuf> {
    let scratch = crate::testsupport::dir(&format!("su-{tag}"));
    crate::testsupport::Scratch::new(scratch.path().to_path_buf(), scratch)
}

const VP_PANE: &str = "w9:p8";

#[test]
fn test_defaults_are_the_vp_and_owner_and_file_is_private() {
    // The names come from the environment when set; pin the defaults for this test.
    std::env::remove_var("DUNKINGSHEEP_VP_NAME");
    std::env::remove_var("DUNKINGSHEEP_OWNER_NAME");
    std::env::remove_var("DUNKINGSHEEP_VP_PANE");
    std::env::remove_var("DUNKINGSHEEP_OWNER_PANE");
    let scratch = dir("defaults");
    let path = scratch.join("superusers.json");
    let store = Superusers::new(&path, true).unwrap();
    assert!(store.names.contains("VP of agentry"));
    assert!(store.names.contains("owner"));
    assert!(!store.empty());
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(0o600, mode);
}

#[test]
fn test_add_and_remove_panes() {
    let scratch = dir("addremove");
    let path = scratch.join("superusers.json");
    let mut store = Superusers::new(&path, true).unwrap();
    store.add(VP_PANE).unwrap();
    assert!(store.is_superuser(VP_PANE, None));
    assert!(store.remove(VP_PANE).unwrap());
    assert!(!store.is_superuser(VP_PANE, None));
}

#[test]
fn test_flat_list_and_explicit_empty() {
    let d = dir("flat");
    let path = d.join("superusers.json");
    std::fs::write(
        &path,
        json!({"superusers": ["w1:p1", "VP of agentry"]}).to_string(),
    )
    .unwrap();
    let store = Superusers::new(&path, true).unwrap();
    assert!(store.is_superuser("w1:p1", None));
    assert!(store.is_superuser("w1:p2", Some(&json!({"label": "VP of agentry"}))));
    let empty = Superusers::new(d.join("empty.json"), false).unwrap();
    empty.save().unwrap();
    assert!(empty.empty());
}

#[test]
fn a_roles_file_yields_its_owner_lanes_as_panes_and_no_names() {
    let (panes, names) = parse_allowlist(&json!({"lanes": {
        "%1": {"role": "owner"}, "%2": {"role": "worker"}, " %3 ": {"role": "owner"}, "%4": "x"}}))
    .unwrap();
    assert_eq!(BTreeSet::from(["%1".to_string(), "%3".to_string()]), panes);
    assert!(names.is_empty());
    // "panes" present beats "lanes".
    let (panes, _) =
        parse_allowlist(&json!({"lanes": {"%1": {"role": "owner"}}, "panes": ["%9"]})).unwrap();
    assert_eq!(BTreeSet::from(["%9".to_string()]), panes);
}

#[test]
fn a_non_object_allowlist_is_refused_with_the_pythons_text() {
    let error = parse_allowlist(&json!(["x"])).unwrap_err().to_string();
    assert!(
        error.contains("allowlist must contain a JSON object"),
        "{error}"
    );
}

#[test]
fn match_allowlist_matches_ids_exactly_and_names_by_any_label_case_insensitively() {
    let panes = BTreeSet::from(["w1:p1".to_string()]);
    let names = BTreeSet::from(["VP of Agentry".to_string()]);
    assert!(match_allowlist(&panes, &names, " w1:p1 ", None));
    assert!(!match_allowlist(&panes, &names, "", None));
    for field in ["label", "tab_label", "workspace_label"] {
        let info = json!({ field: "  vp OF agentry " });
        assert!(
            match_allowlist(&panes, &names, "%7", Some(&info)),
            "{field}"
        );
    }
    assert!(!match_allowlist(
        &panes,
        &names,
        "%7",
        Some(&json!({"label": "someone"}))
    ));
    assert!(!match_allowlist(
        &panes,
        &BTreeSet::new(),
        "%7",
        Some(&json!({"label": "VP of agentry"}))
    ));
}

#[test]
fn a_missing_file_grants_nothing_but_a_corrupt_one_is_an_error() {
    let d = dir("missing");
    let path = d.join("superusers.json");
    let (panes, names) = read_panes_locked(&path).unwrap();
    assert!(panes.is_empty() && names.is_empty());
    std::fs::write(&path, "{not json").unwrap();
    assert!(read_panes_locked(&path).is_err());
    assert!(locked_allowlist(&path, |_| Ok(())).is_err());
}

#[test]
fn the_file_allowlist_reads_under_a_shared_lock_that_blocks_a_writer_until_released() {
    let d = dir("locked");
    let path = d.join("superusers.json");
    std::fs::write(&path, json!({"panes": ["%1"]}).to_string()).unwrap();
    let allow = FileAllowlist(path.clone());
    let mut writer_blocked = false;
    allow
        .with_locked(&mut |matches| {
            assert!(matches("%1", None));
            assert!(!matches("%2", None));
            // While the shared lock is held, an exclusive try-lock must fail.
            writer_blocked = Guard::try_acquire(&lock_path(&path), Mode::Exclusive)
                .unwrap()
                .is_none();
            Ok(())
        })
        .unwrap();
    assert!(writer_blocked, "a shared reader did not exclude a writer");
    // Released on return: the writer can now take it.
    assert!(Guard::try_acquire(&lock_path(&path), Mode::Exclusive)
        .unwrap()
        .is_some());
}

#[test]
fn refresh_rereads_a_file_edited_under_us() {
    let d = dir("refresh");
    let path = d.join("superusers.json");
    std::fs::write(&path, json!({"panes": ["%1"]}).to_string()).unwrap();
    let mut store = Superusers::new(&path, true).unwrap();
    assert!(!store.refresh().unwrap(), "nothing changed");
    std::fs::write(&path, json!({"panes": ["%1", "%2", "%3"]}).to_string()).unwrap();
    assert!(store.refresh().unwrap());
    assert!(store.is_superuser("%3", None));
}

#[test]
fn a_falsy_entry_in_the_flat_list_is_dropped_not_turned_into_a_name() {
    // t1303: Python's str(entry or "") makes null/false/0/"" the empty string. Rust used
    // str(entry), so a stray null became the name "None": a pane labelled "None" would
    // have been a superuser (fail-open). The reviewer's reproducer, verbatim.
    let (panes, names) = parse_allowlist(&json!({"superusers":
        ["w9:p8", "VP of agentry", " a ", null, 1, "", "x:", false, 0, [], {}]}))
    .unwrap();
    assert_eq!(
        BTreeSet::from(["w9:p8".to_string(), "x:".to_string()]),
        panes
    );
    assert_eq!(
        BTreeSet::from([
            "1".to_string(),
            "VP of agentry".to_string(),
            "a".to_string()
        ]),
        names
    );
    // And the practical consequence: nothing labelled "None" is a superuser.
    assert!(!match_allowlist(
        &panes,
        &names,
        "%7",
        Some(&json!({"label": "None"}))
    ));
}

#[test]
fn refresh_after_the_file_is_deleted_reseeds_the_defaults_and_revokes_the_pane() {
    // t1325 / t1307: Python's refresh() calls load() with create=True.
    std::env::remove_var("DUNKINGSHEEP_VP_NAME");
    std::env::remove_var("DUNKINGSHEEP_OWNER_NAME");
    std::env::remove_var("DUNKINGSHEEP_VP_PANE");
    std::env::remove_var("DUNKINGSHEEP_OWNER_PANE");
    let d = dir("refresh-deleted");
    let path = d.join("superusers.json");
    // The pane is granted by a file that EXISTS when the store loads it (that is what sets
    // the stat stamp refresh() compares against).
    std::fs::write(&path, json!({"panes": ["w9:p8"]}).to_string()).unwrap();
    let mut store = Superusers::new(&path, true).unwrap();
    assert!(store.is_superuser("w9:p8", None));
    std::fs::remove_file(&path).unwrap();
    assert!(store.refresh().unwrap(), "a vanished file is a change");
    assert!(
        !store.is_superuser("w9:p8", None),
        "the pane grant must be revoked"
    );
    assert!(path.exists(), "the file is recreated");
    assert!(store.names.contains("VP of agentry") && store.names.contains("owner"));
    assert!(store.panes.is_empty());
    let on_disk = read_panes_locked(&path).unwrap();
    assert!(on_disk.1.contains("owner") && on_disk.0.is_empty());
}

#[test]
fn a_store_that_seeded_itself_does_not_notice_a_later_deletion_like_the_python() {
    // PARITY QUIRK, pinned: Python's load() returns before setting `_stamp` when it seeds,
    // so a store created on a missing file has stamp None, and after the file is deleted
    // again `stat` is None too: refresh() says "unchanged" and keeps the grant in memory.
    // (Verified against the real dunk_superusers.) Changing it is a behaviour decision.
    std::env::remove_var("DUNKINGSHEEP_VP_PANE");
    std::env::remove_var("DUNKINGSHEEP_OWNER_PANE");
    let scratch = dir("seeded-then-deleted");
    let path = scratch.join("superusers.json");
    let mut store = Superusers::new(&path, true).unwrap();
    store.add("w9:p8").unwrap();
    std::fs::remove_file(&path).unwrap();
    assert!(!store.refresh().unwrap());
    assert!(store.is_superuser("w9:p8", None));
}
