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

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Conversation {
    pub mailbox: String,
    pub messages: Vec<Value>,
    pub count: usize,
    pub waiting: Vec<String>,
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
        self.flock
            .stores()
            .inbox
            .ok_or_else(|| DunkError::Refused("the owner inbox store is not attached".to_string()))
    }

    /// Read one owner conversation window without consuming or acknowledging rows.
    pub fn conversation(
        &self,
        limit: Option<i64>,
        before: Option<f64>,
        self_pane_id: Option<&str>,
    ) -> Result<Conversation, DunkError> {
        let store = self.authorised_store(self_pane_id)?;
        let limit = limit.filter(|limit| *limit != 0).unwrap_or(50);
        let mut messages = store.conversation(OWNER_MAILBOX, Some(limit), before)?;
        let waiting = messages
            .iter()
            .filter(|row| {
                row.get("state").and_then(Value::as_str) == Some("queued")
                    && row.get("answered").and_then(Value::as_bool) != Some(true)
                    && !row.get("reply_to").is_some_and(value_truthy)
            })
            .filter_map(|row| row.get("id").and_then(Value::as_str).map(str::to_string))
            .collect();
        let count = messages.len();
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

    /// Answer an owner question once, then acknowledge that question.
    pub fn reply(
        &self,
        message_id: &str,
        body: Option<&Value>,
        self_pane_id: Option<&str>,
    ) -> Result<Reply, DunkError> {
        let store = self.authorised_store(self_pane_id)?;
        let question = store.get_message(message_id)?;
        if normalise_mailbox(&question.summary.mailbox) != OWNER_MAILBOX {
            return Err(DunkError::Refused(format!(
                "{message_id} is not an {OWNER_MAILBOX} message (mailbox {})",
                python_repr(&question.summary.mailbox)
            )));
        }
        if question
            .summary
            .reply_to
            .as_deref()
            .is_some_and(|reply_to| !reply_to.is_empty())
        {
            return Err(DunkError::Refused(format!(
                "{message_id} is itself a reply; answer the question it belongs to"
            )));
        }
        let replies = store.replies_to(&question.summary.message_id)?;
        if let Some(first) = replies.first() {
            let answered_by = first.get("id").and_then(Value::as_str).unwrap_or("None");
            return Err(DunkError::Refused(format!(
                "{message_id} was already answered by {answered_by}; one answer per question, so the latency metric has one edge"
            )));
        }
        let body = body.map(python_str).unwrap_or_default();
        if body.trim().is_empty() {
            return Err(DunkError::Refused("a reply needs a body".to_string()));
        }
        let transport = question.summary.transport.clone();
        let sender = self_pane_id.filter(|pane| !pane.is_empty());
        let outgoing = Outgoing {
            mailbox: OWNER_MAILBOX,
            body: Some(&body),
            sender,
            mailbox_label: Some(OWNER_MAILBOX),
            transport: transport.as_deref(),
            reply_to: Some(&question.summary.message_id),
            ..Outgoing::default()
        };
        let reply_id = store.send(&outgoing)?;
        let reply = store.get_message(&reply_id)?;
        store.ack(
            std::slice::from_ref(&question.summary.message_id),
            Some(OWNER_MAILBOX),
        )?;
        Ok(Reply {
            reply,
            question: message_id.to_string(),
            transport,
            acked: question.summary.message_id,
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
        Value::Number(value) => value.to_string(),
        Value::Array(_) | Value::Object(_) => python_repr_value(value),
    }
}

fn python_repr_value(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::String(value) => python_repr(value),
        Value::Number(value) => value.to_string(),
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
        assert!(error(invalid_transport).contains("unsupported transport"));

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
    fn failed_ack_leaves_the_already_created_reply_row() {
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
            Some(&Value::String("reply persists".into())),
            Some(DESK),
        );
        assert!(error(failed).contains("fixture ack failure"));
        assert_eq!(1, fixture.store.replies_to(&question).unwrap().len());
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
}
