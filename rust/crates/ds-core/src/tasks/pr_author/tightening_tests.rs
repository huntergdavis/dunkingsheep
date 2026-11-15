use super::tests::work;
use super::*;
fn state(tasks: &Tasks) -> Vec<String> {
    let db = Connection::open(tasks.path()).unwrap();
    let mut out = Vec::new();
    for table in [
        "tasks",
        "task_leases",
        "task_events",
        "pr_authorship",
        "pr_author_corrections",
        "pr_author_exclusions",
    ] {
        let mut stmt = db
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
            .unwrap();
        let rows = stmt
            .query_map([], |r| {
                Ok((0..r.as_ref().column_count())
                    .map(|i| format!("{:?}", r.get_ref(i).unwrap()))
                    .collect::<Vec<_>>()
                    .join("|"))
            })
            .unwrap();
        out.extend(rows.map(|r| format!("{table}:{}", r.unwrap())));
    }
    out
}
fn binding(tasks: &Tasks, id: &str) -> (Option<String>, Option<i64>) {
    Connection::open(tasks.path())
        .unwrap()
        .query_row("SELECT repo,pr FROM tasks WHERE id=?1", [id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap()
}
fn planner(
    tasks: &Tasks,
    pane: &str,
    project: &str,
    ids: &[&str],
) -> planner_scope::PlannerIdentity {
    let pid = i64::from(std::process::id());
    let start = crate::proc::process_start(pid).unwrap();
    let db = Connection::open(tasks.path()).unwrap();
    planner_scope::set_grant(
        &db,
        pane,
        pid,
        &start,
        &lane_project::ProjectScope::Project(project.into()),
    )
    .unwrap();
    db.execute(
        "INSERT OR REPLACE INTO planner_repo_projects VALUES ('o/r','alpha')",
        [],
    )
    .unwrap();
    for id in ids {
        db.execute(
            "INSERT OR REPLACE INTO planner_task_projects VALUES (?1,'alpha')",
            [id],
        )
        .unwrap();
    }
    planner_scope::PlannerIdentity {
        pane: pane.into(),
        pid,
        start,
        recipients: vec![],
        expected_kind: None,
    }
}
#[tokio::test]
async fn author_binding_is_atomic_and_mismatch_or_idempotent_repeat_writes_nothing() {
    let dir = crate::testsupport::dir("author-bind-once");
    let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
    let (id, caller) = work(&tasks, "%1").await;
    assert_eq!(binding(&tasks, &id), (None, None));
    let h = "a".repeat(40);
    tasks
        .declare_pr_author(&id, "o/r", 7, &h, "%1", &caller, crate::clock::now_s())
        .await
        .unwrap();
    assert_eq!(binding(&tasks, &id), (Some("o/r".into()), Some(7)));
    let before = state(&tasks);
    assert!(tasks
        .declare_pr_author(&id, "o/r", 8, &h, "%1", &caller, crate::clock::now_s())
        .await
        .is_err());
    assert_eq!(before, state(&tasks));
    assert!(tasks
        .declare_pr_author(&id, "other/r", 7, &h, "%1", &caller, crate::clock::now_s())
        .await
        .is_err());
    assert_eq!(before, state(&tasks));
    tasks
        .declare_pr_author(&id, "o/r", 7, &h, "%1", &caller, crate::clock::now_s())
        .await
        .unwrap();
    assert_eq!(before, state(&tasks));
    tasks
        .declare_pr_author(
            &id,
            "o/r",
            7,
            &"b".repeat(40),
            "%1",
            &caller,
            crate::clock::now_s(),
        )
        .await
        .unwrap();
    assert_eq!(binding(&tasks, &id), (Some("o/r".into()), Some(7)));
}
#[tokio::test]
async fn author_binding_concurrent_first_declarations_cannot_split_a_work_task() {
    let dir = crate::testsupport::dir("author-bind-race");
    let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
    let (id, caller) = work(&tasks, "%1").await;
    let h = "a".repeat(40);
    let now = crate::clock::now_s();
    let (a, b) = tokio::join!(
        tasks.declare_pr_author(&id, "o/r", 7, &h, "%1", &caller, now),
        tasks.declare_pr_author(&id, "o/r", 8, &h, "%1", &caller, now)
    );
    assert_ne!(a.is_ok(), b.is_ok());
    let bound = binding(&tasks, &id).1.unwrap();
    assert_eq!(bound, if a.is_ok() { 7 } else { 8 });
    assert_eq!(
        Connection::open(tasks.path())
            .unwrap()
            .query_row("SELECT COUNT(*) FROM pr_authorship", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}
#[tokio::test]
async fn author_binding_planner_correction_keeps_history_and_repairs_live_author_route() {
    let dir = crate::testsupport::dir("author-correct");
    let path = dir.path().join("tasks.db");
    let tasks = Tasks::open(&path).await.unwrap();
    let (wrong, c1) = work(&tasks, "%1").await;
    let (real, _) = work(&tasks, "%2").await;
    let h = "a".repeat(40);
    tasks
        .declare_pr_author(&wrong, "o/r", 7, &h, "%1", &c1, crate::clock::now_s())
        .await
        .unwrap();
    // The coordinator may explicitly override a work-task binding, with that old value audited.
    Connection::open(tasks.path())
        .unwrap()
        .execute("UPDATE tasks SET repo='o/r',pr=99 WHERE id=?1", [&real])
        .unwrap();
    let p = planner(&tasks, "%9", "alpha", &[&wrong, &real]);
    let review = tasks
        .add_with(
            "global",
            "review",
            "author lane: %1",
            AddOptions {
                kind: Some("review".into()),
                author: Some("%1".into()),
                repo: Some("o/r".into()),
                pr: Some("7".into()),
                head: Some(h.clone()),
                assignees: vec!["%2".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
    Connection::open(tasks.path())
        .unwrap()
        .execute(
            "INSERT OR REPLACE INTO planner_task_projects VALUES (?1,'alpha')",
            [&review.id],
        )
        .unwrap();
    assert!(matches!(
        tasks
            .claim_next(
                "global",
                "%2",
                ClaimOptions {
                    allow_busy: true,
                    kinds: Some(vec!["review".into()]),
                    exclude_author: Some("%2".into()),
                    ..Default::default()
                }
            )
            .await
            .unwrap(),
        ClaimOutcome::Claimed { .. }
    ));
    let result = tasks
        .correct_pr_author(
            "o/r",
            7,
            &h,
            &real,
            "%2",
            "wrong first writer",
            p,
            crate::clock::now_s(),
        )
        .await
        .unwrap();
    assert_eq!(result.author.as_deref(), Some("%2"));
    assert_eq!(result.corrected_by.as_deref(), Some("%9"));
    assert_eq!(result.contributors, vec!["%1", "%2"]);
    assert_eq!(binding(&tasks, &real), (Some("o/r".into()), Some(7)));
    assert_eq!(
        tasks.authors_of("o/r", 99).await.unwrap(),
        vec!["%2"],
        "rebinding must preserve previous related-work exclusions"
    );
    let r = tasks.get_full_wire(&review.id, None).await.unwrap();
    assert_eq!(r.wire["author"], "%2");
    assert_eq!(r.wire["assignees"], json!([]));
    assert_eq!(r.wire["status"], "open");
    assert!(r.wire["body"]
        .as_str()
        .unwrap()
        .contains("planner correction by %9"));
    let db = Connection::open(tasks.path()).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM task_leases WHERE task_id=?1",
            [&review.id],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    let meta: String = db
        .query_row(
            "SELECT meta FROM task_events WHERE task_id=?1 AND actor='%9' ORDER BY id DESC LIMIT 1",
            [&real],
            |r| r.get(0),
        )
        .unwrap();
    let audit: JsonValue = serde_json::from_str(&meta).unwrap();
    assert_eq!(audit["pr_author_correction"]["before"], "%1");
    assert_eq!(audit["pr_author_correction"]["after"], "%2");
    assert_eq!(audit["pr_author_correction"]["binding_before"]["pr"], 99);
    assert_eq!(audit["reason"], "wrong first writer");
    assert!(tasks
        .set_verdict_with_author_guard(&review.id, "accept", Some("%2"), true)
        .await
        .is_err());
    tasks
        .declare_pr_author(&wrong, "o/r", 7, &h, "%1", &c1, crate::clock::now_s())
        .await
        .unwrap();
    assert_eq!(
        tasks
            .pr_author("o/r", 7, &h)
            .await
            .unwrap()
            .unwrap()
            .author
            .as_deref(),
        Some("%2")
    );
    drop(db);
    drop(tasks);
    let reopened = Tasks::open(&path).await.unwrap();
    assert_eq!(
        reopened
            .pr_author("o/r", 7, &h)
            .await
            .unwrap()
            .unwrap()
            .corrected_by
            .as_deref(),
        Some("%9")
    );
}
#[tokio::test]
async fn author_binding_correction_refuses_unscoped_stale_self_or_unaudited_changes_without_writes()
{
    let dir = crate::testsupport::dir("author-correction-refuse");
    let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
    let (a, c) = work(&tasks, "%1").await;
    let (b, _) = work(&tasks, "%2").await;
    let h = "a".repeat(40);
    tasks
        .declare_pr_author(&a, "o/r", 7, &h, "%1", &c, crate::clock::now_s())
        .await
        .unwrap();
    let p = planner(&tasks, "%9", "alpha", &[&a, &b]);
    let before = state(&tasks);
    assert!(tasks
        .correct_pr_author(
            "o/r",
            7,
            &h,
            &b,
            "%2",
            " ",
            p.clone(),
            crate::clock::now_s()
        )
        .await
        .is_err());
    assert_eq!(before, state(&tasks));
    let mut stale = p.clone();
    stale.start = "stale".into();
    assert!(tasks
        .correct_pr_author(
            "o/r",
            7,
            &h,
            &b,
            "%2",
            "reason",
            stale,
            crate::clock::now_s()
        )
        .await
        .is_err());
    assert_eq!(before, state(&tasks));
    assert!(tasks
        .correct_pr_author(
            "o/r",
            7,
            &h,
            &b,
            "%9",
            "reason",
            p.clone(),
            crate::clock::now_s()
        )
        .await
        .is_err());
    assert_eq!(before, state(&tasks));
    assert!(tasks
        .correct_pr_author(
            "o/r",
            7,
            &h,
            &b,
            "%3",
            "reason",
            p.clone(),
            crate::clock::now_s()
        )
        .await
        .is_err());
    assert_eq!(before, state(&tasks));
    assert!(tasks
        .correct_pr_author(
            "o/r",
            7,
            &"b".repeat(40),
            &b,
            "%2",
            "reason",
            p.clone(),
            crate::clock::now_s()
        )
        .await
        .is_err());
    assert_eq!(before, state(&tasks));
    let own = planner(&tasks, "%1", "alpha", &[&a, &b]);
    let before = state(&tasks);
    assert!(tasks
        .correct_pr_author("o/r", 7, &h, &b, "%2", "reason", own, crate::clock::now_s())
        .await
        .is_err());
    assert_eq!(before, state(&tasks));
    let wrong = planner(&tasks, "%9", "beta", &[&a, &b]);
    let before = state(&tasks);
    assert!(tasks
        .correct_pr_author(
            "o/r",
            7,
            &h,
            &b,
            "%2",
            "reason",
            wrong,
            crate::clock::now_s()
        )
        .await
        .is_err());
    assert_eq!(before, state(&tasks));
    planner(&tasks, "%9", "alpha", &[&a, &b]);
    Connection::open(tasks.path())
        .unwrap()
        .execute(
            "UPDATE planner_task_projects SET project='beta' WHERE task_id=?1",
            [&b],
        )
        .unwrap();
    let before = state(&tasks);
    assert!(tasks
        .correct_pr_author("o/r", 7, &h, &b, "%2", "reason", p, crate::clock::now_s())
        .await
        .is_err());
    assert_eq!(before, state(&tasks));
}
