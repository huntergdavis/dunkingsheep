//! t3600: the review sweep covers every fleet repo, even when github-repos.json omits one.
//! Before this, a config that named only dunkingsheep left poolrad-decomp PRs out of review, so
//! they never received a `dunkingsheep/review` status and GitHub blocked their merge.

use super::{with_fleet_repos, ConfiguredReviewPolicy, ReviewRepoConfig};
use crate::github::DEFAULT_REPOS;

#[test]
fn a_config_that_omits_a_fleet_repo_still_sweeps_it() {
    let repos = with_fleet_repos(vec!["huntergdavis/dunkingsheep".to_owned()]);
    assert_eq!(
        repos,
        vec![
            "huntergdavis/dunkingsheep".to_owned(),
            "huntergdavis/poolrad-decomp".to_owned(),
        ]
    );
}

#[test]
fn a_config_keeps_its_own_extra_repos_and_gains_the_fleet() {
    let repos = with_fleet_repos(vec!["owner/other".to_owned()]);
    assert_eq!(
        repos,
        vec![
            "owner/other",
            "huntergdavis/dunkingsheep",
            "huntergdavis/poolrad-decomp",
        ]
    );
}

#[test]
fn a_fleet_repo_the_config_already_names_is_not_duplicated() {
    let configured = vec![
        "huntergdavis/poolrad-decomp".to_owned(),
        "huntergdavis/dunkingsheep".to_owned(),
    ];
    assert_eq!(with_fleet_repos(configured.clone()), configured);
}

#[test]
fn the_production_policy_sweeps_poolrad_when_the_live_file_lists_only_dunkingsheep() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("github-repos.json"),
        r#"{"repos": ["huntergdavis/dunkingsheep"]}"#,
    )
    .unwrap();
    let policy = ConfiguredReviewPolicy {
        config_dir: dir.path().to_path_buf(),
    };
    let repos = policy.repos().unwrap();
    assert!(
        repos
            .iter()
            .any(|repo| repo == "huntergdavis/poolrad-decomp"),
        "poolrad-decomp is swept: {repos:?}"
    );
    assert_eq!(repos.len(), DEFAULT_REPOS.len(), "{repos:?}");
}
