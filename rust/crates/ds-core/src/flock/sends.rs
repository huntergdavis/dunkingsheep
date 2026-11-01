//! T33 (t801), slices 1 and t1742: send decisions and the unguarded submit-confirmation ladder -
//! `Message`, `dead_session`, `send_blocker`, `clear_to_send`, `box_holds`, `send_submit_key`,
//! `press_until_submitted`. Ports `Message` (state and `outcome` / `to_dict`),
//! `Flock._dead_session`, `send_blocker`, `_clear_to_send`, `_box_holds`, `_send_submit_key`,
//! `_press_until_submitted`, `_pane_harness`, `read_screen`/`read_box`.
//!
//! The Python reaches the backend, the lease table and the clock through `self`; here they are
//! [`SendEnv`], so every decision is testable with a scripted pane. NOT in this slice (next
//! NOT in these slices (later t801 work): `send_now`/`send_keys_now`, `_enqueue`/`_deliver` and
//! the per-pane outbox task, `submit_text`/`press_keys`, `_flush_once`, clear_box.
//!
//! POLICY (owner, 2026-10-23/24) kept verbatim: a busy agent is NOT a hold reason, and there
//! is NO "a human is typing" hold - every message goes through this daemon, so a non-empty box
//! is not evidence of a person. The only refusals are a DEAD session (read, not claimed) and an
//! unknowable composer boundary.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::atomic::Ordering;
use std::sync::{Mutex, MutexGuard};

use super::panes::PaneInfoEnv;
use super::resolve::{resolve_target, resolve_target_dict};
use super::Flock;
use crate::clock;
use crate::error::DunkError;
use crate::terminal::composer::federated_composer;
use crate::terminal::screen::{
    banner_marker, box_text, is_our_send, modal_open, status_marker, strip_ansi, typed_text,
    AMBIGUOUS_COMPOSER, DEAD_SESSION_MARKERS, WORKING_MARKERS,
};

/// `CONSUMPTION_READ_LINES`.
pub const CONSUMPTION_READ_LINES: i64 = 40;
/// `TYPING_SETTLE_S`: the box must read clear on two checks this far apart.
pub const TYPING_SETTLE_S: f64 = 1.0;
/// `_press_until_submitted`: quiet time before each retry key.
pub const SUBMIT_GAPS: [f64; 4] = [0.2, 1.0, 3.0, 6.0];
/// `_press_until_submitted`: wait budget for confirming one press.
pub const SUBMIT_CONFIRM_S: f64 = 2.0;
/// `_press_until_submitted`: pane reads during one confirmation window.
pub const SUBMIT_POLL_S: [f64; 5] = [0.1, 0.2, 0.4, 0.8, 1.0];
/// `_press_until_submitted`: repeated reads that distinguish a redraw from a cleared box.
pub const SUBMIT_VERIFY_GAPS: [f64; 2] = [0.4, 1.0];
/// `_press_until_submitted`: try Enter twice before falling back to C-m.
pub const SUBMIT_KEY_AFTER: usize = 2;
/// `_press_until_submitted`: number of press-and-check rounds.
pub const SUBMIT_ATTEMPTS: usize = SUBMIT_GAPS.len();

/// `HARNESS_QUEUES_WHILE_BUSY` (measured, not assumed): claude, pi and codex queue what is typed
/// while they work (codex via Tab). An unknown harness keeps the capability hold.
pub fn harness_queues_while_busy(harness: Option<&str>) -> bool {
    matches!(harness, Some("claude" | "pi" | "codex"))
}

/// `_pane_generation`: bind a pane to both its pid and the kernel process start time.
///
/// A pid can be reused after a pane is replaced, so missing process evidence fails closed.
pub fn pane_generation(env: &dyn PaneInfoEnv, pane_id: &str) -> Option<String> {
    let pane = env.get_pane(pane_id)?;
    pane_generation_with(&pane, crate::proc::process_start)
}

