//! `flock/loops_stuck.rs` (T36c, slice 2): the stuck-observer loop shell and the dialog
//! observer step, from `dunk_core._stuck_observer_loop` and `_dialog_observer_step`.
//!
//! The loop wakes every `STUCK_OBSERVE_TICK_S` (5 s) until the stop signal or `closed`. Each tick:
//! the DIALOG scan (independent of automatic key recovery; it types nothing, it only reports a
//! dialog that has held a pane), then, only when `stuck_config.enabled`, the stalled-delivery step.
//!
//! The dialog step scans live agent panes at most every 30 s. A pane whose screen shows a modal
//! keeps its `since` while the (generation, signature) pair is unchanged; an absent, non-modal or
//! replaced pane starts a fresh stability interval. The step reports through
//! [`StuckEnv::note_stale_panel`] and never answers a dialog (2.9.0 pressed Escape and did it
//! 1042 times).
//!
//! The observer partitions native federated composers from strict composers and delegates
//! strict submissions to the existing guarded recovery transition in `sends`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

use crate::error::DunkError;
use crate::flock::commands_inbox::InboxEnv;
use crate::flock::loop_env::LoopFuture;
use crate::flock::panes::PaneInfoEnv;
use crate::flock::sends::{self, SendEnv, StuckCandidate};
use crate::flock::Flock;
use crate::metrics::Event;
use crate::terminal::screen::{collapse, modal_open, strip_ansi};

/// `MODAL_WAIT_S`: wait for a stable dialog before reporting it.
pub const MODAL_WAIT_S: f64 = 30.0;

/// `STUCK_OBSERVE_TICK_S`.
pub const STUCK_OBSERVE_TICK_S: f64 = 5.0;
/// The dialog scan's own period (`self._dialog_scan_at = now + 30`).
pub const DIALOG_SCAN_PERIOD_S: f64 = 30.0;
/// `_collapse(...)[-4000:]`: how much of the screen is the signature.
const SIGNATURE_CHARS: usize = 4000;

/// What the observers reach through `self`.
pub trait StuckEnv: Send + Sync {
    /// `self.herdr.list_panes()`.
    fn list_panes(&self) -> LoopFuture<'_, Result<Vec<Value>, DunkError>>;
    /// `self.read_screen(pane)`; `None` when unreadable.
    fn read_screen<'a>(&'a self, pane_id: &'a str) -> LoopFuture<'a, Option<String>>;
    /// `self._pane_generation(pane)`.
    fn pane_generation<'a>(&'a self, pane_id: &'a str) -> LoopFuture<'a, Option<String>>;
    /// `self._note_stale_panel(pane, ("modal", text), since)`.
    fn note_stale_panel<'a>(
        &'a self,
        pane_id: &'a str,
        signature_text: &'a str,
        since: f64,
    ) -> LoopFuture<'a, ()>;
    /// `self.closed`.
    fn closed(&self) -> bool {
        false
    }
    /// `self.stuck_config.enabled`.
    fn stuck_recovery_enabled(&self) -> bool {
        false
    }
    /// `self._stuck_observer_step()`: how many recoveries it acted on.
    fn stuck_observer_step(&self) -> LoopFuture<'_, usize>;
}

/// The queued per-pane worker is the sole observer while a message remains queued.
fn outbox_owns_pane(flock: &Flock, pane_id: &str) -> bool {
    flock
        .outbox
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(pane_id)
        .is_some_and(|queue| queue.iter().any(|message| message.status == "queued"))
}

fn retire_candidate(
    flock: &Flock,
    pane_id: &str,
    observed: &StuckCandidate,
    status: &str,
    now: f64,
) {
    let mut candidates = flock
        .stuck_candidates
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(live) = candidates.get_mut(pane_id) {
        if live.message_id == observed.message_id && live.created_at == observed.created_at {
            live.eligible = false;
            live.status = status.into();
            live.retired_at = Some(now);
        }
    }
}

