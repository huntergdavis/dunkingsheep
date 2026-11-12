//! Watchdog delivery adapters and one deterministic run engine. No clients or credentials are
//! discovered here; the sender, owner transport, pane provider, and host sampler are injected.
use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::{
    config::validate_config,
    evaluate::{self, HostSampler, PanesProvider},
    planner::dedupe_key,
    planner::{advance_streaks, plan},
    report::{digest, line, recoveries, render_lines},
    state::{load_state, save_state},
};
use crate::client::{ClientError, FlockClient, Transport};
use crate::error::DunkError;

/// Use the caller's configured client credentials; the system sender suppresses pane identity.
/// `call` never starts a daemon and never retries a write after submission.
pub fn inbox_sender<T: Transport>(
    client: &mut FlockClient<T>,
    to: &str,
    subject: &str,
    body: &str,
) -> Result<(), ClientError> {
    client.call(
        "inbox_send",
        json!({"to":to,"subject":subject,"body":body,"system_sender":"watchdog"})
            .as_object()
            .unwrap()
            .clone(),
    )?;
    Ok(())
}

/// Sender errors retain their Python exception class for the aggregated recipient diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{kind}: {message}")]
pub struct SendError {
    pub kind: String,
    pub message: String,
}

fn refused(text: impl Into<String>) -> DunkError {
    DunkError::Refused(text.into())
}
fn read_json(path: &Path) -> Result<Value, DunkError> {
    serde_json::from_str(&std::fs::read_to_string(path)?).map_err(|e| refused(e.to_string()))
}

/// Return an integer's exact JSON spelling. `arbitrary_precision` keeps Python-sized integers
/// in `Number`, but `as_i64`/`as_u64` still reject values outside machine ranges.
fn integer_text(value: &Value) -> Option<String> {
    let Value::Number(number) = value else {
        return None;
    };
    let text = number.to_string();
    let digits = text.strip_prefix('-').unwrap_or(&text);
    (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())).then_some(text)
}

fn compare_integer_text(left: &str, right: &str) -> Ordering {
    fn parts(value: &str) -> (bool, &str) {
        let negative = value.starts_with('-');
        let digits = value
            .strip_prefix('-')
            .unwrap_or(value)
            .trim_start_matches('0');
        if digits.is_empty() {
            (false, "0")
        } else {
            (negative, digits)
        }
    }
    let (left_negative, left_digits) = parts(left);
    let (right_negative, right_digits) = parts(right);
    match (left_negative, right_negative) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (false, false) => left_digits
            .len()
            .cmp(&right_digits.len())
            .then_with(|| left_digits.cmp(right_digits)),
        (true, true) => right_digits
            .len()
            .cmp(&left_digits.len())
            .then_with(|| right_digits.cmp(left_digits)),
    }
}

fn compare_json_integers(left: &Value, right: &Value) -> Option<Ordering> {
    Some(compare_integer_text(
        &integer_text(left)?,
        &integer_text(right)?,
    ))
}

