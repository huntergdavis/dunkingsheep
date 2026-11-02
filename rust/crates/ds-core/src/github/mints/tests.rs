use super::*;
use std::os::unix::fs::PermissionsExt;

fn minted(dir: &Path, pane: &str, at: f64) {
    assert_eq!(
        record_author_mint(pane, Some(at), None, dir).unwrap(),
        Some(dir.join(AUTHOR_MINT_FILE))
    );
}
fn author(dir: &Path, since: f64, now: f64) -> Option<String> {
    author_from_mints(Some(since), None, Some(now), dir).unwrap()
}

#[test]
fn test_the_only_recent_mint_is_the_author() {
    let dir = crate::testsupport::dir("mint-only");
    minted(dir.path(), "%101", 9940.0);
    assert_eq!(author(dir.path(), 8200.0, 10000.0).as_deref(), Some("%101"));
}
#[test]
fn test_two_lanes_in_the_window_attribute_nobody() {
    let dir = crate::testsupport::dir("mint-ambiguous");
    minted(dir.path(), "%101", 9940.0);
    minted(dir.path(), "%108", 9970.0);
    assert_eq!(author(dir.path(), 8200.0, 10000.0), None);
}
#[test]
fn test_a_mint_before_the_window_is_not_the_author_of_this_head() {
    let dir = crate::testsupport::dir("mint-before");
    minted(dir.path(), "%101", 2800.0);
    assert_eq!(author(dir.path(), 8200.0, 10000.0), None);
}
#[test]
fn test_the_same_lane_minting_twice_is_still_one_author() {
    let dir = crate::testsupport::dir("mint-twice");
    minted(dir.path(), "%101", 9400.0);
    minted(dir.path(), "%101", 9940.0);
    assert_eq!(author(dir.path(), 8200.0, 10000.0).as_deref(), Some("%101"));
}
#[test]
fn test_a_missing_or_unreadable_record_attributes_nobody() {
    let dir = crate::testsupport::dir("mint-read-failure");
    let path = dir.path().join(AUTHOR_MINT_FILE);
    assert_eq!(author(dir.path(), 0.0, 10000.0), None);
    fs::write(&path, "{not json").unwrap();
    assert_eq!(author(dir.path(), 0.0, 10000.0), None);
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert_eq!(author(dir.path(), 0.0, 10000.0), None);
    fs::remove_dir(&path).unwrap();
    fs::write(&path, "[1, 2]").unwrap();
    assert_eq!(author(dir.path(), 0.0, 10000.0), None);
    fs::write(&path, "{\"panes\": []}").unwrap();
    assert_eq!(author(dir.path(), 0.0, 10000.0), None);
    fs::write(&path, "λ").unwrap();
    assert_eq!(author(dir.path(), 0.0, 10000.0), None);
}
#[test]
fn test_the_record_is_private_and_prunes_old_mints() {
    let dir = crate::testsupport::dir("mint-private");
    minted(dir.path(), "%101", 10000.0);
    minted(dir.path(), "%101", 100000.0);
    let path = dir.path().join(AUTHOR_MINT_FILE);
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let value: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(value["panes"]["%101"].as_array().unwrap().len(), 1);
}
#[test]
fn author_mints_match_python_oracle_cases() {
    let cases: Vec<Value> = serde_json::from_str(include_str!("oracle.json")).unwrap();
    assert_eq!(cases.len(), 18);
    for case in cases {
        let dir = crate::testsupport::dir("mint-python-oracle");
        let path = dir.path().join("ledger.json");
        if let Some(initial) = case["initial"].as_str() {
            fs::write(&path, initial).unwrap();
        }
        let number = |key: &str| match case[key].as_str().unwrap() {
            "nan" => f64::NAN,
            "inf" => f64::INFINITY,
            "-inf" => f64::NEG_INFINITY,
            value => value.parse::<f64>().unwrap(),
        };
        let result = record_author_mint(
            case["pane"].as_str().unwrap(),
            Some(number("at")),
            Some(&path),
            dir.path(),
        );
        if let Some(error) = case["error"].as_str() {
            assert_eq!(result.unwrap_err().to_string(), error, "{}", case["name"]);
        } else {
            assert_eq!(
                result.unwrap().is_some(),
                case["recorded"].as_bool().unwrap(),
                "{}",
                case["name"]
            );
        }
        assert_eq!(
            fs::read_to_string(&path).ok().as_deref(),
            case["text"].as_str(),
            "{}",
            case["name"]
        );
        let result = author_from_mints(
            Some(number("since")),
            Some(&path),
            Some(number("now")),
            dir.path(),
        );
        if let Some(error) = case["readerror"].as_str() {
            assert_eq!(result.unwrap_err().to_string(), error, "{}", case["name"]);
        } else {
            assert_eq!(
                result.unwrap().as_deref(),
                case["author"].as_str(),
                "{}",
                case["name"]
            );
        }
    }
}
#[test]
fn author_mint_window_endpoints_and_numeric_read_types_match_python() {
    let dir = crate::testsupport::dir("mint-endpoints");
    let path = dir.path().join(AUTHOR_MINT_FILE);
    for (stamps, expected) in [("[100]", None), ("[200]", Some("%1")), ("[200.001]", None)] {
        fs::write(&path, format!("{{\"panes\":{{\"%1\":{stamps}}}}}")).unwrap();
        assert_eq!(author(dir.path(), 100.0, 200.0).as_deref(), expected);
    }
    fs::write(
        &path,
        r#"{"panes":{"%1":[" 1_999 ","١٩٩٩",null,false,"bad"],"%2":[0]}}"#,
    )
    .unwrap();
    assert_eq!(
        author_from_mints(None, None, Some(2000.0), dir.path())
            .unwrap()
            .as_deref(),
        Some("%1")
    );
    fs::write(&path, r#"{"panes":{"%1":{"1999":false}}}"#).unwrap();
    assert_eq!(author(dir.path(), 200.0, 2000.0).as_deref(), Some("%1"));
    fs::write(&path, r#"{"panes":{"%1":true}}"#).unwrap();
    assert_eq!(
        author_from_mints(None, None, Some(2000.0), dir.path())
            .unwrap_err()
            .to_string(),
        "'bool' object is not iterable"
    );
    fs::write(
        &path,
        format!("{{\"panes\":{{\"%1\":[{}]}}}}", "9".repeat(400)),
    )
    .unwrap();
    assert_eq!(
        author_from_mints(None, None, Some(2000.0), dir.path())
            .unwrap_err()
            .to_string(),
        "int too large to convert to float"
    );
}
#[test]
fn repos_config_defaults_shapes_and_python_string_conversion() {
    let dir = crate::testsupport::dir("mint-repos");
    let path = dir.path().join("github-repos.json");
    let defaults = DEFAULT_REPOS.map(String::from).to_vec();
    assert_eq!(repos_config(dir.path()), defaults);
    for input in [
        "{bad",
        "[]",
        "{}",
        r#"{"repos":[]}"#,
        r#"{"repos":"owner/repo"}"#,
        "λ",
    ] {
        fs::write(&path, input).unwrap();
        assert_eq!(repos_config(dir.path()), defaults);
    }
    fs::write(
        &path,
        r#"{"repos":["o/r","o/r",null,true,1,1.0,{"a":"é"},["x","y"]]}"#,
    )
    .unwrap();
    assert_eq!(
        repos_config(dir.path()),
        [
            "o/r",
            "o/r",
            "None",
            "True",
            "1",
            "1.0",
            "{'a': 'é'}",
            "['x', 'y']"
        ]
    );
    fs::write(&path, r#"[NaN, Infinity, -Infinity, 1e999, -0, -0.0]"#).unwrap();
    assert_eq!(
        repos_config(dir.path()),
        ["nan", "inf", "-inf", "inf", "0", "-0.0"]
    );
    fs::write(&path, r#"["__ds_mint_number__0", 42]"#).unwrap();
    assert_eq!(repos_config(dir.path()), ["__ds_mint_number__0", "42"]);
}
#[test]
fn author_mint_paths_and_write_replace_failures_follow_source() {
    let dir = crate::testsupport::dir("mint-write-failures");
    assert_eq!(
        author_mint_path(None, dir.path()),
        dir.path().join(AUTHOR_MINT_FILE)
    );
    assert_eq!(
        author_mint_path(Some(Path::new("")), dir.path()),
        dir.path().join(AUTHOR_MINT_FILE)
    );
    let path = dir.path().join("nested/custom.json");
    assert_eq!(author_mint_path(Some(&path), dir.path()), path);
    assert_eq!(
        record_author_mint("  ", Some(1.0), Some(&path), dir.path()).unwrap(),
        None
    );
    assert!(!path.parent().unwrap().exists());
    assert_eq!(
        record_author_mint("%1", Some(1.0), Some(&path), dir.path()).unwrap(),
        Some(path.clone())
    );
    assert!(path.is_file());
    assert!(!dir.path().join(AUTHOR_MINT_FILE).exists());
    let parent_file = dir.path().join("blocked");
    fs::write(&parent_file, "file").unwrap();
    assert_eq!(
        record_author_mint(
            "%1",
            Some(1.0),
            Some(&parent_file.join("ledger")),
            dir.path()
        )
        .unwrap(),
        None
    );
    let destination = dir.path().join("directory");
    fs::create_dir(&destination).unwrap();
    assert_eq!(
        record_author_mint("%1", Some(1.0), Some(&destination), dir.path()).unwrap(),
        None
    );
    let temporary = dir.path().join("directory.tmp");
    assert!(destination.is_dir());
    assert!(temporary.is_file());
    assert_eq!(
        fs::metadata(&temporary).unwrap().permissions().mode() & 0o777,
        0o600
    );
    fs::remove_dir(&destination).unwrap();
    assert_eq!(
        record_author_mint("%2", Some(2.0), Some(&destination), dir.path()).unwrap(),
        Some(destination.clone())
    );
    assert!(!temporary.exists());
    assert_eq!(
        fs::read_to_string(destination).unwrap(),
        r#"{"panes": {"%2": [2.0]}, "updated_at": 2.0}"#
    );
}
