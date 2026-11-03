//! T33 (t801), slices 1 and t1742: send decisions and the unguarded submit-confirmation ladder -
//! `Message`, `dead_session`, `send_blocker`, `clear_to_send`, `box_holds`, `send_submit_key`,
//! `press_until_submitted`. Ports `Message` (state and `outcome` / `to_dict`),
//! `Flock._dead_session`, `send_blocker`, `_clear_to_send`, `_box_holds`, `_send_submit_key`,
//! `_press_until_submitted`, `_pane_harness`, `read_screen`/`read_box`.
//!
//! The Python reaches the backend, the lease table and the clock through `self`; here they are
//! [`SendEnv`], so every decision is testable with a scripted pane. NOT in this slice (next
//! NOT in these slices (later t801 work): `send_now`/`send_keys_now`, `_enqueue`/`_deliver`,
//! the per-pane outbox task, and public `clear_box`.
//!
//! POLICY (owner, 2026-10-23/24) kept verbatim: a busy agent is NOT a hold reason, and there
//! is NO "a human is typing" hold - every message goes through this daemon, so a non-empty box
//! is not evidence of a person. The only refusals are a DEAD session (read, not claimed) and an
//! unknowable composer boundary.

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::Ordering;

use super::commands_inbox::NOTICE_PREFIX;
use super::guarded_submit::confirm_guarded_submission;
use super::panes::PaneInfoEnv;
use super::resolve::{resolve_target, resolve_target_dict};
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::ThreadId;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::Flock;
use crate::clock;
use crate::error::DunkError;
use crate::terminal::composer::{
    composer_region_text, federated_composer, styled_composer_boundary, styled_sigil_state,
};
use crate::terminal::screen::{
    banner_marker, box_text, is_our_send, modal_open, prompt_body, splitlines, status_marker,
    strip_ansi, typed_text, AMBIGUOUS_COMPOSER, DEAD_SESSION_MARKERS, WORKING_MARKERS,
};
use crate::terminal::screen::{collapse, CONSUMPTION_PREFIX_CHARS};

const DEFAULT_TYPING_LOCK_TIMEOUT_S: f64 = 10.0;
const TYPING_LOCK_POLL: Duration = Duration::from_millis(10);

#[derive(Debug, Default)]
struct TypingLockState {
    owner: Option<ThreadId>,
    depth: usize,
}

/// The process-wide reentrant typing mutex. Its state is kept separate from the
/// per-pane locks because only one keystroke burst at a time may type anywhere.
#[derive(Debug, Default)]
pub(crate) struct TypingLock {
    state: Mutex<TypingLockState>,
    changed: Condvar,
    owner: Mutex<Option<(String, f64)>>,
}

/// An acquired global typing lock. Drop restores the previous owner and releases
/// one reentrant level, including when a burst returns early or unwinds.
///
/// This guard is intentionally not `Send`: its destructor must run on the same
/// thread that acquired the reentrant lock. The type system rejects moving it:
///
/// ```compile_fail
/// fn require_send<T: Send>() {}
/// fn typing_guard_must_not_be_send() {
///     require_send::<ds_core::flock::sends::TypingGuard<'static>>();
/// }
/// ```
#[derive(Debug)]
pub struct TypingGuard<'a> {
    lock: &'a TypingLock,
    previous_owner: Option<(String, f64)>,
    _not_send: PhantomData<Rc<()>>,
}

impl Drop for TypingGuard<'_> {
    fn drop(&mut self) {
        let mut state = lock_unpoison(&self.lock.state);
        let mut owner = lock_unpoison(&self.lock.owner);
        *owner = self.previous_owner.take();
        drop(owner);

        debug_assert!(state.depth > 0);
        state.depth -= 1;
        if state.depth == 0 {
            state.owner = None;
            self.lock.changed.notify_one();
        }
    }
}

fn lock_unpoison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn typing_timeout() -> Duration {
    let seconds = std::env::var("DUNKINGSHEEP_TYPING_LOCK_TIMEOUT_S")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0 && *seconds <= 86_400.0)
        .unwrap_or(DEFAULT_TYPING_LOCK_TIMEOUT_S);
    Duration::from_secs_f64(seconds)
}

fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn owner_description(owner: Option<&(String, f64)>) -> String {
    match owner {
        Some((pane_id, since)) => format!("('{}', {since})", pane_id.replace('\'', "\\'")),
        None => "None".into(),
    }
}

impl TypingLock {
    fn acquire(
        &self,
        pane_id: &str,
        timeout: Duration,
    ) -> Result<Option<(String, f64)>, DunkError> {
        let started = Instant::now();
        let deadline = started.checked_add(timeout).unwrap_or(started);
        let thread_id = std::thread::current().id();
        let mut state = lock_unpoison(&self.state);
        loop {
            match state.owner.as_ref() {
                None => {
                    state.owner = Some(thread_id);
                    state.depth = 1;
                    break;
                }
                Some(owner) if *owner == thread_id => {
                    state.depth += 1;
                    break;
                }
                Some(_) => {
                    let now = Instant::now();
                    if now >= deadline {
                        let owner = lock_unpoison(&self.owner);
                        return Err(DunkError::Refused(format!(
                            "the typing lock was held for more than {}s by {}; refusing to type concurrently",
                            timeout.as_secs(),
                            owner_description(owner.as_ref())
                        )));
                    }
                    let remaining = deadline.saturating_duration_since(now);
                    let wait = remaining.min(TYPING_LOCK_POLL);
                    let (next, _) = self
                        .changed
                        .wait_timeout(state, wait)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    state = next;
                }
            }
        }

        let mut owner = lock_unpoison(&self.owner);
        let previous = owner.replace((pane_id.to_string(), now_seconds()));
        drop(owner);
        drop(state);
        Ok(previous)
    }
}

impl Flock {
    /// Return the stable mutex for one pane. Acquire this lock before
    /// [`Flock::typing_guard`] and hold it across the whole send/confirmation path.
    pub fn send_lock_for(&self, pane_id: &str) -> Arc<Mutex<()>> {
        let mut locks = lock_unpoison(&self.pane_send_locks);
        locks
            .entry(pane_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Acquire the fleet-wide typing lock for one key burst. The guard must be
    /// dropped before any confirmation wait or retry delay. Callers acquire the
    /// per-pane send lock first, preserving the Python lock order.
    pub fn typing_guard(&self, pane_id: &str) -> Result<TypingGuard<'_>, DunkError> {
        self.typing_guard_with_timeout(pane_id, typing_timeout())
    }

    pub(super) fn typing_guard_with_timeout(
        &self,
        pane_id: &str,
        timeout: Duration,
    ) -> Result<TypingGuard<'_>, DunkError> {
        let previous_owner = self.typing_lock.acquire(pane_id, timeout)?;
        Ok(TypingGuard {
            lock: &self.typing_lock,
            previous_owner,
            _not_send: PhantomData,
        })
    }
}

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
/// `_remember_typed`: how many recent texts to retain for each pane.
const REMEMBERED_SENDS: usize = 4;

/// Hold detail shared with Python's `BOX_HOLD` contract.
pub const BOX_HOLD: &str = "the box would not clear (text left untouched)";
#[allow(dead_code)] // Used by the dependent outbox worker's delivery entry point.
const HOLD_BACKOFF_BASE_S: f64 = 5.0;
#[allow(dead_code)] // Used by the dependent outbox worker's delivery entry point.
const HOLD_BACKOFF_CAP_S: f64 = 300.0;
#[allow(dead_code)] // Used by the dependent outbox worker's delivery entry point.
const HOLD_ATTEMPT_LIMIT: i64 = 10;

#[allow(dead_code)] // Used by the dependent outbox worker's delivery entry point.
fn hold_backoff_s(attempts: i64) -> f64 {
    (HOLD_BACKOFF_BASE_S * 2_f64.powi(attempts.saturating_sub(1).max(0) as i32))
        .min(HOLD_BACKOFF_CAP_S)
}

pub type HoldSignature = (String, String);

pub(super) fn hold_signature(env: &dyn SendEnv, pane_id: &str) -> HoldSignature {
    let Some(screen) = env.read_pane(pane_id, CONSUMPTION_READ_LINES, true) else {
        return ("gone".into(), String::new());
    };
    if modal_open(&screen) {
        let modal = collapse(&strip_ansi(&screen));
        let start = modal
            .char_indices()
            .nth(modal.chars().count().saturating_sub(4000))
            .map(|(index, _)| index)
            .unwrap_or(0);
        return ("modal".into(), modal[start..].to_owned());
    }
    if let Some(text) = federated_composer(env.federated_input(), &screen) {
        return ("box".into(), text);
    }
    let (text, detail) = box_text(&screen);
    if detail.starts_with(AMBIGUOUS_COMPOSER) {
        return ("gone".into(), String::new());
    }
    match typed_text(&screen) {
        Some(_) => ("box".into(), text),
        None => ("gone".into(), String::new()),
    }
}

