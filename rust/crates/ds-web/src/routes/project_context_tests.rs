//! t3250: real temporary stores for two projects and the global store; no daemon, no network.

use super::*;
use crate::routes::auth::{AuthError, AuthFuture};
use axum::body::{to_bytes, Body};
use axum::http::Request;
use std::collections::BTreeMap;
use std::fs;
use tower::ServiceExt;

struct Auth(Option<Identity>);
impl AuthBackend for Auth {
    fn identity<'a>(
        &'a self,
        _: Option<&'a str>,
        _: Option<&'a str>,
    ) -> AuthFuture<'a, Result<Identity, AuthError>> {
        Box::pin(async move { self.0.ok_or(AuthError::SignInRequired) })
    }
    fn login<'a>(&'a self, _: &'a str, _: &'a str) -> AuthFuture<'a, Result<String, AuthError>> {
        panic!("unexpected login")
    }
    fn logout<'a>(&'a self, _: Option<&'a str>) -> AuthFuture<'a, Result<(), AuthError>> {
        panic!("unexpected logout")
    }
    fn change_password<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: Option<&'a str>,
    ) -> AuthFuture<'a, Result<(), AuthError>> {
        panic!("unexpected password")
    }
}

fn write_entry(store: &Path, class: &str, slug: &str, meta: &[(&str, &str)], body: &str) {
    let folder = store.join(class);
    fs::create_dir_all(&folder).unwrap();
    // The store requires these keys; a test entry gives each a default it can override.
    let mut fields: Vec<(&str, String)> = vec![
        ("class", class.to_string()),
        ("subject", slug.to_string()),
        ("type", "reference".to_string()),
        ("scope", "project".to_string()),
        ("authority", "agent".to_string()),
        ("stated", "2026-11-01".to_string()),
    ];
    for (key, value) in meta {
        match fields.iter_mut().find(|(k, _)| k == key) {
            Some(field) => field.1 = value.to_string(),
            None => fields.push((key, value.to_string())),
        }
    }
    let mut text = String::from("---\n");
    for (key, value) in fields {
        text.push_str(&format!("{key}: {value}\n"));
    }
    text.push_str("---\n");
    text.push_str(body);
    fs::write(folder.join(format!("{slug}.md")), text).unwrap();
}

struct World {
    _dir: tempfile::TempDir,
    config: ProjectContextConfig,
    alpha: PathBuf,
    beta: PathBuf,
}

/// Two projects and a global store, each with a ground truth and a fact.
fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let config_dir = dir.path().join("config");
    let alpha = dir.path().join("alpha-repo");
    let beta = dir.path().join("beta-repo");
    let a = alpha.join(".dunkingsheep");
    write_entry(
        &a,
        "ground-truth",
        "alpha-rule",
        &[
            ("type", "rule"),
            ("authority", "owner"),
            ("source", "owner mail"),
            ("subject", "alpha rule"),
        ],
        "Only alpha may say this.\n",
    );
    write_entry(
        &a,
        "fact",
        "alpha-fact",
        &[
            ("type", "measurement"),
            ("authority", "measured"),
            ("subject", "alpha numbers"),
            ("stated", "2020-01-01"),
            ("recheck_after", "30d"),
        ],
        "alpha measured 3 things\n",
    );
    write_entry(
        &a,
        "fact",
        "alpha-old",
        &[("subject", "alpha old"), ("superseded_by", "alpha-fact")],
        "old <script>alert(1)</script>\n",
    );
    let b = beta.join(".dunkingsheep");
    write_entry(
        &b,
        "fact",
        "beta-fact",
        &[("subject", "beta numbers")],
        "beta measured 7 things\n",
    );
    let global = config_dir.join("context");
    write_entry(
        &global,
        "ground-truth",
        "global-rule",
        &[("authority", "owner"), ("subject", "everyone")],
        "A global rule.\n",
    );
    fs::create_dir_all(&config_dir).unwrap();
    let map = json!({
        "alpha": alpha.to_str().unwrap(),
        "beta": beta.to_str().unwrap(),
        "relative": "some/dir",
        "nostore": dir.path().to_str().unwrap(),
    });
    let map_path = config_dir.join(MAP_FILE);
    fs::write(&map_path, map.to_string()).unwrap();
    fs::set_permissions(&map_path, fs::Permissions::from_mode(0o600)).unwrap();
    let uid = fs::metadata("/proc/self").unwrap().uid();
    World {
        config: ProjectContextConfig {
            config_dir,
            global_store: global,
            uid,
        },
        alpha,
        beta,
        _dir: dir,
    }
}

