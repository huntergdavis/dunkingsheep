//! Guarded, durable declarations; an ordinary CI observer is never an author.
use super::*;

type LeaseProof = (
    String,
    String,
    String,
    Option<String>,
    Option<i64>,
    Option<String>,
);

pub(crate) const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS pr_authorship (
 id INTEGER PRIMARY KEY AUTOINCREMENT, repo TEXT NOT NULL, pr INTEGER NOT NULL,
 head TEXT NOT NULL, pane TEXT NOT NULL, task_id TEXT NOT NULL, at REAL NOT NULL,
 UNIQUE(repo,pr,head,pane));
 CREATE INDEX IF NOT EXISTS pr_authorship_pr ON pr_authorship(repo,pr,id);
 CREATE TABLE IF NOT EXISTS pr_author_notice_sent(repo TEXT NOT NULL,pr INTEGER NOT NULL,message_id TEXT NOT NULL,PRIMARY KEY(repo,pr));";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attribution {
    pub author: Option<String>,
    pub task: Option<String>,
    pub contributors: Vec<String>,
}

/// All declarations remain exclusions, including those attached to earlier heads.
pub(crate) fn contributors(
    conn: &Connection,
    repo: &str,
    pr: i64,
) -> rusqlite::Result<Vec<String>> {
    conn.prepare("SELECT DISTINCT pane FROM pr_authorship WHERE repo=?1 AND pr=?2 ORDER BY pane")?
        .query_map(params![repo, pr], |r| r.get(0))?
        .collect()
}

