//! `flock/dunks.rs` (T32, slice 1): the `Dunker` data model and the pure helpers around it.
//! Ports `dunk_core.Dunker` (fields, `__init__` defaults, `target_label`, `set_target`,
//! `to_dict`, `from_dict`), `format_interval`, `parse_interval`, `expand_template`.
//!
//! NOT in this slice (tracked on t800): the `Flock` dunk methods (add/update/remove/start/
//! stop/toggle/stop_all/fire/list/get), `_save`/`_load` of `dunks.json`, the per-dunk tokio
//! timer, idle gating and the skip/hold backpressure. Those need the outbox and stuck state
//! that have no Rust owner yet.

use std::sync::OnceLock;

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::clock;
use crate::error::DunkError;

/// `DEFAULT_TEXT`.
pub const DEFAULT_TEXT: &str = "continue";
/// `DEFAULT_INTERVAL_MINUTES`.
pub const DEFAULT_INTERVAL_MINUTES: f64 = 10.0;

/// `Dunker.FIELDS`, in the Python's order: this is the persisted key order of `dunks.json`.
pub const FIELDS: [&str; 28] = [
    "id",
    "name",
    "target_pane_id",
    "target_agent",
    "target_tab",
    "target_workspace",
    "target_cwd",
    "interval_minutes",
    "text",
    "running",
    "only_when",
    "max_sends",
    "send_count",
    "created_at",
    "started_at",
    "last_sent_at",
    "next_send_at",
    "last_error",
    "skip_if_unconsumed",
    "skip_count",
    "total_skips",
    "last_skipped_at",
    "awaiting_consumption",
    "last_sent_text",
    "unknown_skips",
    "hold_while_typing",
    "hold_count",
    "last_held_at",
];

/// `TEMPLATE_RE`.
fn template_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\{(id|name|count|target|interval|time|date)\}").expect("static regex")
    })
}

/// Python's `format(x, "g")`: six significant digits, no trailing zeros. As in C, the
/// exponent that picks fixed or scientific is the one AFTER rounding to six digits
/// (999999.5 -> `1e+06`), and scientific is used below 1e-4 (`5e-05`).
fn fmt_g(value: f64) -> String {
    if value == 0.0 {
        return "0".into();
    }
    let sci = format!("{value:.5e}");
    let (mantissa, exponent) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exponent.parse().unwrap_or(0);
    let trim = |text: &str| -> String {
        if text.contains('.') {
            text.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            text.to_string()
        }
    };
    if !(-4..6).contains(&exp) {
        return format!(
            "{}e{}{:02}",
            trim(mantissa),
            if exp < 0 { '-' } else { '+' },
            exp.abs()
        );
    }
    let decimals = (5 - exp).max(0) as usize;
    trim(&format!("{value:.decimals$}"))
}

/// `format_interval`: `10`, `1.5`, or `30s` under a minute; round-trips `parse_interval`.
pub fn format_interval(minutes: f64) -> String {
    if 0.0 < minutes && minutes < 1.0 {
        return format!("{}s", fmt_g(minutes * 60.0));
    }
    if minutes == minutes.trunc() {
        return format!("{}", minutes as i64);
    }
    fmt_g(minutes)
}

/// `parse_interval` for a string (the numeric and bool arms of the Python are the caller's:
/// a number is already minutes, see `parse_interval_number`).
pub fn parse_interval(value: &str) -> Result<f64, DunkError> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"^([0-9]*\.?[0-9]+)\s*(s|sec|secs|m|min|mins|h|hr|hrs|)$")
            .expect("static regex")
    });
    let text = value.trim().to_lowercase();
    let bad = || {
        DunkError::Refused(format!(
            "bad interval {} (try 10, 90s, 1.5h)",
            crate::context::store::py_repr(value)
        ))
    };
    let caps = re.captures(&text).ok_or_else(bad)?;
    let number: f64 = caps[1].parse().map_err(|_| bad())?;
    let unit = &caps[2];
    let minutes = if unit.starts_with('s') {
        number / 60.0
    } else if unit.starts_with('h') {
        number * 60.0
    } else {
        number
    };
    parse_interval_number(minutes)
}

/// The numeric arm of `parse_interval`: positive minutes.
pub fn parse_interval_number(minutes: f64) -> Result<f64, DunkError> {
    if minutes > 0.0 {
        Ok(minutes)
    } else {
        Err(DunkError::Refused("interval must be positive".into()))
    }
}