pub(super) fn pane_generation_with(
    pane: &Value,
    process_start: impl FnOnce(i64) -> Option<String>,
) -> Option<String> {
    let pid_value = pane
        .get("pane_pid")
        .filter(|value| json_truthy(value))
        .or_else(|| pane.get("pid").filter(|value| json_truthy(value)))?;
    let pid = match pid_value {
        Value::Null | Value::Array(_) | Value::Object(_) => return None,
        Value::Bool(value) => i64::from(*value),
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_u64().and_then(|n| i64::try_from(n).ok()))?,
        Value::String(value) => value.trim().parse::<i64>().ok()?,
    };
    let start = process_start(pid).filter(|start| !start.is_empty())?;
    Some(format!("pid:{pid}:start:{start}"))
}

fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(values) => !values.is_empty(),
        Value::Object(values) => !values.is_empty(),
    }
}

/// What the send decisions read from outside the pane text.
pub trait SendEnv {
    /// `herdr.read_pane(pane_id, lines, source="visible", ansi)`; `None` = unreadable.
    fn read_pane(&self, pane_id: &str, lines: i64, ansi: bool) -> Option<String>;
    /// `_pane_harness`: the pane's agent, lowercased, `None` when empty or unknown.
    fn pane_harness(&self, pane_id: &str) -> Option<String>;
    /// `_session_process_gone`: a lease's recorded caller process is no longer running.
    fn session_process_gone(&self, pane_id: &str) -> bool;
    /// `_read_status`: the backend's status for the pane, lowercased.
    fn read_status(&self, pane_id: &str) -> Option<String>;
    /// `herdr.send_keys(pane_id, key)` -> `(ok, detail)`.
    fn send_key(&mut self, pane_id: &str, key: &str) -> (bool, String);
    /// `DUNKINGSHEEP_FEDERATED_INPUT == "1"`.
    fn federated_input(&self) -> bool;
}

/// Clock, per-key guarded send, and side effects needed by the unguarded submit-confirmation
/// ladder. A production adapter is supplied by later runtime work; this trait does not make this
/// helper an end-to-end send path.
pub trait SubmitEnv: SendEnv {
    /// Injected sleep; tests advance a fake clock instead of waiting.
    fn sleep_s(&mut self, seconds: f64);
    /// Injected monotonic seconds, used to bound each confirmation window.
    fn now_s(&mut self) -> f64;
    /// Send one retry key under the caller's pane guard, returning the actual key selected.
    fn send_submit_key_guarded(
        &mut self,
        pane_id: &str,
        fallback: &str,
        busy_hint: bool,
    ) -> (bool, String, Option<String>);
    /// Previously learned submit key for this pane.
    fn remembered_submit_key(&self, pane_id: &str) -> Option<String>;
    /// Remember the key that succeeded; Tab is normalized to Enter by the caller.
    fn remember_submit_key(&mut self, pane_id: &str, key: &str);
    /// Record the existing `submit.attempt` metric.
    fn record_submit_attempt(
        &mut self,
        pane_id: &str,
        outcome: &str,
        attempts: usize,
        bytes: usize,
    );
    /// Whether the pane entered a working state after the send's readable, idle baseline.
    fn turn_started(&mut self, pane_id: &str, baseline: &str) -> bool {
        !is_working_status(Some(baseline))
            && is_working_status(self.read_status(pane_id).as_deref())
    }
}

/// Pane resolution and the submit ladder needed by the explicit stranded-text flush.
/// Production command wiring is supplied by a later runtime leaf.
pub trait FlushEnv: SubmitEnv + PaneInfoEnv {
    /// `herdr.server_error_hint()` for an empty pane listing.
    fn server_error_hint(&self) -> String;
}

