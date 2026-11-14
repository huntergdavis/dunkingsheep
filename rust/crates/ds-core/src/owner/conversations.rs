//! Synchronous owner-mailbox operations over the inbox store.

use std::path::PathBuf;

use serde::Serialize;
use serde_json::Value;

use crate::error::DunkError;
use crate::flock::mailbox::OWNER_MAILBOX;
use crate::flock::panes::PaneInfoEnv;
use crate::flock::Flock;
use crate::inbox::{normalise_mailbox, validate_transport};
use crate::inbox_store::{Message, Outgoing, Store};

/// Borrowed arguments for one inbound owner message.
pub struct OwnerIngest<'a> {
    pub body: Option<&'a Value>,
    pub transport: Option<&'a str>,
    pub external_id: Option<&'a str>,
    pub subject: Option<&'a str>,
    pub attachments: &'a [PathBuf],
}

/// Raw wire arguments for the API-only owner-ingest compatibility boundary.
/// Typed callers should continue using [`OwnerIngest`].
pub struct OwnerIngestWire<'a> {
    pub body: Option<&'a Value>,
    pub transport: Option<&'a Value>,
    pub external_id: Option<&'a Value>,
    pub subject: Option<&'a Value>,
    pub attachments: Option<&'a Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Conversation {
    pub mailbox: String,
    pub messages: Vec<Value>,
    pub count: usize,
    pub waiting: Vec<String>,
}

/// Source and audience are independent. All retains the legacy API projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConversationFilter {
    All,
    AllSources { include_internal: bool },
    Human { include_internal: bool },
    OwnerLane { include_internal: bool },
}
impl ConversationFilter {
    pub fn parse(name: &str, include_internal: bool) -> Result<Self, DunkError> {
        match name {
            "all" if !include_internal => Ok(Self::All),
            "all_sources" => Ok(Self::AllSources { include_internal }),
            "human" => Ok(Self::Human { include_internal }),
            "owner_lane" => Ok(Self::OwnerLane { include_internal }),
            _ => Err(DunkError::Refused(
                "invalid conversation source filter".into(),
            )),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::AllSources { .. } => "all_sources",
            Self::Human { .. } => "human",
            Self::OwnerLane { .. } => "owner_lane",
        }
    }
    pub fn include_internal(self) -> bool {
        match self {
            Self::All => false,
            Self::AllSources { include_internal }
            | Self::Human { include_internal }
            | Self::OwnerLane { include_internal } => include_internal,
        }
    }
    pub fn root_partition(self) -> Option<bool> {
        match self {
            Self::Human { .. } => Some(true),
            Self::OwnerLane { .. } => Some(false),
            _ => None,
        }
    }
}

/// A page is an outbound notice for the owner's phone (t665, FLAT_FLEET 7.2), not a question:
/// it carries this sender and subject and no reply_to, so no desk answers it, and owner_outbox
/// returns it to the telegram transport only.
pub const OWNER_PAGE_SENDER: &str = "owner-page";
pub const OWNER_PAGE_SUBJECT: &str = "PAGE OWNER";
pub const OWNER_PAGE_TRANSPORT: &str = "telegram";
/// The transport of an owner page that is recorded to the owner mailbox only (t3700): it is never
/// written to the Telegram outbox, because the outbox reads only the `telegram` transport.
pub const OWNER_PAGE_MAILBOX_TRANSPORT: &str = "report";
/// The watchdog's critical classes that may reach the phone (t3700, owner decision). Everything else
/// is mailbox-only.
pub const OWNER_PAGE_EMERGENCY_CLASSES: [&str; 3] = ["security", "outage", "money"];

/// The transport an owner page is recorded on (t3700): `telegram` for a critical emergency page,
/// `report` (mailbox only) for every other page, including a page with no class.
pub fn owner_page_transport(class: Option<&str>) -> &'static str {
    let class = class.map(|value| value.trim().to_ascii_lowercase());
    match class.as_deref() {
        Some(value) if OWNER_PAGE_EMERGENCY_CLASSES.contains(&value) => OWNER_PAGE_TRANSPORT,
        _ => OWNER_PAGE_MAILBOX_TRANSPORT,
    }
}

/// Explicit subject prefixes of routine lane mail (t3592, coordinator decision i49515). Mail carrying one
/// stays in storage and in the task evidence, but is not listed in the owner conversation. `[owner-decision]`
/// and `[owner-blocker]` are not listed here, so they stay visible, and so does unmarked legacy mail.
pub const ROUTINE_SUBJECT_PREFIXES: [&str; 2] = ["[review-verdict]", "[review-note]"];

/// Whether a stored mail row is routine and must not appear in the owner conversation. Only the subject
/// prefix decides: contradictory words in the body never override the marker. Owner replies (a row with
/// `reply_to`) are never routine.
pub fn is_routine_mail(row: &Value) -> bool {
    if row.get("reply_to").is_some_and(value_truthy) {
        return false;
    }
    let subject = row
        .get("subject")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim_start();
    ROUTINE_SUBJECT_PREFIXES
        .iter()
        .any(|prefix| subject.starts_with(prefix))
}