/// One dunk. Plain data plus a little presentation logic (`dunk_core.Dunker`).
#[derive(Debug, Clone, PartialEq)]
pub struct Dunker {
    pub id: String,
    pub name: String,
    pub target_pane_id: Option<String>,
    pub target_agent: Option<String>,
    pub target_tab: Option<String>,
    pub target_workspace: Option<String>,
    pub target_cwd: Option<String>,
    pub interval_minutes: f64,
    pub text: String,
    pub running: bool,
    pub only_when: Option<String>,
    pub max_sends: Option<i64>,
    pub send_count: i64,
    pub created_at: f64,
    pub started_at: Option<f64>,
    pub last_sent_at: Option<f64>,
    pub next_send_at: Option<f64>,
    pub last_error: Option<String>,
    pub skip_if_unconsumed: bool,
    /// The CURRENT run's skips; resets when the dunk sends.
    pub skip_count: i64,
    /// Lifetime skips: history, never the thing a threshold fires on.
    pub total_skips: i64,
    pub last_skipped_at: Option<f64>,
    pub awaiting_consumption: bool,
    pub last_sent_text: Option<String>,
    /// Consecutive inconclusive box reads (capped by `MAX_UNKNOWN_SKIPS`).
    pub unknown_skips: i64,
    /// Never type over a human (defaults on for old state files).
    pub hold_while_typing: bool,
    pub hold_count: i64,
    pub last_held_at: Option<f64>,
    /// Volatile, not persisted.
    pub status: String,
}

fn opt_str(data: &Map<String, Value>, key: &str) -> Option<String> {
    match data.get(key) {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Null) | None => None,
        Some(other) => Some(other.to_string()),
    }
}

fn opt_f64(data: &Map<String, Value>, key: &str) -> Option<f64> {
    data.get(key).and_then(Value::as_f64)
}

/// Python truthiness of a JSON value.
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(map)) => !map.is_empty(),
    }
}

/// `int(kwargs.get(key) or 0)`.
fn int_or_zero(data: &Map<String, Value>, key: &str) -> i64 {
    match data.get(key) {
        Some(Value::Number(n)) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(0),
        Some(Value::String(text)) => text.trim().parse().unwrap_or(0),
        Some(Value::Bool(true)) => 1,
        _ => 0,
    }
}

impl Dunker {
    /// `Dunker(id)` with every default.
    pub fn new(id: &str) -> Dunker {
        Dunker::with(id, &Map::new())
    }

    /// `Dunker.__init__(id, **kwargs)`: the Python's `or` defaults, key by key.
    pub fn with(id: &str, kwargs: &Map<String, Value>) -> Dunker {
        let interval = match kwargs.get("interval_minutes") {
            Some(v) if truthy(Some(v)) => v
                .as_f64()
                .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
                .unwrap_or(DEFAULT_INTERVAL_MINUTES),
            _ => DEFAULT_INTERVAL_MINUTES,
        };
        let skip_count = int_or_zero(kwargs, "skip_count");
        let total = int_or_zero(kwargs, "total_skips");
        Dunker {
            id: id.to_string(),
            name: opt_str(kwargs, "name").unwrap_or_default(),
            target_pane_id: opt_str(kwargs, "target_pane_id"),
            target_agent: opt_str(kwargs, "target_agent"),
            target_tab: opt_str(kwargs, "target_tab"),
            target_workspace: opt_str(kwargs, "target_workspace"),
            target_cwd: opt_str(kwargs, "target_cwd"),
            interval_minutes: interval,
            text: opt_str(kwargs, "text").unwrap_or_else(|| DEFAULT_TEXT.to_string()),
            running: truthy(kwargs.get("running")),
            only_when: opt_str(kwargs, "only_when").filter(|s| !s.is_empty()),
            max_sends: Some(int_or_zero(kwargs, "max_sends")).filter(|n| *n != 0),
            send_count: int_or_zero(kwargs, "send_count"),
            created_at: opt_f64(kwargs, "created_at")
                .filter(|f| *f != 0.0)
                .unwrap_or_else(clock::now_s),
            started_at: opt_f64(kwargs, "started_at"),
            last_sent_at: opt_f64(kwargs, "last_sent_at"),
            next_send_at: opt_f64(kwargs, "next_send_at"),
            last_error: opt_str(kwargs, "last_error"),
            skip_if_unconsumed: truthy(kwargs.get("skip_if_unconsumed")),
            skip_count,
            total_skips: if total != 0 { total } else { skip_count },
            last_skipped_at: opt_f64(kwargs, "last_skipped_at"),
            awaiting_consumption: truthy(kwargs.get("awaiting_consumption")),
            last_sent_text: opt_str(kwargs, "last_sent_text"),
            unknown_skips: int_or_zero(kwargs, "unknown_skips"),
            hold_while_typing: kwargs
                .get("hold_while_typing")
                .map_or(true, |v| truthy(Some(v))),
            hold_count: int_or_zero(kwargs, "hold_count"),
            last_held_at: opt_f64(kwargs, "last_held_at"),
            status: opt_str(kwargs, "status").unwrap_or_else(|| "Ready".to_string()),
        }
    }