/// `Flock.flush_box`: explicitly submit the text already present in one pane's composer.
/// The initial composer screen is read exactly once; later reads belong to the existing
/// submission-confirmation ladder.
pub fn flush_box(
    env: &mut dyn FlushEnv,
    target: &Value,
    self_pane_id: Option<&str>,
) -> Result<Value, DunkError> {
    let panes = env.list_panes_grouped();
    let pane = if target.is_object() {
        resolve_target_dict(&panes, target)?
    } else {
        let spec = match target {
            Value::Null => String::new(),
            Value::String(spec) => spec.clone(),
            Value::Bool(value) => if *value { "True" } else { "False" }.to_string(),
            other => other.to_string(),
        };
        resolve_target(&panes, &spec, self_pane_id, &env.server_error_hint())?.clone()
    };
    let pane_id = pane
        .get("pane_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (screen, shown, detail) = read_box(env, pane_id);
    if screen.is_none() {
        return Err(DunkError::Refused(format!(
            "could not read the input box of {pane_id}"
        )));
    }
    let label = pane
        .get("tab_label")
        .and_then(Value::as_str)
        .filter(|label| !label.is_empty())
        .unwrap_or(pane_id);
    if detail.starts_with(AMBIGUOUS_COMPOSER) {
        return Ok(json!({
            "pane_id": pane_id,
            "target_label": label,
            "flushed": false,
            "state": "unknown",
            "text": Value::Null,
            "detail": detail,
            "message": format!("refused to submit: {detail}"),
        }));
    }
    if shown.is_empty() {
        return Ok(json!({
            "pane_id": pane_id,
            "target_label": label,
            "flushed": false,
            "text": Value::Null,
            "detail": detail,
            "message": format!("nothing to submit: {detail}"),
        }));
    }
    let (flushed, message) =
        press_until_submitted(env, pane_id, &shown, false, false, StatusBaseline::ReadNow);
    Ok(json!({
        "pane_id": pane_id,
        "target_label": label,
        "flushed": flushed,
        "text": shown,
        "message": message,
    }))
}

/// Statuses that mean the lane was already running a turn before this send.
fn is_working_status(status: Option<&str>) -> bool {
    matches!(status, Some("working" | "busy" | "running" | "processing"))
}

/// `_press_until_submitted`: repeat safe submit keys until the text leaves the box or a turn
/// transition confirms this send. `Captured(None)` stays unreadable and cannot be promoted by a
/// later status read. Each key is delegated to the environment's one-key guarded operation.
pub fn press_until_submitted(
    env: &mut dyn SubmitEnv,
    pane_id: &str,
    text: &str,
    first_pressed: bool,
    busy_hint: bool,
    baseline: StatusBaseline,
) -> (bool, String) {
    if let Some(dead) = dead_session(env, pane_id) {
        return (
            false,
            format!("agent session is dead ({dead}); not submitting into a dead composer"),
        );
    }

    let pre_status = match baseline {
        StatusBaseline::ReadNow => env.read_status(pane_id),
        StatusBaseline::Captured(status) => status,
    };
    let mut key = "Enter".to_string();
    let mut last_hold = None;

    for attempt in 1..=SUBMIT_ATTEMPTS {
        let gap = SUBMIT_GAPS[attempt - 1];
        if gap != 0.0 {
            env.sleep_s(gap);
        }
        if !(first_pressed && attempt == 1) {
            key = env.remembered_submit_key(pane_id).unwrap_or_else(|| {
                if attempt > SUBMIT_KEY_AFTER && attempt % 2 == 1 {
                    "C-m".to_string()
                } else {
                    "Enter".to_string()
                }
            });
            let (ok, detail, actual_key) = env.send_submit_key_guarded(pane_id, &key, busy_hint);
            if !ok {
                return (false, detail);
            }
            if let Some(actual_key) = actual_key {
                key = actual_key;
            }
        }

        let deadline = env.now_s() + SUBMIT_CONFIRM_S;
        last_hold = None;
        for wait in SUBMIT_POLL_S {
            if env.now_s() >= deadline {
                break;
            }
            env.sleep_s(wait);
            last_hold = box_holds(env, pane_id, text);
            if last_hold == Some(false) {
                for verify_gap in SUBMIT_VERIFY_GAPS {
                    env.sleep_s(verify_gap);
                    last_hold = box_holds(env, pane_id, text);
                    if last_hold != Some(false) {
                        break;
                    }
                }
                if last_hold == Some(false) {
                    if attempt > 1 {
                        env.remember_submit_key(pane_id, if key == "Tab" { "Enter" } else { &key });
                    }
                    env.record_submit_attempt(pane_id, "ok", attempt, text.len());
                    return (
                        true,
                        if attempt == 1 {
                            "sent".to_string()
                        } else {
                            format!("sent ({key} x{attempt})")
                        },
                    );
                }
                if last_hold == Some(true) {
                    if turn_started(env, pane_id, pre_status.as_deref()) {
                        if attempt > 1 {
                            env.remember_submit_key(
                                pane_id,
                                if key == "Tab" { "Enter" } else { &key },
                            );
                        }
                        env.record_submit_attempt(pane_id, "ok-turn", attempt, text.len());
                        return (true, "sent (confirmed by the turn it started)".to_string());
                    }
                    break;
                }
            }
        }

        if last_hold != Some(true) {
            if turn_started(env, pane_id, pre_status.as_deref()) {
                if attempt > 1 {
                    env.remember_submit_key(pane_id, if key == "Tab" { "Enter" } else { &key });
                }
                env.record_submit_attempt(pane_id, "ok-turn", attempt, text.len());
                return (true, "sent (confirmed by the turn it started)".to_string());
            }
            continue;
        }

        if turn_started(env, pane_id, pre_status.as_deref()) {
            if attempt > 1 {
                // Python resets a busy Tab echo to Enter once the turn itself confirms delivery.
                env.remember_submit_key(pane_id, "Enter");
            }
            env.record_submit_attempt(pane_id, "ok-turn", attempt, text.len());
            return (true, "sent (confirmed by the turn it started)".to_string());
        }
    }

    if last_hold.is_none() {
        return (
            false,
            format!(
                "submission unconfirmed: input box unreadable or changing after {SUBMIT_ATTEMPTS} presses; the lane may or may not have taken it - check the pane before resending"
            ),
        );
    }
    if is_working_status(pre_status.as_deref()) {
        return (
            false,
            format!(
                "submission unconfirmed: the lane was already working before this send ({SUBMIT_ATTEMPTS} presses); status alone cannot confirm a new message - check the pane"
            ),
        );
    }
    (
        false,
        format!(
            "text did not submit after {SUBMIT_ATTEMPTS} Enter presses; it is still in the pane's input box"
        ),
    )
}

/// Baseline-aware turn confirmation: an unreadable or already-working baseline is never proof.
fn turn_started(env: &mut dyn SubmitEnv, pane_id: &str, baseline: Option<&str>) -> bool {
    let Some(baseline) = baseline else {
        return false;
    };
    if is_working_status(Some(baseline)) {
        return false;
    }
    env.turn_started(pane_id, baseline)
}

/// The baseline captured for a send, distinct from a status sampled when the ladder starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusBaseline {
    /// Sample status at ladder entry, after the dead-session check.
    ReadNow,
    /// Preserve the caller's captured status, including `None` for unreadable.
    Captured(Option<String>),
}