/// Result of one idempotent outbox delivery transition.
#[allow(dead_code)] // Consumed by the dependent outbox worker/runtime integration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeliveryOutcome {
    Sent,
    Failed,
    Held,
    Undeliverable,
    AlreadyDecided,
    Missing,
}

/// Terminal services used by the bounded send preparation path. Composer ownership comes from
/// the flock's recent-send history and queued outbox; a shape match alone never authorizes a key.
pub trait SendPreparationEnv: FlushEnv + super::send_keys::KeyPressEnv {
    fn send_text(&mut self, pane_id: &str, text: &str) -> (bool, String);
    fn attempt_generation(&self, pane_id: &str) -> Option<String>;
    fn persist_typed_attempt(
        &mut self,
        pane_id: &str,
        text: &str,
        press_enter: bool,
        generation: Option<&str>,
    ) -> bool;
}

/// Options that affect only the final, lock-protected composer preflight.
#[derive(Debug, Clone, Copy)]
pub struct SendGuardOptions {
    pub hold_while_typing: bool,
    pub allow_modal: bool,
    pub preserve_draft: bool,
}

impl Default for SendGuardOptions {
    fn default() -> Self {
        Self {
            hold_while_typing: true,
            allow_modal: false,
            preserve_draft: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SendAction {
    pub ok: bool,
    pub detail: String,
    confirmation: Option<SubmitConfirmation>,
    key_confirmation: Option<(String, String)>,
}

#[derive(Debug, Clone)]
struct SubmitConfirmation {
    pane_id: String,
    text: String,
    baseline: Option<String>,
    busy_hint: bool,
    guarded: bool,
}

impl SendAction {
    pub fn completed(ok: bool, detail: impl Into<String>) -> Self {
        Self {
            ok,
            detail: detail.into(),
            confirmation: None,
            key_confirmation: None,
        }
    }
}

/// One deliberately short submit flush. The guard remains held across the key, one poll, and
/// the single box read; the long confirmation ladder must only run after it is dropped.
pub fn flush_once(
    env: &mut dyn FlushEnv,
    pane_id: &str,
    text: &str,
    busy_hint: bool,
    _guard: &TypingGuard<'_>,
) -> bool {
    let (ok, _detail, _) = send_submit_key(env, pane_id, "Enter", busy_hint);
    if !ok {
        return false;
    }
    env.sleep_s(SUBMIT_POLL_S[0]);
    box_holds(env, pane_id, text) == Some(false)
}

/// Make the composer safe for this send. Unknown or human-draft content is never submitted.
/// Foreign text is submitted once only when the caller explicitly permits preserving it, which
/// matches `_ensure_box_clear`; a failed one-shot leaves the original text in place.
pub fn ensure_box_clear(
    flock: &Flock,
    env: &mut dyn SendPreparationEnv,
    pane_id: &str,
    allow_modal: bool,
    preserve_draft: bool,
    busy_hint: bool,
    guard: &TypingGuard<'_>,
) -> Result<(), &'static str> {
    let Some(screen) = env.read_pane(pane_id, CONSUMPTION_READ_LINES, true) else {
        return Ok(());
    };
    if modal_open(&screen) {
        return if allow_modal { Ok(()) } else { Err("dialog") };
    }
    if let Some(federated) = federated_composer(env.federated_input(), &screen) {
        if federated.is_empty() {
            return Ok(());
        }
        if preserve_draft {
            return Err("human_draft");
        }
        return if flush_once(env, pane_id, &federated, busy_hint, guard) {
            Ok(())
        } else {
            Err("federated-pending-input")
        };
    }

    let (shown, detail) = box_text(&screen);
    let known = flock.known_send_texts(pane_id);
    if detail.starts_with(AMBIGUOUS_COMPOSER) {
        if let Some(region) = owned_composer_region(flock, pane_id, &screen, &known) {
            return if flush_once(env, pane_id, &region, busy_hint, guard) {
                Ok(())
            } else {
                Err("stranded-will-not-submit")
            };
        }
        return Err("ambiguous-composer");
    }
    let shown = if shown.is_empty() {
        typed_text(&screen).unwrap_or_default()
    } else {
        shown
    };
    if shown.is_empty() {
        return Ok(());
    }

    let shown_c = collapse(&shown);
    if known.iter().any(|candidate| {
        let candidate = collapse(candidate);
        candidate.starts_with("/model ") && candidate != shown_c && candidate.ends_with(&shown_c)
    }) {
        return Err("partial-model-command");
    }
    let ours = flock.looks_like_ours(pane_id, &shown);
    if preserve_draft && !ours {
        return Err("human_draft");
    }
    if flush_once(env, pane_id, &shown, busy_hint, guard) {
        Ok(())
    } else if ours {
        Err("stranded-will-not-submit")
    } else {
        Err("foreign-text-will-not-submit")
    }
}

/// Type text and perform its initial submit key under one typing guard. The returned action owns
/// the later confirmation; call [`confirm_send_action`] only after dropping the guard.
pub fn submit_text(
    flock: &Flock,
    env: &mut dyn SendPreparationEnv,
    pane_id: &str,
    text: &str,
    press_enter: bool,
    check_box: bool,
    guard: Option<&TypingGuard<'_>>,
) -> SendAction {
    submit_text_with_options(
        flock,
        env,
        pane_id,
        text,
        SubmitOptions {
            press_enter,
            check_box,
            attempt_generation: AttemptGeneration::Capture,
        },
        guard,
    )
}

#[derive(Clone, Copy)]
struct SubmitOptions<'a> {
    press_enter: bool,
    check_box: bool,
    attempt_generation: AttemptGeneration<'a>,
}

#[derive(Clone, Copy)]
enum AttemptGeneration<'a> {
    Capture,
    Captured(Option<&'a str>),
}

/// Delivery passes its pre-guard generation snapshot so a pane replacement during preflight
/// cannot transfer receipt authority to the replacement pane.
fn submit_text_with_options(
    flock: &Flock,
    env: &mut dyn SendPreparationEnv,
    pane_id: &str,
    text: &str,
    options: SubmitOptions<'_>,
    guard: Option<&TypingGuard<'_>>,
) -> SendAction {
    let guarded = guard.is_some();
    let mut perform = |guard: &TypingGuard<'_>| {
        if let Some(dead) = dead_session(env, pane_id) {
            return Err(format!(
                "agent session is dead ({dead}); not submitting into a dead composer"
            ));
        }
        if options.check_box {
            match ensure_box_clear(flock, env, pane_id, false, false, false, guard) {
                Ok(()) => {}
                Err(reason) => return Err(format!("{BOX_HOLD} ({reason})")),
            }
        }
        let baseline = env.read_status(pane_id);
        let generation = match options.attempt_generation {
            AttemptGeneration::Capture => env.attempt_generation(pane_id),
            AttemptGeneration::Captured(generation) => generation.map(str::to_owned),
        };
        let before = env
            .read_pane(pane_id, CONSUMPTION_READ_LINES, true)
            .unwrap_or_default();
        let busy_hint = env.pane_harness(pane_id).as_deref() == Some("codex")
            && (is_working_status(baseline.as_deref())
                || status_marker(
                    &before,
                    &WORKING_MARKERS,
                    CONSUMPTION_READ_LINES as usize,
                    true,
                    true,
                )
                .is_some()
                || strip_ansi(&before)
                    .to_lowercase()
                    .contains("tab to queue message"));
        let (ok, detail) = env.send_text(pane_id, text);
        if !ok {
            return Err(detail);
        }
        flock.remember_typed(pane_id, text);
        if !env.persist_typed_attempt(pane_id, text, options.press_enter, generation.as_deref()) {
            return Err("typed but not submitted: durable attempt receipt failed".into());
        }
        if !options.press_enter {
            return Ok(SendAction::completed(true, "sent"));
        }
        let (ok, detail, _key) = send_submit_key(env, pane_id, "Enter", busy_hint);
        if !ok {
            return Err(detail);
        }
        Ok(SendAction {
            ok: true,
            detail: "sent".into(),
            confirmation: Some(SubmitConfirmation {
                pane_id: pane_id.to_owned(),
                text: text.to_owned(),
                baseline,
                busy_hint,
                guarded,
            }),
            key_confirmation: None,
        })
    };

    let prepared = match guard {
        Some(guard) => perform(guard),
        None => match flock.typing_guard(pane_id) {
            Ok(guard) => perform(&guard),
            Err(error) => return SendAction::completed(false, error.to_string()),
        },
    };
    drop(perform);
    match prepared {
        Ok(value) => value,
        Err(detail) => SendAction::completed(false, detail),
    }
}

