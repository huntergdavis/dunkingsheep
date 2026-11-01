//! T33 (t801), slice 1: the send DECISIONS - `Message`, `dead_session`, `send_blocker`,
//! `clear_to_send`, `box_holds`, `send_submit_key`. Ports `Message` (state and `outcome` /
//! `to_dict`), `Flock._dead_session`, `send_blocker`, `_clear_to_send`, `_box_holds`,
//! `_send_submit_key`, `_pane_harness`, `read_screen`/`read_box`.
//!
//! The Python reaches the backend, the lease table and the clock through `self`; here they are
//! [`SendEnv`], so every decision is testable with a scripted pane. NOT in this slice (next
//! slices of t801): `send_now`/`send_keys_now`, `_enqueue`/`_deliver` and the per-pane outbox
//! task, `submit_text`/`press_keys`, `_press_until_submitted`, `_flush_once`, flush/clear_box.
//!
//! POLICY (owner, 2026-10-23/24) kept verbatim: a busy agent is NOT a hold reason, and there
//! is NO "a human is typing" hold - every message goes through this daemon, so a non-empty box
//! is not evidence of a person. The only refusals are a DEAD session (read, not claimed) and an
//! unknowable composer boundary.

use serde_json::{json, Value};

use crate::clock;
use crate::terminal::composer::federated_composer;
use crate::terminal::screen::{
    banner_marker, box_text, is_our_send, modal_open, status_marker, strip_ansi, typed_text,
    AMBIGUOUS_COMPOSER, DEAD_SESSION_MARKERS, WORKING_MARKERS,
};

/// `CONSUMPTION_READ_LINES`.
pub const CONSUMPTION_READ_LINES: i64 = 40;
/// `TYPING_SETTLE_S`: the box must read clear on two checks this far apart.
pub const TYPING_SETTLE_S: f64 = 1.0;

/// `HARNESS_QUEUES_WHILE_BUSY` (measured, not assumed): claude, pi and codex queue what is typed
/// while they work (codex via Tab). An unknown harness keeps the capability hold.
pub fn harness_queues_while_busy(harness: Option<&str>) -> bool {
    matches!(harness, Some("claude" | "pi" | "codex"))
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