/// `read_screen`: the one way a component asks the pane what it looks like.
pub fn read_screen(
    env: &dyn SendEnv,
    pane_id: &str,
    lines: Option<i64>,
    ansi: bool,
) -> Option<String> {
    env.read_pane(pane_id, lines.unwrap_or(CONSUMPTION_READ_LINES), ansi)
}

/// `read_box`: `(screen, text, detail)` from one read.
pub fn read_box(env: &dyn SendEnv, pane_id: &str) -> (Option<String>, String, String) {
    match read_screen(env, pane_id, None, true) {
        None => (
            None,
            String::new(),
            "the pane could not be read".to_string(),
        ),
        Some(screen) => {
            let (text, detail) = box_text(&screen);
            (Some(screen), text, detail)
        }
    }
}

/// `_dead_session`: the dead-session marker at the BOTTOM of the pane, or `None`. A plain read
/// (a banner needs no styling); only the tail counts, so a live pane whose transcript quotes the
/// phrase is not dead.
pub fn dead_session(env: &dyn SendEnv, pane_id: &str) -> Option<String> {
    let screen = read_screen(env, pane_id, Some(40), false)?;
    banner_marker(&screen, &DEAD_SESSION_MARKERS)
}

/// `send_blocker`: why a send cannot go now, as `(state, reason)`, or `None` if it can. ONE
/// function, so `delivery`, the queue worker and `send_text` cannot disagree.
pub fn send_blocker(env: &dyn SendEnv, pane_id: &str) -> Option<(&'static str, String)> {
    if let Some(marker) = dead_session(env, pane_id) {
        return Some(("dead", format!("dead session ({marker})")));
    }
    if env.session_process_gone(pane_id) {
        return Some(("dead", "the lane's recorded process is gone".to_string()));
    }
    let (screen, _text, detail) = read_box(env, pane_id);
    if screen.as_deref().is_some_and(modal_open) {
        // A panel is not a draft; keys/Enter are how it is dismissed.
        return None;
    }
    if detail.starts_with(AMBIGUOUS_COMPOSER)
        && federated_composer(env.federated_input(), screen.as_deref().unwrap_or("")).is_none()
    {
        return Some(("unknown", detail));
    }
    let harness = env.pane_harness(pane_id);
    if !harness_queues_while_busy(harness.as_deref()) {
        // A working indicator sits a few lines above the composer, so this one check looks a
        // little wider than the 3-line status window.
        if let Some(screen) = &screen {
            if status_marker(screen, &WORKING_MARKERS, 8, true, true).is_some() {
                let who = harness.as_deref().unwrap_or("this harness");
                return Some((
                    "blocked",
                    format!(
                        "{who} does not queue text typed during a turn (measured: notices are lost), so this waits for the turn to end"
                    ),
                ));
            }
        }
    }
    None
}

