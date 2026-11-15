//! Trusted review notices use the ordinary inbox authorization/routing path and an
//! atomic transport receipt. Unknown-author notices are keyed by PR, not by head.
use super::*;
fn send(
    context: &HandlerContext<'_>,
    to: &str,
    subject: &str,
    body: &str,
    key: &str,
) -> Result<Value, DunkError> {
    context.env.with_inbox_env(&mut |env| {
        let sent = context.flock.inbox_review_notice(
            env,
            &crate::flock::commands_inbox::SendArgs {
                to,
                subject: Some(subject),
                body,
                notify: true,
                self_pane_id: context.self_pane_id(),
                agent_only: true,
                ..Default::default()
            },
            key,
        )?;
        Ok(json!({"message_id":sent.message_id,"to":to}))
    })
}
pub(super) fn notify_unknown(context: &HandlerContext<'_>) -> Result<(), DunkError> {
    let Some(tasks) = context.flock.stores().tasks else {
        return Ok(());
    };
    for row in context.block_on(tasks.list_review_rows_visible())?? {
        if !matches!(row.status.as_str(), "open" | "in_progress" | "blocked") {
            continue;
        }
        let full = context.block_on(tasks.get_full_wire(&row.id, None))??;
        let wire = &full.wire;
        if !matches!(
            wire["status"].as_str(),
            Some("open" | "in_progress" | "blocked")
        ) || wire["author"].as_str().is_some_and(|a| !a.is_empty())
        {
            continue;
        }
        let (Some(repo), Some(pr)) = (wire["repo"].as_str(), wire["pr"].as_i64()) else {
            continue;
        };
        if context.block_on(tasks.author_notice_sent(repo, pr))?? {
            continue;
        }
        let coordinators = context.block_on(tasks.review_coordinators(repo))??;
        // One PR-wide notice, addressed only to the explicitly granted project planner.
        let Some(to) = coordinators.first() else {
            continue;
        };
        let subject = format!("Review author unknown: {repo}#{pr}");
        let body=format!("Review filed for {repo}#{pr} with author unknown. Automatic reviewer assignment is held. An independent coordinator must route it; no author is inferred from Git identity, branch labels or ordinary PR waits. The writer should declare the current head with `dunkingsheep pr wait {repo}#{pr} --head FULL_SHA --author-task TASK_ID`. See `dunkingsheep review list`. This notice is sent once per PR, including across later pushes and restarts.");
        let sent = send(
            context,
            to,
            &subject,
            &body,
            &format!("author-unknown:{repo}#{pr}"),
        )?;
        context.block_on(tasks.mark_author_notice_sent(
            repo,
            pr,
            sent["message_id"].as_str().unwrap(),
        ))??;
    }
    Ok(())
}
pub(crate) fn notify_verdict(
    context: &HandlerContext<'_>,
    task: &Value,
    verdict: &str,
) -> Result<Option<Value>, DunkError> {
    let (Some(author), Some(repo), Some(pr), Some(head), Some(id)) = (
        task["author"].as_str().filter(|a| !a.is_empty()),
        task["repo"].as_str(),
        task["pr"].as_i64(),
        task["head"].as_str(),
        task["id"].as_str(),
    ) else {
        return Ok(None);
    };
    let subject = format!("Review {verdict}: {repo}#{pr}");
    let body=format!("Review {id} recorded {verdict} for {repo}#{pr} @ {head}. Read the evidence with `dunkingsheep task read {id}`. This notice reports the recorded verdict, not a merge decision.");
    send(
        context,
        author,
        &subject,
        &body,
        &format!("verdict:{repo}#{pr}:{head}:{id}:{verdict}"),
    )
    .map(Some)
}

pub(super) fn correct(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    if context
        .flock
        .caller_role(context.self_pane_id(), context.env)?
        != "planner"
    {
        return Err(DunkError::Refused(
            "authorship correction requires a planner".into(),
        ));
    }
    let planner = super::require_planner_assignment_binding(context)?.ok_or_else(|| {
        DunkError::Refused("correction requires a live bound planner request".into())
    })?;
    let get = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| DunkError::Refused(format!("{key} is required")))
    };
    let target = get("target")?;
    let (repo, number) = target
        .rsplit_once('#')
        .ok_or_else(|| DunkError::Refused("expected owner/repo#PR".into()))?;
    let pr = number
        .parse::<i64>()
        .ok()
        .filter(|p| *p > 0)
        .ok_or_else(|| DunkError::Refused("expected positive PR".into()))?;
    let tokens = context.env.github_tokens()?;
    let policy = crate::review::ConfiguredReviewPolicy {
        config_dir: tokens.config_dir().to_path_buf(),
    };
    if !crate::review::ReviewRepoConfig::repos(&policy)
        .map_err(DunkError::Refused)?
        .iter()
        .any(|r| r == repo)
    {
        return Err(DunkError::Refused(
            "repository is not in review registry".into(),
        ));
    }
    let head = get("head")?.to_ascii_lowercase();
    let task = get("author_task")?;
    let author = get("author")?;
    let reason = get("reason")?;
    let tasks = context
        .flock
        .stores()
        .tasks
        .ok_or_else(|| DunkError::Refused("task store unavailable".into()))?;
    let a = context.block_on(tasks.correct_pr_author(
        repo,
        pr,
        &head,
        task,
        author,
        reason,
        planner,
        context.env.now_s(),
    ))??;
    context.flock.wake_push();
    Ok(
        json!({"corrected":true,"authorship":{"author":a.author,"task":a.task,"contributors":a.contributors,"head":head,"corrected_by":a.corrected_by,"correction_reason":a.correction_reason,"task_binding":{"repo":repo,"pr":pr}}}),
    )
}
