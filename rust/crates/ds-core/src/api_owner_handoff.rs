//! Point-in-time identity checks for a bounded owner handoff. Direct registry
//! writers remain outside this guarantee; no delegated reply authority is created.
use crate::{
    api::HandlerContext,
    error::DunkError,
    flock::{panes::PaneInfoEnv, Flock},
};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveBinding {
    pane: String,
    pid: i64,
    start: String,
    boot: String,
}

impl LiveBinding {
    #[allow(dead_code)]
    pub(crate) fn pane(&self) -> &str {
        &self.pane
    }
    #[allow(dead_code)]
    pub(crate) fn pid(&self) -> i64 {
        self.pid
    }
    #[allow(dead_code)]
    pub(crate) fn start(&self) -> &str {
        &self.start
    }
    #[allow(dead_code)]
    pub(crate) fn boot(&self) -> &str {
        &self.boot
    }
    #[cfg(test)]
    pub(crate) fn for_test(pane: &str, pid: i64, start: &str, boot: &str) -> Self {
        Self {
            pane: pane.to_owned(),
            pid,
            start: start.to_owned(),
            boot: boot.to_owned(),
        }
    }
}

fn refused() -> DunkError {
    DunkError::Refused(
        "owner handoff requires a live authenticated desk and unique registered owner supervisor"
            .into(),
    )
}

fn live_role<E: PaneInfoEnv + ?Sized>(
    flock: &Flock,
    env: &E,
    pane: &str,
    role: &str,
) -> Result<LiveBinding, DunkError> {
    let info = env.get_pane(pane).ok_or_else(refused)?;
    if info.get("pane_id").and_then(Value::as_str) != Some(pane) {
        return Err(refused());
    }
    let number = |v: &Value| {
        v.as_i64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    };
    let pid = info
        .get("pane_pid")
        .and_then(number)
        .filter(|v| *v > 1)
        .ok_or_else(refused)?;
    let entry = flock
        .with_roles(|roles, _| roles.entry(pane))?
        .ok_or_else(refused)?;
    let start = entry
        .get("start")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(refused)?;
    if entry.get("role").and_then(Value::as_str) != Some(role)
        || entry.get("pane_pid").and_then(number) != Some(pid)
        || crate::proc::process_start(pid).as_deref() != Some(start)
    {
        return Err(refused());
    }
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .map_err(|_| refused())?
        .trim()
        .to_owned();
    if boot.is_empty() {
        return Err(refused());
    }
    Ok(LiveBinding {
        pane: pane.to_owned(),
        pid,
        start: start.to_owned(),
        boot,
    })
}

fn verify_desk(context: &HandlerContext<'_>) -> Result<(), DunkError> {
    if context.request.socket_peer != Some(true) || context.request.identity_bound != Some(true) {
        return Err(refused());
    }
    let desk = context
        .self_pane_id()
        .filter(|p| !p.is_empty())
        .ok_or_else(refused)?;
    let desk_binding = live_role(context.flock, context.env, desk, "desk")?;
    if crate::proc::caller_pane_auth_failure(
        context.request.caller.as_ref(),
        Some(desk_binding.pid),
    )
    .is_some()
    {
        return Err(refused());
    }
    Ok(())
}

#[allow(dead_code)]
pub(super) fn verified_supervisor(context: &HandlerContext<'_>) -> Result<LiveBinding, DunkError> {
    verify_desk(context)?;
    verified_target(context.flock, context.env)
}

fn quarantine_error(
    inbox: &crate::inbox_store::Store,
    question: &str,
    cause: impl std::fmt::Display,
) -> DunkError {
    match inbox.quarantine_existing_owner_handoff(question) {
        Ok(Some(state)) => DunkError::Refused(format!(
            "{cause}; notice quarantined or terminal (prior state: {}); explicit recovery required",
            state.as_deref().unwrap_or("missing")
        )),
        Ok(None) => DunkError::Refused(format!(
            "{cause}; no inbox mapping found; recovery required"
        )),
        Err(error) => DunkError::Refused(format!(
            "{cause}; notice quarantine and durable attention failed: {error}"
        )),
    }
}