/// Persist each accepted submission before trying the next recipient. An accepted submission
/// is not a read receipt. Persistence errors propagate immediately, outside the send catch.
pub fn send_starvation_group(
    config: &Value,
    state: &mut Value,
    item: &Value,
    now: f64,
    mut send: impl FnMut(&str, &str, &str) -> Result<(), SendError>,
) -> Result<(), DunkError> {
    let rule_id = item["rule"]
        .as_str()
        .ok_or_else(|| refused("finding rule must be a string"))?;
    let rule = &config["rules"][rule_id];
    let config_dir = config["stores"]["config_dir"]
        .as_str()
        .ok_or_else(|| refused("config_dir must be a string"))?;
    let path = PathBuf::from(crate::native_session::expanduser(config_dir)).join("superusers.json");
    let data = read_json(&path)?;
    let protected = data["panes"]
        .as_array()
        .filter(|panes| panes.iter().all(Value::is_string))
        .ok_or_else(|| refused("superusers.json panes must be a list of pane ids"))?;
    let recipients: BTreeSet<&str> = rule["superuser_panes"]
        .as_array()
        .ok_or_else(|| refused("superuser_panes must be a list"))?
        .iter()
        .map(|v| {
            v.as_str()
                .ok_or_else(|| refused("superuser_panes must contain pane ids"))
        })
        .collect::<Result<_, _>>()?;
    if !recipients
        .iter()
        .all(|pane| protected.iter().any(|p| p.as_str() == Some(pane)))
    {
        return Err(refused(
            "configured starvation recipients are not all current superusers",
        ));
    }
    let cooldown = rule["cooldown_minutes"]
        .as_f64()
        .ok_or_else(|| refused("cooldown_minutes must be numeric"))?
        * 60.0;
    let state_path = config["state_path"]
        .as_str()
        .ok_or_else(|| refused("state_path must be a string"))?;
    let state_path = PathBuf::from(crate::native_session::expanduser(state_path));
    let key = dedupe_key(item);
    let deliveries = state
        .as_object_mut()
        .ok_or_else(|| refused("state must be an object"))?
        .entry("superuser_deliveries")
        .or_insert_with(|| json!({}));
    deliveries
        .as_object_mut()
        .ok_or_else(|| refused("superuser_deliveries must be an object"))?
        .entry(&key)
        .or_insert_with(|| json!({}));
    let body = digest(
        "SUPERUSER allocation action",
        &[line("NOTIFY SUPERUSERS", item, &json!("no"), None, None)],
    );
    let mut errors = Vec::new();
    for pane in recipients {
        let accepted = state["superuser_deliveries"][&key]
            .as_object()
            .ok_or_else(|| refused("accepted deliveries must be an object"))?;
        if let Some(at) = accepted.get(pane) {
            let at = at
                .as_f64()
                .ok_or_else(|| refused("accepted delivery timestamp must be numeric"))?;
            if now - at < cooldown {
                continue;
            }
        }
        match send(
            pane,
            "watchdog: idle capacity needs backlog allocation",
            &body,
        ) {
            Err(error) => errors.push(format!("{pane}: {error}")),
            Ok(()) => {
                state["superuser_deliveries"][&key][pane] = json!(now);
                save_state(&state_path, state)?;
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(refused(errors.join("; ")))
    }
}

/// Route selection stays with the shared routing reader. This adapter never reads credentials.
/// The concrete Rust route reader is not wired by this leaf; callers provide it explicitly.
#[derive(Debug, thiserror::Error)]
#[error("desk brief post not wired: desk API token handling awaits VP decision")]
pub struct OwnerChannelUnavailable;

pub struct DeskBriefChannel<R> {
    pub confirm_path: PathBuf,
    route: R,
}
impl<R: Fn(&Path) -> Result<String, DunkError>> DeskBriefChannel<R> {
    pub fn new(confirm_path: PathBuf, route: R) -> Self {
        Self {
            confirm_path,
            route,
        }
    }
    pub fn desk_channel(&self) -> Result<String, DunkError> {
        (self.route)(self.confirm_path.parent().unwrap_or_else(|| Path::new(".")))
    }
    pub fn post(&self, _body: &str) -> Result<(), OwnerChannelUnavailable> {
        Err(OwnerChannelUnavailable)
    }
    pub fn confirmed_through(&self) -> Result<Option<Value>, DunkError> {
        let status = read_json(&self.confirm_path)?;
        // Python short-circuits non-object status before resolving the route.
        if !status.is_object() || status.get("channel") != Some(&json!(self.desk_channel()?)) {
            return Err(refused(
                "Telegram confirmation channel differs from configured route or is missing",
            ));
        }
        match status.get("last_brief_id") {
            None | Some(Value::Null) => Ok(None),
            Some(value) if integer_text(value).is_some() => Ok(Some(value.clone())),
            Some(value) => {
                let repr = crate::api::python_value_repr(value);
                Err(refused(format!("last_brief_id is not an integer: {repr}")))
            }
        }
    }
}

/// Injected owner transport. A production caller must provide a deliberately configured
/// adapter; the runner itself never discovers credentials or chooses a destination.
pub trait OwnerTransport {
    fn desk_channel(&self) -> Result<String, DunkError>;
    fn post(&self, body: &str) -> Result<i64, SendError>;
    fn confirmed_through(&self) -> Result<Option<Value>, DunkError>;
    /// True when an accepted post is the delivery itself (the owner-mailbox channel, t3624):
    /// the page is confirmed on post. The desk brief waits for its forwarder and says false.
    fn confirms_on_post(&self) -> bool {
        false
    }
    /// Posts one owner page (t3700). `emergency` is true only for a critical security, outage or
    /// money page: the channel may send that one to the phone. Any other page is mailbox-only. The
    /// default is the plain post.
    fn post_page(&self, _emergency: bool, _class: &str, body: &str) -> Result<i64, SendError> {
        self.post(body)
    }
    /// Whether an accepted post of this page is its delivery (t3700): a mailbox-only page is confirmed
    /// on post; an emergency page to the phone waits for the outbox receipt.
    fn confirms_on_post_for(&self, _emergency: bool) -> bool {
        self.confirms_on_post()
    }
}

/// The owner-page state entry written after an accepted post (t3624). Mirrors
/// `dunk_watchdog.run`: a confirm-on-post channel stamps `confirmed_at` at once.
pub fn owner_page_entry(
    brief_id: i64,
    now: f64,
    desk_channel: &str,
    confirms_on_post: bool,
) -> Value {
    json!({
        "brief_id": brief_id,
        "posted_at": now,
        "confirmed_at": if confirms_on_post { json!(now) } else { Value::Null },
        "desk_channel": desk_channel,
    })
}

impl<R: Fn(&Path) -> Result<String, DunkError>> OwnerTransport for DeskBriefChannel<R> {
    fn desk_channel(&self) -> Result<String, DunkError> {
        self.desk_channel()
    }
    fn post(&self, body: &str) -> Result<i64, SendError> {
        match DeskBriefChannel::post(self, body) {
            Ok(()) => unreachable!("the default desk adapter never reports a successful post"),
            Err(error) => Err(SendError {
                kind: "OwnerChannelUnavailable".into(),
                message: error.to_string(),
            }),
        }
    }
    fn confirmed_through(&self) -> Result<Option<Value>, DunkError> {
        DeskBriefChannel::confirmed_through(self)
    }
}

fn expanded(value: &Value, key: &str) -> Result<PathBuf, DunkError> {
    value[key]
        .as_str()
        .map(crate::native_session::expanduser)
        .map(PathBuf::from)
        .ok_or_else(|| refused(format!("{key} must be a string")))
}

fn confirm_pending(
    config: &Value,
    state: &mut Value,
    channel: &dyn OwnerTransport,
    now: f64,
    problems: &mut Vec<String>,
) {
    let mut pending: Vec<String> = state["owner_pages"]
        .as_object()
        .into_iter()
        .flat_map(|pages| pages.iter())
        .filter(|(_, page)| page["confirmed_at"].is_null() && page["status"] != "undelivered")
        .map(|(key, _)| key.clone())
        .collect();
    pending.sort();
    if pending.is_empty() {
        return;
    }
    let through = match channel.confirmed_through() {
        Ok(through) => through,
        Err(error) => {
            problems.push(format!("owner confirmation unreadable: {error}"));
            return;
        }
    };
    let channel_name = match channel.desk_channel() {
        Ok(name) => name,
        Err(error) => {
            problems.push(format!("owner confirmation unreadable: {error}"));
            return;
        }
    };
    let limit = config["escalation"]["owner"]["confirm_within_minutes"]
        .as_f64()
        .unwrap_or(0.0)
        * 60.0;
    for key in pending {
        let page = &mut state["owner_pages"][&key];
        let page_channel = page["desk_channel"].as_str().unwrap_or("vp").to_owned();
        if page_channel != channel_name {
            page["status"] = json!("undelivered");
            page["undelivered_at"] = json!(now);
            page["undelivered_reason"] = json!("channel-mismatch");
            page["confirmation_channel"] = json!(channel_name);
            problems.push(format!(
                "owner page {key} (brief {}) channel {page_channel} differs from confirmation channel {channel_name}; recorded as UNDELIVERED",
                page["brief_id"]
            ));
        } else if through.as_ref().is_some_and(|id| {
            compare_json_integers(&page["brief_id"], id)
                .is_some_and(|ordering| ordering != Ordering::Greater)
        }) {
            page["confirmed_at"] = json!(now);
        } else if page["posted_at"]
            .as_f64()
            .is_some_and(|posted| now - posted > limit)
        {
            let posted = page["posted_at"].as_f64().unwrap_or(now);
            problems.push(format!(
                "owner page {key} (brief {}) unconfirmed after {:.1}min; recorded as UNDELIVERED",
                page["brief_id"],
                (now - posted) / 60.0
            ));
        }
    }
}

/// Execute a watchdog pass. All effectful dependencies are explicit, so callers and tests choose
/// transports rather than letting this runner discover credentials or destinations.
pub fn run(
    config: &Value,
    now: f64,
    dry_run_flag: bool,
    send: &mut dyn FnMut(&str, &str, &str) -> Result<(), SendError>,
    owner_channel: &dyn OwnerTransport,
    panes_provider: Option<&PanesProvider<'_>>,
    host_sampler: Option<&HostSampler<'_>>,
) -> Result<Value, DunkError> {
    let mut config = config.clone();
    validate_config(&mut config).map_err(|error| refused(error.to_string()))?;
    let live = config["enabled"] == true && config["dry_run"] == false && !dry_run_flag;
    let mode = if live { "live" } else { "dry-run" };
    let state_path = expanded(&config, "state_path")?;
    let (mut state, state_problem) = load_state(&state_path);
    let evaluated = evaluate::evaluate(&config, now, panes_provider, host_sampler, None);
    let findings = evaluated["findings"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let mut problems: Vec<String> = evaluated["errors"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|entry| {
            format!(
                "rule {} SKIPPED: {}",
                entry["rule"].as_str().unwrap_or(""),
                entry["error"].as_str().unwrap_or("")
            )
        })
        .collect();
    if let Some(problem) = &state_problem {
        problems.push(format!("{problem} - owner pages suppressed this run"));
    }
    if live {
        confirm_pending(&config, &mut state, owner_channel, now, &mut problems);
    }
    let skipped = evaluated["skipped"].as_array().cloned().unwrap_or_default();
    let errors = evaluated["errors"].as_array().cloned().unwrap_or_default();
    let mut skipped_all = skipped;
    skipped_all.extend(errors.iter().cloned());
    let owner_blocked = if state_problem.is_some() {
        Some("state-loss")
    } else if !errors.is_empty() {
        Some("self-error")
    } else {
        None
    };
    let streaks = advance_streaks(&config, &state, &findings, now);
    let decisions = plan(&config, &findings, &state, &streaks, now, owner_blocked);
    let recovered = recoveries(&state, &findings);
    let vp_pending: Vec<Value> = decisions
        .iter()
        .filter(|d| d["vp"] == "no")
        .cloned()
        .collect();
    let owner_pending: Vec<Value> = decisions
        .iter()
        .filter(|d| d["owner"] == "no")
        .cloned()
        .collect();
    let mut report = json!({
        "mode": mode,
        "why_not_live": if live { Value::Null } else if dry_run_flag { json!("--dry-run flag") } else if config["enabled"] != true { json!("config enabled=false") } else { json!("config dry_run=true") },
        "now": now, "findings": findings, "skipped": skipped_all, "decisions": decisions,
        "recovered": recovered, "problems": problems, "sent": [], "delivery_error": null,
        "carried_failure": recent_failure(&state, now),
        "delivery_failure_count": state["delivery_failure_count"].as_i64().unwrap_or(0),
        "counts": {"findings": findings.len(), "vp": vp_pending.len(), "owner": owner_pending.len(),
            "needs_attention": vp_pending.len() + decisions.iter().filter(|d| d["vp"] == "rate").count()},
    });
    if !live {
        report["lines"] = json!(render_lines(
            &config,
            &decisions,
            &report["skipped"].as_array().cloned().unwrap_or_default(),
            &recovered,
            mode
        ));
        return Ok(report);
    }

    let current: BTreeSet<String> = findings.iter().map(dedupe_key).collect();
    let mut group_lines = Vec::new();
    let mut vp = vp_pending;
    let group: Vec<Value> = vp
        .iter()
        .filter(|d| d["finding"]["rule"] == "fleet.assignment_starvation")
        .cloned()
        .collect();
    vp.retain(|d| d["finding"]["rule"] != "fleet.assignment_starvation");
    for decision in group {
        let item = &decision["finding"];
        match send_starvation_group(&config, &mut state, item, now, |to, subject, body| {
            send(to, subject, body)
        }) {
            Err(error) => {
                problems.push(format!("superuser allocation alert: {error}"));
                set_decision(&mut report, item, "vp", "transport-error");
            }
            Ok(()) => {
                report["sent"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!("superusers"));
                group_lines.push(line("NOTIFY SUPERUSERS", item, &json!("no"), None, None));
                state["alerts"]
                    .as_object_mut()
                    .unwrap()
                    .entry(dedupe_key(item))
                    .or_insert_with(|| json!({}))["last_alerted"] = json!(now);
            }
        }
    }
    let mut page_lines = Vec::new();
    for decision in owner_pending {
        let item = &decision["finding"];
        // t3700: an emergency page is a critical security, outage or money finding; it alone may reach
        // the phone. Every other owner escalation is mailbox-only and carries no page marker.
        let class = item["class"].as_str().unwrap_or("");
        let emergency = crate::owner::conversations::OWNER_PAGE_EMERGENCY_CLASSES.contains(&class)
            && item["severity"].as_str() == Some("critical");
        let confirms = owner_channel.confirms_on_post_for(emergency);
        let body = digest(
            if emergency {
                "OWNER page"
            } else {
                "OWNER escalation"
            },
            &[line(
                if emergency {
                    "PAGE OWNER"
                } else {
                    "ESCALATION"
                },
                item,
                &json!("no"),
                None,
                None,
            )],
        );
        let desk_channel = match owner_channel.desk_channel() {
            Ok(channel) => channel,
            Err(error) => {
                problems.push(format!(
                    "owner page {} not posted: {error}; recorded as UNDELIVERED",
                    dedupe_key(item)
                ));
                set_decision(&mut report, item, "owner", "transport-error");
                continue;
            }
        };
        match owner_channel.post_page(emergency, class, &body) {
            Err(error) => {
                problems.push(format!(
                    "owner page {} not posted: {}: {}; recorded as UNDELIVERED",
                    dedupe_key(item),
                    error.kind,
                    error.message
                ));
                set_decision(&mut report, item, "owner", "transport-error");
            }
            Ok(brief_id) => {
                state["owner_pages"][dedupe_key(item)] =
                    owner_page_entry(brief_id, now, &desk_channel, confirms);
                state["owner_page_times"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!(now));
                page_lines.push(line(
                    if confirms {
                        "ESCALATION (owner mailbox)"
                    } else {
                        "PAGE OWNER (pending confirmation)"
                    },
                    item,
                    &json!("no"),
                    None,
                    None,
                ));
            }
        }
    }
    for key in recovered["owner"].as_array().into_iter().flatten() {
        let key_text = key.as_str().unwrap_or("");
        let body = digest("OWNER resolved", &[format!("RESOLVED {key_text}")]);
        if let Err(error) = owner_channel.post(&body) {
            problems.push(format!(
                "owner resolved line {key_text} not posted: {}: {}",
                error.kind, error.message
            ));
        }
        if let Some(pages) = state["owner_pages"].as_object_mut() {
            pages.remove(key_text);
        }
    }
    let mut vp_lines = Vec::new();
    if !problems.is_empty() {
        let problem_values: Vec<Value> = problems.iter().map(|p| json!(p)).collect();
        vp_lines.push(line(
            "NOTIFY VP",
            &self_error(&problem_values),
            &json!("no"),
            None,
            None,
        ));
    }
    vp_lines.extend(page_lines);
    vp_lines.extend(
        vp.iter()
            .map(|d| line("NOTIFY VP", &d["finding"], &json!("no"), None, None)),
    );
    let suppressed = decisions.iter().filter(|d| d["vp"] == "rate").count();
    if suppressed > 0 {
        vp_lines.push(format!("{suppressed} further finding(s) suppressed by max_alerts_per_run; run `dunkingsheep watchdog --dry-run` to see all."));
    }
    for key in recovered["vp"].as_array().into_iter().flatten() {
        vp_lines.push(format!("RESOLVED VP dedupe={}", key.as_str().unwrap_or("")));
    }
    let mut all_lines = group_lines;
    all_lines.extend(vp_lines.iter().cloned());
    if !vp_lines.is_empty() {
        let subject = format!(
            "watchdog: {} alert(s){}",
            vp.len(),
            if problems.is_empty() {
                ""
            } else {
                " + self-error"
            }
        );
        match send(
            config["escalation"]["vp"]["mailbox"].as_str().unwrap_or(""),
            &subject,
            &digest("VP digest", &vp_lines),
        ) {
            Ok(()) => {
                report["sent"].as_array_mut().unwrap().push(json!("vp"));
                state.as_object_mut().unwrap().remove("delivery_failure");
            }
            Err(error) => {
                report["delivery_error"] = json!(format!("{}: {}", error.kind, error.message));
                state["last_run"] = json!(now);
                state["delivery_failure"] = json!({"at":now,"target":config["escalation"]["vp"]["mailbox"],"reason":report["delivery_error"],"pending":vp_lines.len(),"lines":vp_lines.iter().take(5).collect::<Vec<_>>()});
                state["delivery_failure_count"] =
                    json!(state["delivery_failure_count"].as_i64().unwrap_or(0) + 1);
                save_state(&state_path, &state)?;
                report["lines"] = json!(all_lines);
                report["problems"] = json!(problems);
                return Ok(report);
            }
        }
    }
    for decision in vp {
        state["alerts"]
            .as_object_mut()
            .unwrap()
            .entry(dedupe_key(&decision["finding"]))
            .or_insert_with(|| json!({}))["last_alerted"] = json!(now);
    }
    for key in recovered["vp"].as_array().into_iter().flatten() {
        state["alerts"]
            .as_object_mut()
            .unwrap()
            .remove(key.as_str().unwrap_or(""));
    }
    for entry in state["alerts"].as_object_mut().unwrap().values_mut() {
        entry
            .as_object_mut()
            .unwrap()
            .entry("first_seen")
            .or_insert(json!(now));
    }
    state["streaks"] = streaks;
    state["owner_page_times"] = json!(state["owner_page_times"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t.as_f64().is_some_and(|at| at > now - 3600.0))
        .cloned()
        .collect::<Vec<_>>());
    let retention = config["state_retention_days"].as_f64().unwrap_or(0.0) * 86400.0;
    let alerts = state["alerts"].as_object_mut().unwrap();
    alerts.retain(|key, entry| {
        current.contains(key) || now - entry["last_seen"].as_f64().unwrap_or(now) <= retention
    });
    for (key, entry) in alerts.iter_mut() {
        if current.contains(key) {
            entry["last_seen"] = json!(now);
        }
    }
    state["last_run"] = json!(now);
    save_state(&state_path, &state)?;
    write_heartbeat(
        &expanded(&config, "heartbeat_path")?,
        now,
        config["cadence_minutes"].as_f64().unwrap_or(0.0),
    )?;
    report["lines"] = json!(all_lines);
    report["problems"] = json!(problems);
    Ok(report)
}

fn set_decision(report: &mut Value, finding: &Value, which: &str, value: &str) {
    if let Some(decisions) = report["decisions"].as_array_mut() {
        if let Some(decision) = decisions.iter_mut().find(|d| d["finding"] == *finding) {
            decision[which] = json!(value);
        }
    }
}

fn self_error(problems: &[Value]) -> Value {
    json!({"rule":"watchdog.self_error","key":"run","severity":"self","class":"self","source":"watchdog-self","summary":format!("watchdog could not do {} thing(s); coverage or owner delivery is affected", problems.len()),"threshold":"-","measured":format!("{} problem(s)", problems.len()),"window":"this run","evidence":{"problems":problems}})
}

fn recent_failure(state: &Value, now: f64) -> Value {
    let max_age = std::env::var("DUNKINGSHEEP_CARRIED_FAILURE_MAX_S")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(43200.0);
    let Some(failure) = state
        .get("delivery_failure")
        .filter(|failure| python_truthy(failure))
    else {
        return Value::Null;
    };
    let Some(at) = failure.get("at").and_then(python_float) else {
        return Value::Null;
    };
    if now - at <= max_age {
        failure.clone()
    } else {
        Value::Null
    }
}

fn python_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn python_float(value: &Value) -> Option<f64> {
    match value {
        Value::Number(value) => value.as_f64(),
        Value::Bool(value) => Some(f64::from(u8::from(*value))),
        Value::String(value) => value.trim().parse().ok(),
        _ => None,
    }
}

fn write_heartbeat(path: &Path, now: f64, cadence: f64) -> Result<(), DunkError> {
    use std::time::{Duration, SystemTime};
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(
        &temp,
        serde_json::to_vec(&json!({"schema":1,"cadence_minutes":cadence,"at":now}))?,
    )?;
    let stamp = SystemTime::UNIX_EPOCH + Duration::from_secs_f64(now.max(0.0));
    let file = std::fs::OpenOptions::new().write(true).open(&temp)?;
    file.set_times(std::fs::FileTimes::new().set_modified(stamp))?;
    std::fs::rename(temp, path)?;
    Ok(())
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;
