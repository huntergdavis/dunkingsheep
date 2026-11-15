//! Refresh unknown, unjudged reviews in the declaration transaction (t5349).
use super::*;

pub(super) fn declared(
    conn: &Connection,
    repo: &str,
    pr: i64,
    head: &str,
    attribution: &Attribution,
    now: f64,
) -> Result<(), DunkError> {
    let Some(author) = attribution.author.as_deref() else {
        return Ok(());
    };
    let ids = {
        let mut statement = conn.prepare(
            "SELECT id FROM tasks WHERE kind='review' AND repo=?1 AND pr=?2 AND head=?3 AND status IN ('open','in_progress','blocked') AND (author IS NULL OR author='') AND (verdict IS NULL OR verdict='')",
        ).map_err(db::map_sqlite)?;
        let ids = statement
            .query_map(params![repo, pr, head], |row| row.get::<_, String>(0))
            .map_err(db::map_sqlite)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(db::map_sqlite)?;
        ids
    };
    let provenance = if let Some(planner) = &attribution.corrected_by {
        format!(
            "planner correction by {planner}; task {}; reason {}; accumulated contributors: {}",
            attribution.task.as_deref().unwrap_or("?"),
            json!(attribution.correction_reason.as_deref().unwrap_or("?")),
            attribution.contributors.join(", ")
        )
    } else {
        format!(
            "guarded author-task declaration; task {}; accumulated contributors: {}",
            attribution.task.as_deref().unwrap_or("?"),
            attribution.contributors.join(", ")
        )
    };
    for id in ids {
        one(conn, &id, head, author, Some(&provenance), now)?;
    }
    Ok(())
}

pub(in crate::tasks) fn one(
    conn: &Connection,
    id: &str,
    head: &str,
    author: &str,
    provenance: Option<&str>,
    now: f64,
) -> Result<bool, DunkError> {
    let row: Option<(String, String, bool)> = conn.query_row(
        "SELECT body,assignees,EXISTS(SELECT 1 FROM task_leases WHERE task_id=tasks.id AND pane_id=?3) FROM tasks WHERE id=?1 AND head=?2 AND kind='review' AND status IN ('open','in_progress','blocked') AND (author IS NULL OR author='') AND (verdict IS NULL OR verdict='')",
        params![id,head,author], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
    ).optional().map_err(db::map_sqlite)?;
    let Some((body, raw, leased)) = row else {
        return Ok(false);
    };
    // Review-sync appends this generated line after the PR body. Replace only that
    // final line, leaving the author's own PR text intact.
    let body = body
        .rsplit_once("\nAuthor provenance: ")
        .filter(|(_, tail)| !tail.trim_end_matches('\n').contains('\n'))
        .map_or(body.as_str(), |(prefix, _)| prefix)
        .replace(
            "author lane: unknown (not attributable; say so in the verdict)",
            &format!("author lane: {author}"),
        );
    let provenance =
        provenance.unwrap_or("current-head explicit work evidence; see author refresh note");
    let body = format!("{body}\nAuthor provenance: {provenance}\n");
    conn.execute(
        "UPDATE tasks SET author=?2,body=?3,updated_at=?4 WHERE id=?1",
        params![id, author, body, now],
    )
    .map_err(db::map_sqlite)?;
    let conflict = leased || decode_list(Some(&raw)).iter().any(|pane| pane == author);
    let suffix = if conflict {
        "; author is assigned or claimed: coordinator must route an independent reviewer"
    } else {
        ""
    };
    conn.execute(
        "INSERT INTO task_events(task_id,event,at,actor,meta) VALUES (?1,'note',?2,'review-sync',?3)",
        params![id,now,json!({"reason":format!("Review author refreshed from explicit PR work evidence: {author}{suffix}"),"head":head,"author":author,"provenance":provenance}).to_string()],
    ).map_err(db::map_sqlite)?;
    Ok(true)
}
