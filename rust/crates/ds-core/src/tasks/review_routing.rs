//! Initial-review routing. Assignment and its audit event share task creation's transaction.
//! This confers no review/verdict authority; the existing claim and verdict gates still apply.
use std::collections::BTreeSet;

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;

use super::{decode_list, lane_project};
use crate::{db, error::DunkError};

pub const RULE: &str = "single_project_reviewer_v1";

/// Fresh registry/backend observations, never service tokens or cached role grace entries.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub candidates: Vec<Candidate>,
    pub unavailable: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub pane: String,
    pub provider: Option<String>,
    pub identity: Option<(i64, String)>,
    pub rejection: Option<String>,
}

/// Called only for an initial review, after its exact repository/project mapping is written.
pub(super) fn apply(
    conn: &Connection,
    id: &str,
    repo: Option<&str>,
    pr: Option<i64>,
    author: Option<&str>,
    snapshot: Snapshot,
    now: f64,
) -> Result<(), DunkError> {
    let project: Option<String> = conn
        .query_row(
            "SELECT project FROM planner_task_projects WHERE task_id=?1",
            params![id],
            |row| row.get(0),
        )
        .optional()
        .map_err(db::map_sqlite)?;
    // Reuse the recorded-work exclusions enforced by planner review assignment. Include
    // every contributor, even when review-sync cannot name one unique explicit author.
    let mut authors = BTreeSet::new();
    if let Some(author) = author.filter(|a| !a.is_empty()) {
        authors.insert(author.to_string());
    }
    let mut stmt = conn.prepare(
        "SELECT assignees,completed_by,author FROM tasks WHERE repo=?1 AND pr=?2 AND kind!='review'",
    ).map_err(db::map_sqlite)?;
    let rows = stmt
        .query_map(params![repo, pr], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(db::map_sqlite)?;
    for row in rows {
        let (assignees, completed_by, author) = row.map_err(db::map_sqlite)?;
        authors.extend(
            decode_list(assignees.as_deref())
                .into_iter()
                .filter(|a| !a.is_empty()),
        );
        authors.extend(
            completed_by
                .into_iter()
                .chain(author)
                .filter(|a| !a.is_empty()),
        );
    }
    let mut candidates = snapshot.candidates;
    candidates.sort_by(|a, b| a.pane.cmp(&b.pane));
    candidates.dedup_by(|a, b| a.pane == b.pane);
    let mut considered = Vec::new();
    let mut eligible = Vec::new();
    for candidate in candidates {
        let mut rejection = candidate.rejection;
        if rejection.is_none() && authors.contains(&candidate.pane) {
            rejection = Some("recorded author or contributor".into());
        }
        if rejection.is_none() {
            rejection = match &candidate.identity {
                Some((pid, start)) if crate::proc::process_alive(*pid, Some(start)) => {
                    match lane_project::get(conn, &candidate.pane, *pid, start)
                        .map_err(db::map_sqlite)?
                    {
                        Some(member)
                            if project.as_ref().is_some_and(|p| {
                                member.scope == lane_project::ProjectScope::Project(p.clone())
                            }) =>
                        {
                            None
                        }
                        _ => Some("missing, stale, global, or different project membership".into()),
                    }
                }
                _ => Some("missing or dead process binding".into()),
            };
        }
        if rejection.is_none() {
            eligible.push(candidate.pane.clone());
        }
        considered.push(json!({"pane":candidate.pane,"provider":candidate.provider,"eligible":rejection.is_none(),"reason":rejection}));
    }
    let reason = if let Some(error) = snapshot.unavailable {
        format!("reviewer inventory unavailable: {error}")
    } else if project.is_none() {
        "repository has no explicit project mapping".into()
    } else if authors.is_empty() {
        "author provenance unavailable".into()
    } else {
        match eligible.len() {
            1 => "exactly one eligible live project reviewer".into(),
            0 => "no eligible live project reviewer".into(),
            _ => format!(
                "multiple eligible live project reviewers: {}",
                eligible.join(", ")
            ),
        }
    };
    let assigned = reason == "exactly one eligible live project reviewer";
    let assignees = if assigned { eligible } else { Vec::new() };
    if assigned {
        conn.execute(
            "UPDATE tasks SET assignees=?1 WHERE id=?2",
            params![super::py_json_list(&assignees), id],
        )
        .map_err(db::map_sqlite)?;
    }
    let meta = json!({"kind":"decision","rule":RULE,"reason":reason,"project":project,
        "candidates":considered,"known_authors":authors,"assignees":assignees});
    conn.execute(
        "INSERT INTO task_events(task_id,event,at,actor,meta) VALUES (?1,?2,?3,'review-sync',?4)",
        params![
            id,
            if assigned {
                "assigned"
            } else {
                "review_routing"
            },
            now,
            meta.to_string()
        ],
    )
    .map_err(db::map_sqlite)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::{AddOptions, Tasks};

    fn candidate(pane: &str) -> Candidate {
        let pid = i64::from(std::process::id());
        Candidate {
            pane: pane.into(),
            provider: Some("codex".into()),
            identity: Some((pid, crate::proc::process_start(pid).unwrap())),
            rejection: None,
        }
    }

    async fn setup(tag: &str, panes: &[&str]) -> crate::testsupport::Scratch<Tasks> {
        let dir = crate::testsupport::dir(&format!("initial-review-routing-{tag}"));
        let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
        let conn = Connection::open(tasks.path()).unwrap();
        conn.execute(
            "INSERT INTO planner_repo_projects(repo,project) VALUES ('o/r','alpha')",
            [],
        )
        .unwrap();
        for pane in panes {
            let (pid, start) = candidate(pane).identity.unwrap();
            lane_project::set(
                &conn,
                &lane_project::Membership {
                    pane: (*pane).into(),
                    pid,
                    start,
                    scope: lane_project::ProjectScope::Project("alpha".into()),
                },
            )
            .unwrap();
        }
        crate::testsupport::Scratch::new(tasks, dir)
    }

    async fn file(
        tasks: &Tasks,
        snapshot: Snapshot,
        author: Option<&str>,
    ) -> (super::super::Task, serde_json::Value) {
        let task = tasks
            .add_review_with_routing_guard(
                "global",
                "review",
                "REVIEW o/r#7 @ head",
                AddOptions {
                    kind: Some("review".into()),
                    created_by: Some("review-sync".into()),
                    author: author.map(str::to_owned),
                    repo: Some("o/r".into()),
                    pr: Some("7".into()),
                    head: Some("head".into()),
                    ..Default::default()
                },
                &[],
                Some(snapshot),
            )
            .await
            .unwrap()
            .unwrap();
        let conn = Connection::open(tasks.path()).unwrap();
        let (event,actor,meta): (String,String,String) = conn.query_row(
            "SELECT event,actor,meta FROM task_events WHERE task_id=?1 ORDER BY id DESC LIMIT 1",
            [&task.id],|row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
        assert_eq!("review-sync", actor);
        let meta: serde_json::Value = serde_json::from_str(&meta).unwrap();
        assert_eq!(RULE, meta["rule"]);
        assert_eq!("decision", meta["kind"]);
        assert_eq!(
            if decode_list(Some(&task.assignees)).is_empty() {
                "review_routing"
            } else {
                "assigned"
            },
            event
        );
        assert_eq!("open", task.status);
        assert!(tasks
            .review_facts(&task.id)
            .await
            .unwrap()
            .verdict
            .is_none());
        (task, meta)
    }

    #[tokio::test]
    async fn sole_reviewer_is_assigned_before_creation_is_observed() {
        let tasks = setup("single", &["%2"]).await;
        let path = tasks.path().to_path_buf();
        let observed = std::sync::Arc::new(std::sync::Mutex::new(None));
        let observation = observed.clone();
        tasks.on_event(Box::new(move |id, event, _, _| {
            if event == "created" {
                let conn = Connection::open(&path).unwrap();
                let raw: String = conn
                    .query_row("SELECT assignees FROM tasks WHERE id=?1", [id], |r| {
                        r.get(0)
                    })
                    .unwrap();
                *observation.lock().unwrap() = Some(decode_list(Some(&raw)));
            }
        }));
        let (task, meta) = file(
            &tasks,
            Snapshot {
                candidates: vec![candidate("%2")],
                unavailable: None,
            },
            Some("%1"),
        )
        .await;
        assert_eq!(Some(vec!["%2".to_string()]), *observed.lock().unwrap());
        assert_eq!(vec!["%2"], decode_list(Some(&task.assignees)));
        assert_eq!("alpha", meta["project"]);
        assert_eq!(Some(true), meta["candidates"][0]["eligible"].as_bool());
    }

    #[tokio::test]
    async fn author_and_all_recorded_contributors_are_excluded() {
        let tasks = setup("contributors", &["%1", "%2", "%3", "%4", "%5"]).await;
        let work = tasks
            .add_with(
                "global",
                "work",
                "",
                AddOptions {
                    repo: Some("o/r".into()),
                    pr: Some("7".into()),
                    assignees: vec!["%2".into()],
                    author: Some("%3".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        Connection::open(tasks.path())
            .unwrap()
            .execute("UPDATE tasks SET completed_by='%4' WHERE id=?1", [work.id])
            .unwrap();
        let (task, meta) = file(
            &tasks,
            Snapshot {
                candidates: (1..=5).map(|n| candidate(&format!("%{n}"))).collect(),
                unavailable: None,
            },
            Some("%1"),
        )
        .await;
        assert_eq!(vec!["%5"], decode_list(Some(&task.assignees)));
        assert_eq!(json!(["%1", "%2", "%3", "%4"]), meta["known_authors"]);
        for row in &meta["candidates"].as_array().unwrap()[..4] {
            assert_eq!("recorded author or contributor", row["reason"]);
        }
    }

    #[tokio::test]
    async fn several_reviewers_remain_unassigned_with_candidates_and_reason() {
        let tasks = setup("several", &["%305", "%314"]).await;
        let (task, meta) = file(
            &tasks,
            Snapshot {
                candidates: vec![candidate("%314"), candidate("%305")],
                unavailable: None,
            },
            Some("%1"),
        )
        .await;
        assert!(decode_list(Some(&task.assignees)).is_empty());
        assert_eq!(
            "multiple eligible live project reviewers: %305, %314",
            meta["reason"]
        );
        assert_eq!(2, meta["candidates"].as_array().unwrap().len());
    }

    #[tokio::test]
    async fn dead_stale_wrong_project_global_and_policy_rejections_fail_closed() {
        let tasks = setup(
            "rejections",
            &["%dead", "%stale", "%other", "%global", "%policy"],
        )
        .await;
        let conn = Connection::open(tasks.path()).unwrap();
        conn.execute(
            "UPDATE lane_project_membership SET proc_start='stale' WHERE pane_id='%stale'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE lane_project_membership SET project='beta' WHERE pane_id='%other'",
            [],
        )
        .unwrap();
        conn.execute("UPDATE lane_project_membership SET scope_kind='global',project=NULL WHERE pane_id='%global'",[]).unwrap();
        let mut dead = candidate("%dead");
        dead.identity.as_mut().unwrap().1 = "not-the-live-start".into();
        let mut policy = candidate("%policy");
        policy.rejection = Some("provider rejected by reviewer policy".into());
        let (task, meta) = file(
            &tasks,
            Snapshot {
                candidates: vec![
                    dead,
                    candidate("%stale"),
                    candidate("%other"),
                    candidate("%global"),
                    policy,
                ],
                unavailable: None,
            },
            Some("%1"),
        )
        .await;
        assert!(decode_list(Some(&task.assignees)).is_empty());
        assert_eq!("no eligible live project reviewer", meta["reason"]);
        assert!(meta["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["eligible"] == false && c["reason"].is_string()));
    }

    #[tokio::test]
    async fn missing_inventory_project_or_provenance_never_assigns() {
        for mode in ["inventory", "project", "author", "zero"] {
            let tasks = setup(mode, &["%2"]).await;
            if mode == "project" {
                Connection::open(tasks.path())
                    .unwrap()
                    .execute("DELETE FROM planner_repo_projects", [])
                    .unwrap();
            }
            let snapshot = Snapshot {
                candidates: if mode == "zero" {
                    vec![]
                } else {
                    vec![candidate("%2")]
                },
                unavailable: (mode == "inventory").then(|| "backend failed".into()),
            };
            let (task, meta) = file(
                &tasks,
                snapshot,
                if mode == "author" { None } else { Some("%1") },
            )
            .await;
            assert!(decode_list(Some(&task.assignees)).is_empty(), "{mode}");
            assert!(meta["reason"].is_string());
        }
    }

    #[tokio::test]
    async fn second_review_routing_is_refused_without_writes() {
        let tasks = setup("second", &["%2"]).await;
        assert!(tasks
            .add_review_with_routing_guard(
                "global",
                "second",
                "",
                AddOptions {
                    kind: Some("review".into()),
                    created_by: Some("review-sync".into()),
                    ..Default::default()
                },
                &["accepted-id".into()],
                Some(Snapshot::default())
            )
            .await
            .is_err());
        let count: i64 = Connection::open(tasks.path())
            .unwrap()
            .query_row("SELECT COUNT(*) FROM tasks", [], |r| r.get(0))
            .unwrap();
        assert_eq!(0, count);
    }
}