async fn get(
    config: &ProjectContextConfig,
    identity: Option<Identity>,
    path: &str,
) -> (u16, Value) {
    let app = router(
        Arc::new(config.clone()),
        Arc::new(Auth(identity)),
        RequestScheme::Http,
    );
    let request = Request::builder()
        .uri(path)
        .header("host", "desk.test")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// Every file under `root` with its bytes and modification time.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, i64)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                stack.push(path.clone());
            }
            let bytes = if meta.is_file() {
                fs::read(&path).unwrap()
            } else {
                Vec::new()
            };
            out.insert(path, (bytes, meta.mtime()));
        }
    }
    out
}

#[tokio::test]
async fn the_list_shows_mapped_projects_bad_entries_and_the_global_store() {
    let w = world();
    let (status, list) = get(&w.config, Some(Identity::Owner), "/api/project-context").await;
    assert_eq!(status, 200);
    assert_eq!(list["map"]["state"], "ok");
    assert_eq!(list["global"]["state"], "ok");
    let by: BTreeMap<String, Value> = list["projects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["name"].as_str().unwrap().to_string(), p.clone()))
        .collect();
    assert_eq!(by["alpha"]["state"], "ok");
    assert_eq!(by["beta"]["state"], "ok");
    assert_eq!(by["relative"]["state"], "mapping_invalid");
    assert_eq!(
        by["relative"]["reason"],
        "the directory is not an absolute path"
    );
    assert_eq!(
        by["nostore"]["reason"],
        "the directory has no .dunkingsheep store"
    );
}

#[tokio::test]
async fn a_project_shows_ground_truths_first_with_provenance_and_nothing_of_another_project() {
    let w = world();
    let before = (
        snapshot(&w.alpha),
        snapshot(&w.beta),
        snapshot(&w.config.global_store),
    );
    let (status, alpha) = get(
        &w.config,
        Some(Identity::Owner),
        "/api/project-context/project/alpha",
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        (
            alpha["state"].as_str(),
            alpha["total"].as_u64(),
            alpha["ground_truths"].as_u64()
        ),
        (Some("ok"), Some(3), Some(1))
    );
    let entries = alpha["entries"].as_array().unwrap();
    assert_eq!(entries[0]["slug"], "alpha-rule");
    assert_eq!(entries[0]["ground_truth"], true);
    assert_eq!(
        (
            entries[0]["authority"].as_str(),
            entries[0]["source"].as_str(),
            entries[0]["type"].as_str()
        ),
        (Some("owner"), Some("owner mail"), Some("rule"))
    );
    let fact = entries.iter().find(|e| e["slug"] == "alpha-fact").unwrap();
    assert_eq!(
        (fact["ground_truth"].as_bool(), fact["stale"].as_bool()),
        (Some(false), Some(true)),
        "stated 2020 with a 30 day recheck"
    );
    let old = entries.iter().find(|e| e["slug"] == "alpha-old").unwrap();
    assert_eq!(old["superseded_by"], "alpha-fact");
    // Bodies are text: hostile markup comes back as characters for the desk to escape.
    assert!(old["body"]
        .as_str()
        .unwrap()
        .contains("<script>alert(1)</script>"));
    let text = alpha.to_string();
    assert!(
        !text.contains("beta") && !text.contains("global-rule"),
        "no other store leaks in"
    );

    // Search stays inside the project.
    let (_, found) = get(
        &w.config,
        Some(Identity::Owner),
        "/api/project-context/project/alpha?q=MEASURED",
    )
    .await;
    assert_eq!(found["total"], 1);
    assert_eq!(found["entries"][0]["slug"], "alpha-fact");
    let (_, none) = get(
        &w.config,
        Some(Identity::Owner),
        "/api/project-context/project/alpha?q=beta",
    )
    .await;
    assert_eq!(
        (none["state"].as_str(), none["total"].as_u64()),
        (Some("no_match"), Some(0))
    );
    let (_, beta) = get(
        &w.config,
        Some(Identity::Owner),
        "/api/project-context/project/beta",
    )
    .await;
    assert_eq!(beta["total"], 1);
    assert!(!beta.to_string().contains("alpha"));
    let (_, global) = get(
        &w.config,
        Some(Identity::Owner),
        "/api/project-context/global",
    )
    .await;
    assert_eq!(
        (global["scope"].as_str(), global["total"].as_u64()),
        (Some("global"), Some(1))
    );

    // Read-only: every store is byte-identical, and no index file appeared.
    assert_eq!(
        before,
        (
            snapshot(&w.alpha),
            snapshot(&w.beta),
            snapshot(&w.config.global_store)
        )
    );
}

#[tokio::test]
async fn unknown_unmapped_and_invalid_projects_are_explicit_states() {
    let w = world();
    for (path, state) in [
        ("/api/project-context/project/gamma", "not_mapped"),
        ("/api/project-context/project/relative", "mapping_invalid"),
        ("/api/project-context/project/..%2Fbeta", "not_mapped"),
    ] {
        let (status, view) = get(&w.config, Some(Identity::Owner), path).await;
        assert_eq!(
            (status, view["state"].as_str()),
            (200, Some(state)),
            "{path}"
        );
        assert!(view.get("entries").is_none());
    }
}

#[tokio::test]
async fn a_map_that_others_could_write_or_that_is_a_link_is_not_used() {
    let w = world();
    let map = w.config.config_dir.join(MAP_FILE);
    fs::set_permissions(&map, fs::Permissions::from_mode(0o666)).unwrap();
    let (_, list) = get(&w.config, Some(Identity::Owner), "/api/project-context").await;
    assert_eq!(
        (
            list["map"]["state"].as_str(),
            list["map"]["reason"].as_str()
        ),
        (
            Some("invalid"),
            Some("the map is writable by group or others")
        )
    );
    let (_, view) = get(
        &w.config,
        Some(Identity::Owner),
        "/api/project-context/project/alpha",
    )
    .await;
    assert_eq!(view["state"], "map_invalid");
    // A map owned by someone else.
    fs::set_permissions(&map, fs::Permissions::from_mode(0o600)).unwrap();
    let mut other = w.config.clone();
    other.uid = w.config.uid.wrapping_add(1);
    let (_, list) = get(&other, Some(Identity::Owner), "/api/project-context").await;
    assert_eq!(
        list["map"]["reason"],
        "the map is not owned by the desk's user"
    );
    // A link in place of the map.
    let real = w.config.config_dir.join("real.json");
    fs::rename(&map, &real).unwrap();
    std::os::unix::fs::symlink(&real, &map).unwrap();
    let (_, list) = get(&w.config, Some(Identity::Owner), "/api/project-context").await;
    assert_eq!(list["map"]["reason"], "the map is not a regular file");
    // No map at all.
    fs::remove_file(&map).unwrap();
    let (_, list) = get(&w.config, Some(Identity::Owner), "/api/project-context").await;
    assert_eq!(list["map"]["state"], "absent");
    let (_, view) = get(
        &w.config,
        Some(Identity::Owner),
        "/api/project-context/project/alpha",
    )
    .await;
    assert_eq!(view["state"], "not_mapped");
}

#[tokio::test]
async fn an_entry_that_is_a_link_or_leaves_the_store_is_skipped() {
    let w = world();
    let outside = w.alpha.join("secret.md");
    fs::write(
        &outside,
        "---\nclass: fact\nsubject: outside\n---\nnot a store entry\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(&outside, w.alpha.join(".dunkingsheep/fact/linked.md")).unwrap();
    std::os::unix::fs::symlink(
        w.beta.join(".dunkingsheep/fact/beta-fact.md"),
        w.alpha.join(".dunkingsheep/fact/beta-link.md"),
    )
    .unwrap();
    let (_, view) = get(
        &w.config,
        Some(Identity::Owner),
        "/api/project-context/project/alpha",
    )
    .await;
    assert_eq!(
        (view["total"].as_u64(), view["skipped_links"].as_u64()),
        (Some(3), Some(2))
    );
    let text = view.to_string();
    assert!(!text.contains("not a store entry") && !text.contains("beta measured"));
}

#[tokio::test]
async fn an_unreadable_entry_is_shown_under_its_class_directory_with_the_error() {
    let w = world();
    fs::write(
        w.alpha.join(".dunkingsheep/ground-truth/broken.md"),
        "no front matter at all\n",
    )
    .unwrap();
    let (_, view) = get(
        &w.config,
        Some(Identity::Owner),
        "/api/project-context/project/alpha",
    )
    .await;
    let broken = view["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["slug"] == "broken")
        .unwrap()
        .clone();
    assert_eq!(
        (broken["class"].as_str(), broken["ground_truth"].as_bool()),
        (Some("ground-truth"), Some(true))
    );
    assert!(
        broken["error"].as_str().unwrap().contains("front matter"),
        "{broken}"
    );
}

#[tokio::test]
async fn long_bodies_are_truncated_with_a_marker() {
    let w = world();
    write_entry(
        &w.alpha.join(".dunkingsheep"),
        "history",
        "long",
        &[("subject", "long")],
        &"x".repeat(MAX_BODY_CHARS + 50),
    );
    let (_, view) = get(
        &w.config,
        Some(Identity::Owner),
        "/api/project-context/project/alpha?q=long",
    )
    .await;
    let entry = &view["entries"][0];
    assert_eq!(entry["body_truncated"], true);
    assert_eq!(
        entry["body"].as_str().unwrap().chars().count(),
        MAX_BODY_CHARS
    );
}

#[tokio::test]
async fn only_the_owner_may_browse() {
    let w = world();
    for path in [
        "/api/project-context",
        "/api/project-context/global",
        "/api/project-context/project/alpha",
    ] {
        let (status, _) = get(&w.config, Some(Identity::Api), path).await;
        assert_eq!(status, 403, "{path}");
        let (status, _) = get(&w.config, None, path).await;
        assert_eq!(status, 401, "{path}");
    }
}