/// `_clear_to_send`: true once the box has read clear on two checks `TYPING_SETTLE_S` apart.
/// `wait(seconds)` is the settle wait and returns `true` when it was INTERRUPTED (the Python's
/// `stop_event.wait`), which makes this false; production passes a sleep that returns false.
pub fn clear_to_send(env: &dyn SendEnv, pane_id: &str, wait: &dyn Fn(f64) -> bool) -> bool {
    if send_blocker(env, pane_id).is_some() {
        return false;
    }
    if wait(TYPING_SETTLE_S) {
        return false;
    }
    // The second read is for REDRAW safety, not for somebody's text. An unreadable screen is
    // inconclusive and does not hold - the send's own confirmation decides.
    match read_screen(env, pane_id, None, true) {
        None => true,
        Some(_) => dead_session(env, pane_id).is_none(),
    }
}

/// `_box_holds`: true if `text` still sits in the pane's input box, false if nothing of ours is
/// waiting there, `None` if the pane could not be read or the box state is unknowable.
pub fn box_holds(env: &dyn SendEnv, pane_id: &str, text: &str) -> Option<bool> {
    let screen = read_screen(env, pane_id, None, true)?;
    if modal_open(&screen) {
        // A panel is open: our text left the box and opened it. Enter would act on the panel.
        return Some(false);
    }
    if let Some(federated) = federated_composer(env.federated_input(), &screen) {
        // A nonempty live composer is pending even when scrolled.
        return Some(!federated.is_empty());
    }
    let shown = typed_text(&screen).unwrap_or_default();
    if shown.is_empty() {
        // "EMPTY" IS NOT ALWAYS PROOF: when the reader decided the last prompt row is
        // TRANSCRIPT, the box may hold a multi-line draft. Unknown, which the submit ladder
        // treats as "wait for a turn" (VP i24194).
        let (text_seen, detail) = box_text(&screen);
        if text_seen.is_empty() && detail.contains("transcript") {
            return None;
        }
        return Some(false);
    }
    Some(is_our_send(&shown, Some(text), true))
}