/// Fresh point-in-time target check; caller must supply a trusted pane snapshot.
#[allow(dead_code)]
pub(crate) fn verified_target<E: PaneInfoEnv + ?Sized>(
    flock: &Flock,
    env: &E,
) -> Result<LiveBinding, DunkError> {
    let registry = flock.lane_registry(true)?;
    let mut matches = registry.iter().filter_map(|(pane, row)| {
        let (name, declared) = Flock::row_identity(Some(row));
        (declared && name.as_deref() == Some("codex-rust-owner")).then_some(pane.as_str())
    });
    let pane = matches.next().ok_or_else(refused)?;
    if matches.next().is_some() {
        return Err(refused());
    }
    live_role(flock, env, pane, "owner")
}

/// The caller receives pending/quiet/timeout rather than a delivery assertion.
#[allow(dead_code)]
pub(super) fn handoff(context: &HandlerContext<'_>, question: &str) -> Result<Value, DunkError> {
    // Authenticate the desk before allowing a request to quarantine anything.
    verify_desk(context)?;
    let stores = context.flock.stores();
    let inbox = stores.inbox.ok_or_else(|| {
        DunkError::Refused("inbox store unavailable; notice state cannot be checked".into())
    })?;
    let recovery = "Ask the owner supervisor to inspect its inbox and assigned task, check whether the original question is already answered, and use owner reply with the original question ID for the final answer. Do not create another notice blindly; attention records require operator review.";
    if let Some((task, pane, prior_state)) = inbox.owner_handoff_attention(question)? {
        return Ok(
            serde_json::json!({"question_id": question, "task_id": task, "supervisor_pane": pane, "prior_notice_state": prior_state, "status": "recovery_required", "recovery": recovery}),
        );
    }
    let target = verified_target(context.flock, context.env)
        .map_err(|error| quarantine_error(&inbox, question, error))?;
    let supervisor = target.pane.clone();
    let tasks = stores
        .tasks
        .ok_or_else(|| quarantine_error(&inbox, question, "task store unavailable"))?;
    if inbox.reconcile_answered_owner_notice(question)? {
        context.block_on(tasks.reconcile_answered_owner_handoff(question))??;
        return Err(DunkError::Refused(
            "owner question already answered; obsolete handoff reconciled".into(),
        ));
    }
    if let Err(error) = inbox.validate_owner_handoff_question(question) {
        if inbox.reconcile_answered_owner_notice(question)? {
            context.block_on(tasks.reconcile_answered_owner_handoff(question))??;
            return Err(DunkError::Refused(
                "owner question already answered; obsolete handoff reconciled".into(),
            ));
        }
        return Err(quarantine_error(&inbox, question, error));
    }
    let task = context
        .block_on(tasks.ensure_owner_handoff_task(
            question,
            &supervisor,
            target.pid,
            &target.start,
            &target.boot,
        ))
        .and_then(|result| result)
        .map_err(|error| quarantine_error(&inbox, question, error))?;
    let mapping = context
        .block_on(tasks.owner_handoff_mapping(question))
        .and_then(|result| result)
        .map_err(|error| quarantine_error(&inbox, question, error))?
        .ok_or_else(|| quarantine_error(&inbox, question, "handoff mapping disappeared"))?;
    if mapping.task != task
        || mapping.pane != supervisor
        || mapping.pid != target.pid
        || mapping.start != target.start
        || mapping.boot != target.boot
    {
        return Err(quarantine_error(
            &inbox,
            question,
            "handoff identity changed",
        ));
    }
    context.block_on(crate::tasks::owner_handoff::postcheck_pending_notice(
        &tasks, &inbox, question, &mapping,
    ))??;
    if !verified_supervisor(context).is_ok_and(|binding| binding == target) {
        return Err(quarantine_error(
            &inbox,
            question,
            format!("supervisor identity changed after task {task}"),
        ));
    }
    let handoff = match inbox.ensure_owner_handoff_notice(question, &supervisor, &task) {
        Ok(handoff) => handoff,
        Err(error) => {
            if inbox.reconcile_answered_owner_notice(question)? {
                context.block_on(tasks.reconcile_answered_owner_handoff(question))??;
            }
            return Err(quarantine_error(
                &inbox,
                question,
                format!("handoff task {task} needs reconciliation: {error}"),
            ));
        }
    };
    context.block_on(crate::tasks::owner_handoff::postcheck_pending_notice(
        &tasks, &inbox, question, &mapping,
    ))??;
    if inbox.reconcile_answered_owner_notice(question)? {
        context.block_on(tasks.reconcile_answered_owner_handoff(question))??;
        return Err(DunkError::Refused(
            "owner question answered during handoff; obsolete work reconciled".into(),
        ));
    }
    if !verified_supervisor(context).is_ok_and(|binding| binding == target) {
        return Err(quarantine_error(
            &inbox,
            question,
            format!(
                "supervisor identity changed after notice {}",
                handoff.notice_id
            ),
        ));
    }
    let result = context.env.with_inbox_env(&mut |env| {
        use crate::flock::commands_inbox::{notice_text, recipient_is_quiet};
        let quiet = recipient_is_quiet(env.inbox_silent_mailboxes().as_deref(), "codex-rust-owner", "codex-rust-owner", Some(&supervisor));
        let mut notification = "not_attempted".to_string();
        if handoff.notice_state == "queued" {
            notification = if quiet { "quiet".into() } else {
                context.block_on(crate::tasks::owner_handoff::postcheck_pending_notice(&tasks, &inbox, question, &mapping))??;
                if inbox.reconcile_answered_owner_notice(question)? {
                    context.block_on(tasks.reconcile_answered_owner_handoff(question))??;
                    return Ok(serde_json::json!({"question_id": question, "status": "answered"}));
                }
                if !verified_supervisor(context).is_ok_and(|binding| binding == target) {
                    return Err(DunkError::Refused("supervisor identity changed before notification".into()));
                }
                let text = notice_text("codex-rust-owner", 1, Some("owner-handoff"), Some("owner question"));
                if env.fold_into_queued_notice(&supervisor, &text, &handoff.notice_id).is_some() { "queued".into() }
                else { match env.send_now(&supervisor, &text, &handoff.notice_id) {
                    Ok(nudge) if nudge.queued => "queued".into(),
                    Ok(_) => "sent".into(),
                    Err(_) => "failed".into(),
                }}
            };
        }
        let waiting_s = (env.now() - handoff.created_at).max(0.0);
        Ok(serde_json::json!({"question_id": question, "task_id": task, "notice_id": handoff.notice_id, "transport": handoff.transport, "notice_state": handoff.notice_state, "notification": notification, "waiting_s": waiting_s, "timeout": waiting_s >= 300.0, "status": "pending", "recovery": recovery}))
    });
    result.map_err(|error| quarantine_error(&inbox, question, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Panes {
        pid: i64,
    }
    impl PaneInfoEnv for Panes {
        fn list_panes_grouped(&self) -> Vec<Value> {
            vec![json!({"pane_id":"%1", "pane_pid":self.pid})]
        }
        fn get_pane(&self, pane: &str) -> Option<Value> {
            (pane == "%1").then(|| json!({"pane_id":"%1", "pane_pid":self.pid}))
        }
    }

    #[test]
    fn owner_handoff_target_requires_unique_declared_live_owner_binding() {
        let dir = tempfile::tempdir().unwrap();
        let flock = Flock::new(dir.path().join("launches"), dir.path().join("lanes"), false);
        let mut roles = crate::roles::Roles::new(
            &crate::config::Env::default(),
            Some(dir.path().join("roles")),
            Some(dir.path().join("tokens")),
            None,
        )
        .unwrap();
        let pid = i64::from(std::process::id());
        let start = crate::proc::process_start(pid).unwrap();
        roles
            .register(
                "%1",
                "owner",
                Some(pid),
                Some(&start),
                None,
                None,
                None,
                None,
            )
            .unwrap();
        flock.attach_roles(roles, None);
        assert!(flock.remember_lane("%1", "codex-rust-owner", true));
        let binding = verified_target(&flock, &Panes { pid }).unwrap();
        assert_eq!(binding.pane(), "%1");
        assert_eq!(binding.pid, pid);
        assert!(verified_target(&flock, &Panes { pid: pid + 1 }).is_err());
        assert!(flock.remember_lane("%2", "codex-rust-owner", true));
        assert!(verified_target(&flock, &Panes { pid }).is_err());
    }
}