/// Strict-composer recovery for one observer tick. Recovery remains delegated to the
/// existing `Flock::unwedge` transition; this scan only supplies a generation-bound candidate.
pub(crate) fn strict_stuck_observer_scan<E: SendEnv + PaneInfoEnv>(
    flock: &Flock,
    env: &mut E,
    now: f64,
) -> Result<usize, DunkError> {
    let candidates = flock
        .stuck_candidates
        .lock()
        .map_err(|_| DunkError::Refused("stuck candidate registry unavailable".into()))?
        .iter()
        .map(|(pane, entry)| (pane.clone(), entry.clone()))
        .collect::<Vec<_>>();
    let mut acted = 0;
    for (pane_id, entry) in candidates {
        let screen = sends::read_screen(env, &pane_id, None, true);
        if screen
            .as_deref()
            .and_then(|screen| {
                crate::terminal::composer::federated_composer(env.federated_input(), screen)
            })
            .is_some()
        {
            continue; // federated observer exclusively owns recognized native frames
        }
        if !entry.eligible {
            if entry.recovery_outcome.as_deref() == Some("consumed")
                && !entry.receipts_reconciled.unwrap_or(false)
            {
                flock.publish_stuck_receipt(&pane_id, &entry);
            }
            continue;
        }
        if outbox_owns_pane(flock, &pane_id) {
            continue;
        }
        let Some(generation) = sends::pane_generation(env, &pane_id) else {
            flock
                .stuck_tracker
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .forget(&pane_id);
            continue;
        };
        if entry.generation.as_deref() != Some(generation.as_str()) {
            retire_candidate(flock, &pane_id, &entry, "generation-changed", now);
            flock
                .stuck_tracker
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .forget(&pane_id);
            continue;
        }
        let Some(screen) = screen else {
            flock
                .stuck_tracker
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .forget(&pane_id);
            continue;
        };
        let Some(content) = flock.recovery_box_text(env, &pane_id, &screen) else {
            flock
                .stuck_tracker
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .forget(&pane_id);
            continue;
        };
        if content.is_empty() {
            flock
                .stuck_tracker
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .forget(&pane_id);
            retire_candidate(flock, &pane_id, &entry, "consumed", now);
            continue;
        }
        let history_message = flock
            .message_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|message| message.id == entry.message_id && message.pane_id == pane_id)
            .cloned();
        let message = history_message.or_else(|| flock.stuck_candidate_message(&pane_id, env));
        if let Some(message) = message {
            acted += usize::from(flock.unwedge(
                env,
                &pane_id,
                &content,
                None,
                Some(&message),
                now,
                Some(&entry.message_id),
                true,
            ));
        }
    }
    flock.save()?;
    Ok(acted)
}