impl Tasks {
    /// Caller identity is supplied by the socket dispatcher, never the request body.
    #[allow(clippy::too_many_arguments)]
    pub async fn declare_pr_author(
        &self,
        task: &str,
        repo: &str,
        pr: i64,
        head: &str,
        pane: &str,
        caller: &crate::proc::Caller,
        now: f64,
    ) -> Result<Attribution, DunkError> {
        if head.len() != 40
            || !head.bytes().all(|b| b.is_ascii_hexdigit())
            || pr <= 0
            || repo.is_empty()
        {
            return Err(DunkError::Refused(
                "authorship needs a repository, PR and full head".into(),
            ));
        }
        let (task, repo, head, pane, caller) = (
            task.to_owned(),
            repo.to_owned(),
            head.to_ascii_lowercase(),
            pane.to_owned(),
            caller.clone(),
        );
        self.writer.call(move |conn| {
            let tx=conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).map_err(db::map_sqlite)?;
            let row: Option<LeaseProof> = tx.query_row(
                "SELECT t.kind,t.status,t.assignees,t.repo,l.caller_pid,l.caller_start FROM tasks t JOIN task_leases l ON l.task_id=t.id WHERE t.id=?1 AND l.pane_id=?2 AND l.expires_at>?3",
                params![task,pane,now], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?)))
                .optional().map_err(db::map_sqlite)?;
            let Some((kind,status,assignees,bound_repo,pid,start))=row else {return Err(DunkError::Refused("author-task requires your live work-task lease".into()));};
            if kind!="work" || status!="in_progress" || !decode_list(Some(&assignees)).contains(&pane)
                || bound_repo.as_deref().is_some_and(|r|r!=repo)
                || caller.pane_id.as_deref()!=Some(pane.as_str()) || caller.pid.is_none() || caller.start.is_none()
                || pid!=caller.pid || start!=caller.start
                || !crate::proc::process_alive(caller.pid.unwrap(),caller.start.as_deref()) {
                return Err(DunkError::Refused("author-task requires your identity-bound live work-task lease".into()));
            }
            // No updates: the first declaration on a head is its primary author. Later
            // co-authors (and authors of future heads) accumulate as exclusions forever.
            let inserted=tx.execute("INSERT OR IGNORE INTO pr_authorship(repo,pr,head,pane,task_id,at) VALUES (?1,?2,?3,?4,?5,?6)",params![repo,pr,head,pane,task,now]).map_err(db::map_sqlite)?;
            if inserted>0 {
                tx.execute("INSERT INTO task_events(task_id,event,at,actor,meta) VALUES (?1,'note',?2,?3,?4)",params![task,now,pane,json!({"kind":"evidence","reason":format!("Declared PR authorship: {repo}#{pr} @ {head}"),"pr_authorship":{"repo":repo,"pr":pr,"head":head,"pane":pane}}).to_string()]).map_err(db::map_sqlite)?;
            }
            let result=attribution(&tx,&repo,pr,&head)?.expect("declaration exists");
            tx.commit().map_err(db::map_sqlite)?;
            Ok(result)
        }).await
    }
    pub async fn pr_author(
        &self,
        repo: &str,
        pr: i64,
        head: &str,
    ) -> Result<Option<Attribution>, DunkError> {
        let (repo, head) = (repo.to_owned(), head.to_owned());
        self.writer
            .call(move |conn| attribution(conn, &repo, pr, &head))
            .await
    }
    pub async fn author_notice_sent(&self, repo: &str, pr: i64) -> Result<bool, DunkError> {
        let repo = repo.to_owned();
        self.writer
            .call(move |c| {
                c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM pr_author_notice_sent WHERE repo=?1 AND pr=?2)",
                    params![repo, pr],
                    |r| r.get(0),
                )
                .map_err(db::map_sqlite)
            })
            .await
    }
    pub async fn mark_author_notice_sent(
        &self,
        repo: &str,
        pr: i64,
        id: &str,
    ) -> Result<(), DunkError> {
        let (repo, id) = (repo.to_owned(), id.to_owned());
        self.writer
            .call(move |c| {
                c.execute(
                    "INSERT OR IGNORE INTO pr_author_notice_sent VALUES (?1,?2,?3)",
                    params![repo, pr, id],
                )
                .map_err(db::map_sqlite)?;
                Ok(())
            })
            .await
    }
    /// Only owner-granted, live project planners are coordinator notice recipients.
    pub async fn review_coordinators(&self, repo: &str) -> Result<Vec<String>, DunkError> {
        let repo = repo.to_owned();
        self.writer.call(move |conn| {
            let mut stmt=conn.prepare("SELECT g.pane_id,g.pane_pid,g.proc_start FROM planner_scope_grants g JOIN planner_repo_projects p ON p.project=g.project WHERE p.repo=?1 AND g.scope_kind='project' ORDER BY g.pane_id").map_err(db::map_sqlite)?;
            let rows=stmt.query_map([repo],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?))).map_err(db::map_sqlite)?;
            let mut out=Vec::new();
            for row in rows {let(pane,pid,start)=row.map_err(db::map_sqlite)?; if crate::proc::process_alive(pid,Some(&start)){out.push(pane);}}
            Ok(out)
        }).await
    }
}
fn attribution(
    conn: &Connection,
    repo: &str,
    pr: i64,
    head: &str,
) -> Result<Option<Attribution>, DunkError> {
    let contributors = contributors(conn, repo, pr).map_err(db::map_sqlite)?;
    if contributors.is_empty() {
        return Ok(None);
    }
    let primary:Option<(String,String)>=conn.query_row("SELECT pane,task_id FROM pr_authorship WHERE repo=?1 AND pr=?2 AND head=?3 ORDER BY id LIMIT 1",params![repo,pr,head],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(db::map_sqlite)?;
    let (author, task) = primary.map(|(a, t)| (Some(a), Some(t))).unwrap_or_default();
    Ok(Some(Attribution {
        author,
        task,
        contributors,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn work(tasks: &Tasks, pane: &str) -> (String, crate::proc::Caller) {
        let task = tasks
            .add_with(
                "global",
                "authorship fixture",
                "private evidence",
                AddOptions {
                    assignees: vec![pane.into()],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let pid = i64::from(std::process::id());
        let start = crate::proc::process_start(pid).unwrap();
        tasks
            .claim_next(
                "global",
                pane,
                ClaimOptions {
                    allow_busy: true,
                    caller: Some((pid, Some(start.clone()), None)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        (
            task.id,
            crate::proc::Caller::new(Some(pid), Some(start), None, Some(pane.into()), "agent"),
        )
    }
    #[tokio::test]
    async fn review_author_claims_are_first_writer_per_head_and_keep_prior_contributors_after_restart(
    ) {
        let dir = crate::testsupport::dir("pr-author-accumulate");
        let path = dir.path().join("tasks.db");
        let tasks = Tasks::open(&path).await.unwrap();
        let (a, ca) = work(&tasks, "%1").await;
        let (b, cb) = work(&tasks, "%2").await;
        let h = "a".repeat(40);
        let next = "b".repeat(40);
        let first = tasks
            .declare_pr_author(&a, "o/r", 7, &h, "%1", &ca, crate::clock::now_s())
            .await
            .unwrap();
        assert_eq!(first.author.as_deref(), Some("%1"));
        tasks
            .declare_pr_author(&b, "o/r", 7, &h, "%2", &cb, crate::clock::now_s())
            .await
            .unwrap();
        // Repeating the original declaration is idempotent, not an ownership rewrite.
        tasks
            .declare_pr_author(&a, "o/r", 7, &h, "%1", &ca, crate::clock::now_s())
            .await
            .unwrap();
        let second = tasks
            .declare_pr_author(&b, "o/r", 7, &next, "%2", &cb, crate::clock::now_s())
            .await
            .unwrap();
        assert_eq!(second.author.as_deref(), Some("%2"));
        assert_eq!(second.contributors, vec!["%1", "%2"]);
        tasks.mark_author_notice_sent("o/r", 7, "i1").await.unwrap();
        drop(tasks);
        let reopened = Tasks::open(&path).await.unwrap();
        assert_eq!(
            reopened
                .pr_author("o/r", 7, &h)
                .await
                .unwrap()
                .unwrap()
                .author
                .as_deref(),
            Some("%1")
        );
        let stale = reopened
            .pr_author("o/r", 7, &"c".repeat(40))
            .await
            .unwrap()
            .unwrap();
        assert!(stale.author.is_none());
        assert_eq!(stale.contributors, vec!["%1", "%2"]);
        assert_eq!(
            reopened.authors_of("o/r", 7).await.unwrap(),
            vec!["%1", "%2"]
        );
        assert!(reopened.author_notice_sent("o/r", 7).await.unwrap());
        assert!(!reopened.author_notice_sent("o/r", 8).await.unwrap());
        // Ordinary pre-feature schema/setup remains usable and leaves new state intact.
        let old = Connection::open(&path).unwrap();
        old.execute_batch(super::super::SCHEMA).unwrap();
        assert_eq!(2, contributors(&old, "o/r", 7).unwrap().len());
    }
    #[tokio::test]
    async fn review_author_refuses_foreign_expired_unbound_or_review_task_leases() {
        let dir = crate::testsupport::dir("pr-author-refuse");
        let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
        let (id, caller) = work(&tasks, "%1").await;
        let head = "a".repeat(40);
        let now = crate::clock::now_s();
        assert!(tasks
            .declare_pr_author(&id, "o/r", 7, &head, "%2", &caller, now)
            .await
            .is_err());
        let mut forged = caller.clone();
        forged.start = Some("wrong process".into());
        assert!(tasks
            .declare_pr_author(&id, "o/r", 7, &head, "%1", &forged, now)
            .await
            .is_err());
        assert!(tasks
            .declare_pr_author(&id, "o/r", 7, &head, "%1", &caller, now + 3600.)
            .await
            .is_err());
        Connection::open(tasks.path())
            .unwrap()
            .execute("UPDATE tasks SET kind='review' WHERE id=?1", [&id])
            .unwrap();
        assert!(tasks
            .declare_pr_author(&id, "o/r", 7, &head, "%1", &caller, now)
            .await
            .is_err());
        assert!(tasks.pr_author("o/r", 7, &head).await.unwrap().is_none());
    }
    #[tokio::test]
    async fn review_author_coauthors_cannot_claim_or_record_a_verdict_for_a_later_head() {
        let dir = crate::testsupport::dir("pr-author-exclude");
        let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
        let (id, caller) = work(&tasks, "%1").await;
        tasks
            .declare_pr_author(
                &id,
                "o/r",
                7,
                &"a".repeat(40),
                "%1",
                &caller,
                crate::clock::now_s(),
            )
            .await
            .unwrap();
        let review = tasks
            .add_with(
                "global",
                "review",
                "",
                AddOptions {
                    kind: Some("review".into()),
                    author: Some("%2".into()),
                    repo: Some("o/r".into()),
                    pr: Some("7".into()),
                    head: Some("b".repeat(40)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let outcome = tasks
            .claim_next(
                "global",
                "%1",
                ClaimOptions {
                    allow_busy: true,
                    kinds: Some(vec!["review".into()]),
                    exclude_author: Some("%1".into()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(matches!(outcome, ClaimOutcome::Refused { .. }));
        assert!(tasks
            .set_verdict_with_author_guard(&review.id, "accept", Some("%1"), true)
            .await
            .is_err());
        assert!(tasks.verdict_of(&review.id).await.unwrap().is_none());
    }
    #[tokio::test]
    async fn review_author_unknown_is_not_auto_claimed_before_coordinator_assignment() {
        let dir = crate::testsupport::dir("pr-author-manual-route");
        let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
        let review = tasks
            .add_with(
                "global",
                "unknown review",
                "author unknown",
                AddOptions {
                    kind: Some("review".into()),
                    created_by: Some("review-sync".into()),
                    repo: Some("o/r".into()),
                    pr: Some("7".into()),
                    head: Some("a".repeat(40)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let options = ClaimOptions {
            kinds: Some(vec!["review".into()]),
            exclude_author: Some("%2".into()),
            ..Default::default()
        };
        assert!(matches!(
            tasks
                .claim_next("global", "%2", options.clone())
                .await
                .unwrap(),
            ClaimOutcome::Refused { .. }
        ));
        tasks.assign(&review.id, &["%2"], "add").await.unwrap();
        assert!(matches!(
            tasks.claim_next("global", "%2", options).await.unwrap(),
            ClaimOutcome::Claimed { .. }
        ));
    }
}