/// Run confirmation after the typing guard has been released. `guarded=true` selects the bounded
/// short rounds; otherwise the accepted long retry ladder is used.
pub fn confirm_send_action(env: &mut dyn SendPreparationEnv, action: &mut SendAction) {
    if let Some((pane_id, text)) = action.key_confirmation.take() {
        let result = env.confirm_pending(&pane_id, &text);
        action.ok = result.0;
        action.detail = result.1;
        return;
    }
    let Some(confirmation) = action.confirmation.take() else {
        return;
    };
    let baseline = StatusBaseline::Captured(confirmation.baseline);
    let result = if confirmation.guarded {
        confirm_guarded_submission(
            env,
            &confirmation.pane_id,
            &confirmation.text,
            "Enter",
            confirmation.busy_hint,
            baseline,
        )
    } else {
        press_until_submitted(
            env,
            &confirmation.pane_id,
            &confirmation.text,
            true,
            confirmation.busy_hint,
            baseline,
        )
    };
    action.ok = result.0;
    action.detail = result.1;
}

/// Route an already-normalized key list once while the caller holds the typing guard. Pending
/// text confirmation is deferred into the returned action so its waits occur after lock release.
pub fn press_keys(
    env: &mut dyn SendPreparationEnv,
    pane_id: &str,
    normalized_keys: &[String],
    _guard: &TypingGuard<'_>,
) -> SendAction {
    struct DeferConfirm<'a> {
        inner: &'a mut dyn super::send_keys::KeyPressEnv,
        pending: Option<String>,
    }
    impl super::send_keys::KeyPressEnv for DeferConfirm<'_> {
        fn send_keys(&mut self, pane_id: &str, keys: &[String]) -> (bool, String) {
            self.inner.send_keys(pane_id, keys)
        }
        fn pending_text(&mut self, pane_id: &str) -> Option<String> {
            self.inner.pending_text(pane_id)
        }
        fn confirm_pending(&mut self, _pane_id: &str, text: &str) -> (bool, String) {
            self.pending = Some(text.to_owned());
            (
                true,
                "confirmation deferred until after typing guard release".into(),
            )
        }
    }
    let mut deferred = DeferConfirm {
        inner: env,
        pending: None,
    };
    let result = super::send_keys::press_keys_unlocked(&mut deferred, pane_id, normalized_keys);
    if let Some(text) = deferred.pending {
        SendAction {
            ok: result.0,
            detail: result.1,
            confirmation: None,
            key_confirmation: Some((pane_id.to_owned(), text)),
        }
    } else {
        SendAction::completed(result.0, result.1)
    }
}

/// Final guarded decision: same-pane lock, fleet typing guard, permission recheck, box policy,
/// then one action. `ok=None` distinguishes a held send from an action that ran and failed.
pub fn send_guarded(
    flock: &Flock,
    env: &mut dyn SendPreparationEnv,
    pane_id: &str,
    options: SendGuardOptions,
    mut permission: impl FnMut() -> Option<String>,
    action: impl FnOnce(&mut dyn SendPreparationEnv, &TypingGuard<'_>) -> Result<SendAction, String>,
) -> (bool, Option<bool>, String) {
    let pane_lock = flock.send_lock_for(pane_id);
    let _pane_guard = lock_unpoison(&pane_lock);
    let typing_guard = match flock.typing_guard(pane_id) {
        Ok(guard) => guard,
        Err(error) => return (false, None, error.to_string()),
    };
    if let Some(detail) = permission() {
        return (false, None, detail);
    }
    if options.hold_while_typing {
        if let Some(marker) = dead_session(env, pane_id) {
            return (false, None, format!("dead session ({marker})"));
        }
        if env.session_process_gone(pane_id) {
            return (
                false,
                None,
                "the lane's recorded process is gone".to_string(),
            );
        }
        match ensure_box_clear(
            flock,
            env,
            pane_id,
            options.allow_modal,
            options.preserve_draft,
            false,
            &typing_guard,
        ) {
            Ok(()) => {}
            Err(reason) => return (false, None, format!("{BOX_HOLD} ({reason})")),
        }
    }
    let result = action(env, &typing_guard);
    drop(typing_guard);
    match result {
        Ok(action) if !action.ok && action.detail.starts_with(BOX_HOLD) => {
            (false, None, action.detail)
        }
        Ok(mut action) => {
            // Keep the per-pane lock across confirmation, but release the global typing lock.
            confirm_send_action(env, &mut action);
            (true, Some(action.ok), action.detail)
        }
        Err(detail) => (true, Some(false), detail),
    }
}

/// Python's `_composer_ownership`: use plain geometry when it delimits one prompt; otherwise
/// permit the styled multi-sigil fallback only when the complete dim transcript spans are
/// accounted for and the complete live region exactly matches text owned by this flock.
fn owned_composer_region(
    flock: &Flock,
    pane_id: &str,
    screen: &str,
    known: &[String],
) -> Option<String> {
    let plain_region = composer_region_text(screen);
    if !plain_region.is_empty() {
        return exact_owned_region(&plain_region, known).then_some(plain_region);
    }

    let (status, region) = styled_composer_boundary(screen);
    if status != "delimited"
        || region.is_empty()
        || !dim_bodies_accounted(flock, pane_id, screen)
        || !exact_owned_region(&region, known)
    {
        return None;
    }
    Some(region)
}

fn exact_owned_region(region: &str, known: &[String]) -> bool {
    let region = collapse(region);
    !region.is_empty() && known.iter().any(|candidate| collapse(candidate) == region)
}

/// `_dim_bodies_accounted`: account each complete dim sigil span through the next sigil.
/// Sent history and every nonempty outbox row are provenance; the latter intentionally includes
/// statuses beyond `queued`, unlike the stricter live-composer ownership candidate set.
fn dim_bodies_accounted(flock: &Flock, pane_id: &str, screen: &str) -> bool {
    let rows = splitlines(screen);
    let sigils: Vec<(usize, Option<String>, &'static str)> = rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| {
            prompt_body(row).map(|body| (index, Some(body), styled_sigil_state(row)))
        })
        .collect();

    let mut known = flock
        .sent_text_by_pane
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(pane_id)
        .cloned()
        .unwrap_or_default();
    {
        let outbox = outbox_lock(&flock.outbox);
        known.extend(
            outbox
                .get(pane_id)
                .into_iter()
                .flatten()
                .filter(|message| !message.text.is_empty())
                .map(|message| message.text.clone()),
        );
    }
    let exact: Vec<String> = known
        .iter()
        .filter(|text| !text.is_empty())
        .map(|text| collapse(text))
        .collect();

    for (position, (index, body, state)) in sigils.iter().enumerate() {
        if *state != "dim" {
            continue;
        }
        let end = sigils
            .get(position + 1)
            .map(|(next, _, _)| *next)
            .unwrap_or(rows.len());
        let mut parts = vec![body.clone().unwrap_or_default()];
        parts.extend(
            rows[*index + 1..end]
                .iter()
                .map(|row| strip_ansi(row).trim().to_string()),
        );
        let body = collapse(&parts.join(" "));
        if body.is_empty() || !exact.iter().any(|candidate| candidate == &body) {
            return false;
        }
    }
    true
}

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

