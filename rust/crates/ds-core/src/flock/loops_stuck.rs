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
//! NOT PORTED here (a named hook, default 0): `_stuck_observer_step` itself and
//! `_federated_recovery_step`. They need `_unwedge`, `_box_text`, the outbox and the `Message`
//! type from T33 (`flock/sends.rs`, t801, not landed). The durable stale-panel alert reporter
//! is provided below for the production `StuckEnv` hook.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::error::DunkError;
use crate::flock::commands_inbox::InboxEnv;
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
    fn list_panes(&self) -> Result<Vec<Value>, DunkError>;
    /// `self.read_screen(pane)`; `None` when unreadable.
    fn read_screen(&self, pane_id: &str) -> Option<String>;
    /// `self._pane_generation(pane)`.
    fn pane_generation(&self, pane_id: &str) -> Option<String>;
    /// `self._note_stale_panel(pane, ("modal", text), since)`, with the current pane generation
    /// captured by this same observation.
    fn note_stale_panel(
        &self,
        pane_id: &str,
        generation: Option<&str>,
        signature_text: &str,
        since: f64,
    );
    /// `self.closed`.
    fn closed(&self) -> bool {
        false
    }
    /// `self.stuck_config.enabled`.
    fn stuck_recovery_enabled(&self) -> bool {
        false
    }
    /// `self._stuck_observer_step()`: how many recoveries it acted on. Not ported (needs T33).
    fn stuck_observer_step(&self) -> usize {
        0
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
#[derive(Debug, Default)]
pub struct StalePanelReporter {
    warned_modal: BTreeSet<String>,
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

        if self.warned_modal.insert(pane_id.to_string()) {
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
    pub fn step(&mut self, env: &dyn StuckEnv, now: f64) -> usize {
        if now < self.scan_at {
            return 0;
        }
        self.scan_at = now + DIALOG_SCAN_PERIOD_S;
        let panes = match env.list_panes() {
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
            let Some(screen) = env.read_screen(pane_id) else {
                continue;
            };
            if !modal_open(&screen) {
                continue;
            }
            let signature = tail_chars(&collapse(&strip_ansi(&screen)), SIGNATURE_CHARS);
            let generation = env.pane_generation(pane_id);
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
            env.note_stale_panel(pane_id, generation.as_deref(), &signature, since);
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
        dialogs.step(env.as_ref(), crate::clock::now_s());
        if !env.stuck_recovery_enabled() {
            continue; // key recovery disabled
        }
        env.stuck_observer_step();
    }
}