/// Submit the existing federated composer exactly once after a durable tracker reservation.
/// This path never sends candidate text and therefore preserves drafts and scrolled input.
pub(crate) fn federated_recovery_step<E: SendEnv + PaneInfoEnv>(
    flock: &Flock,
    env: &mut E,
    now: f64,
    wait: &dyn Fn(f64),
) -> Result<usize, DunkError> {
    let config = *flock
        .stuck_config
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !config.enabled || !env.federated_input() {
        return Ok(0);
    }
    let candidates = flock
        .stuck_candidates
        .lock()
        .map_err(|_| DunkError::Refused("stuck candidate registry unavailable".into()))?
        .iter()
        .map(|(pane, entry)| (pane.clone(), entry.clone()))
        .collect::<Vec<_>>();
    let mut acted = 0;
    for (pane_id, entry) in candidates {
        if outbox_owns_pane(flock, &pane_id)
            || !entry.eligible
            || !entry.press_enter
            || entry.kind != "text"
            || !matches!(entry.status.as_str(), "sent" | "failed")
            || entry.generation.as_deref().is_none_or(str::is_empty)
        {
            continue;
        }
        let Some(screen) = sends::read_screen(env, &pane_id, None, true) else {
            continue;
        };
        let Some(content) = crate::terminal::composer::federated_composer(true, &screen) else {
            continue;
        };
        let generation = sends::pane_generation(env, &pane_id);
        let dead = sends::dead_session(env, &pane_id).is_some();
        let busy = crate::terminal::screen::status_marker(
            &screen,
            &crate::terminal::screen::WORKING_MARKERS,
            40,
            true,
            true,
        )
        .is_some();
        let ready = generation.as_deref() == entry.generation.as_deref()
            && !busy
            && !dead
            && !env.session_process_gone(&pane_id);
        let identity = format!("federated:{}", entry.message_id);
        let mut observation = crate::stuck::Observation::new(
            &pane_id,
            Some(&identity),
            Some(&content),
            if ready { "ready" } else { "unknown" },
        );
        observation.generation = generation.clone();
        let decision = flock
            .stuck_tracker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .decide(&observation, &config, now);
        if !decision.recovers() || !flock.persist_enabled || flock.save().is_err() {
            continue;
        }

        let pane_lock = flock.send_lock_for(&pane_id);
        let Ok(_pane_guard) = pane_lock.lock() else {
            continue;
        };
        let Ok(_typing_guard) = flock.typing_guard(&pane_id) else {
            continue;
        };
        let current = sends::read_screen(env, &pane_id, None, true);
        let live = flock
            .stuck_candidates
            .lock()
            .ok()
            .and_then(|rows| rows.get(&pane_id).cloned());
        if live.as_ref() != Some(&entry)
            || sends::pane_generation(env, &pane_id) != generation
            || current
                .as_deref()
                .and_then(|s| crate::terminal::composer::federated_composer(true, s))
                != Some(content.clone())
            || content.is_empty()
            || sends::dead_session(env, &pane_id).is_some()
            || env.session_process_gone(&pane_id)
            || current.as_deref().is_none_or(|s| {
                crate::terminal::screen::modal_open(s)
                    || crate::terminal::screen::status_marker(
                        s,
                        &crate::terminal::screen::WORKING_MARKERS,
                        40,
                        true,
                        true,
                    )
                    .is_some()
            })
        {
            continue;
        }
        let (pressed, detail) = env.send_key(&pane_id, "Enter");
        if !pressed {
            tracing::warn!(pane_id, detail, "federated stuck recovery key refused");
            continue;
        }
        drop(_typing_guard);
        drop(_pane_guard);
        wait(sends::SUBMIT_POLL_S[0]);
        let after = sends::read_screen(env, &pane_id, None, true);
        let verified = after
            .as_deref()
            .and_then(|s| crate::terminal::composer::federated_composer(true, s))
            == Some(String::new())
            && sends::pane_generation(env, &pane_id) == generation;
        if let Some(metrics) = flock.stores().metrics {
            metrics.record(&Event {
                at: now,
                kind: "stuck.recover".into(),
                pane_id: Some(pane_id.clone()),
                source: Some("federated_watchdog".into()),
                outcome: Some(if verified { "consumed" } else { "unverified" }.into()),
                reason: Some("authorized preserve-submit; no text replayed".into()),
                ..Event::default()
            });
        }
        tracing::warn!(pane_id, verified, "federated preserve-submit completed");
        acted += 1;
    }
    flock.save()?;
    Ok(acted)
}