/// Stable identity for a composer string that has no queued Message row, such as a dunk notice.
/// This matches Python's `text:` plus the first 16 hexadecimal digits of SHA-256.
pub fn text_identity(content: &str) -> Option<String> {
    let flat = crate::stuck::collapse(content);
    if flat.is_empty() {
        return None;
    }
    let digest = Sha256::digest(flat.as_bytes());
    let prefix = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Some(format!("text:{prefix}"))
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
pub(super) fn is_working_status(status: Option<&str>) -> bool {
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

#[cfg(test)]
mod send_lock_tests {
    use super::*;
    use std::ffi::OsString;
    use std::sync::TryLockError;

    static TYPING_TIMEOUT_ENV_LOCK: Mutex<()> = Mutex::new(());

    struct RestoreTypingTimeout(Option<OsString>);

    impl Drop for RestoreTypingTimeout {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => std::env::set_var("DUNKINGSHEEP_TYPING_LOCK_TIMEOUT_S", value),
                None => std::env::remove_var("DUNKINGSHEEP_TYPING_LOCK_TIMEOUT_S"),
            }
        }
    }

    fn test_flock() -> (Arc<Flock>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let flock = Arc::new(Flock::new(
            dir.path().join("launches.json"),
            dir.path(),
            false,
        ));
        (flock, dir)
    }

    #[test]
    fn send_lock_serializes_the_same_pane_and_isolates_different_panes() {
        let (flock, _dir) = test_flock();
        let first = flock.send_lock_for("w1:p1");
        let same_pane = flock.send_lock_for("w1:p1");
        let other_pane = flock.send_lock_for("w1:p2");
        assert!(Arc::ptr_eq(&first, &same_pane));
        assert!(!Arc::ptr_eq(&first, &other_pane));

        let _held = first.lock().unwrap();
        assert!(matches!(
            same_pane.try_lock(),
            Err(TryLockError::WouldBlock)
        ));
        assert!(other_pane.try_lock().is_ok());
    }

    #[test]
    fn typing_lock_serializes_bursts_but_is_free_during_confirmation() {
        let (flock, _dir) = test_flock();
        let pane_send = flock.send_lock_for("w1:p1");
        let _whole_send = pane_send.lock().unwrap();

        let burst = flock
            .typing_guard_with_timeout("w1:p1", Duration::from_millis(20))
            .unwrap();
        let other_flock = flock.clone();
        let blocked_burst = std::thread::spawn(move || {
            other_flock
                .typing_guard_with_timeout("w1:p2", Duration::from_millis(20))
                .unwrap_err()
                .to_string()
        })
        .join()
        .unwrap();
        assert!(blocked_burst.contains("the typing lock was held for more than 0s by ('w1:p1',"));

        // Confirmation waits keep the whole-send lock but release the global lock, so
        // another pane can type its own burst while this pane waits for confirmation.
        drop(burst);
        let other_pane_burst = flock
            .typing_guard_with_timeout("w1:p2", Duration::from_millis(20))
            .unwrap();
        drop(other_pane_burst);
        assert!(matches!(
            pane_send.try_lock(),
            Err(TryLockError::WouldBlock)
        ));
    }

    #[test]
    fn typing_lock_timeout_refuses_before_the_guarded_action_runs() {
        let (flock, _dir) = test_flock();
        let _held = flock
            .typing_guard_with_timeout("w1:p1", Duration::from_millis(20))
            .unwrap();

        let other_flock = flock.clone();
        let action_runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let other_action_runs = action_runs.clone();
        let result = std::thread::spawn(move || {
            let guard = other_flock.typing_guard_with_timeout("w1:p2", Duration::ZERO)?;
            other_action_runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            drop(guard);
            Ok::<(), DunkError>(())
        })
        .join()
        .unwrap();
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("refusing to type concurrently"));

        assert_eq!(0, action_runs.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn public_typing_guard_honors_zero_environment_timeout() {
        let _env_lock = TYPING_TIMEOUT_ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let restore = RestoreTypingTimeout(std::env::var_os("DUNKINGSHEEP_TYPING_LOCK_TIMEOUT_S"));
        std::env::set_var("DUNKINGSHEEP_TYPING_LOCK_TIMEOUT_S", "0");

        let (flock, _dir) = test_flock();
        let held = flock.typing_guard("w1:p1").unwrap();
        let other_flock = flock.clone();
        let action_runs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let other_action_runs = action_runs.clone();
        let (sent, received) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let result = other_flock
                .typing_guard("w1:p2")
                .map(|guard| {
                    other_action_runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    drop(guard);
                })
                .map_err(|error| error.to_string());
            sent.send(result).unwrap();
        });

        let received = received.recv_timeout(Duration::from_secs(1));
        drop(held);
        waiter.join().unwrap();
        drop(restore);
        let result =
            received.expect("a zero timeout must refuse immediately rather than use the default");

        let error = result.unwrap_err();
        assert!(
            error.starts_with("the typing lock was held for more than 0s by ('w1:p1',"),
            "unexpected refusal: {error}"
        );
        assert_eq!(0, action_runs.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    fn typing_lock_is_reentrant_for_nested_same_thread_bursts() {
        let (flock, _dir) = test_flock();
        let outer = flock
            .typing_guard_with_timeout("w1:p1", Duration::from_millis(20))
            .unwrap();
        let inner = flock
            .typing_guard_with_timeout("w1:p1", Duration::from_millis(20))
            .unwrap();
        drop(inner);

        let other_flock = flock.clone();
        let blocked = std::thread::spawn(move || {
            other_flock
                .typing_guard_with_timeout("w1:p2", Duration::ZERO)
                .unwrap_err()
                .to_string()
        })
        .join()
        .unwrap();
        assert!(blocked.contains("('w1:p1',"));
        drop(outer);
        assert!(flock
            .typing_guard_with_timeout("w1:p2", Duration::from_millis(20))
            .is_ok());
    }
}

/// Baseline-aware turn confirmation: an unreadable or already-working baseline is never proof.
pub(super) fn turn_started(env: &mut dyn SubmitEnv, pane_id: &str, baseline: Option<&str>) -> bool {
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
#[allow(dead_code)] // Used by the later message-list integration slice.
const MESSAGE_HISTORY: usize = 50;
pub(crate) const DECIDED_KEPT: usize = 500;

#[derive(Debug, Default)]
struct CompletionState {
    #[allow(dead_code)] // Read by the later outbox runtime's outcome caller.
    started: bool,
    settled: bool,
}

/// The transient equivalent of Python's `threading.Event` on a message.
#[derive(Debug, Default)]
pub struct CompletionWaiter {
    state: Mutex<CompletionState>,
    changed: Condvar,
}

impl CompletionWaiter {
    #[allow(dead_code)] // Used by the separate terminal outcome interface in this leaf.
    fn begin(&self) -> bool {
        let mut state = lock_unpoison(&self.state);
        if state.started {
            return false;
        }
        state.started = true;
        true
    }

    #[allow(dead_code)] // Used by the separate terminal outcome interface in this leaf.
    fn settle(&self) {
        let mut state = lock_unpoison(&self.state);
        if !state.settled {
            state.settled = true;
            self.changed.notify_all();
        }
    }

    pub fn is_set(&self) -> bool {
        lock_unpoison(&self.state).settled
    }

    pub fn wait(&self) {
        let mut state = lock_unpoison(&self.state);
        while !state.settled {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }
}

impl PartialEq for CompletionWaiter {
    fn eq(&self, other: &Self) -> bool {
        self.is_set() == other.is_set()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DecidedTombstone {
    pub status: String,
    pub created_at: Option<f64>,
}

pub(crate) fn decided_tombstone_matches_row(
    tombstone: &DecidedTombstone,
    row_created_at: Option<f64>,
) -> bool {
    if !matches!(
        tombstone.status.as_str(),
        "sent" | "failed" | "cancelled" | "undeliverable"
    ) {
        return false;
    }
    let Some(tombstone_created_at) = tombstone.created_at.filter(|stamp| stamp.is_finite()) else {
        return false;
    };
    let Some(row_created_at) = row_created_at.filter(|stamp| stamp.is_finite()) else {
        return false;
    };
    (tombstone_created_at - row_created_at).abs() <= 1e-3
}

#[derive(Debug, Default)]
pub(crate) struct DecidedMessages {
    #[allow(dead_code)] // Read by the restore/message-list integration slices.
    pub by_id: BTreeMap<String, DecidedTombstone>,
    #[allow(dead_code)] // Preserves Python dict insertion order for bounded eviction.
    pub(crate) order: VecDeque<String>,
}

/// `Message`: one direct message waiting in (or delivered from) a pane's outbox. The completion
/// waiter is transient; the `deliver` callable belongs to the outbox worker slice.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub id: String,
    pub pane_id: String,
    pub target_label: String,
    pub text: String,
    /// "send_text" or the dunk id for a fired dunk.
    pub source: String,
    /// queued | typed | sent | failed | cancelled
    pub status: String,
    pub hold_while_typing: bool,
    /// "text", or "keys" (how a panel is dismissed).
    pub kind: String,
    /// `send --no-enter` leaves the text in the box deliberately; not a stalled delivery.
    pub press_enter: bool,
    /// True for a timer-triggered send (Python's transient `attempt.scheduled`).
    pub scheduled: bool,
    /// Original template of a scheduled attempt, retained for post-restart eligibility checks.
    pub schedule_template: Option<String>,
    pub attempt_generation: Option<String>,
    /// Time at which the guarded composer write completed, persisted in the stuck candidate.
    pub typed_at: Option<f64>,
    /// An inbox notice: one per pane, never stacked.
    pub is_notice: bool,
    pub notify_for: Vec<String>,
    pub detail: Option<String>,
    pub holds: i64,
    pub next_attempt_at: Option<f64>,
    pub hold_signature: Option<String>,
    pub hold_signature_kind: Option<String>,
    pub created_at: f64,
    pub finished_at: Option<f64>,
    /// Completion event shared by clones and deliberately omitted from the wire form.
    pub done: Arc<CompletionWaiter>,
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
            scheduled: false,
            schedule_template: None,
            attempt_generation: None,
            typed_at: None,
            is_notice: false,
            notify_for: Vec::new(),
            detail: None,
            holds: 0,
            next_attempt_at: None,
            hold_signature: None,
            hold_signature_kind: None,
            created_at: clock::now_s(),
            finished_at: None,
            done: Arc::new(CompletionWaiter::default()),
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

/// The latest delivery intent observed for one pane by the stuck observer.
///
/// This is in-memory only; snapshot persistence belongs to the separate snapshot leaf.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)] // The stuck-recovery consumer is owned by a later leaf.
pub(crate) struct StuckCandidate {
    pub message_id: String,
    pub created_at: f64,
    pub press_enter: bool,
    pub status: String,
    pub kind: String,
    pub scheduled: bool,
    pub source: String,
    pub notify_for: Vec<String>,
    pub text: String,
    pub label: String,
    pub generation: Option<String>,
    pub eligible: bool,
    pub superseded_by: String,
    pub transcript_texts: Vec<String>,
    pub typed_at: Option<f64>,
    pub native_binding: Option<(i64, String, String)>,
    pub recovery_outcome: Option<String>,
    pub recovery_detail: Option<String>,
    pub recovered_at: Option<f64>,
    pub receipts_reconciled: Option<bool>,
    pub native_refusal: Option<String>,
    pub unobservable_logged: Option<bool>,
    pub native_recovery: Option<bool>,
    pub schedule_template: Option<String>,
    pub retired_at: Option<f64>,
    pub consumed_at: Option<f64>,
    /// Unknown Python candidate keys are retained at top level for forward-compatible saves.
    pub extra_fields: Map<String, Value>,
}

impl StuckCandidate {
    #[allow(dead_code)] // Called when the later stuck-recovery leaf wires this state.
    fn from_message(message: &Message) -> Self {
        let eligible = (message.status == "failed"
            || message.status == "typed"
            || (message.scheduled && message.status == "sent"))
            && message.press_enter
            && message.kind == "text"
            && message
                .attempt_generation
                .as_deref()
                .is_some_and(|generation| !generation.is_empty());
        Self {
            message_id: message.id.clone(),
            created_at: message.created_at,
            press_enter: message.press_enter,
            status: message.status.clone(),
            kind: message.kind.clone(),
            scheduled: message.scheduled,
            source: message.source.clone(),
            notify_for: message.notify_for.clone(),
            text: message.text.clone(),
            label: message.target_label.clone(),
            generation: message.attempt_generation.clone(),
            eligible,
            superseded_by: message.id.clone(),
            transcript_texts: Vec::new(),
            typed_at: message.typed_at,
            native_binding: None,
            recovery_outcome: None,
            recovery_detail: None,
            recovered_at: None,
            receipts_reconciled: None,
            native_refusal: None,
            unobservable_logged: None,
            native_recovery: None,
            schedule_template: message.schedule_template.clone(),
            retired_at: None,
            consumed_at: None,
            extra_fields: Map::new(),
        }
    }
}

const CANDIDATE_SNAPSHOT_FIELDS: &[&str] = &[
    "message_id",
    "created_at",
    "press_enter",
    "status",
    "kind",
    "scheduled",
    "source",
    "notify_for",
    "text",
    "label",
    "generation",
    "eligible",
    "superseded_by",
    "transcript_texts",
    "typed_at",
    "native_binding",
    "recovery_outcome",
    "recovery_detail",
    "recovered_at",
    "receipts_reconciled",
    "native_refusal",
    "unobservable_logged",
    "native_recovery",
    "schedule_template",
    "retired_at",
    "consumed_at",
];

pub(crate) fn candidate_snapshot(candidate: &StuckCandidate) -> Value {
    let native_binding = candidate
        .native_binding
        .as_ref()
        .map(|(pid, start, session)| json!([pid, start, session]));
    let known = json!({
        "message_id": candidate.message_id,
        "created_at": candidate.created_at,
        "press_enter": candidate.press_enter,
        "status": candidate.status,
        "kind": candidate.kind,
        "scheduled": candidate.scheduled,
        "source": candidate.source,
        "notify_for": candidate.notify_for,
        "text": candidate.text,
        "label": candidate.label,
        "generation": candidate.generation,
        "eligible": candidate.eligible,
        "superseded_by": candidate.superseded_by,
        "transcript_texts": candidate.transcript_texts,
        "typed_at": candidate.typed_at,
        "native_binding": native_binding,
        "recovery_outcome": candidate.recovery_outcome,
        "recovery_detail": candidate.recovery_detail,
        "recovered_at": candidate.recovered_at,
        "receipts_reconciled": candidate.receipts_reconciled,
        "native_refusal": candidate.native_refusal,
        "unobservable_logged": candidate.unobservable_logged,
        "native_recovery": candidate.native_recovery,
        "schedule_template": candidate.schedule_template,
        "retired_at": candidate.retired_at,
        "consumed_at": candidate.consumed_at,
    });
    let mut snapshot = candidate.extra_fields.clone();
    if let Some(fields) = known.as_object() {
        snapshot.extend(fields.clone());
    }
    Value::Object(snapshot)
}

pub(crate) fn candidate_from_snapshot(value: &Value) -> Option<StuckCandidate> {
    let row = value.as_object()?;
    let string = |key: &str| row.get(key).and_then(Value::as_str).map(str::to_owned);
    let native_binding = row
        .get("native_binding")
        .and_then(Value::as_array)
        .and_then(|parts| {
            Some((
                parts.first()?.as_i64()?,
                parts.get(1)?.as_str()?.to_owned(),
                parts.get(2)?.as_str()?.to_owned(),
            ))
        });
    let extra_fields = row
        .iter()
        .filter(|(key, _)| !CANDIDATE_SNAPSHOT_FIELDS.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    Some(StuckCandidate {
        message_id: string("message_id")?,
        created_at: row.get("created_at")?.as_f64()?,
        press_enter: row
            .get("press_enter")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        status: string("status").unwrap_or_else(|| "unknown".into()),
        kind: string("kind").unwrap_or_else(|| "text".into()),
        scheduled: row
            .get("scheduled")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        source: string("source").unwrap_or_default(),
        notify_for: row
            .get("notify_for")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        text: string("text").unwrap_or_default(),
        label: string("label").unwrap_or_default(),
        generation: string("generation"),
        eligible: row
            .get("eligible")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        superseded_by: string("superseded_by").unwrap_or_default(),
        transcript_texts: row
            .get("transcript_texts")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
        typed_at: row.get("typed_at").and_then(Value::as_f64),
        native_binding,
        recovery_outcome: string("recovery_outcome"),
        recovery_detail: string("recovery_detail"),
        recovered_at: row.get("recovered_at").and_then(Value::as_f64),
        receipts_reconciled: row.get("receipts_reconciled").and_then(Value::as_bool),
        native_refusal: string("native_refusal"),
        unobservable_logged: row.get("unobservable_logged").and_then(Value::as_bool),
        native_recovery: row.get("native_recovery").and_then(Value::as_bool),
        schedule_template: string("schedule_template"),
        retired_at: row.get("retired_at").and_then(Value::as_f64),
        consumed_at: row.get("consumed_at").and_then(Value::as_f64),
        extra_fields,
    })
}

pub(crate) fn message_snapshot(message: &Message) -> Value {
    json!({
        "id": message.id,
        "pane_id": message.pane_id,
        "target_label": message.target_label,
        "text": message.text,
        "source": message.source,
        "status": message.status,
        "hold_while_typing": message.hold_while_typing,
        "kind": message.kind,
        "press_enter": message.press_enter,
        "scheduled": message.scheduled,
        "schedule_template": message.schedule_template,
        "attempt_generation": message.attempt_generation,
        "is_notice": message.is_notice,
        "notify_for": message.notify_for,
        "detail": message.detail,
        "holds": message.holds,
        "next_attempt_at": message.next_attempt_at,
        "hold_signature": message.hold_signature.as_ref().map(|text| {
            json!([message.hold_signature_kind.as_deref().unwrap_or("box"), text])
        }),
        "created_at": message.created_at,
        "finished_at": message.finished_at,
        "typed_at": message.typed_at,
    })
}

pub(crate) fn message_from_snapshot(value: &Value) -> Option<Message> {
    let row = value.as_object()?;
    let field = |key: &str| row.get(key);
    let string = |key: &str| field(key).and_then(Value::as_str).map(str::to_owned);
    let hold_signature = || match field("hold_signature")? {
        Value::Array(parts) if parts.len() == 2 => {
            Some((parts[0].as_str()?.to_owned(), parts[1].as_str()?.to_owned()))
        }
        // Older Rust snapshots stored only the box text. Keep that text comparable as a box
        // signature so upgrading does not discard an existing held-message backoff.
        Value::String(text) => Some(("box".to_owned(), text.clone())),
        _ => None,
    };
    let id = string("id")?;
    let pane_id = string("pane_id")?;
    let target_label = string("target_label").unwrap_or_else(|| pane_id.clone());
    let text = string("text").unwrap_or_default();
    let source = string("source").unwrap_or_default();
    let mut message = Message::new(&id, &pane_id, &target_label, &text, &source);
    message.status = "queued".into();
    message.hold_while_typing = field("hold_while_typing")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    message.kind = string("kind").unwrap_or_else(|| "text".into());
    message.press_enter = field("press_enter")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    message.scheduled = field("scheduled").and_then(Value::as_bool).unwrap_or(false);
    message.schedule_template = string("schedule_template");
    message.attempt_generation = string("attempt_generation");
    message.is_notice = field("is_notice").and_then(Value::as_bool).unwrap_or(false);
    message.notify_for = field("notify_for")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    message.detail = string("detail");
    message.holds = field("holds").and_then(Value::as_i64).unwrap_or(0);
    message.next_attempt_at = field("next_attempt_at").and_then(Value::as_f64);
    if let Some((kind, text)) = hold_signature() {
        message.hold_signature_kind = Some(kind);
        message.hold_signature = Some(text);
    }
    message.created_at = field("created_at")
        .and_then(Value::as_f64)
        .unwrap_or(message.created_at);
    message.finished_at = field("finished_at").and_then(Value::as_f64);
    message.typed_at = field("typed_at").and_then(Value::as_f64);
    Some(message)
}

#[allow(dead_code)] // Called when the later stuck-recovery leaf wires this state.
fn candidate_is_newer(left: &StuckCandidate, right: &StuckCandidate) -> bool {
    match left.created_at.total_cmp(&right.created_at) {
        std::cmp::Ordering::Greater => return true,
        std::cmp::Ordering::Less => return false,
        std::cmp::Ordering::Equal => {}
    }
    let left_digits: String = left
        .message_id
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    let right_digits: String = right
        .message_id
        .chars()
        .filter(char::is_ascii_digit)
        .collect();
    let left_digits = left_digits.trim_start_matches('0');
    let right_digits = right_digits.trim_start_matches('0');
    let id_order = match (left_digits.is_empty(), right_digits.is_empty()) {
        (true, false) => std::cmp::Ordering::Less,
        (false, true) => std::cmp::Ordering::Greater,
        _ => left_digits
            .len()
            .cmp(&right_digits.len())
            .then_with(|| left_digits.cmp(right_digits)),
    };
    id_order.then_with(|| left.message_id.cmp(&right.message_id)) == std::cmp::Ordering::Greater
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

pub(crate) fn outbox_lock(
    outbox: &Mutex<BTreeMap<String, Vec<Message>>>,
) -> MutexGuard<'_, BTreeMap<String, Vec<Message>>> {
    outbox
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Flock {
    /// Deliver one queued message, persisting every hold and decided transition.
    /// The outbox lock protects only state selection and mutation; terminal I/O always runs
    /// after it has been released.
    #[allow(dead_code)] // The dependent outbox worker/runtime wires this bounded delivery edge.
    pub(crate) fn deliver_message(
        &self,
        message_id: &str,
        now: f64,
        env: &mut dyn SendPreparationEnv,
        permission: impl FnMut() -> Option<String>,
    ) -> DeliveryOutcome {
        let mut message = {
            let mut outbox = outbox_lock(&self.outbox);
            let Some(message) = outbox
                .values_mut()
                .flat_map(|queue| queue.iter_mut())
                .find(|message| message.id == message_id)
            else {
                return if self
                    .decided
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .by_id
                    .contains_key(message_id)
                {
                    DeliveryOutcome::AlreadyDecided
                } else {
                    DeliveryOutcome::Missing
                };
            };
            if message.status == "cancelled" || message.status != "queued" {
                return DeliveryOutcome::AlreadyDecided;
            }
            message.clone()
        };

        if message.attempt_generation.is_none() {
            // Capture identity before any terminal action, without holding the queue mutex.
            message.attempt_generation = env.attempt_generation(&message.pane_id);
            let mut outbox = outbox_lock(&self.outbox);
            if let Some(stored) = outbox
                .get_mut(&message.pane_id)
                .and_then(|queue| queue.iter_mut().find(|row| row.id == message_id))
            {
                if stored.attempt_generation.is_none() {
                    stored.attempt_generation = message.attempt_generation.clone();
                }
                message.attempt_generation = stored.attempt_generation.clone();
            }
        }

        let pane_id = message.pane_id.clone();
        let options = SendGuardOptions {
            hold_while_typing: message.hold_while_typing,
            allow_modal: message.kind == "keys",
            preserve_draft: false,
        };
        let (sent, ok, detail) =
            send_guarded(self, env, &pane_id, options, permission, |env, guard| {
                if message.kind == "keys" {
                    let keys: Vec<String> =
                        message.text.split_whitespace().map(str::to_owned).collect();
                    Ok(press_keys(env, &pane_id, &keys, guard))
                } else {
                    Ok(submit_text_with_options(
                        self,
                        env,
                        &pane_id,
                        &message.text,
                        SubmitOptions {
                            press_enter: message.press_enter,
                            check_box: false,
                            attempt_generation: AttemptGeneration::Captured(
                                message.attempt_generation.as_deref(),
                            ),
                        },
                        Some(guard),
                    ))
                }
            });

        if !sent {
            let terminal;
            let attempts;
            let hold_signature = hold_signature(env, &pane_id);
            {
                let mut outbox = outbox_lock(&self.outbox);
                let Some(queue) = outbox.get_mut(&pane_id) else {
                    return DeliveryOutcome::Missing;
                };
                let Some(stored) = queue.iter_mut().find(|row| row.id == message_id) else {
                    return DeliveryOutcome::Missing;
                };
                stored.holds = stored.holds.saturating_add(1);
                attempts = stored.holds.min(HOLD_ATTEMPT_LIMIT);
                terminal = stored.holds >= HOLD_ATTEMPT_LIMIT;
                if terminal {
                    stored.status = "undeliverable".into();
                    stored.detail = Some(format!(
                        "undeliverable after {attempts} refused attempts (last reason: {}); not retried again",
                        if detail.is_empty() { "unknown" } else { &detail }
                    ));
                    stored.finished_at = Some(clock::now_s());
                } else {
                    let delay = hold_backoff_s(attempts);
                    stored.next_attempt_at = Some(now + delay);
                    stored.hold_signature_kind = Some(hold_signature.0);
                    stored.hold_signature = Some(hold_signature.1);
                    stored.detail = Some(if detail.is_empty() { "held" } else { &detail }.into());
                }
                message = stored.clone();
                let empty = if terminal {
                    queue.retain(|row| row.id != message_id);
                    queue.is_empty()
                } else {
                    false
                };
                if empty {
                    outbox.remove(&pane_id);
                }
            }
            if let Some(metrics) = self.stores().metrics {
                let reason = (!detail.is_empty()).then_some(detail.clone());
                metrics.record(&crate::metrics::Event {
                    at: 0.0,
                    kind: "message.held".into(),
                    pane_id: Some(message.pane_id.clone()),
                    mailbox: Some(message.target_label.clone()),
                    sender: None,
                    source: Some(message.source.clone()),
                    outcome: Some("held".into()),
                    reason,
                    duration_ms: None,
                    attempts: Some(attempts),
                    bytes: Some(message.text.len() as i64),
                });
            }
            if terminal {
                message.detail = Some(format!(
                    "undeliverable after {attempts} refused attempts (last reason: {}); not retried again",
                    if detail.is_empty() { "unknown" } else { &detail }
                ));
                if let Some(metrics) = self.stores().metrics {
                    metrics.record(&crate::metrics::Event {
                        at: 0.0,
                        kind: "message.undeliverable".into(),
                        pane_id: Some(message.pane_id.clone()),
                        mailbox: Some(message.target_label.clone()),
                        sender: None,
                        source: Some(message.source.clone()),
                        outcome: Some("undeliverable".into()),
                        reason: Some(detail.clone()),
                        duration_ms: None,
                        attempts: Some(attempts),
                        bytes: Some(message.text.len() as i64),
                    });
                }
                self.remember(&message);
                let _ = self.save();
                self.record_delivery_outcome(
                    &message,
                    false,
                    message.detail.as_deref(),
                    "undeliverable",
                    None,
                );
                DeliveryOutcome::Undeliverable
            } else {
                let _ = self.save();
                DeliveryOutcome::Held
            }
        } else {
            message.status = if ok.unwrap_or(false) {
                "sent"
            } else {
                "failed"
            }
            .into();
            message.detail = Some(detail.clone());
            message.finished_at = Some(clock::now_s());
            self.set_last_message(if ok.unwrap_or(false) {
                format!("Sent to {}", message.target_label)
            } else {
                format!("Send failed: {detail}")
            });
            {
                let mut outbox = outbox_lock(&self.outbox);
                let Some(queue) = outbox.get_mut(&pane_id) else {
                    return DeliveryOutcome::Missing;
                };
                queue.retain(|row| row.id != message_id);
                if queue.is_empty() {
                    outbox.remove(&pane_id);
                }
            }
            self.remember(&message);
            let _ = self.save();
            let succeeded = ok.unwrap_or(false);
            self.record_delivery_outcome(
                &message,
                succeeded,
                Some(&detail),
                if succeeded { "ok" } else { "failed" },
                Some("message.sent"),
            );
            if succeeded {
                DeliveryOutcome::Sent
            } else {
                DeliveryOutcome::Failed
            }
        }
    }

    /// `_record_delivery_outcome`: record terminal metrics/receipts once and always release
    /// the message waiter. Store failures are best-effort bookkeeping, never worker failures.
    #[allow(dead_code)] // Called by a dependent T33 outbox integration slice.
    pub(crate) fn record_delivery_outcome(
        &self,
        message: &Message,
        ok: bool,
        detail: Option<&str>,
        outcome: &str,
        metric: Option<&str>,
    ) {
        if !message.done.begin() {
            return;
        }

        let stores = self.stores();
        if let (Some(kind), Some(metrics)) = (metric, stores.metrics.as_ref()) {
            let finished_at = message
                .finished_at
                .filter(|finished_at| *finished_at != 0.0)
                .unwrap_or_else(clock::now_s);
            let duration_ms = ((finished_at - message.created_at) * 1000.0) as i64;
            metrics.record(&crate::metrics::Event {
                at: 0.0,
                kind: if ok {
                    kind.to_string()
                } else {
                    "message.failed".to_string()
                },
                pane_id: Some(message.pane_id.clone()),
                mailbox: Some(message.target_label.clone()),
                sender: None,
                source: Some(message.source.clone()),
                outcome: Some(outcome.to_string()),
                reason: if ok {
                    None
                } else {
                    Some(
                        detail
                            .filter(|detail| !detail.is_empty())
                            .unwrap_or("unconfirmed")
                            .to_string(),
                    )
                },
                duration_ms: Some(duration_ms),
                attempts: (message.holds != 0).then_some(message.holds),
                bytes: Some(message.text.len() as i64),
            });
        }

        if let Some(inbox) = stores.inbox.as_ref() {
            for inbox_id in &message.notify_for {
                let _ = inbox.record_notify(
                    inbox_id,
                    if ok { "sent" } else { "failed" },
                    detail,
                    None,
                    None,
                );
            }
        }

        message.done.settle();
    }

    /// `_remember`: retain the decided message, its id-reuse-safe tombstone, and stuck intent.
    /// Snapshot persistence belongs to the caller after this in-memory transition.
    #[allow(dead_code)] // Called by dependent T33 delivery and restore integration slices.
    pub(crate) fn remember(&self, message: &Message) {
        self.note_stuck_candidate(message);

        let mut history = self
            .message_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        history.push_back(message.clone());
        while history.len() > MESSAGE_HISTORY {
            history.pop_front();
        }
        drop(history);

        let mut decided = self
            .decided
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let status = if message.status.is_empty() {
            "unknown"
        } else {
            message.status.as_str()
        };
        let is_new = !decided.by_id.contains_key(&message.id);
        decided.by_id.insert(
            message.id.clone(),
            DecidedTombstone {
                status: status.to_string(),
                created_at: Some(message.created_at),
            },
        );
        if is_new {
            decided.order.push_back(message.id.clone());
        }
        while decided.order.len() > DECIDED_KEPT {
            if let Some(oldest) = decided.order.pop_front() {
                decided.by_id.remove(&oldest);
            }
        }
    }

    /// Keep the pane's latest delivery intent for the stuck observer.
    ///
    /// Completion order is not intent order: a late outcome for an older message cannot
    /// replace a newer send, cancellation, or deliberate no-enter intent. Ineligible
    /// intents are retained as tombstones so that they still supersede older candidates.
    /// This only mutates in-memory state; snapshot persistence has a separate owner.
    #[allow(dead_code)] // Runtime consumers are assigned to dependent T33 integration slices.
    pub(crate) fn note_stuck_candidate(&self, message: &Message) {
        if message.pane_id.is_empty() {
            return;
        }
        let mut candidate = StuckCandidate::from_message(message);
        let mut candidates = self
            .stuck_candidates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(existing) = candidates.get(&message.pane_id) else {
            candidates.insert(message.pane_id.clone(), candidate);
            return;
        };
        if existing.message_id == candidate.message_id
            && existing.created_at.to_bits() == candidate.created_at.to_bits()
        {
            candidate.typed_at = message.typed_at.or(existing.typed_at);
            candidate.native_binding = existing.native_binding.clone();
            candidate.recovery_outcome = existing.recovery_outcome.clone();
            candidate.recovery_detail = existing.recovery_detail.clone();
            candidate.recovered_at = existing.recovered_at;
            candidate.receipts_reconciled = existing.receipts_reconciled;
            candidate.native_refusal = existing.native_refusal.clone();
            candidate.unobservable_logged = existing.unobservable_logged;
            candidate.native_recovery = existing.native_recovery;
            candidate.schedule_template = message
                .schedule_template
                .clone()
                .or_else(|| existing.schedule_template.clone());
            candidate.retired_at = existing.retired_at;
            candidate.consumed_at = existing.consumed_at;
            candidate.extra_fields = existing.extra_fields.clone();
            if candidate.recovery_outcome.as_deref() == Some("consumed") {
                candidate.status = "consumed".into();
                candidate.eligible = false;
            }
        }
        if candidate_is_newer(existing, &candidate) {
            return;
        }
        if existing
            .generation
            .as_ref()
            .is_some_and(|generation| Some(generation) == candidate.generation.as_ref())
        {
            let mut transcript = existing.transcript_texts.clone();
            let same_message = existing.message_id == candidate.message_id
                && existing.created_at.to_bits() == candidate.created_at.to_bits();
            if !same_message && existing.status == "sent" && existing.press_enter {
                transcript.push(existing.text.clone());
            }
            transcript.retain(|text| !text.is_empty());
            let mut unique = Vec::with_capacity(REMEMBERED_SENDS);
            for text in transcript {
                if !unique.contains(&text) {
                    unique.push(text);
                }
            }
            candidate.transcript_texts = unique
                .into_iter()
                .rev()
                .take(REMEMBERED_SENDS)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
        }
        candidates.insert(message.pane_id.clone(), candidate);
    }

    /// Apply an observer result only to the candidate identity that produced it.
    /// Recovery decisions are persisted before any inbox receipt can be published.
    #[allow(dead_code)] // The concrete stuck-observer caller is a separate runtime slice.
    pub(crate) fn reconcile_stuck(
        &self,
        pane_id: &str,
        message_id: &str,
        outcome: &str,
        detail: &str,
        created_at: Option<f64>,
    ) {
        let Some(created_at) = created_at else {
            tracing::warn!(
                pane_id,
                message_id,
                "skipping stuck reconciliation without created_at"
            );
            return;
        };
        let entry = {
            let mut candidates = self
                .stuck_candidates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(candidate) = candidates.get_mut(pane_id) else {
                return;
            };
            if candidate.message_id != message_id || candidate.created_at != created_at {
                return;
            }
            candidate.recovery_outcome = Some(outcome.to_string());
            candidate.recovery_detail = Some(detail.to_string());
            candidate.recovered_at = Some(clock::now_s());
            match outcome {
                "consumed" => {
                    candidate.eligible = false;
                    candidate.status = "consumed".to_string();
                }
                "exhausted" => candidate.eligible = false,
                _ => {}
            }
            candidate.clone()
        };

        if let Err(error) = self.save() {
            tracing::warn!(pane_id, message_id, error = %error, "failed to save stuck recovery outcome");
            return;
        }
        if outcome == "consumed" {
            self.publish_stuck_receipt_after_save(pane_id, &entry);
        }
    }

    /// Repair receipts for a previously persisted consumed outcome, including after restart.
    /// The pre-write save is intentional: callers cannot publish an in-memory-only decision.
    #[allow(dead_code)] // The restart repair caller is a separate runtime slice.
    pub(crate) fn publish_stuck_receipt(&self, pane_id: &str, entry: &StuckCandidate) {
        if !self.stuck_candidate_is_consumed(pane_id, entry) {
            return;
        }
        if let Err(error) = self.save() {
            tracing::warn!(pane_id, message_id = %entry.message_id, error = %error, "failed to save consumed recovery before receipt repair");
            return;
        }
        self.publish_stuck_receipt_after_save(pane_id, entry);
    }

    fn publish_stuck_receipt_after_save(&self, pane_id: &str, entry: &StuckCandidate) {
        if !self.stuck_candidate_is_consumed(pane_id, entry) {
            return;
        }
        let stores = self.stores();
        if !entry.notify_for.is_empty() && stores.inbox.is_none() {
            tracing::warn!(pane_id, message_id = %entry.message_id, "inbox store unavailable; recovered receipts remain pending");
            return;
        }
        let Some(detail) = entry.recovery_detail.as_deref() else {
            tracing::warn!(pane_id, message_id = %entry.message_id, "recovery detail missing; recovered receipt remains pending");
            return;
        };
        let reason = format!("recovered: {detail}");
        if let Some(inbox) = stores.inbox.as_ref() {
            for inbox_id in &entry.notify_for {
                if let Err(error) = inbox.record_notify(inbox_id, "sent", Some(&reason), None, None)
                {
                    tracing::warn!(pane_id, message_id = %entry.message_id, inbox_id, error = %error, "recovered prompt receipt could not be reconciled");
                    return;
                }
            }
        }

        let previous = {
            let mut candidates = self
                .stuck_candidates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(live) = candidates.get_mut(pane_id) else {
                return;
            };
            if live.message_id != entry.message_id
                || live.created_at != entry.created_at
                || live.recovery_outcome.as_deref() != Some("consumed")
            {
                return;
            }
            let previous = live.receipts_reconciled;
            live.receipts_reconciled = Some(true);
            previous
        };
        if let Err(error) = self.save() {
            let mut candidates = self
                .stuck_candidates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(live) = candidates.get_mut(pane_id) {
                if live.message_id == entry.message_id && live.created_at == entry.created_at {
                    live.receipts_reconciled = previous;
                }
            }
            tracing::warn!(pane_id, message_id = %entry.message_id, error = %error, "failed to save recovered receipt reconciliation marker");
        }
    }

    fn stuck_candidate_is_consumed(&self, pane_id: &str, entry: &StuckCandidate) -> bool {
        self.stuck_candidates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(pane_id)
            .is_some_and(|live| {
                live.message_id == entry.message_id
                    && live.created_at == entry.created_at
                    && live.recovery_outcome.as_deref() == Some("consumed")
            })
    }

    /// Bind native identity only to the exact pre-send candidate and its captured process
    /// generation. This prevents a resolver result for a replacement pane from being adopted.
    pub(crate) fn bind_native_candidate(
        &self,
        pane_id: &str,
        message_id: &str,
        created_at: f64,
        generation: Option<&str>,
        binding: &crate::recovery_native::PromptSessionBinding,
    ) -> bool {
        let expected_generation = format!("pid:{}:start:{}", binding.pid, binding.start);
        if generation != Some(expected_generation.as_str()) {
            return false;
        }
        let mut candidates = self
            .stuck_candidates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(candidate) = candidates.get_mut(pane_id) else {
            return false;
        };
        if candidate.message_id != message_id
            || candidate.created_at.to_bits() != created_at.to_bits()
            || candidate.generation.as_deref() != Some(expected_generation.as_str())
        {
            return false;
        }
        candidate.native_binding = Some((
            binding.pid,
            binding.start.clone(),
            binding.session_id.clone(),
        ));
        true
    }

    /// Persist typed-attempt authority before the caller sends the first Enter. `pane` is the
    /// already-resolved pane record from the caller's terminal adapter; native lookup failure
    /// leaves the strict generation-bound candidate intact and does not block the save.
    #[allow(dead_code)] // The concrete submit-text caller is a separate runtime slice.
    pub(crate) fn persist_typed_attempt(
        &self,
        message: &mut Message,
        pane_id: &str,
        text: &str,
        press_enter: bool,
        pane: &Value,
    ) -> Result<bool, DunkError> {
        if message.pane_id != pane_id || message.text != text {
            return Ok(true);
        }
        let previous_status = message.status.clone();
        message.press_enter = press_enter;
        message.typed_at = Some(clock::now_s());
        message.status = "typed".into();
        let result = (|| {
            self.note_stuck_candidate(message);
            if let Ok(binding) = crate::recovery_native::resolve_prompt_session(pane) {
                self.bind_native_candidate(
                    pane_id,
                    &message.id,
                    message.created_at,
                    message.attempt_generation.as_deref(),
                    &binding,
                );
            }
            self.save()?;
            Ok(true)
        })();
        message.status = previous_status;
        result
    }

    /// Reconstruct the last candidate only while the pane still has the generation recorded
    /// before that attempt. This never adopts the current process as a substitute identity.
    #[allow(dead_code)] // The stuck-observer caller is a separate runtime slice.
    pub(crate) fn stuck_candidate_message(
        &self,
        pane_id: &str,
        env: &dyn PaneInfoEnv,
    ) -> Option<Message> {
        let candidate = lock_unpoison(&self.stuck_candidates)
            .get(pane_id)
            .cloned()?;
        let generation = candidate.generation.clone()?;
        if !candidate.eligible
            || generation.is_empty()
            || pane_generation(env, pane_id).as_deref() != Some(generation.as_str())
        {
            return None;
        }
        let mut message = Message::new(
            &candidate.message_id,
            pane_id,
            if candidate.label.is_empty() {
                pane_id
            } else {
                &candidate.label
            },
            &candidate.text,
            "stuck_candidate",
        );
        message.status = if candidate.status.is_empty() {
            "failed"
        } else {
            &candidate.status
        }
        .to_string();
        message.press_enter = candidate.press_enter;
        message.created_at = candidate.created_at;
        message.kind = candidate.kind;
        message.scheduled = candidate.scheduled;
        message.notify_for = candidate.notify_for;
        message.attempt_generation = Some(generation);
        Some(message)
    }

    /// Find the exact queued or bound message represented by the complete composer text.
    /// Queue head order is deliberately irrelevant: another draft may be ahead of this send.
    #[allow(dead_code)] // The stuck-observer caller is a separate runtime slice.
    pub(crate) fn stranded_message(
        &self,
        pane_id: &str,
        content: &str,
        env: &dyn PaneInfoEnv,
    ) -> Option<Message> {
        if content.is_empty() {
            return None;
        }
        let want = crate::stuck::collapse(content);
        if want.is_empty() {
            return None;
        }
        if let Some(candidate) = self.stuck_candidate_message(pane_id, env) {
            if crate::stuck::collapse(&candidate.text) == want {
                return Some(candidate);
            }
        }
        let rows = outbox_lock(&self.outbox)
            .get(pane_id)
            .into_iter()
            .flatten()
            .filter(|message| message.status == "queued")
            .cloned()
            .collect::<Vec<_>>();
        if let Some(message) = rows.iter().rev().find(|message| {
            !message.text.is_empty() && crate::stuck::collapse(&message.text) == want
        }) {
            return Some(message.clone());
        }
        let prefix = want.chars().take(40).collect::<String>();
        rows.iter()
            .rev()
            .find(|message| {
                !message.text.is_empty()
                    && crate::stuck::collapse(&message.text).starts_with(&prefix)
            })
            .cloned()
    }

    /// Remember text this flock typed into a pane, moving duplicate text to newest.
    ///
    /// A later observer uses this bounded history to distinguish a stranded send from a
    /// human draft. The state remains in memory, as in Python's `sent_text_by_pane`.
    #[allow(dead_code)] // The guarded send caller is assigned to a dependent T33 leaf.
    pub(crate) fn remember_typed(&self, pane_id: &str, text: &str) {
        let mut history = self
            .sent_text_by_pane
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let recent = history.entry(pane_id.to_string()).or_default();
        if let Some(index) = recent.iter().position(|previous| previous == text) {
            recent.remove(index);
        }
        recent.push(text.to_string());
        if recent.len() > REMEMBERED_SENDS {
            let excess = recent.len() - REMEMBERED_SENDS;
            recent.drain(..excess);
        }
    }

    /// Whether a composer contains text owned by this flock, from a notice, recent typed
    /// history, or the pane's queued outbox messages.
    ///
    /// Candidate state is cloned under its own short lock and all comparisons happen after
    /// releasing the locks. This helper performs no pane reads and holds no lock across one.
    #[allow(dead_code)] // The send observer caller is assigned to a dependent T33 leaf.
    pub(crate) fn looks_like_ours(&self, pane_id: &str, shown: &str) -> bool {
        if !shown.is_empty() && collapse(shown).starts_with(NOTICE_PREFIX.trim()) {
            return true;
        }
        if shown.is_empty() {
            return false;
        }

        let candidates = self.known_send_texts(pane_id);

        let shown = collapse(shown);
        for candidate in &candidates {
            let full = collapse(candidate);
            if full.is_empty() {
                continue;
            }
            let consumption_prefix: String = full.chars().take(CONSUMPTION_PREFIX_CHARS).collect();
            if full.starts_with(&shown) || shown.starts_with(&consumption_prefix) {
                return true;
            }
        }
        candidates
            .iter()
            .any(|candidate| is_our_send(shown.as_str(), Some(candidate), true))
    }

    /// Clone the exact ownership evidence used by composer preflight. Queued outbox text is
    /// eligible only while its message is still queued.
    fn known_send_texts(&self, pane_id: &str) -> Vec<String> {
        let mut candidates = self
            .sent_text_by_pane
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(pane_id)
            .cloned()
            .unwrap_or_default();
        {
            let outbox = outbox_lock(&self.outbox);
            candidates.extend(
                outbox
                    .get(pane_id)
                    .into_iter()
                    .flatten()
                    .filter(|message| message.status == "queued" && !message.text.is_empty())
                    .map(|message| message.text.clone()),
            );
        }
        candidates
    }

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