/// The number the watchdog compares for an owner page: the digits of its mailbox id (`i47710`
/// -> 47710). None for any id that is not one of ours, so a stranger row never confirms a page.
pub fn owner_page_number(message_id: &str) -> Option<i64> {
    message_id.strip_prefix('i')?.parse::<i64>().ok()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ingested {
    pub message: Message,
    pub duplicate: bool,
    pub transport: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub reply: Message,
    pub question: String,
    pub transport: Option<String>,
    pub acked: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Outbox {
    pub transport: Option<String>,
    pub messages: Vec<Message>,
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Delivered {
    pub message: Message,
    pub transport: Option<String>,
    pub delivered_at: Option<f64>,
}

/// Owner mailbox service. It keeps all work on the caller's synchronous request thread.
pub struct OwnerConversations<'a> {
    flock: &'a Flock,
    pane_env: &'a dyn PaneInfoEnv,
}

impl<'a> OwnerConversations<'a> {
    /// Construction is inert: authorization and store lookup happen per operation.
    pub fn new(flock: &'a Flock, pane_env: &'a dyn PaneInfoEnv) -> Self {
        Self { flock, pane_env }
    }

    fn authorised_store(
        &self,
        self_pane_id: Option<&str>,
    ) -> Result<std::sync::Arc<Store>, DunkError> {
        self.authorised_role(self_pane_id)?;
        self.inbox_store()
    }

    fn authorised_role(&self, self_pane_id: Option<&str>) -> Result<(), DunkError> {
        let role = match self_pane_id.filter(|pane| !pane.is_empty()) {
            Some(pane) => self.flock.caller_role(Some(pane), self.pane_env)?,
            None => "owner".to_string(),
        };
        if !matches!(role.as_str(), "desk" | "web" | "owner" | "system") {
            return Err(DunkError::Refused(format!(
                "the owner mailbox is the desk's and the owner's; role {} may not read or answer it",
                python_repr(&role)
            )));
        }
        Ok(())
    }

    fn inbox_store(&self) -> Result<std::sync::Arc<Store>, DunkError> {
        self.flock
            .stores()
            .inbox
            .ok_or_else(|| DunkError::Refused("the owner inbox store is not attached".to_string()))
    }

    /// The owner mailbox plus the mailbox of every owner-role lane (t3535). A lane's send to its
    /// own name (for example `codex-rust-owner`) is recorded under that name, with the owner
    /// pane's id, so the conversation must read it too or lane messages never reach the desk.
    fn conversation_mailboxes(&self, store: &Store) -> Result<Vec<String>, DunkError> {
        let described = self.flock.with_roles(|roles, _| roles.describe())?;
        let owner_panes: Vec<String> = described
            .get("lanes")
            .and_then(Value::as_object)
            .map(|lanes| {
                lanes
                    .iter()
                    .filter(|(_, entry)| entry.get("role").and_then(Value::as_str) == Some("owner"))
                    .map(|(pane, _)| pane.clone())
                    .collect()
            })
            .unwrap_or_default();
        let mut mailboxes = vec![OWNER_MAILBOX.to_string()];
        for pane in owner_panes {
            for mailbox in crate::inbox::mailboxes_for_pane(store.read_pool(), &pane)? {
                if !mailboxes.contains(&mailbox) {
                    mailboxes.push(mailbox);
                }
            }
        }
        Ok(mailboxes)
    }

    /// Read one owner conversation window without consuming or acknowledging rows.
    pub fn conversation(
        &self,
        limit: Option<i64>,
        before: Option<f64>,
        self_pane_id: Option<&str>,
    ) -> Result<Conversation, DunkError> {
        self.conversation_window(
            limit,
            before,
            None,
            self_pane_id,
            ConversationFilter::All,
            None,
        )
    }

    pub fn conversation_cursor(
        &self,
        limit: i64,
        before_id: Option<&str>,
        self_pane_id: Option<&str>,
    ) -> Result<Conversation, DunkError> {
        self.conversation_filtered_cursor(limit, before_id, self_pane_id, ConversationFilter::All)
    }

    /// Explicit source filters are owner/web-only, including the raw internal lane view.
    pub fn conversation_filtered_cursor(
        &self,
        limit: i64,
        before_id: Option<&str>,
        self_pane_id: Option<&str>,
        filter: ConversationFilter,
    ) -> Result<Conversation, DunkError> {
        if !(1..=201).contains(&limit) {
            return Err(DunkError::Refused(
                "history page limit must be 1..201".into(),
            ));
        }
        if filter == ConversationFilter::All {
            return self.conversation_window(
                Some(limit),
                None,
                Some(before_id),
                self_pane_id,
                filter,
                None,
            );
        }
        let role = self.flock.caller_role(self_pane_id, self.pane_env)?;
        let result = if matches!(role.as_str(), "owner" | "web") {
            self.conversation_window(
                Some(limit),
                None,
                Some(before_id),
                self_pane_id,
                filter,
                Some(&role),
            )
        } else {
            Err(DunkError::Refused(
                "filtered history requires the owner or web role".into(),
            ))
        };
        if result.is_err() {
            eprintln!(
                "{}",
                serde_json::json!({
                    "event":"owner_history_filter", "role":role, "pane":self_pane_id,
                    "source_filter":filter.name(), "include_internal":filter.include_internal(),
                    "before_id":before_id, "result":"refused_or_error"
                })
            );
        }
        result
    }

    fn conversation_window(
        &self,
        limit: Option<i64>,
        before: Option<f64>,
        cursor: Option<Option<&str>>,
        self_pane_id: Option<&str>,
        filter: ConversationFilter,
        audit_role: Option<&str>,
    ) -> Result<Conversation, DunkError> {
        let store = self.authorised_store(self_pane_id)?;
        let limit = limit.filter(|limit| *limit != 0).unwrap_or(50);
        let mailboxes = match filter {
            ConversationFilter::Human { .. } => vec![OWNER_MAILBOX.to_owned()],
            ConversationFilter::OwnerLane { .. } => self
                .conversation_mailboxes(&store)?
                .into_iter()
                .filter(|key| normalise_mailbox(key) != OWNER_MAILBOX)
                .collect(),
            ConversationFilter::All | ConversationFilter::AllSources { .. } => {
                self.conversation_mailboxes(&store)?
            }
        };
        // Routine lane mail (review verdicts and notes, marked by subject) is kept in storage and listed nowhere
        // here (t3592). Owner decisions, owner blockers, owner replies and unmarked legacy mail stay listed.
        let mut messages: Vec<Value> = if let Some(before_id) = cursor {
            store.conversation_filtered_cursor(&mailboxes, limit, before_id, filter)?
        } else {
            store.conversation_across_visible(&mailboxes, Some(limit), before)?
        };
        // `waiting` is the desk's list of owner questions. The window now also carries lane
        // mailboxes (t3535), but `reply()` only answers rows in the owner mailbox, so a lane's
        // unread mail must not be listed here: the same predicate `reply()` enforces.
        let waiting = messages
            .iter()
            .filter(|row| {
                row.get("mailbox")
                    .and_then(Value::as_str)
                    .is_some_and(|mailbox| normalise_mailbox(mailbox) == OWNER_MAILBOX)
                    && row.get("state").and_then(Value::as_str) == Some("queued")
                    && row.get("answered").and_then(Value::as_bool) != Some(true)
                    && !row.get("reply_to").is_some_and(value_truthy)
            })
            .filter_map(|row| row.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        let count = messages.len();
        if let Some(role) = audit_role {
            // One metadata-only record per request, including the lookahead row in count.
            eprintln!(
                "{}",
                serde_json::json!({
                    "event":"owner_history_filter", "role":role, "pane":self_pane_id,
                    "source_filter":filter.name(), "include_internal":filter.include_internal(),
                    "mailboxes":mailboxes, "before_id":cursor.flatten(), "count":count,
                    "first_id":messages.first().and_then(|row| row.get("id")),
                    "last_id":messages.last().and_then(|row| row.get("id")), "result":"ok"
                })
            );
        }
        Ok(Conversation {
            mailbox: OWNER_MAILBOX.to_string(),
            messages: std::mem::take(&mut messages),
            count,
            waiting,
        })
    }

    /// Idempotently ingest one transport message, preserving find-then-send races.
    pub fn ingest(
        &self,
        args: OwnerIngest<'_>,
        self_pane_id: Option<&str>,
    ) -> Result<Ingested, DunkError> {
        let store = self.authorised_store(self_pane_id)?;
        let transport = validate_transport(args.transport)
            .map_err(DunkError::Refused)?
            .ok_or_else(|| {
                DunkError::Refused("an ingested owner question needs a transport".to_string())
            })?;
        if args.external_id.is_some_and(|id| !id.is_empty()) {
            if let Some(message) = store.find_by_external_id(OWNER_MAILBOX, args.external_id)? {
                return Ok(Ingested {
                    message,
                    duplicate: true,
                    transport,
                });
            }
        }

        let body = args.body.map(python_str).unwrap_or_default();
        let outgoing = Outgoing {
            mailbox: OWNER_MAILBOX,
            body: Some(&body),
            subject: args.subject,
            sender: None,
            mailbox_label: Some(OWNER_MAILBOX),
            pane_id: None,
            attachments: args.attachments,
            transport: Some(&transport),
            reply_to: None,
            external_id: args.external_id,
        };
        let message_id = store.send(&outgoing)?;
        let message = store.get_message(&message_id)?;
        self.flock.invalidate_view();
        Ok(Ingested {
            message,
            duplicate: false,
            transport,
        })
    }

    /// Ingest raw API values while preserving Python's role/transport/dedup and
    /// Inbox.send attachment-before-subject binding order.
    pub fn ingest_wire(
        &self,
        args: OwnerIngestWire<'_>,
        self_pane_id: Option<&str>,
    ) -> Result<Ingested, DunkError> {
        self.authorised_role(self_pane_id)?;
        let transport_value = args
            .transport
            .filter(|value| !value.is_null())
            .map(python_str);
        let transport = validate_transport(transport_value.as_deref())
            .map_err(DunkError::Refused)?
            .ok_or_else(|| {
                DunkError::Refused("an ingested owner question needs a transport".to_string())
            })?;
        let external_id = args
            .external_id
            .filter(|value| value_truthy(value))
            .map(python_str);
        let store = self.inbox_store()?;
        if external_id.as_deref().is_some_and(|id| !id.is_empty()) {
            if let Some(message) =
                store.find_by_external_id(OWNER_MAILBOX, external_id.as_deref())?
            {
                return Ok(Ingested {
                    message,
                    duplicate: true,
                    transport,
                });
            }
        }

        let body = args.body.map(python_str).unwrap_or_default();
        let message_id = store.send_owner_ingest_wire(
            &body,
            args.subject,
            args.attachments,
            &transport,
            external_id.as_deref(),
        )?;
        let message = store.get_message(&message_id)?;
        self.flock.invalidate_view();
        Ok(Ingested {
            message,
            duplicate: false,
            transport,
        })
    }

    /// Only the owner and the system may record or confirm an owner page (t665).
    fn require_page_role(&self, self_pane_id: Option<&str>) -> Result<(), DunkError> {
        let role = match self_pane_id.filter(|pane| !pane.is_empty()) {
            Some(pane) => self.flock.caller_role(Some(pane), self.pane_env)?,
            None => "owner".to_string(),
        };
        if !matches!(role.as_str(), "owner" | "system") {
            return Err(DunkError::Refused(format!(
                "only the owner and the system may record or confirm an owner page; role {} may not",
                python_repr(&role)
            )));
        }
        Ok(())
    }

    /// The highest owner page the Telegram transport has delivered, as a mailbox number (t665).
    /// The watchdog confirms a page once this reaches the page's own number: pages leave the
    /// outbox in id order, so the delivered ones are a prefix.
    pub fn page_confirmed_through(
        &self,
        self_pane_id: Option<&str>,
    ) -> Result<Option<i64>, DunkError> {
        self.require_page_role(self_pane_id)?;
        self.inbox_store()?.owner_page_delivered_through()
    }

    /// Record one owner page for the Telegram outbox (t665). Only the owner and the system may
    /// record one. `external_id` is the producer's dedupe key: a repeat returns the row it
    /// already made, so a re-raised page never reaches the phone twice.
    pub fn page(
        &self,
        body: Option<&Value>,
        external_id: Option<&Value>,
        self_pane_id: Option<&str>,
        class: Option<&Value>,
    ) -> Result<Ingested, DunkError> {
        self.require_page_role(self_pane_id)?;
        let body = body.map(python_str).unwrap_or_default();
        if body.trim().is_empty() {
            return Err(DunkError::Refused("an owner page needs a body".to_string()));
        }
        let external_id = external_id
            .filter(|value| value_truthy(value))
            .map(python_str)
            .filter(|id| !id.is_empty());
        // t3700: only a critical emergency page reaches the phone; any other page is recorded to the
        // owner mailbox on a transport the Telegram outbox does not read.
        let class = class.filter(|value| value_truthy(value)).map(python_str);
        let transport = owner_page_transport(class.as_deref());
        let store = self.inbox_store()?;
        if let Some(id) = external_id.as_deref() {
            if let Some(message) = store.find_by_external_id(OWNER_MAILBOX, Some(id))? {
                let transport = message
                    .summary
                    .transport
                    .clone()
                    .unwrap_or_else(|| OWNER_PAGE_MAILBOX_TRANSPORT.to_string());
                return Ok(Ingested {
                    message,
                    duplicate: true,
                    transport,
                });
            }
        }
        let outgoing = Outgoing {
            mailbox: OWNER_MAILBOX,
            body: Some(&body),
            subject: Some(OWNER_PAGE_SUBJECT),
            sender: Some(OWNER_PAGE_SENDER),
            mailbox_label: Some(OWNER_MAILBOX),
            pane_id: None,
            attachments: &[],
            transport: Some(transport),
            reply_to: None,
            external_id: external_id.as_deref(),
        };
        let message_id = store.send(&outgoing)?;
        let message = store.get_message(&message_id)?;
        self.flock.invalidate_view();
        Ok(Ingested {
            message,
            duplicate: false,
            transport: transport.to_string(),
        })
    }

    /// Answer an owner question once, then acknowledge that question.
    pub fn reply(
        &self,
        message_id: &str,
        body: Option<&Value>,
        self_pane_id: Option<&str>,
    ) -> Result<Reply, DunkError> {
        let store = self.authorised_store(self_pane_id)?;
        let body = body.map(python_str).unwrap_or_default();
        let sender = self_pane_id
            .filter(|pane| !pane.is_empty())
            .or(Some("owner-service"));
        let reply = store.answer_owner_question(message_id, &body, sender)?;
        let transport = reply.summary.transport.clone();
        Ok(Reply {
            reply,
            question: message_id.to_string(),
            transport,
            acked: message_id.to_string(),
        })
    }

    /// List undelivered owner replies for one transport.
    pub fn outbox(
        &self,
        transport: Option<&str>,
        limit: Option<i64>,
        self_pane_id: Option<&str>,
    ) -> Result<Outbox, DunkError> {
        let store = self.authorised_store(self_pane_id)?;
        let messages = store.owner_outbox(transport, limit)?;
        let count = messages.len();
        Ok(Outbox {
            transport: transport.map(str::to_string),
            messages,
            count,
        })
    }

    /// Record one transport delivery receipt and return the full stored message.
    pub fn delivered(
        &self,
        message_id: &str,
        transport: Option<&str>,
        external_id: Option<&str>,
        self_pane_id: Option<&str>,
    ) -> Result<Delivered, DunkError> {
        let store = self.authorised_store(self_pane_id)?;
        let message = store.mark_delivered(message_id, transport, external_id)?;
        Ok(Delivered {
            delivered_at: message.summary.delivered_at,
            message,
            transport: transport.map(str::to_string),
        })
    }
}

fn value_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn python_str(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::String(value) => value.clone(),
        Value::Number(value) => python_number_string(value),
        Value::Array(_) | Value::Object(_) => python_repr_value(value),
    }
}

fn python_repr_value(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::String(value) => python_repr(value),
        Value::Number(value) => python_number_string(value),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(python_repr_value)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| format!("{}: {}", python_repr(key), python_repr_value(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

pub(crate) fn python_number_string(value: &serde_json::Number) -> String {
    let raw = value.to_string();
    if raw
        .chars()
        .any(|character| matches!(character, '.' | 'e' | 'E'))
    {
        // serde_json's arbitrary_precision feature preserves the input token, while
        // Python json.loads first rounds it to an IEEE-754 float. Format that decoded
        // value so wire text follows Python even for rounded and infinite floats.
        if let Ok(decoded) = raw.parse::<f64>() {
            if !decoded.is_finite() {
                return crate::usage::py_float_repr(decoded);
            }
            if let Some(normalized) = serde_json::Number::from_f64(decoded) {
                return python_float_string(&normalized.to_string());
            }
        }
    }
    raw
}

fn python_float_string(text: &str) -> String {
    let (sign, unsigned) = text
        .strip_prefix('-')
        .map(|unsigned| ("-", unsigned))
        .unwrap_or(("", text));
    let (mantissa, explicit_exponent) = unsigned
        .split_once(['e', 'E'])
        .map(|(mantissa, exponent)| (mantissa, exponent.parse::<i32>().unwrap_or_default()))
        .unwrap_or((unsigned, 0));
    let (integer, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = format!("{integer}{fraction}");
    let Some(first_nonzero) = digits.bytes().position(|digit| digit != b'0') else {
        return format!("{sign}0.0");
    };
    let significant = digits[first_nonzero..].trim_end_matches('0');
    let exponent = explicit_exponent + integer.len() as i32 - first_nonzero as i32 - 1;

    if (-4..16).contains(&exponent) {
        let point = exponent + 1;
        if point <= 0 {
            format!("{sign}0.{}{}", "0".repeat((-point) as usize), significant)
        } else if point as usize >= significant.len() {
            format!(
                "{sign}{}{}.0",
                significant,
                "0".repeat(point as usize - significant.len())
            )
        } else {
            format!(
                "{sign}{}.{}",
                &significant[..point as usize],
                &significant[point as usize..]
            )
        }
    } else {
        let mantissa = if significant.len() == 1 {
            significant.to_string()
        } else {
            format!("{}.{}", &significant[..1], &significant[1..])
        };
        format!(
            "{sign}{mantissa}e{}{:02}",
            if exponent < 0 { '-' } else { '+' },
            exponent.abs()
        )
    }
}

fn python_repr(value: &str) -> String {
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut quoted = String::with_capacity(value.len() + 2);
    quoted.push(quote);
    for character in value.chars() {
        match character {
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\t' => quoted.push_str("\\t"),
            character if character == quote => {
                quoted.push('\\');
                quoted.push(character);
            }
            character => quoted.push(character),
        }
    }
    quoted.push(quote);
    quoted
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use crate::config::Env;
    use crate::flock::panes::Stores;
    use crate::roles::Roles;
    use crate::server::RequestCtx;
    use crate::terminal::{fakes::FakeTerminal, Terminal};

    use std::sync::Mutex;

    const DESK: &str = "w1:p1";
    const OWNER: &str = "w1:p2";
    const WORKER: &str = "w1:p3";

    struct NoPanes;

    impl PaneInfoEnv for NoPanes {
        fn list_panes_grouped(&self) -> Vec<Value> {
            Vec::new()
        }

        fn get_pane(&self, _: &str) -> Option<Value> {
            None
        }
    }

    struct TestEnv(Mutex<Box<dyn Terminal>>);

    impl Default for TestEnv {
        fn default() -> Self {
            Self(Mutex::new(Box::new(FakeTerminal::default())))
        }
    }

    impl crate::api::CommandEnv for TestEnv {
        fn terminal(&self) -> &Mutex<Box<dyn Terminal>> {
            &self.0
        }

        fn now_s(&self) -> f64 {
            crate::clock::now_s()
        }
    }

    impl PaneInfoEnv for TestEnv {
        fn list_panes_grouped(&self) -> Vec<Value> {
            Vec::new()
        }

        fn get_pane(&self, _: &str) -> Option<Value> {
            None
        }
    }

    struct TestStatus {
        version: std::sync::atomic::AtomicUsize,
        dunks: crate::flock::dunks::DunkList,
    }

    impl TestStatus {
        fn new() -> Self {
            Self {
                version: std::sync::atomic::AtomicUsize::new(1),
                dunks: crate::flock::dunks::DunkList::new(),
            }
        }
    }

    impl crate::flock::status::StatusEnv for TestStatus {
        fn version(&self) -> String {
            self.version
                .load(std::sync::atomic::Ordering::SeqCst)
                .to_string()
        }
        fn federated_input(&self) -> Value {
            Value::Null
        }
        fn code_status(&self) -> serde_json::Map<String, Value> {
            serde_json::Map::new()
        }
        fn dunks(&self) -> &crate::flock::dunks::DunkList {
            &self.dunks
        }
        fn started_at(&self) -> f64 {
            1.0
        }
        fn pid(&self) -> u32 {
            1
        }
        fn state_file(&self) -> Option<String> {
            None
        }
        fn inbox_count(&self, _: &str) -> Result<Value, String> {
            Ok(Value::Null)
        }
        fn now(&self) -> f64 {
            1_000.0
        }
        fn queued_messages(&self) -> usize {
            0
        }
        fn inbox_unread(&self) -> Result<i64, DunkError> {
            Ok(0)
        }
        fn task_totals(&self) -> Result<Value, DunkError> {
            Ok(Value::Null)
        }
        fn task_followers(&self) -> usize {
            0
        }
        fn quarantined_mailboxes(&self) -> Option<Value> {
            None
        }
        fn last_message(&self) -> String {
            String::new()
        }
        fn herdr_available(&self) -> bool {
            false
        }
        fn herdr_hint(&self) -> String {
            String::new()
        }
    }

    fn dispatch_owner_conversation(
        fixture: &Fixture,
        pane: Option<&str>,
        socket_peer: bool,
    ) -> Result<Value, DunkError> {
        fn handler(
            context: &crate::api::HandlerContext<'_>,
            _: crate::api::Args,
        ) -> Result<Value, DunkError> {
            let result = OwnerConversations::new(context.flock, context.env).conversation(
                None,
                None,
                context.self_pane_id(),
            )?;
            serde_json::to_value(result).map_err(DunkError::from)
        }

        let env = TestEnv::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("test runtime");
        let request = RequestCtx {
            self_pane_id: pane.map(str::to_string),
            socket_peer: Some(socket_peer),
            ..RequestCtx::default()
        };
        let context = crate::api::HandlerContext {
            flock: &fixture.flock,
            request: &request,
            env: &env,
            runtime: runtime.handle(),
        };
        let mut local_dispatcher = crate::api::Dispatcher::new();
        local_dispatcher.register("owner_conversation", handler);
        local_dispatcher.dispatch(
            &context,
            &crate::api::FlockGate(&context),
            "owner_conversation",
            None,
        )
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        flock: Flock,
        store: Arc<Store>,
    }

    impl Fixture {
        fn new(tag: &str, attach_store: bool) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let flock = Flock::new(
                dir.path().join(format!("{tag}-launches.json")),
                dir.path().join(format!("{tag}-lanes.json")),
                false,
            );
            let mut roles = Roles::new(
                &Env::default(),
                Some(dir.path().join(format!("{tag}-roles.json"))),
                Some(dir.path().join(format!("{tag}-tokens.json"))),
                None,
            )
            .unwrap();
            for (pane, role) in [(DESK, "desk"), (OWNER, "owner"), (WORKER, "worker")] {
                roles
                    .register(pane, role, None, None, None, None, None, None)
                    .unwrap();
            }
            flock.attach_roles(roles, None);
            let store = Arc::new(
                Store::open(
                    dir.path().join(format!("{tag}-inbox.db")),
                    dir.path().join(format!("{tag}-blobs")),
                )
                .unwrap(),
            );
            if attach_store {
                flock.attach_stores(Stores {
                    inbox: Some(store.clone()),
                    ..Stores::default()
                });
            }
            Self {
                _dir: dir,
                flock,
                store,
            }
        }

        fn service(&self) -> OwnerConversations<'_> {
            OwnerConversations::new(&self.flock, &NoPanes)
        }

        fn send(
            &self,
            mailbox: &str,
            body: &str,
            transport: Option<&str>,
            reply_to: Option<&str>,
        ) -> String {
            self.store
                .send(&Outgoing {
                    mailbox,
                    body: Some(body),
                    mailbox_label: Some(mailbox),
                    transport,
                    reply_to,
                    ..Outgoing::default()
                })
                .unwrap()
        }
    }

    fn error<T: std::fmt::Debug>(result: Result<T, DunkError>) -> String {
        result.unwrap_err().to_string()
    }

    #[test]
    fn concurrent_owner_answers_have_one_winner_and_keep_transport() {
        let fixture = Fixture::new("owner-concurrent-answer", true);
        let question = fixture.send(OWNER_MAILBOX, "question", Some("telegram"), None);
        let (first, second) = std::thread::scope(|scope| {
            let a = scope.spawn(|| {
                fixture
                    .service()
                    .reply(&question, Some(&Value::String("one".into())), None)
            });
            let b = scope.spawn(|| {
                fixture
                    .service()
                    .reply(&question, Some(&Value::String("two".into())), None)
            });
            (a.join().unwrap(), b.join().unwrap())
        });
        assert_ne!(first.is_ok(), second.is_ok());
        let answer = first.or(second).unwrap();
        assert_eq!(answer.transport.as_deref(), Some("telegram"));
        assert_eq!(
            answer.reply.summary.reply_to.as_deref(),
            Some(question.as_str())
        );
        assert_eq!(fixture.store.replies_to(&question).unwrap().len(), 1);
        assert_eq!(
            fixture.store.get_message(&question).unwrap().summary.state,
            "acked"
        );
    }

    #[test]
    fn independent_store_connections_cannot_answer_twice() {
        let fixture = Fixture::new("owner-concurrent-stores", true);
        let other = Store::open(
            fixture.store.path(),
            fixture._dir.path().join("other-blobs"),
        )
        .unwrap();
        let question = fixture.send(OWNER_MAILBOX, "question", Some("web"), None);
        let (first, second) = std::thread::scope(|scope| {
            let a = scope.spawn(|| fixture.store.answer_owner_question(&question, "one", None));
            let b = scope.spawn(|| other.answer_owner_question(&question, "two", None));
            (a.join().unwrap(), b.join().unwrap())
        });
        assert_ne!(first.is_ok(), second.is_ok());
        assert_eq!(fixture.store.replies_to(&question).unwrap().len(), 1);
    }

    #[test]
    fn invalid_legacy_transport_does_not_create_an_answer() {
        let fixture = Fixture::new("owner-invalid-transport", true);
        let question = fixture.send(OWNER_MAILBOX, "question", Some("web"), None);
        let db = rusqlite::Connection::open(fixture.store.path()).unwrap();
        db.execute(
            "UPDATE messages SET transport = ? WHERE id = ?",
            ["bad transport!", &question],
        )
        .unwrap();
        assert!(fixture
            .service()
            .reply(&question, Some(&Value::String("answer".into())), None)
            .is_err());
        assert!(fixture.store.replies_to(&question).unwrap().is_empty());
        assert_eq!(
            fixture.store.get_message(&question).unwrap().summary.state,
            "queued"
        );
    }

    #[test]
    fn get_message_is_full_read_only_and_preserves_missing_id_error() {
        let fixture = Fixture::new("owner-get-full", true);
        let attachment = fixture._dir.path().join("note.txt");
        std::fs::write(&attachment, b"fixture attachment").unwrap();
        let created = fixture
            .service()
            .ingest(
                OwnerIngest {
                    body: Some(&Value::String("question".into())),
                    transport: Some("web"),
                    external_id: Some("web:1"),
                    subject: Some("fixture"),
                    attachments: std::slice::from_ref(&attachment),
                },
                Some(DESK),
            )
            .unwrap();
        let id = created.message.summary.message_id.clone();
        let before = fixture.store.get_message(&id).unwrap();
        let again = fixture.store.get_message(&id).unwrap();
        assert_eq!("queued", again.summary.state);
        assert_eq!(None, again.read_at);
        assert_eq!(1, again.attachments.len());
        assert_eq!("note.txt", again.attachments[0].filename);
        assert_eq!(before, again);
        assert_eq!(
            "no message with id \"missing\"",
            fixture
                .store
                .get_message("missing")
                .unwrap_err()
                .to_string()
        );
    }

    #[test]
    fn role_check_precedes_store_lookup_and_no_pane_is_trusted_in_process() {
        let fixture = Fixture::new("owner-role-first", false);
        let service = fixture.service();
        assert_eq!(
            "the owner mailbox is the desk's and the owner's; role 'worker' may not read or answer it",
            error(service.conversation(Some(10), None, Some(WORKER)))
        );
        assert_eq!(
            "the owner inbox store is not attached",
            error(service.conversation(None, None, None))
        );

        let attached = Fixture::new("owner-trusted", true);
        assert_eq!(
            0,
            attached
                .service()
                .conversation(None, None, None)
                .unwrap()
                .count
        );
    }

    #[test]
    fn python_str_matches_float_exponents_in_scalars_and_nested_json() {
        let value: Value = serde_json::from_str("5.960464477539062e-7").unwrap();
        assert_eq!("5.960464477539062e-07", python_str(&value));

        let values: Value =
            serde_json::from_str("[5.960464477539062e-7,1e16,1e-5,0.0001,1.0]").unwrap();
        assert_eq!(
            "[5.960464477539062e-07, 1e+16, 1e-05, 0.0001, 1.0]",
            python_str(&values)
        );

        let object: Value = serde_json::from_str("{\"value\":5.960464477539062e-7}").unwrap();
        assert_eq!("{'value': 5.960464477539062e-07}", python_str(&object));
    }

    #[test]
    fn request_identity_isolated_by_dispatch_guard_gate_and_outer_anonymous_refusal() {
        let fixture = Fixture::new("owner-request-isolation", true);
        std::thread::scope(|scope| {
            let desk = scope.spawn(|| dispatch_owner_conversation(&fixture, Some(DESK), true));
            let worker = scope.spawn(|| dispatch_owner_conversation(&fixture, Some(WORKER), true));
            let anonymous = scope.spawn(|| dispatch_owner_conversation(&fixture, None, false));
            assert_eq!(0, desk.join().unwrap().unwrap()["count"]);
            assert!(error(worker.join().unwrap()).contains("not permitted for role worker"));
            assert!(error(anonymous.join().unwrap()).contains("not permitted for role anonymous"));
        });
    }

    #[test]
    fn conversation_preserves_raw_provenance_and_waiting_rules() {
        let fixture = Fixture::new("owner-conversation", true);
        let question = fixture.send(OWNER_MAILBOX, "question", Some("telegram"), None);
        let reply = fixture.send(OWNER_MAILBOX, "answer", Some("telegram"), Some(&question));
        let window = fixture
            .service()
            .conversation(Some(0), None, Some(DESK))
            .unwrap();
        assert_eq!(2, window.count);
        assert!(window.waiting.is_empty());
        assert_eq!(question, window.messages[0]["id"]);
        assert_eq!(reply, window.messages[1]["id"]);
        assert_eq!(
            "queued",
            fixture.store.get_message(&question).unwrap().summary.state
        );
        let question_row = window
            .messages
            .iter()
            .find(|row| row["id"] == question)
            .unwrap();
        let reply_row = window
            .messages
            .iter()
            .find(|row| row["id"] == reply)
            .unwrap();
        assert_eq!("telegram", question_row["transport"]);
        assert_eq!(Value::Null, question_row["sender"]);
        assert_eq!(question, reply_row["reply_to"]);
        assert_eq!(true, question_row["answered"]);

        let waiting_id = fixture.send(OWNER_MAILBOX, "unanswered", Some("web"), None);
        let waiting = fixture
            .service()
            .conversation(Some(-1), None, Some(DESK))
            .unwrap();
        assert_eq!(3, waiting.messages.len(), "SQLite LIMIT -1 means no limit");
        let all = fixture
            .service()
            .conversation(None, None, Some(DESK))
            .unwrap();
        assert_eq!(vec![waiting_id], all.waiting);
    }

    #[test]
    fn ingest_validates_transport_before_dedup_and_duplicate_skips_body_and_attachments() {
        let fixture = Fixture::new("owner-ingest", true);
        let first = fixture
            .service()
            .ingest(
                OwnerIngest {
                    body: Some(&Value::String("first".into())),
                    transport: Some("web"),
                    external_id: Some("web:duplicate"),
                    subject: None,
                    attachments: &[],
                },
                Some(DESK),
            )
            .unwrap();
        assert!(!first.duplicate);
        let invalid_transport = fixture.service().ingest(
            OwnerIngest {
                body: Some(&Value::Null),
                transport: Some("carrier-pigeon"),
                external_id: Some("web:duplicate"),
                subject: None,
                attachments: &[],
            },
            Some(DESK),
        );
        assert!(error(invalid_transport).contains("transport must be one of"));

        let missing_attachment = fixture._dir.path().join("not-created");
        let duplicate = fixture
            .service()
            .ingest(
                OwnerIngest {
                    body: Some(&Value::Null),
                    transport: Some("telegram"),
                    external_id: Some("web:duplicate"),
                    subject: None,
                    attachments: std::slice::from_ref(&missing_attachment),
                },
                Some(DESK),
            )
            .unwrap();
        assert!(duplicate.duplicate);
        assert_eq!("telegram", duplicate.transport);
        assert_eq!(
            first.message.summary.message_id,
            duplicate.message.summary.message_id
        );
        assert_eq!(
            1,
            fixture
                .store
                .conversation(OWNER_MAILBOX, None, None)
                .unwrap()
                .len()
        );
    }

    #[test]
    fn concurrent_ingest_keeps_unique_index_race_errors_visible() {
        let fixture = Fixture::new("owner-ingest-race", true);
        let connection = rusqlite::Connection::open(fixture.store.path()).unwrap();
        connection
            .execute_batch(
                "CREATE TRIGGER hold_owner_race_insert BEFORE INSERT ON messages \
                 WHEN NEW.mailbox = 'owner' AND NEW.external_id = 'web:race' \
                 BEGIN SELECT length(hex(randomblob(12000000))); END;",
            )
            .unwrap();
        drop(connection);

        const CALLERS: usize = 8;
        let barrier = Arc::new(std::sync::Barrier::new(CALLERS));
        let fixture_ref = &fixture;
        let results = std::thread::scope(|scope| {
            let callers = (0..CALLERS)
                .map(|_| {
                    let barrier = barrier.clone();
                    let fixture = fixture_ref;
                    scope.spawn(move || {
                        barrier.wait();
                        fixture.service().ingest(
                            OwnerIngest {
                                body: Some(&Value::String("racing question".into())),
                                transport: Some("web"),
                                external_id: Some("web:race"),
                                subject: None,
                                attachments: &[],
                            },
                            Some(DESK),
                        )
                    })
                })
                .collect::<Vec<_>>();
            callers
                .into_iter()
                .map(|caller| caller.join().unwrap())
                .collect::<Vec<_>>()
        });

        assert_eq!(
            1,
            results
                .iter()
                .filter(|result| result.as_ref().is_ok_and(|message| !message.duplicate))
                .count()
        );
        let failures = results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .collect::<Vec<_>>();
        assert!(
            !failures.is_empty(),
            "the unique-index race must have losers"
        );
        assert!(failures
            .iter()
            .all(|failure| failure.to_string().contains("UNIQUE constraint failed")));
        assert_eq!(
            1,
            fixture
                .store
                .conversation(OWNER_MAILBOX, None, None)
                .unwrap()
                .len()
        );
    }

    #[test]
    fn reply_checks_existing_answer_before_body_and_rejects_reply_rows() {
        let fixture = Fixture::new("owner-reply-order", true);
        let question = fixture.send(OWNER_MAILBOX, "question", Some("telegram"), None);
        let answered = fixture
            .service()
            .reply(&question, Some(&Value::String("answer".into())), Some(DESK))
            .unwrap();
        assert_eq!("telegram", answered.transport.as_deref().unwrap());
        assert_eq!(Some(DESK), answered.reply.summary.sender.as_deref());
        assert_eq!(
            Some(question.as_str()),
            answered.reply.summary.reply_to.as_deref()
        );
        assert_eq!(question, answered.acked);

        assert!(error(fixture.service().reply(
            &question,
            Some(&Value::String(" ".into())),
            Some(DESK)
        ))
        .contains("already answered by"));
        assert!(error(fixture.service().reply(
            &answered.reply.summary.message_id,
            Some(&Value::String("another".into())),
            Some(DESK),
        ))
        .contains("is itself a reply"));
        assert_eq!(
            "no message with id \"absent\"",
            error(fixture.service().reply("absent", None, Some(DESK)))
        );
    }

    #[test]
    fn failed_ack_rolls_back_the_reply() {
        let fixture = Fixture::new("owner-reply-ack-failure", true);
        let question = fixture.send(OWNER_MAILBOX, "question", Some("web"), None);
        let connection = rusqlite::Connection::open(fixture.store.path()).unwrap();
        connection
            .execute_batch(&format!(
                "CREATE TRIGGER reject_owner_ack BEFORE UPDATE OF state ON messages \
                 WHEN OLD.id = '{question}' AND NEW.state = 'acked' \
                 BEGIN SELECT RAISE(FAIL, 'fixture ack failure'); END;"
            ))
            .unwrap();
        let failed = fixture.service().reply(
            &question,
            Some(&Value::String("reply rolls back".into())),
            Some(DESK),
        );
        assert!(error(failed).contains("fixture ack failure"));
        assert!(fixture.store.replies_to(&question).unwrap().is_empty());
    }

    #[test]
    fn outbox_isolated_by_owner_transport_and_delivery_keeps_first_receipt() {
        let fixture = Fixture::new("owner-outbox", true);
        let question = fixture.send(OWNER_MAILBOX, "question", Some("telegram"), None);
        let reply = fixture
            .service()
            .reply(&question, Some(&Value::String("answer".into())), Some(DESK))
            .unwrap()
            .reply;
        let lane = fixture.send(
            "worker-lane",
            "not an owner reply",
            Some("telegram"),
            Some("lane-question"),
        );
        let telegram = fixture
            .service()
            .outbox(Some("telegram"), Some(0), Some(DESK))
            .unwrap();
        assert_eq!(0, telegram.count);
        let telegram = fixture
            .service()
            .outbox(Some("telegram"), Some(20), Some(DESK))
            .unwrap();
        assert_eq!(
            vec![reply.summary.message_id.clone()],
            telegram
                .messages
                .iter()
                .map(|m| m.summary.message_id.clone())
                .collect::<Vec<_>>()
        );
        assert!(!telegram
            .messages
            .iter()
            .any(|m| m.summary.message_id == lane));

        let first = fixture
            .service()
            .delivered(
                &reply.summary.message_id,
                Some("telegram"),
                Some("receipt-1"),
                Some(DESK),
            )
            .unwrap();
        let repeated = fixture
            .service()
            .delivered(
                &reply.summary.message_id,
                Some("telegram"),
                Some("receipt-2"),
                Some(DESK),
            )
            .unwrap();
        assert_eq!(
            Some("receipt-1"),
            repeated.message.summary.external_id.as_deref()
        );
        assert_eq!(first.delivered_at, repeated.delivered_at);
        assert!(fixture
            .service()
            .outbox(Some("telegram"), Some(20), Some(DESK))
            .unwrap()
            .messages
            .is_empty());
        assert!(error(
            fixture
                .service()
                .delivered("missing", None, None, Some(DESK))
        )
        .contains("a delivery receipt needs a transport"));
    }

    #[test]
    fn ingest_invalidates_the_view_only_after_a_fresh_success() {
        let fixture = Fixture::new("owner-ingest-view", true);
        let status = TestStatus::new();
        let view_version = || {
            fixture
                .flock
                .view_cache()
                .view(&status, None, false, 3600.0)["status"]["version"]
                .clone()
        };
        assert_eq!("1", view_version());
        let first = fixture
            .service()
            .ingest(
                OwnerIngest {
                    body: Some(&Value::String("question".into())),
                    transport: Some("web"),
                    external_id: Some("web:cached"),
                    subject: None,
                    attachments: &[],
                },
                Some(DESK),
            )
            .unwrap();
        status.version.store(2, std::sync::atomic::Ordering::SeqCst);
        assert_eq!("2", view_version());

        status.version.store(3, std::sync::atomic::Ordering::SeqCst);
        let duplicate = fixture
            .service()
            .ingest(
                OwnerIngest {
                    body: Some(&Value::Null),
                    transport: Some("web"),
                    external_id: Some("web:cached"),
                    subject: None,
                    attachments: &[],
                },
                Some(DESK),
            )
            .unwrap();
        assert!(duplicate.duplicate);
        assert_eq!(
            first.message.summary.message_id,
            duplicate.message.summary.message_id
        );
        assert_eq!("2", view_version());
    }
    #[test]
    fn owner_handoff_reconciliation_preserves_actual_reply_transport() {
        let fixture = Fixture::new("owner-handoff-recovery", true);
        for (transport, task) in [("web", "t1"), ("telegram", "t2")] {
            let question = fixture.send(OWNER_MAILBOX, "question", Some(transport), None);
            let notice = fixture
                .store
                .ensure_owner_handoff_notice(&question, "%1", task)
                .unwrap();
            let picked_up = fixture
                .store
                .read("codex-rust-owner", 10, false, true)
                .unwrap();
            assert_eq!(picked_up.len(), 1);
            assert_eq!(picked_up[0].summary.message_id, notice.notice_id);
            assert_eq!(picked_up[0].summary.state, "read");
            let answer = fixture
                .service()
                .reply(&question, Some(&Value::String("final".into())), Some(OWNER))
                .unwrap();
            assert_eq!(answer.transport.as_deref(), Some(transport));
            assert_eq!(
                answer.reply.summary.reply_to.as_deref(),
                Some(question.as_str())
            );
            assert!(fixture
                .store
                .reconcile_answered_owner_notice(&question)
                .unwrap());
            assert_eq!(
                fixture
                    .store
                    .get_message(&notice.notice_id)
                    .unwrap()
                    .summary
                    .state,
                "cancelled"
            );
            assert!(fixture
                .service()
                .reply(
                    &question,
                    Some(&Value::String("duplicate".into())),
                    Some(OWNER)
                )
                .is_err());
        }
    }

    #[test]
    fn owner_handoff_attention_allows_owner_to_answer_original_question() {
        let fixture = Fixture::new("owner-handoff-manual-recovery", true);
        let question = fixture.send(OWNER_MAILBOX, "question", Some("web"), None);
        fixture
            .store
            .ensure_owner_handoff_notice(&question, "%1", "t1")
            .unwrap();
        fixture
            .store
            .quarantine_existing_owner_handoff(&question)
            .unwrap();
        let attention = fixture
            .store
            .owner_handoff_attention(&question)
            .unwrap()
            .unwrap();
        assert_eq!(attention.0, "t1");
        assert_eq!(attention.1, "%1");
        let answer = fixture
            .service()
            .reply(&question, Some(&Value::String("final".into())), Some(OWNER))
            .unwrap();
        assert_eq!(answer.transport.as_deref(), Some("web"));
        assert_eq!(
            answer.reply.summary.reply_to.as_deref(),
            Some(question.as_str())
        );
        assert!(fixture
            .service()
            .reply(
                &question,
                Some(&Value::String("duplicate".into())),
                Some(OWNER)
            )
            .is_err());
    }
}

#[cfg(test)]
#[path = "conversation_mailboxes_tests.rs"]
mod conversation_mailboxes_tests;

/// t3592: the routine-mail fixtures (coordinator decision i49515).
#[cfg(test)]
#[path = "routine_mail_tests.rs"]
mod routine_mail_tests;

/// t3714: the owner conversation through `OwnerConversations::conversation` (the t3711 requirement).
#[cfg(test)]
#[path = "conversation_routine_tests.rs"]
mod conversation_routine_tests;