    /// `target_label`: `(no target)`, the tab (or pane id), and `tab / agent` when known.
    pub fn target_label(&self) -> String {
        let Some(pane) = self.target_pane_id.as_deref().filter(|p| !p.is_empty()) else {
            return "(no target)".into();
        };
        let tab = self
            .target_tab
            .clone()
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| pane.to_string());
        match self.target_agent.as_deref().filter(|a| !a.is_empty()) {
            Some(agent) => format!("{tab} / {agent}"),
            None => tab,
        }
    }

    /// `set_target`: point at a pane row (`pane_id`, `agent`, `cwd`, `tab_label`/`tab_id`,
    /// `workspace_label`/`workspace_id`).
    pub fn set_target(&mut self, pane: &Map<String, Value>) {
        let pick = |a: &str, b: &str| {
            opt_str(pane, a)
                .filter(|s| !s.is_empty())
                .or_else(|| opt_str(pane, b))
        };
        self.target_pane_id = opt_str(pane, "pane_id");
        self.target_agent = opt_str(pane, "agent");
        self.target_cwd = opt_str(pane, "cwd");
        self.target_tab = pick("tab_label", "tab_id");
        self.target_workspace = pick("workspace_label", "workspace_id");
    }

    fn field(&self, name: &str) -> Value {
        let text = |v: &Option<String>| v.as_ref().map_or(Value::Null, |s| json!(s));
        let num = |v: &Option<f64>| v.map_or(Value::Null, |n| json!(n));
        match name {
            "id" => json!(self.id),
            "name" => json!(self.name),
            "target_pane_id" => text(&self.target_pane_id),
            "target_agent" => text(&self.target_agent),
            "target_tab" => text(&self.target_tab),
            "target_workspace" => text(&self.target_workspace),
            "target_cwd" => text(&self.target_cwd),
            "interval_minutes" => json!(self.interval_minutes),
            "text" => json!(self.text),
            "running" => json!(self.running),
            "only_when" => text(&self.only_when),
            "max_sends" => self.max_sends.map_or(Value::Null, |n| json!(n)),
            "send_count" => json!(self.send_count),
            "created_at" => json!(self.created_at),
            "started_at" => num(&self.started_at),
            "last_sent_at" => num(&self.last_sent_at),
            "next_send_at" => num(&self.next_send_at),
            "last_error" => text(&self.last_error),
            "skip_if_unconsumed" => json!(self.skip_if_unconsumed),
            "skip_count" => json!(self.skip_count),
            "total_skips" => json!(self.total_skips),
            "last_skipped_at" => num(&self.last_skipped_at),
            "awaiting_consumption" => json!(self.awaiting_consumption),
            "last_sent_text" => text(&self.last_sent_text),
            "unknown_skips" => json!(self.unknown_skips),
            "hold_while_typing" => json!(self.hold_while_typing),
            "hold_count" => json!(self.hold_count),
            "last_held_at" => num(&self.last_held_at),
            _ => Value::Null,
        }
    }

    /// The persisted pairs: exactly `FIELDS`, in the Python's order. `dunks.json` is written
    /// from this, because `serde_json::Map` sorts its keys (no `preserve_order` feature).
    pub fn persisted_pairs(&self) -> Vec<(&'static str, Value)> {
        FIELDS.iter().map(|f| (*f, self.field(f))).collect()
    }

    /// The persisted fields as a map (key order is NOT preserved; see `persisted_pairs`).
    pub fn persisted(&self) -> Map<String, Value> {
        self.persisted_pairs()
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect()
    }

    /// `to_dict`: the persisted fields plus `status`, `target_label`, `next_send_in_s`.
    pub fn to_dict(&self) -> Map<String, Value> {
        let mut data = self.persisted();
        data.insert("status".into(), json!(self.status));
        data.insert("target_label".into(), json!(self.target_label()));
        let in_s = match (self.running, self.next_send_at) {
            (true, Some(next)) if next != 0.0 => {
                json!(((next - clock::now_s()).round_ties_even() as i64).max(0))
            }
            _ => Value::Null,
        };
        data.insert("next_send_in_s".into(), in_s);
        data
    }

    /// `from_dict`: drops the three derived keys, then constructs.
    pub fn from_dict(data: &Map<String, Value>) -> Result<Dunker, DunkError> {
        let mut data = data.clone();
        let id = match data.remove("id") {
            Some(Value::String(id)) => id,
            _ => return Err(DunkError::Refused("a dunk needs an id".into())),
        };
        for key in ["status", "target_label", "next_send_in_s"] {
            data.remove(key);
        }
        Ok(Dunker::with(&id, &data))
    }
}

/// `expand_template`: the known `{placeholders}`; other braces are left alone.
pub fn expand_template(text: &str, dunker: &Dunker) -> String {
    let now = chrono::Local::now();
    template_re()
        .replace_all(text, |caps: &regex::Captures| match &caps[1] {
            "id" => dunker.id.clone(),
            "name" if dunker.name.is_empty() => dunker.id.clone(),
            "name" => dunker.name.clone(),
            "count" => (dunker.send_count + 1).to_string(),
            "target" => dunker.target_label(),
            "interval" => format_interval(dunker.interval_minutes),
            "time" => now.format("%H:%M:%S").to_string(),
            _ => now.format("%Y-%m-%d").to_string(),
        })
        .into_owned()
}