impl StuckEnv for crate::server_runtime::LoopRuntimeAdapter {
    fn list_panes(&self) -> LoopFuture<'_, Result<Vec<Value>, DunkError>> {
        self.helper_stuckenv_list_panes()
    }

    fn read_screen<'a>(&'a self, pane_id: &'a str) -> LoopFuture<'a, Option<String>> {
        self.helper_stuckenv_read_screen(pane_id)
    }

    fn pane_generation<'a>(&'a self, pane_id: &'a str) -> LoopFuture<'a, Option<String>> {
        self.helper_stuckenv_pane_generation(pane_id)
    }

    fn note_stale_panel<'a>(
        &'a self,
        pane_id: &'a str,
        signature_text: &'a str,
        since: f64,
    ) -> LoopFuture<'a, ()> {
        self.helper_stuckenv_note_stale_panel(pane_id, signature_text, since)
    }

    fn closed(&self) -> bool {
        self.helper_stuckenv_closed()
    }

    fn stuck_recovery_enabled(&self) -> bool {
        self.helper_stuckenv_stuck_recovery_enabled()
    }

    fn stuck_observer_step(&self) -> LoopFuture<'_, usize> {
        Box::pin(async move {
            let worker = self.with_loop_terminal(|flock, terminal| {
                let now = crate::clock::now_s();
                let federated = match federated_recovery_step(flock, terminal, now, &|seconds| {
                    std::thread::sleep(Duration::from_secs_f64(seconds.max(0.0)))
                }) {
                    Ok(acted) => acted,
                    Err(error) => {
                        tracing::error!(error = %error, "federated recovery scan failed; strict scan will continue");
                        0
                    }
                };
                match strict_stuck_observer_scan(flock, terminal, now) {
                    Ok(strict) => federated + strict,
                    Err(error) => {
                        tracing::error!(error = %error, "strict stuck recovery scan failed");
                        0 // Python's outer scan failure discards the federated acted count.
                    }
                }
            });
            worker.await.unwrap_or_else(|error| {
                tracing::error!(error = %error, "stuck observer terminal worker failed");
                0
            })
        })
    }
}

/// One pane's observation: `(generation, signature_text, since)`.
type Observation = (Option<String>, String, f64);

/// `_dialog_scan_at` and `_dialog_observations`.
#[derive(Debug, Default)]
pub struct DialogObserver {
    scan_at: f64,
    observations: BTreeMap<String, Observation>,
}

/// The `_warned_modal` state and alert integration for a production `StuckEnv`.
/// The runtime adapter owns one reporter alongside its observer and calls
/// [`note`](Self::note) from `StuckEnv::note_stale_panel`.
#[derive(Clone, Debug, Default)]
pub struct StalePanelReporter {
    warned_modal: Arc<Mutex<BTreeSet<String>>>,
}

impl StalePanelReporter {
    pub fn new() -> Self {
        Self::default()
    }