/// `_send_submit_key`: submit using the harness's current queue action. Codex Tab queues during
/// a turn; Claude and pi queue with Enter. Re-read on every attempt: the turn or a dialog can
/// change between retries. A learned Tab must never leak into a later idle send.
pub fn send_submit_key(
    env: &mut dyn SendEnv,
    pane_id: &str,
    fallback: &str,
    busy_hint: bool,
) -> (bool, String, Option<String>) {
    let Some(screen) = read_screen(env, pane_id, None, true) else {
        return (
            false,
            "submission unconfirmed: pane unreadable before key".to_string(),
            None,
        );
    };
    if modal_open(&screen) {
        return (
            false,
            "submission held: a dialog opened before the key".to_string(),
            None,
        );
    }
    let mut key = if matches!(fallback, "Enter" | "C-m") {
        fallback
    } else {
        "Enter"
    };
    if env.pane_harness(pane_id).as_deref() == Some("codex") {
        let working = busy_hint
            || matches!(
                env.read_status(pane_id).as_deref(),
                Some("working" | "busy" | "running" | "processing")
            )
            || status_marker(
                &screen,
                &WORKING_MARKERS,
                CONSUMPTION_READ_LINES as usize,
                true,
                true,
            )
            .is_some()
            || strip_ansi(&screen)
                .to_lowercase()
                .contains("tab to queue message");
        if working {
            key = "Tab";
        }
    }
    let (ok, detail) = env.send_key(pane_id, key);
    (ok, detail, Some(key.to_string()))
}

/// The four outcomes a caller can act on, as one machine-readable field (`Message.OUTCOMES`).
pub const OUTCOMES: [&str; 5] = ["ok", "stored", "queued", "unconfirmed", "refused"];

/// `Message`: one direct message waiting in (or delivered from) a pane's outbox. The `deliver`
/// callable and `done` event are the outbox task's business (next slice); this is the state and
/// its wire form.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub id: String,
    pub pane_id: String,
    pub target_label: String,
    pub text: String,
    /// "send_text" or the dunk id for a fired dunk.
    pub source: String,
    /// queued | sent | failed | cancelled
    pub status: String,
    pub hold_while_typing: bool,
    /// "text", or "keys" (how a panel is dismissed).
    pub kind: String,
    /// `send --no-enter` leaves the text in the box deliberately; not a stalled delivery.
    pub press_enter: bool,
    pub attempt_generation: Option<String>,
    /// An inbox notice: one per pane, never stacked.
    pub is_notice: bool,
    pub notify_for: Vec<String>,
    pub detail: Option<String>,
    pub holds: i64,
    pub next_attempt_at: Option<f64>,
    pub hold_signature: Option<String>,
    pub created_at: f64,
    pub finished_at: Option<f64>,
}

impl Message {
    pub fn new(id: &str, pane_id: &str, target_label: &str, text: &str, source: &str) -> Message {
        Message {
            id: id.to_string(),
            pane_id: pane_id.to_string(),
            target_label: target_label.to_string(),
            text: text.to_string(),
            source: source.to_string(),
            status: "queued".to_string(),
            hold_while_typing: true,
            kind: "text".to_string(),
            press_enter: true,
            attempt_generation: None,
            is_notice: false,
            notify_for: Vec::new(),
            detail: None,
            holds: 0,
            next_attempt_at: None,
            hold_signature: None,
            created_at: clock::now_s(),
            finished_at: None,
        }
    }