/// `_numeric_id`: the number in `d<n>`, else 0.
pub fn numeric_id(dunk_id: &str) -> i64 {
    dunk_id
        .strip_prefix('d')
        .filter(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
        .and_then(|rest| rest.parse().ok())
        .unwrap_or(0)
}

/// `_check_only_when`: `None`, `""`, `none`, `any`, `always` clear the gate; `idle` is the
/// only gate.
pub fn check_only_when(value: Option<&str>) -> Result<Option<String>, DunkError> {
    match value {
        None | Some("" | "none" | "None" | "any" | "always") => Ok(None),
        Some("idle") => Ok(Some("idle".into())),
        Some(_) => Err(DunkError::Refused(
            "only_when must be 'idle' or empty".into(),
        )),
    }
}

/// `_check_max_sends`: empty/zero is no cap; otherwise a positive integer.
pub fn check_max_sends(value: Option<&str>) -> Result<Option<i64>, DunkError> {
    let bad = || DunkError::Refused("max_sends must be a positive integer".into());
    match value.map(str::trim) {
        None | Some("" | "0") => Ok(None),
        Some(text) => match text.parse::<i64>() {
            Ok(n) if n > 0 => Ok(Some(n)),
            _ => Err(bad()),
        },
    }
}

/// `_check_bool`.
pub fn check_bool(value: &str, name: &str) -> Result<bool, DunkError> {
    match value.trim().to_lowercase().as_str() {
        "" | "0" | "false" | "no" | "n" | "off" => Ok(false),
        "1" | "true" | "yes" | "y" | "on" => Ok(true),
        _ => Err(DunkError::Refused(format!("{name} must be true or false"))),
    }
}

/// `Flock.add` arguments. `None` means "not given" (the Python default).
#[derive(Debug, Default, Clone)]
pub struct AddArgs {
    pub text: Option<String>,
    pub interval: Option<String>,
    pub name: Option<String>,
    pub start: bool,
    pub only_when: Option<String>,
    pub max_sends: Option<String>,
    pub skip_if_unconsumed: Option<String>,
    pub hold_while_typing: Option<String>,
}

/// `Flock.update` arguments. `only_when` and `max_sends` are tri-state like the Python's
/// `...` sentinel: outer `None` leaves the field alone, `Some(None)` clears it.
#[derive(Debug, Default, Clone)]
pub struct UpdateArgs {
    pub text: Option<String>,
    pub interval: Option<String>,
    pub name: Option<String>,
    pub only_when: Option<Option<String>>,
    pub max_sends: Option<Option<String>>,
    pub skip_if_unconsumed: Option<String>,
    pub hold_while_typing: Option<String>,
}

/// The dunk list of `Flock` (`dunkers`, `next_id`) and the methods that mutate it. The target
/// is resolved by the caller (`Flock.resolve_target`) and passed as a pane row. The timer
/// thread (`_spawn_timer`/`_halt`) is NOT here: `start`/`stop` change the persisted fields only.
#[derive(Debug)]
pub struct DunkList {
    inner: std::sync::Mutex<ListState>,
}

#[derive(Debug)]
struct ListState {
    dunkers: Vec<Dunker>,
    next_id: i64,
}

impl Default for DunkList {
    fn default() -> Self {
        DunkList::new()
    }
}

impl DunkList {
    pub fn new() -> DunkList {
        DunkList {
            inner: std::sync::Mutex::new(ListState {
                dunkers: Vec::new(),
                next_id: 1,
            }),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, ListState> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// `_load` (the dunk part): `next_id` and every row that parses; a corrupt row is
    /// skipped, restored rows read `Restored`. A running dunk with a stale `next_send_at`
    /// is pushed `RESTORE_GRACE_S` ahead (its timer is the caller's to respawn).
    pub fn load(&self, payload: &Map<String, Value>) {
        let mut state = self.state();
        state.next_id = int_or_zero(payload, "next_id");
        if state.next_id == 0 {
            state.next_id = 1;
        }
        let rows = payload
            .get("dunkers")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for row in rows {
            let Some(map) = row.as_object() else { continue };
            let Ok(mut dunker) = Dunker::from_dict(map) else {
                continue;
            };
            dunker.status = "Restored".into();
            state.next_id = state.next_id.max(numeric_id(&dunker.id) + 1);
            state.dunkers.push(dunker);
        }
        let now = clock::now_s();
        for dunker in state.dunkers.iter_mut().filter(|d| d.running) {
            if dunker.next_send_at.map_or(true, |at| at == 0.0 || at < now) {
                dunker.next_send_at = Some(now + RESTORE_GRACE_S);
            }
        }
    }

    /// What `_save` writes for the dunk part: `next_id` and one ordered row per dunk.
    pub fn snapshot(&self) -> (i64, Vec<Vec<(&'static str, Value)>>) {
        let state = self.state();
        (
            state.next_id,
            state.dunkers.iter().map(Dunker::persisted_pairs).collect(),
        )
    }

    /// `_find`: exact id, else a bare number means `d<number>`.
    fn find(state: &ListState, dunk_id: &str) -> Result<usize, DunkError> {
        let wanted = dunk_id.trim();
        let position = state
            .dunkers
            .iter()
            .position(|d| d.id == wanted)
            .or_else(|| {
                let numeric = !wanted.is_empty() && wanted.chars().all(|c| c.is_ascii_digit());
                numeric
                    .then(|| {
                        state
                            .dunkers
                            .iter()
                            .position(|d| d.id == format!("d{wanted}"))
                    })
                    .flatten()
            });
        position.ok_or_else(|| DunkError::Refused(format!("no dunk with id {wanted:?}")))
    }

    pub fn get(&self, dunk_id: &str) -> Result<Map<String, Value>, DunkError> {
        let state = self.state();
        Ok(state.dunkers[Self::find(&state, dunk_id)?].to_dict())
    }

    pub fn list(&self) -> Vec<Map<String, Value>> {
        self.state().dunkers.iter().map(Dunker::to_dict).collect()
    }

    /// `Flock.add`. `pane` is the resolved target row, if a target was given.
    pub fn add(
        &self,
        pane: Option<&Map<String, Value>>,
        args: &AddArgs,
    ) -> Result<Map<String, Value>, DunkError> {
        let interval = match args.interval.as_deref() {
            Some(text) => parse_interval(text)?,
            None => DEFAULT_INTERVAL_MINUTES,
        };
        let gate_given = args.only_when.is_some();
        let only_when = check_only_when(args.only_when.as_deref())?;
        let max_sends = check_max_sends(args.max_sends.as_deref())?;
        let skip = match &args.skip_if_unconsumed {
            Some(v) => check_bool(v, "skip_if_unconsumed")?,
            None => false,
        };
        let hold = match &args.hold_while_typing {
            Some(v) => check_bool(v, "hold_while_typing")?,
            None => true,
        };
        let mut state = self.state();
        let mut dunker = Dunker::new(&format!("d{}", state.next_id));
        dunker.name = args.name.clone().unwrap_or_default();
        dunker.interval_minutes = interval;
        dunker.text = args
            .text
            .clone()
            .unwrap_or_else(|| DEFAULT_TEXT.to_string());
        dunker.only_when = only_when;
        dunker.max_sends = max_sends;
        dunker.skip_if_unconsumed = skip;
        dunker.hold_while_typing = hold;
        state.next_id += 1;
        if let Some(pane) = pane {
            dunker.set_target(pane);
            dunker.status = "Target set".into();
            // Typing into an agent mid-turn can abort that turn, silently. A schedule aimed at
            // an agent waits for it to be idle unless the caller says otherwise.
            if !gate_given
                && dunker
                    .target_agent
                    .as_deref()
                    .is_some_and(|a| !a.is_empty())
            {
                dunker.only_when = Some("idle".into());
            }
        }
        if args.start {
            if pane.is_some() {
                start_fields(&mut dunker)?;
            } else {
                dunker.status = "No target - press c".into();
            }
        }
        let out = dunker.to_dict();
        state.dunkers.push(dunker);
        Ok(out)
    }

    /// `Flock.update`.
    pub fn update(
        &self,
        dunk_id: &str,
        pane: Option<&Map<String, Value>>,
        args: &UpdateArgs,
    ) -> Result<Map<String, Value>, DunkError> {
        let interval = args.interval.as_deref().map(parse_interval).transpose()?;
        let only_when = match &args.only_when {
            Some(v) => Some(check_only_when(v.as_deref())?),
            None => None,
        };
        let max_sends = match &args.max_sends {
            Some(v) => Some(check_max_sends(v.as_deref())?),
            None => None,
        };
        let skip = args
            .skip_if_unconsumed
            .as_deref()
            .map(|v| check_bool(v, "skip_if_unconsumed"))
            .transpose()?;
        let hold = args
            .hold_while_typing
            .as_deref()
            .map(|v| check_bool(v, "hold_while_typing"))
            .transpose()?;
        let mut state = self.state();
        let index = Self::find(&state, dunk_id)?;
        let dunker = &mut state.dunkers[index];
        let mut changed: Vec<&str> = Vec::new();
        if let Some(pane) = pane {
            dunker.set_target(pane);
            changed.push("target");
        }
        if let Some(text) = &args.text {
            dunker.text = text.clone();
            changed.push("text");
        }
        if let Some(interval) = interval {
            if interval != dunker.interval_minutes {
                dunker.interval_minutes = interval;
                if dunker.running {
                    let now = clock::now_s();
                    let anchor = dunker.last_sent_at.or(dunker.started_at).unwrap_or(now);
                    dunker.next_send_at = Some((now + 1.0).max(anchor + interval * 60.0));
                }
                changed.push("interval");
            }
        }
        if let Some(name) = &args.name {
            dunker.name = name.clone();
            changed.push("name");
        }
        if let Some(gate) = only_when {
            dunker.only_when = gate;
            changed.push("only_when");
        }
        if let Some(cap) = max_sends {
            dunker.max_sends = cap;
            changed.push("max_sends");
        }
        if let Some(flag) = skip {
            dunker.skip_if_unconsumed = flag;
            changed.push("skip_if_unconsumed");
        }
        if let Some(flag) = hold {
            dunker.hold_while_typing = flag;
            changed.push("hold_while_typing");
        }
        if let Some(last) = changed.last() {
            let words = last.replace('_', " ");
            let mut chars = words.chars();
            let head = chars.next().map(|c| c.to_uppercase().collect::<String>());
            dunker.status = format!(
                "{}{} set",
                head.unwrap_or_default(),
                chars.as_str().to_lowercase()
            );
        }
        Ok(dunker.to_dict())
    }

    /// `Flock.remove`.
    pub fn remove(&self, dunk_id: &str) -> Result<Map<String, Value>, DunkError> {
        let mut state = self.state();
        let index = Self::find(&state, dunk_id)?;
        let mut dunker = state.dunkers.remove(index);
        dunker.running = false;
        Ok(dunker.to_dict())
    }

    /// `Flock.start` (the persisted fields; the timer is the caller's).
    pub fn start(&self, dunk_id: &str) -> Result<Map<String, Value>, DunkError> {
        let mut state = self.state();
        let index = Self::find(&state, dunk_id)?;
        let dunker = &mut state.dunkers[index];
        start_fields(dunker)?;
        Ok(dunker.to_dict())
    }

    /// `Flock.stop`.
    pub fn stop(&self, dunk_id: &str) -> Result<Map<String, Value>, DunkError> {
        let mut state = self.state();
        let index = Self::find(&state, dunk_id)?;
        let dunker = &mut state.dunkers[index];
        stop_fields(dunker);
        Ok(dunker.to_dict())
    }

    /// `Flock.toggle`.
    pub fn toggle(&self, dunk_id: &str) -> Result<Map<String, Value>, DunkError> {
        let running = {
            let state = self.state();
            state.dunkers[Self::find(&state, dunk_id)?].running
        };
        if running {
            self.stop(dunk_id)
        } else {
            self.start(dunk_id)
        }
    }

    /// The dunker-halting half of `close_target`: every RUNNING dunker aimed at a pane in
    /// `doomed` stops with status `Target closed` and no next send. Returns their ids in list
    /// order. (The Python also cancels the per-dunk timer thread, `_halt`; there is none here,
    /// so the persisted fields are the whole stop. A stopped dunk therefore can never report
    /// itself as scheduled again: nothing is left to overwrite the status.)
    pub fn halt_aimed_at(&self, doomed: &[String]) -> Vec<String> {
        let mut state = self.state();
        let mut stopped = Vec::new();
        for dunker in state.dunkers.iter_mut() {
            let aimed = dunker
                .target_pane_id
                .as_deref()
                .is_some_and(|pane| doomed.iter().any(|d| d == pane));
            if dunker.running && aimed {
                dunker.running = false;
                dunker.next_send_at = None;
                dunker.status = "Target closed".into();
                stopped.push(dunker.id.clone());
            }
        }
        stopped
    }

    /// `Flock.stop_all`.
    pub fn stop_all(&self) -> Vec<Map<String, Value>> {
        let mut state = self.state();
        for dunker in state.dunkers.iter_mut().filter(|d| d.running) {
            stop_fields(dunker);
        }
        state.dunkers.iter().map(Dunker::to_dict).collect()
    }
}

/// `RESTORE_GRACE_S`.
pub const RESTORE_GRACE_S: f64 = 5.0;

/// `_start` without the timer: needs a target; sets the countdown.
fn start_fields(dunker: &mut Dunker) -> Result<(), DunkError> {
    if dunker.target_pane_id.as_deref().map_or(true, str::is_empty) {
        dunker.status = "No target - press c".into();
        return Err(DunkError::Refused(format!("{} has no target", dunker.id)));
    }
    let now = clock::now_s();
    dunker.running = true;
    dunker.started_at = Some(now);
    dunker.next_send_at = Some(now + dunker.interval_minutes * 60.0);
    dunker.last_error = None;
    Ok(())
}

fn stop_fields(dunker: &mut Dunker) {
    dunker.running = false;
    dunker.next_send_at = None;
    dunker.status = "Stopped".into();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kw(value: Value) -> Map<String, Value> {
        value.as_object().cloned().expect("object")
    }

    #[test]
    fn numbers_and_suffixes() {
        assert_eq!(10.0, parse_interval("10").unwrap());
        assert_eq!(1.5, parse_interval("90s").unwrap());
        assert_eq!(90.0, parse_interval("1.5h").unwrap());
        assert_eq!(2.0, parse_interval("2m").unwrap());
        assert_eq!(0.5, parse_interval_number(0.5).unwrap());
        assert_eq!(10.0, parse_interval_number(10.0).unwrap());
    }

    #[test]
    fn format_round_trips() {
        for minutes in [10.0, 1.5, 0.5, 1.0 / 12.0, 120.0] {
            let back = parse_interval(&format_interval(minutes)).unwrap();
            assert!((back - minutes).abs() < 1e-9, "{minutes} -> {back}");
        }
        assert_eq!("30s", format_interval(0.5));
        assert_eq!("5s", format_interval(1.0 / 12.0));
        assert_eq!("10", format_interval(10.0));
        assert_eq!("1.5", format_interval(1.5));
    }

    #[test]
    fn rejects_bad_values() {
        for bad in ["0", "abc", "", "10x"] {
            assert!(parse_interval(bad).is_err(), "{bad:?}");
        }
        assert!(parse_interval_number(-1.0).is_err());
    }

    #[test]
    fn known_placeholders_expand_and_others_survive() {
        let dunker = Dunker::with(
            "d7",
            &kw(
                json!({"name": "nightly", "interval_minutes": 60, "target_pane_id": "w1:p2",
                       "target_tab": "Claude Proj", "target_agent": "claude", "send_count": 2}),
            ),
        );
        let text =
            r#"dunk {id} "{name}" #{count} -> {target} every {interval}m {"json": 1} {unknown}"#;
        assert_eq!(
            r#"dunk d7 "nightly" #3 -> Claude Proj / claude every 60m {"json": 1} {unknown}"#,
            expand_template(text, &dunker)
        );
    }

    #[test]
    fn name_falls_back_to_id() {
        assert_eq!("d1", expand_template("{name}", &Dunker::new("d1")));
    }

    #[test]
    fn defaults_match_the_python_constructor() {
        let d = Dunker::new("d1");
        assert_eq!(
            ("", DEFAULT_TEXT, 10.0),
            (d.name.as_str(), d.text.as_str(), d.interval_minutes)
        );
        assert!(d.hold_while_typing && !d.running && !d.skip_if_unconsumed);
        assert_eq!("Ready", d.status);
        assert_eq!("(no target)", d.target_label());
        // total_skips falls back to skip_count for an old state file
        assert_eq!(
            4,
            Dunker::with("d2", &kw(json!({"skip_count": 4}))).total_skips
        );
        // `or None`: an empty gate and a zero cap are None
        let d = Dunker::with("d3", &kw(json!({"only_when": "", "max_sends": 0})));
        assert!(d.only_when.is_none() && d.max_sends.is_none());
    }

    #[test]
    fn to_dict_round_trips_through_from_dict_and_keeps_field_order() {
        let mut d = Dunker::with(
            "d9",
            &kw(json!({"target_pane_id": "w1:p1", "max_sends": 3})),
        );
        d.set_target(&kw(
            json!({"pane_id": "w2:p1", "agent": "claude", "cwd": "/x",
                                "tab_label": "", "tab_id": "t7", "workspace_id": "w2"}),
        ));
        assert_eq!(Some("t7"), d.target_tab.as_deref());
        assert_eq!(Some("w2"), d.target_workspace.as_deref());
        assert_eq!("t7 / claude", d.target_label());
        let keys: Vec<&str> = d.persisted_pairs().iter().map(|(k, _)| *k).collect();
        assert_eq!(FIELDS.to_vec(), keys);
        let again = Dunker::from_dict(&d.to_dict()).unwrap();
        assert_eq!(d.persisted(), again.persisted());
        assert!(d.to_dict()["next_send_in_s"].is_null(), "not running");
    }

    #[test]
    fn a_bad_interval_is_quoted_like_python_repr() {
        let error = parse_interval("abc").unwrap_err().to_string();
        assert!(
            error.contains("bad interval 'abc' (try 10, 90s, 1.5h)"),
            "{error}"
        );
    }

    #[test]
    fn fmt_g_matches_python_g() {
        assert_eq!("5e-05", fmt_g(5e-05));
        assert_eq!("0.0001", fmt_g(1e-4));
        assert_eq!("1e+06", fmt_g(999999.5));
        assert_eq!("999999", fmt_g(999999.0));
        assert_eq!("1.5", fmt_g(1.5));
        assert_eq!("0.833333", fmt_g(5.0 / 6.0));
        assert_eq!("1.23457e+08", fmt_g(123456789.0));
        // through the public path: 3e-3 s is 0.00005 minutes
        assert_eq!("0.003s", format_interval(0.00005));
    }

    #[test]
    fn next_send_in_s_rounds_half_to_even_like_python() {
        let mut d = Dunker::new("d1");
        d.running = true;
        let now = clock::now_s();
        // 0.5 s and 2.5 s ahead are ties: Python round() gives 0 and 2
        d.next_send_at = Some(now + 2.5);
        let two = d.to_dict()["next_send_in_s"].as_i64().unwrap();
        assert!(two == 2 || two == 3, "clock moved between reads: {two}");
        assert_eq!(0.0_f64.round_ties_even(), 0.5_f64.round_ties_even());
        assert_eq!(2.0, 2.5_f64.round_ties_even());
        assert_eq!(4.0, 3.5_f64.round_ties_even());
    }

    fn pane(id: &str, agent: Option<&str>) -> Map<String, Value> {
        kw(json!({"pane_id": id, "agent": agent, "tab_label": "Claude Proj"}))
    }

    #[test]
    fn add_assigns_ids_and_resolves_target_labels() {
        let list = DunkList::new();
        let first = list
            .add(Some(&pane("w1:p1", Some("claude"))), &AddArgs::default())
            .unwrap();
        let second = list
            .add(
                None,
                &AddArgs {
                    name: Some("x".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(json!("d1"), first["id"]);
        assert_eq!(json!("d2"), second["id"]);
        assert_eq!(json!("Claude Proj / claude"), first["target_label"]);
        assert_eq!(json!("(no target)"), second["target_label"]);
        // an agent target defaults to the idle gate; no target stays ungated
        assert_eq!(json!("idle"), first["only_when"]);
        assert!(second["only_when"].is_null());
        // saying only_when=any is honoured
        let any = AddArgs {
            only_when: Some("any".into()),
            ..Default::default()
        };
        assert!(list
            .add(Some(&pane("w1:p2", Some("claude"))), &any)
            .unwrap()["only_when"]
            .is_null());
    }

    #[test]
    fn start_needs_a_target_and_stop_clears_the_countdown() {
        let list = DunkList::new();
        list.add(None, &AddArgs::default()).unwrap();
        assert!(list.start("d1").is_err());
        let started = list
            .add(
                Some(&pane("w1:p1", None)),
                &AddArgs {
                    start: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(json!(true), started["running"]);
        assert!(started["next_send_at"].is_number());
        let stopped = list.stop("2").unwrap(); // a bare number means d2
        assert_eq!(json!(false), stopped["running"]);
        assert!(stopped["next_send_at"].is_null());
        assert_eq!(json!("Stopped"), stopped["status"]);
        assert_eq!(json!(true), list.toggle("d2").unwrap()["running"]);
        let all = list.stop_all();
        assert!(all.iter().all(|d| d["running"] == json!(false)));
    }

    #[test]
    fn update_changes_fields_and_clears_with_an_explicit_none() {
        let list = DunkList::new();
        list.add(
            None,
            &AddArgs {
                only_when: Some("idle".into()),
                max_sends: Some("3".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let out = list
            .update(
                "d1",
                None,
                &UpdateArgs {
                    text: Some("hi".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(json!("hi"), out["text"]);
        assert_eq!(json!("idle"), out["only_when"], "omitted leaves it alone");
        assert_eq!(json!("Text set"), out["status"]);
        let cleared = list
            .update(
                "d1",
                None,
                &UpdateArgs {
                    only_when: Some(None),
                    max_sends: Some(None),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(cleared["only_when"].is_null() && cleared["max_sends"].is_null());
        assert_eq!(json!("Max sends set"), cleared["status"]);
    }

    #[test]
    fn remove_get_and_the_refusals() {
        let list = DunkList::new();
        list.add(None, &AddArgs::default()).unwrap();
        assert!(list.get("d9").is_err());
        assert!(list
            .add(
                None,
                &AddArgs {
                    only_when: Some("busy".into()),
                    ..Default::default()
                }
            )
            .is_err());
        assert!(list
            .add(
                None,
                &AddArgs {
                    max_sends: Some("-1".into()),
                    ..Default::default()
                }
            )
            .is_err());
        assert!(list
            .add(
                None,
                &AddArgs {
                    interval: Some("abc".into()),
                    ..Default::default()
                }
            )
            .is_err());
        assert_eq!(json!("d1"), list.remove("d1").unwrap()["id"]);
        assert!(list.list().is_empty());
    }

    #[test]
    fn load_restores_rows_skips_corrupt_ones_and_keeps_ids_moving() {
        let src = DunkList::new();
        src.add(None, &AddArgs::default()).unwrap();
        src.add(
            Some(&pane("w1:p1", None)),
            &AddArgs {
                start: true,
                ..Default::default()
            },
        )
        .unwrap();
        let (next_id, rows) = src.snapshot();
        let mut dunkers: Vec<Value> = rows
            .into_iter()
            .map(|row| Value::Object(row.into_iter().map(|(k, v)| (k.to_string(), v)).collect()))
            .collect();
        dunkers.push(json!("garbage"));
        dunkers[1]["next_send_at"] = json!(1.0); // stale
        let list = DunkList::new();
        list.load(&kw(json!({"next_id": next_id, "dunkers": dunkers})));
        let all = list.list();
        assert_eq!(2, all.len());
        assert_eq!(json!("Restored"), all[0]["status"]);
        assert!(all[1]["next_send_at"].as_f64().unwrap() > clock::now_s());
        assert_eq!(
            json!("d3"),
            list.add(None, &AddArgs::default()).unwrap()["id"]
        );
    }

    #[test]
    fn numeric_id_reads_d_numbers() {
        assert_eq!(
            (7, 0, 0),
            (numeric_id("d7"), numeric_id("x7"), numeric_id("d"))
        );
    }
}
