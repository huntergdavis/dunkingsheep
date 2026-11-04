//! `dunk_views.project_summary` (T38, slice 6): everything one project page needs, from the
//! context stores, the lanes and the task list. `project=None` (or "" or "global") means the
//! global store. Every field is a deterministic read: no model, no inference.
//!
//! The lanes and the task list are a trait (`SummaryEnv`); the context stores are the real
//! `context::Store`, passed in resolved so a caller (and a test) chooses which global store
//! is read.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::views::{git_merges, iso, PROJECT_REPOS};
use crate::context::digest::first_line;
use crate::context::store::{Store, CLASSES};
use crate::error::DunkError;

/// What `project_summary` reads besides the context stores.
pub trait SummaryEnv {
    /// Fully expanded global context root from the daemon's captured startup configuration.
    fn global_context_root(&self) -> Result<PathBuf, DunkError> {
        Err(DunkError::Refused(
            "project summary context root is not wired".into(),
        ))
    }
    /// Expand and absolutize a raw path using the captured HOME, never request-time HOME.
    fn project_dir(&self, _raw: &str) -> Result<PathBuf, DunkError> {
        Err(DunkError::Refused(
            "project summary path adapter is not wired".into(),
        ))
    }
    /// `flock.resolve_lanes()`.
    fn lanes(&self) -> Result<Vec<Value>, DunkError>;
    /// `flock.task_list(include_done=..., limit=500)["tasks"]`.
    fn task_list(&self, include_done: bool) -> Result<Vec<Value>, DunkError>;
    /// `git_merges(repo)`: the repository's own record. Defaults to the real `git log`.
    fn merges(&self, repo: &str) -> Result<Value, DunkError> {
        Ok(git_merges(repo, 10))
    }
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `os.path.abspath(os.path.expanduser(path))` for the project directory.
fn project_dir_of(project: &str, path: Option<&str>) -> PathBuf {
    let raw = path
        .filter(|p| !p.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("~/workspace/{project}"));
    let expanded = match (raw.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => raw,
    };
    let p = PathBuf::from(&expanded);
    let abs = if p.is_absolute() {
        p
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    };
    // abspath normalises `.` and `..` lexically.
    let mut out = PathBuf::new();
    for part in abs.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// POSIX expanduser using captured HOME and an explicit named-account lookup.
/// Path.expanduser (global store) refuses unknown users; os.path.expanduser (project) keeps them.
pub(crate) fn expand_summary_path(
    raw: &Path,
    home: &Path,
    strict: bool,
    lookup: impl FnOnce(&str) -> Result<Option<PathBuf>, DunkError>,
) -> Result<PathBuf, DunkError> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let bytes = raw.as_os_str().as_bytes();
    if bytes.first() != Some(&b'~') {
        return Ok(raw.to_path_buf());
    }
    let end = bytes
        .iter()
        .position(|byte| *byte == b'/')
        .unwrap_or(bytes.len());
    let selected = if end == 1 {
        Some(home.to_path_buf())
    } else {
        let user = std::str::from_utf8(&bytes[1..end])
            .map_err(|error| DunkError::Refused(error.to_string()))?;
        lookup(user)?
    };
    let Some(selected) = selected else {
        return if strict {
            Err(DunkError::Refused(
                "Could not determine home directory.".into(),
            ))
        } else {
            Ok(raw.to_path_buf())
        };
    };
    let prefix = selected.as_os_str().as_bytes();
    let trimmed = prefix
        .iter()
        .rposition(|byte| *byte != b'/')
        .map_or(0, |i| i + 1);
    let mut expanded = prefix[..trimmed].to_vec();
    expanded.extend_from_slice(&bytes[end..]);
    if expanded.is_empty() {
        expanded.push(b'/');
    }
    Ok(PathBuf::from(std::ffi::OsString::from_vec(expanded)))
}

/// POSIX abspath/normpath, including the special exactly-two-leading-slashes case.
pub(crate) fn absolute_summary_path(path: &Path, cwd: &Path) -> PathBuf {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let bytes = joined.as_os_str().as_bytes();
    let double_root = bytes.starts_with(b"//") && !bytes.starts_with(b"///");
    let mut parts: Vec<&[u8]> = Vec::new();
    for part in bytes.split(|byte| *byte == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    let mut out = if double_root {
        b"//".to_vec()
    } else {
        b"/".to_vec()
    };
    for (i, part) in parts.iter().enumerate() {
        if i != 0 {
            out.push(b'/');
        }
        out.extend_from_slice(part);
    }
    PathBuf::from(std::ffi::OsString::from_vec(out))
}

/// The label, the project dir and the stores a summary reads. `global` is the global store
/// (`Store(global_store=True)`); a project reads `select_stores(<dir>/.dunkingsheep)`, which
/// is global plus the project store when its directory exists.
pub fn stores_for(
    project: Option<&str>,
    path: Option<&str>,
    global: Store,
) -> (String, Option<PathBuf>, Vec<(String, Store)>) {
    match project.filter(|p| !p.is_empty() && *p != "global") {
        None => ("global".into(), None, vec![("global".into(), global)]),
        Some(name) => {
            let dir = project_dir_of(name, path);
            let store = Store::new(Some(&dir.join(".dunkingsheep")), false);
            let mut stores = vec![("global".to_string(), global)];
            if store.root.is_dir() {
                stores.push(("project".to_string(), store));
            }
            (name.to_string(), Some(dir), stores)
        }
    }
}

/// Store selection after the API has validated and resolved the raw project path.
/// These roots are already expanded; constructing them directly avoids ambient HOME reads.
pub fn stores_for_resolved(
    project_dir: Option<&Path>,
    global_root: PathBuf,
) -> Vec<(String, Store)> {
    let mut stores = vec![(
        "global".into(),
        Store {
            root: global_root,
            scope: "global",
        },
    )];
    if let Some(dir) = project_dir {
        let root = dir.join(".dunkingsheep");
        if root.is_dir() {
            stores.push((
                "project".into(),
                Store {
                    root,
                    scope: "project",
                },
            ));
        }
    }
    stores
}

fn str_of<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// `project_summary`.
pub fn project_summary(
    env: &dyn SummaryEnv,
    label: &str,
    project_dir: Option<&Path>,
    stores: &[(String, Store)],
    now: f64,
) -> Result<Value, DunkError> {
    let mut counts: serde_json::Map<String, Value> = serde_json::Map::new();
    let (mut truths, mut articles): (Vec<Value>, Vec<Value>) = (Vec::new(), Vec::new());
    for (name, target) in stores {
        if !target.exists() {
            continue;
        }
        let live = target.live_entries();
        for class in CLASSES {
            let n = live
                .iter()
                .filter(|e| e.get("class") == Some(class))
                .count() as i64;
            let total = counts.get(class).and_then(Value::as_i64).unwrap_or(0) + n;
            counts.insert(class.to_string(), json!(total));
        }
        for entry in &live {
            let class = entry.get("class").unwrap_or("");
            let first: String = first_line(&entry.body).chars().take(160).collect();
            let item = json!({
                "store": name, "class": class, "slug": entry.slug,
                "subject": entry.get("subject"), "authority": entry.get("authority"),
                "stated": entry.get("stated"), "first_line": first,
            });
            if class == "ground-truth" {
                truths.push(item)
            } else {
                articles.push(item)
            }
        }
    }
    let prefix = project_dir.map(|d| d.display().to_string());
    let lanes: Vec<Value> = env
        .lanes()?
        .into_iter()
        .filter(|lane| {
            truthy(lane.get("agent"))
                && prefix
                    .as_deref()
                    .is_some_and(|p| str_of(lane, "cwd").starts_with(p))
        })
        .collect();
    let in_flight: Vec<Value> = env
        .task_list(false)?
        .iter()
        .filter(|t| str_of(t, "status") == "in_progress")
        .map(|t| json!({"id": t["id"], "title": t["title"], "status": t["status"], "priority": t["priority"],
                        "assignees": if truthy(t.get("assignees")) { t["assignees"].clone() } else { json!([]) }}))
        .collect();
    let mut history: Vec<Value> = env
        .task_list(true)?
        .iter()
        .filter(|t| str_of(t, "status") == "done" && truthy(t.get("completed_at")))
        .map(|t| json!({"at": iso(t["completed_at"].as_f64()), "kind": "task.done", "id": t["id"], "title": t["title"], "by": t.get("completed_by").cloned().unwrap_or(Value::Null)}))
        .collect();
    history.sort_by(|a, b| str_of(b, "at").cmp(str_of(a, "at")));
    let has_history = !history.is_empty();
    history.truncate(50);
    let key = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    truths.sort_by_key(|t| key(t, "subject"));
    articles.sort_by_key(|a| (key(a, "class"), key(a, "subject")));
    let merges: serde_json::Map<String, Value> = PROJECT_REPOS
        .iter()
        .filter(|(name, _)| label == "global" || label == *name)
        .map(|(name, repo)| env.merges(repo).map(|merges| (name.to_string(), merges)))
        .collect::<Result<_, _>>()?;
    Ok(json!({
        "project": label,
        "as_of": iso(Some(now)),
        "path": prefix,
        "knowledge": {"counts": counts, "ground_truths": truths, "articles_total": articles.len(), "articles": articles},
        "now": {
            "agents": lanes.iter().map(|l| json!({"name": l.get("agent_name").cloned().unwrap_or(Value::Null), "pane_id": l["pane_id"], "state": l.get("agent_status").cloned().unwrap_or(Value::Null)})).collect::<Vec<_>>(),
            "tasks_in_flight": in_flight,
        },
        "merges": merges,
        "history": history,
        "history_note": if has_history { "merges and decisions are the repository's record; this timeline is tasks completed here" } else { "no completed task has a timestamp yet" },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Env {
        lanes: Vec<Value>,
        tasks: Vec<Value>,
    }

    impl SummaryEnv for Env {
        fn lanes(&self) -> Result<Vec<Value>, DunkError> {
            Ok(self.lanes.clone())
        }
        fn task_list(&self, include_done: bool) -> Result<Vec<Value>, DunkError> {
            Ok(self
                .tasks
                .iter()
                .filter(|t| include_done || t["status"] != "done")
                .cloned()
                .collect())
        }
        fn merges(&self, repo: &str) -> Result<Value, DunkError> {
            Ok(json!({"repo": repo, "merges": [], "as_of": null, "reason": "stub"}))
        }
    }

    fn entry(root: &Path, class: &str, slug: &str, subject: &str, body: &str) {
        let dir = root.join(class);
        std::fs::create_dir_all(&dir).unwrap();
        let text = format!("---\nclass: {class}\nsubject: {subject}\ntype: measurement\nscope: project\nstated: 2026-10-01\nauthority: owner\n---\n{body}\n");
        std::fs::write(dir.join(format!("{slug}.md")), text).unwrap();
    }

    fn tmp(tag: &str) -> crate::testsupport::Scratch<PathBuf> {
        let scratch = crate::testsupport::dir(&format!("psummary-{tag}"));
        crate::testsupport::Scratch::new(scratch.path().to_path_buf(), scratch)
    }

    #[test]
    fn a_project_summary_reads_knowledge_agents_tasks_and_history() {
        let dir = tmp("full");
        let root = dir.join(".dunkingsheep");
        entry(
            &root,
            "ground-truth",
            "no-reboot",
            "no-reboot",
            "Never reboot the host.\nsecond line",
        );
        entry(&root, "fact", "zeta", "zeta-fact", "z body");
        entry(&root, "fact", "alpha", "alpha-fact", "a body");
        let global_dir = tmp("empty-global");
        let global = Store::new(Some(&global_dir), false);
        let stores = vec![
            ("global".to_string(), global),
            ("project".to_string(), Store::new(Some(&root), false)),
        ];
        let env = Env {
            lanes: vec![
                json!({"pane_id": "%1", "agent": "pi", "agent_name": "w1", "agent_status": "idle", "cwd": format!("{}/sub", dir.display())}),
                json!({"pane_id": "%2", "agent": "pi", "agent_name": "elsewhere", "cwd": "/other"}),
                json!({"pane_id": "%3", "agent": "", "cwd": dir.display().to_string()}),
            ],
            tasks: vec![
                json!({"id": "t1", "title": "doing", "status": "in_progress", "priority": "high", "assignees": ["%1"]}),
                json!({"id": "t2", "title": "old", "status": "done", "completed_at": 100.0, "completed_by": "%1"}),
                json!({"id": "t3", "title": "newer", "status": "done", "completed_at": 200.0}),
                json!({"id": "t4", "title": "no stamp", "status": "done"}),
            ],
        };
        let out = project_summary(&env, "demo", Some(&dir), &stores, 86_400.0).unwrap();
        assert_eq!(out["project"], "demo");
        assert_eq!(out["as_of"], "1970-01-02T00:00:00Z");
        assert_eq!(out["path"], dir.display().to_string());
        let k = &out["knowledge"];
        assert_eq!(
            k["counts"],
            json!({"ground-truth": 1, "fact": 2, "provisional": 0, "history": 0})
        );
        assert_eq!(
            k["ground_truths"][0]["first_line"],
            "Never reboot the host."
        );
        assert_eq!(k["ground_truths"][0]["store"], "project");
        let subjects: Vec<&str> = k["articles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["subject"].as_str().unwrap())
            .collect();
        assert_eq!(
            subjects,
            ["alpha-fact", "zeta-fact"],
            "sorted by class then subject"
        );
        assert_eq!(k["articles_total"], 2);
        assert_eq!(
            out["now"]["agents"],
            json!([{"name": "w1", "pane_id": "%1", "state": "idle"}])
        );
        assert_eq!(
            out["now"]["tasks_in_flight"],
            json!([{"id": "t1", "title": "doing", "status": "in_progress", "priority": "high", "assignees": ["%1"]}])
        );
        let ids: Vec<&str> = out["history"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            ["t3", "t2"],
            "newest first; a done task with no completed_at is not history"
        );
        assert_eq!(out["history"][1]["by"], "%1");
        assert!(out["history_note"]
            .as_str()
            .unwrap()
            .starts_with("merges and decisions"));
        assert_eq!(
            out["merges"].as_object().unwrap().keys().count(),
            0,
            "a project that is not one of PROJECT_REPOS has no merge list"
        );
    }

    #[test]
    fn the_global_summary_has_no_path_no_agents_and_every_repos_merges() {
        let global_dir = tmp("g-empty");
        let global = Store::new(Some(&global_dir), false);
        let (label, dir, stores) = stores_for(None, None, global);
        assert_eq!(
            (label.as_str(), dir.is_none(), stores.len()),
            ("global", true, 1)
        );
        let env = Env {
            lanes: vec![json!({"pane_id": "%1", "agent": "pi", "cwd": "/anything"})],
            tasks: vec![],
        };
        let out = project_summary(&env, &label, dir.as_deref(), &stores, 0.0).unwrap();
        assert!(out["path"].is_null());
        assert_eq!(
            out["now"]["agents"],
            json!([]),
            "no project dir: no lane is 'in' it"
        );
        assert_eq!(
            out["knowledge"]["counts"],
            json!({}),
            "a store that does not exist adds no counts"
        );
        assert_eq!(
            out["merges"].as_object().unwrap().len(),
            PROJECT_REPOS.len()
        );
        assert_eq!(out["history_note"], "no completed task has a timestamp yet");
    }

    #[test]
    fn a_project_named_like_a_repo_gets_only_its_own_merges() {
        let env = Env {
            lanes: vec![],
            tasks: vec![],
        };
        let out = project_summary(
            &env,
            "dunkingsheep",
            Some(Path::new("/nonexistent/dunkingsheep")),
            &[],
            0.0,
        )
        .unwrap();
        assert_eq!(
            out["merges"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["dunkingsheep"]
        );
    }

    #[test]
    fn stores_for_resolves_the_project_dir_and_adds_the_project_store_only_when_it_exists() {
        let dir = tmp("resolve");
        let (g2_dir, g3_dir, g4_dir) = (tmp("g2"), tmp("g3"), tmp("g4"));
        let (label, got, none_yet) = stores_for(
            Some("demo"),
            Some(dir.to_str().unwrap()),
            Store::new(Some(&g2_dir), false),
        );
        assert_eq!(
            (label.as_str(), got.as_deref()),
            ("demo", Some(dir.as_path()))
        );
        assert_eq!(
            none_yet.len(),
            1,
            "no .dunkingsheep directory yet: global only"
        );
        std::fs::create_dir_all(dir.join(".dunkingsheep")).unwrap();
        let (_, _, both) = stores_for(
            Some("demo"),
            Some(dir.to_str().unwrap()),
            Store::new(Some(&g3_dir), false),
        );
        assert_eq!(
            both.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            ["global", "project"]
        );
        let (g, _, only) = stores_for(Some("global"), None, Store::new(Some(&g4_dir), false));
        assert_eq!((g.as_str(), only.len()), ("global", 1));
    }
    #[test]
    fn summary_path_expansion_distinguishes_named_users_and_global_errors() {
        let captured = Path::new("/captured");
        let no_lookup =
            |_: &str| -> Result<Option<PathBuf>, DunkError> { panic!("unexpected named lookup") };
        assert_eq!(
            expand_summary_path(Path::new("~/a"), captured, false, no_lookup).unwrap(),
            Path::new("/captured/a")
        );
        assert_eq!(
            expand_summary_path(Path::new("~"), Path::new("/"), false, no_lookup).unwrap(),
            Path::new("/")
        );
        assert_eq!(
            expand_summary_path(Path::new("~alice/a"), captured, false, |name| {
                assert_eq!(name, "alice");
                Ok(Some(PathBuf::from("/accounts/alice/")))
            })
            .unwrap(),
            Path::new("/accounts/alice/a")
        );
        assert_eq!(
            expand_summary_path(Path::new("~missing/a"), captured, false, |_| Ok(None)).unwrap(),
            Path::new("~missing/a")
        );
        assert_eq!(
            expand_summary_path(Path::new("~missing/context"), captured, true, |_| Ok(None))
                .unwrap_err()
                .to_string(),
            "Could not determine home directory."
        );
        for (raw, expected) in [
            ("a/../b", "/cwd/b"),
            ("/../../a", "/a"),
            ("//host/a/../b", "//host/b"),
            ("///host/./b", "/host/b"),
        ] {
            assert_eq!(
                absolute_summary_path(Path::new(raw), Path::new("/cwd")).as_os_str(),
                expected
            );
        }
    }
}