    /// `_note_stale_panel`: durable alerts retry independently of one-time logging and metrics.
    /// `generation` is the current process-bound pane generation from the observer scan.
    #[allow(clippy::too_many_arguments)]
    pub fn note(
        &mut self,
        flock: &Flock,
        inbox: &dyn InboxEnv,
        pane_id: &str,
        signature_kind: &str,
        generation: Option<&str>,
        signature: Option<&str>,
        since: f64,
    ) {
        self.note_at(
            flock,
            inbox,
            pane_id,
            signature_kind,
            generation,
            signature,
            since,
            crate::clock::now_s(),
            None,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn note_at(
        &mut self,
        flock: &Flock,
        inbox: &dyn InboxEnv,
        pane_id: &str,
        signature_kind: &str,
        generation: Option<&str>,
        signature: Option<&str>,
        since: f64,
        now: f64,
        path: Option<&Path>,
    ) {
        if signature_kind != "modal" || now - since < MODAL_WAIT_S {
            return;
        }
        let mut owner = |body: &str| crate::dialog_alerts::post_owner_from_environment(body);
        let mut vp = |body: &str| crate::dialog_alerts::post_vp(flock, inbox, body, None);
        self.note_with_sinks(
            flock,
            pane_id,
            signature_kind,
            generation,
            signature,
            since,
            now,
            path,
            &mut owner,
            &mut vp,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn note_with_sinks(
        &mut self,
        flock: &Flock,
        pane_id: &str,
        signature_kind: &str,
        generation: Option<&str>,
        signature: Option<&str>,
        since: f64,
        now: f64,
        path: Option<&Path>,
        owner: &mut dyn FnMut(&str) -> Result<crate::dialog_alerts::SinkDelivery, String>,
        vp: &mut dyn FnMut(&str) -> Result<crate::dialog_alerts::SinkDelivery, String>,
    ) {
        if signature_kind != "modal" || now - since < MODAL_WAIT_S {
            return;
        }
        match crate::dialog_alerts::notify(
            pane_id,
            generation,
            signature,
            owner,
            vp,
            path,
            Some(now),
        ) {
            Ok(attempts) => {
                for attempt in attempts.into_iter().filter(|attempt| !attempt.delivered) {
                    eprintln!(
                        "dialog alert {} -> {} failed ({}); will retry",
                        pane_id,
                        attempt.sink,
                        attempt.error.as_deref().unwrap_or("undeliverable")
                    );
                }
            }
            Err(error) => eprintln!("could not notify blocked dialog on {pane_id}: {error}"),
        }

        // Hold the shared lock only for the once-per-pane decision, never metrics or sink I/O.
        let should_warn = self
            .warned_modal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(pane_id.to_string());
        if should_warn {
            if let Some(metrics) = flock.stores().metrics {
                metrics.record(&Event {
                    at: now,
                    kind: "panel.blocking".into(),
                    pane_id: Some(pane_id.into()),
                    outcome: Some("held".into()),
                    reason: Some("dialog".into()),
                    duration_ms: Some(((now - since) * 1000.0) as i64),
                    ..Event::default()
                });
            }
            eprintln!(
                "{pane_id}: a dialog has been open for {}s and blocks message delivery. It is waiting for a person to answer it; nothing will be typed or pressed there until they do.",
                (now - since) as i64
            );
        }
    }
}

/// The last 4000 characters (Python slices code points, not bytes).
fn tail_chars(text: &str, count: usize) -> String {
    let total = text.chars().count();
    text.chars().skip(total.saturating_sub(count)).collect()
}

impl DialogObserver {
    pub fn new() -> Self {
        Self::default()
    }

    /// `_dialog_observer_step(now)`: scan live agent dialogs every 30 s even without queued mail.
    /// No keys. Returns how many panes currently show a modal.
    pub async fn step(&mut self, env: &dyn StuckEnv, now: f64) -> usize {
        if now < self.scan_at {
            return 0;
        }
        self.scan_at = now + DIALOG_SCAN_PERIOD_S;
        let panes = match env.list_panes().await {
            Ok(panes) => panes,
            Err(error) => {
                eprintln!("dialog observer could not list panes: {error}");
                return 0;
            }
        };
        let mut current: BTreeMap<String, Observation> = BTreeMap::new();
        for pane in &panes {
            let Some(pane_id) = pane
                .get("pane_id")
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty())
            else {
                continue;
            };
            if !pane.get("agent").is_some_and(truthy) {
                continue;
            }
            let Some(screen) = env.read_screen(pane_id).await else {
                continue;
            };
            if !modal_open(&screen) {
                continue;
            }
            let signature = tail_chars(&collapse(&strip_ansi(&screen)), SIGNATURE_CHARS);
            let generation = env.pane_generation(pane_id).await;
            let since = match self.observations.get(pane_id) {
                Some((previous_generation, previous_signature, since))
                    if *previous_generation == generation && *previous_signature == signature =>
                {
                    *since
                }
                _ => now,
            };
            current.insert(
                pane_id.to_string(),
                (generation.clone(), signature.clone(), since),
            );
            env.note_stale_panel(pane_id, &signature, since).await;
        }
        // Absent/nonmodal/replaced panes start a fresh stability interval.
        self.observations = current;
        self.observations.len()
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_none_or(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// The loop: wait the tick, then the dialog scan, then (when enabled) the stalled-delivery step,
/// until `stop` flips to true or the flock is closed.
pub async fn run_stuck_observer(
    env: Arc<dyn StuckEnv>,
    tick: Duration,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    let mut dialogs = DialogObserver::new();
    loop {
        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    return;
                }
                continue;
            }
            _ = tokio::time::sleep(tick) => {}
        }
        if env.closed() {
            return;
        }
        // Dialog reporting is independent of automatic key recovery.
        dialogs.step(env.as_ref(), crate::clock::now_s()).await;
        if !env.stuck_recovery_enabled() {
            continue; // key recovery disabled
        }
        env.stuck_observer_step().await;
    }
}
