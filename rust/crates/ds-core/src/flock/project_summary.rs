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

/// What `project_summary` reads besides the context stores.
pub trait SummaryEnv {
    /// `flock.resolve_lanes()`.
    fn lanes(&self) -> Vec<Value>;
    /// `flock.task_list(include_done=..., limit=500)["tasks"]`.
    fn task_list(&self, include_done: bool) -> Vec<Value>;
    /// `git_merges(repo)`: the repository's own record. Defaults to the real `git log`.
    fn merges(&self, repo: &str) -> Value {
        git_merges(repo, 10)
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
) -> Value {
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
        .lanes()
        .into_iter()
        .filter(|lane| {
            truthy(lane.get("agent"))
                && prefix
                    .as_deref()
                    .is_some_and(|p| str_of(lane, "cwd").starts_with(p))
        })
        .collect();
    let in_flight: Vec<Value> = env
        .task_list(false)
        .iter()
        .filter(|t| str_of(t, "status") == "in_progress")
        .map(|t| json!({"id": t["id"], "title": t["title"], "status": t["status"], "priority": t["priority"],
                        "assignees": if truthy(t.get("assignees")) { t["assignees"].clone() } else { json!([]) }}))
        .collect();
    let mut history: Vec<Value> = env
        .task_list(true)
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
        .map(|(name, repo)| (name.to_string(), env.merges(repo)))
        .collect();
    json!({
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Env {
        lanes: Vec<Value>,
        tasks: Vec<Value>,
    }

    impl SummaryEnv for Env {
        fn lanes(&self) -> Vec<Value> {
            self.lanes.clone()
        }
        fn task_list(&self, include_done: bool) -> Vec<Value> {
            self.tasks
                .iter()
                .filter(|t| include_done || t["status"] != "done")
                .cloned()
                .collect()
        }
        fn merges(&self, repo: &str) -> Value {
            json!({"repo": repo, "merges": [], "as_of": null, "reason": "stub"})
        }
    }

    fn entry(root: &Path, class: &str, slug: &str, subject: &str, body: &str) {
        let dir = root.join(class);
        std::fs::create_dir_all(&dir).unwrap();
        let text = format!("---\nclass: {class}\nsubject: {subject}\ntype: measurement\nscope: project\nstated: 2026-10-01\nauthority: owner\n---\n{body}\n");
        std::fs::write(dir.join(format!("{slug}.md")), text).unwrap();
    }

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ds-psummary-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
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
        let global = Store::new(Some(&tmp("empty-global")), false);
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
        let out = project_summary(&env, "demo", Some(&dir), &stores, 86_400.0);
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
        let global = Store::new(Some(&tmp("g-empty")), false);
        let (label, dir, stores) = stores_for(None, None, global);
        assert_eq!(
            (label.as_str(), dir.is_none(), stores.len()),
            ("global", true, 1)
        );
        let env = Env {
            lanes: vec![json!({"pane_id": "%1", "agent": "pi", "cwd": "/anything"})],
            tasks: vec![],
        };
        let out = project_summary(&env, &label, dir.as_deref(), &stores, 0.0);
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
        );
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
        let (label, got, none_yet) = stores_for(
            Some("demo"),
            Some(dir.to_str().unwrap()),
            Store::new(Some(&tmp("g2")), false),
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
            Store::new(Some(&tmp("g3")), false),
        );
        assert_eq!(
            both.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            ["global", "project"]
        );
        let (g, _, only) = stores_for(Some("global"), None, Store::new(Some(&tmp("g4")), false));
        assert_eq!((g.as_str(), only.len()), ("global", 1));
    }
}