    /// `_queued_message`.
    pub fn queued_message(&self) -> String {
        if self.holds > 0 {
            format!("queued: the box in {} would not clear", self.target_label)
        } else {
            format!(
                "queued for {}, delivering once the input box is clear",
                self.target_label
            )
        }
    }

    /// `outcome`: "unconfirmed" is NOT a refusal and must not invite a blind resend.
    pub fn outcome(&self) -> &'static str {
        match self.status.as_str() {
            "queued" => "queued",
            "sent" => "ok",
            _ => {
                let detail = self.detail.clone().unwrap_or_default().to_lowercase();
                if detail.contains("unconfirmed") || detail.contains("did not submit") {
                    "unconfirmed"
                } else {
                    "refused"
                }
            }
        }
    }

    /// `to_dict`, evaluated at `now`.
    pub fn to_dict_at(&self, now: f64) -> Value {
        let queued = self.status == "queued";
        json!({
            "message_id": self.id, "pane_id": self.pane_id, "target_label": self.target_label,
            "text": self.text, "source": self.source, "status": self.status,
            "queued": queued,
            "ok": if queued { Value::Null } else { json!(self.status == "sent") },
            "outcome": self.outcome(),
            "message": if queued {
                self.queued_message()
            } else {
                match self.detail.as_deref() {
                    Some(detail) if !detail.is_empty() => detail.to_string(),
                    _ => self.status.clone(),
                }
            },
            "holds": self.holds, "created_at": self.created_at, "finished_at": self.finished_at,
            "waiting_s": if queued { json!((now - self.created_at) as i64) } else { Value::Null },
        })
    }

    pub fn to_dict(&self) -> Value {
        self.to_dict_at(clock::now_s())
    }
}

/// Snapshot persistence is deliberately invoked after worker startup and after the
/// outbox mutex is released, matching Python's `_enqueue` then `_save` ordering.
/// The store reads the current whole-Flock state when called, so a fast worker may
/// already have changed the queue by then.
pub trait SnapshotStore: Send + Sync {
    fn save(&self, flock: &Flock) -> Result<(), DunkError>;
}

/// Ensure the per-pane delivery worker exists. `enqueue` calls this while holding
/// the outbox mutex, after appending the message. Implementations must not re-enter
/// the outbox during this call.
pub trait OutboxWake: Send + Sync {
    fn ensure_worker(&self, flock: &Flock, pane_id: &str) -> Result<(), DunkError>;
}

fn outbox_lock(
    outbox: &Mutex<BTreeMap<String, Vec<Message>>>,
) -> MutexGuard<'_, BTreeMap<String, Vec<Message>>> {
    outbox
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Flock {
    /// Allocate a unique monotonically increasing `mN` message identity and build
    /// the queued message record. Delivery behavior is supplied by later leaves.
    pub fn new_message(
        &self,
        pane_id: &str,
        target_label: &str,
        text: &str,
        source: &str,
    ) -> Result<Message, DunkError> {
        let id = self
            .next_message_id
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .map_err(|_| DunkError::Refused("message id exhausted".into()))?;
        Ok(Message::new(
            &format!("m{id}"),
            pane_id,
            target_label,
            text,
            source,
        ))
    }

    /// Append a message to its pane's FIFO, ensure its worker under the queue lock,
    /// then save the current whole-Flock snapshot after releasing that lock.
    pub fn enqueue(
        &self,
        message: Message,
        wake: &dyn OutboxWake,
        store: &dyn SnapshotStore,
    ) -> Result<(), DunkError> {
        let pane_id = message.pane_id.clone();
        {
            let mut outbox = outbox_lock(&self.outbox);
            outbox.entry(pane_id.clone()).or_default().push(message);
            wake.ensure_worker(self, &pane_id)?;
        }
        store.save(self)
    }

    /// Current pane queues for the snapshot-store seam and deterministic tests.
    pub fn outbox_snapshot(&self) -> BTreeMap<String, Vec<Message>> {
        outbox_lock(&self.outbox).clone()
    }
}
