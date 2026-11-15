//! Planner corrections append history; they never erase a declared contributor.
use super::*;
type PrimaryOverride = (String, String, String, String);
pub(super) fn latest(
    conn: &Connection,
    repo: &str,
    pr: i64,
    head: &str,
) -> Result<Option<PrimaryOverride>, DunkError> {
    conn.query_row("SELECT pane,task_id,planner,reason FROM pr_author_corrections WHERE repo=?1 AND pr=?2 AND head=?3 ORDER BY id DESC LIMIT 1",params![repo,pr,head],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(db::map_sqlite)
}
impl Tasks {
    /// Only the API's freshly verified planner identity may reach this operation.
    #[allow(clippy::too_many_arguments)]
    pub async fn correct_pr_author(
        &self,
        repo: &str,
        pr: i64,
        head: &str,
        task: &str,
        author: &str,
        reason: &str,
        planner: planner_scope::PlannerIdentity,
        now: f64,
    ) -> Result<Attribution, DunkError> {
        if reason.trim().is_empty()
            || reason.len() > 16_384
            || head.len() != 40
            || !head.bytes().all(|b| b.is_ascii_hexdigit())
            || pr <= 0
            || repo.is_empty()
            || author == "global"
            || !validate_scope(author).is_ok_and(|v| v == author)
        {
            return Err(DunkError::Refused(
                "correction needs a full head, work task, pane ID and nonempty reason".into(),
            ));
        }
        let (repo, head, task, author, reason) = (
            repo.to_owned(),
            head.to_ascii_lowercase(),
            task.to_owned(),
            author.to_owned(),
            reason.trim().to_owned(),
        );
        let request = CorrectionRequest {
            repo,
            pr,
            head,
            task,
            author,
            reason,
            planner,
            now,
        };
        self.writer
            .call(move |conn| correct_in(conn, request))
            .await
    }
}

struct CorrectionRequest {
    repo: String,
    pr: i64,
    head: String,
    task: String,
    author: String,
    reason: String,
    planner: planner_scope::PlannerIdentity,
    now: f64,
}
fn correct_in(conn: &mut Connection, request: CorrectionRequest) -> Result<Attribution, DunkError> {
    let CorrectionRequest {
        repo,
        pr,
        head,
        task,
        author,
        reason,
        planner,
        now,
    } = request;

    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(db::map_sqlite)?;
    let expected_project = planner_scope::review_creation_project(&tx, &planner, Some(&repo))
        .map_err(db::map_sqlite)?;
    let task_project: Option<String> = tx
        .query_row(
            "SELECT project FROM planner_task_projects WHERE task_id=?1",
            [&task],
            |r| r.get(0),
        )
        .optional()
        .map_err(db::map_sqlite)?;
    if !crate::proc::process_alive(planner.pid, Some(&planner.start))
        || expected_project.is_none()
        || expected_project.as_ref() != Some(&task_project)
        || !planner_scope::allows(&tx, &planner.pane, planner.pid, &planner.start, &task)
            .map_err(db::map_sqlite)?
        || planner_scope::is_mapped_foreign(&tx, &planner, &task).map_err(db::map_sqlite)?
    {
        return Err(DunkError::Refused(
            "live planner grant must authorize the repository and work task".into(),
        ));
    }
    let before = attribution(&tx, &repo, pr, &head)?
        .filter(|a| a.author.is_some())
        .ok_or_else(|| DunkError::Refused("no declaration exists for this exact head".into()))?;
    if planner.pane == author || before.contributors.contains(&planner.pane) {
        return Err(DunkError::Refused(
            "a contributor cannot correct its own PR attribution".into(),
        ));
    }
    type Work = (
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
    );
    let (kind, raw, completer, recorded_author, old_repo, old_pr): Work = tx
        .query_row(
            "SELECT kind,assignees,completed_by,author,repo,pr FROM tasks WHERE id=?1",
            [&task],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .map_err(db::map_sqlite)?;
    let mut workers = decode_list(Some(&raw));
    workers.extend(completer);
    workers.extend(recorded_author);
    if kind != "work" || !workers.contains(&author) {
        return Err(DunkError::Refused(
            "corrected author must be associated with the named work task".into(),
        ));
    }
    if let (Some(old_repo), Some(old_pr)) = (old_repo.as_deref(), old_pr) {
        if old_repo != repo || old_pr != pr {
            for worker in &workers {
                if !worker.is_empty() {
                    tx.execute(
                        "INSERT OR IGNORE INTO pr_author_exclusions VALUES (?1,?2,?3,?4,?5,?6,?7)",
                        params![old_repo, old_pr, worker, task, planner.pane, reason, now],
                    )
                    .map_err(db::map_sqlite)?;
                }
            }
        }
    }
    let old_author = before.author.as_deref().unwrap();
    let audit = json!({"kind":"correction","reason":reason,"pr_author_correction":{"repo":repo,"pr":pr,"head":head,"before":old_author,"after":author,"planner":planner.pane,"planner_binding":{"pid":planner.pid,"start":planner.start},"task":task,"binding_before":{"repo":old_repo,"pr":old_pr},"binding_after":{"repo":repo,"pr":pr},"preserved_workers":workers}});
    tx.execute(
        "UPDATE tasks SET repo=?1,pr=?2,updated_at=?3 WHERE id=?4",
        params![repo, pr, now, task],
    )
    .map_err(db::map_sqlite)?;
    // Also retain the corrected writer in the original-format exclusion table.
    tx.execute("INSERT OR IGNORE INTO pr_authorship(repo,pr,head,pane,task_id,at) VALUES (?1,?2,?3,?4,?5,?6)",params![repo,pr,head,author,task,now]).map_err(db::map_sqlite)?;
    tx.execute("INSERT INTO pr_author_corrections(repo,pr,head,pane,task_id,planner,prior_author,reason,at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![repo,pr,head,author,task,planner.pane,old_author,reason,now]).map_err(db::map_sqlite)?;
    tx.execute(
        "INSERT INTO task_events(task_id,event,at,actor,meta) VALUES (?1,'note',?2,?3,?4)",
        params![task, now, planner.pane, audit.to_string()],
    )
    .map_err(db::map_sqlite)?;
    let contributors = contributors(&tx, &repo, pr).map_err(db::map_sqlite)?;
    type Review = (String, Option<String>, String, String, String);
    let reviews: Vec<Review> = {
        let mut s=tx.prepare("SELECT id,author,assignees,body,status FROM tasks WHERE kind='review' AND repo=?1 AND pr=?2 AND head=?3 AND status IN ('open','in_progress','blocked') AND (verdict IS NULL OR verdict='')").map_err(db::map_sqlite)?;
        let mapped = s
            .query_map(params![repo, pr, head], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .map_err(db::map_sqlite)?;
        mapped
            .collect::<rusqlite::Result<_>>()
            .map_err(db::map_sqlite)?
    };
    for (id, prior, raw, mut body, status) in reviews {
        let review_project: Option<String> = tx
            .query_row(
                "SELECT project FROM planner_task_projects WHERE task_id=?1",
                [&id],
                |r| r.get(0),
            )
            .optional()
            .map_err(db::map_sqlite)?;
        if expected_project.as_ref() != Some(&review_project)
            || !planner_scope::allows(&tx, &planner.pane, planner.pid, &planner.start, &id)
                .map_err(db::map_sqlite)?
        {
            continue;
        }
        let new = if prior
            .as_deref()
            .is_none_or(|a| a.is_empty() || a == old_author)
        {
            let label = prior
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or("unknown (not attributable; say so in the verdict)");
            body = body.replace(
                &format!("author lane: {label}"),
                &format!("author lane: {author}"),
            );
            body.push_str(&format!(
                "\nAuthor provenance: planner correction by {}; reason {}\n",
                planner.pane,
                json!(reason)
            ));
            Some(author.clone())
        } else {
            prior
        };
        let assignees: Vec<_> = decode_list(Some(&raw))
            .into_iter()
            .filter(|p| !contributors.contains(p))
            .collect();
        let mut revoked = 0;
        for pane in &contributors {
            revoked += tx
                .execute(
                    "DELETE FROM task_leases WHERE task_id=?1 AND pane_id=?2",
                    params![id, pane],
                )
                .map_err(db::map_sqlite)?;
        }
        let remaining: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM task_leases WHERE task_id=?1)",
                [&id],
                |r| r.get(0),
            )
            .map_err(db::map_sqlite)?;
        if revoked > 0 && !remaining {
            super::super::retire_member(&tx, &id)?;
        }
        let status = if status == "in_progress" && revoked > 0 && !remaining {
            "open"
        } else {
            &status
        };
        tx.execute(
            "UPDATE tasks SET author=?1,assignees=?2,body=?3,status=?4,updated_at=?5 WHERE id=?6",
            params![
                new,
                super::super::py_json_list(&assignees),
                body,
                status,
                now,
                id
            ],
        )
        .map_err(db::map_sqlite)?;
        let mut review_audit = audit.clone();
        review_audit["revoked_contributor_leases"] = json!(revoked);
        review_audit["assignees"] = json!(assignees);
        tx.execute(
            "INSERT INTO task_events(task_id,event,at,actor,meta) VALUES (?1,'note',?2,?3,?4)",
            params![id, now, planner.pane, review_audit.to_string()],
        )
        .map_err(db::map_sqlite)?;
    }
    let result = attribution(&tx, &repo, pr, &head)?.expect("correction recorded");
    tx.commit().map_err(db::map_sqlite)?;
    Ok(result)
}
