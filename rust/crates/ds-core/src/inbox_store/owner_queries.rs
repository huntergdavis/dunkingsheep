//! Owner conversation reads and transport delivery receipts.
//!
//! These APIs port the five store methods in dunk_inbox.py without adding owner-service
//! orchestration or changing the inbox schema.

use rusqlite::types::{Value as SqlValue, ValueRef};
use rusqlite::{params, params_from_iter, OptionalExtension, Row};
use serde_json::{json, Map, Value};

use crate::clock::now_s;
use crate::db::map_sqlite;
use crate::error::DunkError;
use crate::flock::mailbox::OWNER_MAILBOX;
use crate::inbox::{mailbox_where, validate_transport};

use super::{attachments_of, full_from_row, py_repr, refuse, Message, Store, FULL_COLUMNS};

fn required_transport(transport: Option<&str>, message: &str) -> Result<String, DunkError> {
    validate_transport(transport)
        .map_err(refuse)?
        .ok_or_else(|| refuse(message))
}

fn raw_value(value: ValueRef<'_>) -> Value {
    match value {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(number) => json!(number),
        ValueRef::Real(number) => json!(number),
        ValueRef::Text(bytes) => Value::String(String::from_utf8_lossy(bytes).into_owned()),
        ValueRef::Blob(bytes) => Value::Array(bytes.iter().map(|byte| json!(byte)).collect()),
    }
}

fn raw_row(row: &Row<'_>) -> rusqlite::Result<Value> {
    let statement = row.as_ref();
    let mut object = Map::new();
    for index in 0..statement.column_count() {
        let name = statement.column_name(index)?.to_string();
        object.insert(name, raw_value(row.get_ref(index)?));
    }
    Ok(Value::Object(object))
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn full_messages(
    connection: &rusqlite::Connection,
    sql: &str,
    values: &[SqlValue],
) -> Result<Vec<Message>, DunkError> {
    let mut messages = {
        let mut statement = connection.prepare(sql).map_err(map_sqlite)?;
        let rows = statement
            .query_map(params_from_iter(values.iter()), |row| {
                full_from_row(row, now_s())
            })
            .map_err(map_sqlite)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite)?
    };
    for message in &mut messages {
        message.attachments = attachments_of(connection, &message.summary.message_id)?;
    }
    Ok(messages)
}

fn full_message(
    connection: &rusqlite::Connection,
    message_id: &str,
) -> Result<Option<Message>, DunkError> {
    let values = [SqlValue::Text(message_id.to_string())];
    let mut messages = full_messages(
        connection,
        &format!("SELECT {FULL_COLUMNS} FROM messages WHERE id = ?"),
        &values,
    )?;
    Ok(if messages.is_empty() {
        None
    } else {
        Some(messages.remove(0))
    })
}

impl Store {
    /// Return every raw message column that replies to this id, oldest first.
    pub fn replies_to(&self, message_id: &str) -> Result<Vec<Value>, DunkError> {
        let message_id = message_id.to_string();
        self.readers.with(move |connection| {
            let mut statement = connection
                .prepare("SELECT * FROM messages WHERE reply_to = ? ORDER BY created_at")
                .map_err(map_sqlite)?;
            let rows = statement
                .query_map([message_id], raw_row)
                .map_err(map_sqlite)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite)
        })
    }

    /// Return the mailbox conversation as raw rows, oldest first, with answered flags.
    pub fn conversation(
        &self,
        mailbox: &str,
        limit: Option<i64>,
        before: Option<f64>,
    ) -> Result<Vec<Value>, DunkError> {
        self.conversation_across(&[mailbox.to_string()], limit, before)
    }

    /// One conversation window over several mailboxes (t3535: the owner mailbox and the owner
    /// lane's own mailbox name), oldest first, with answered flags.
    pub fn conversation_across(
        &self,
        mailboxes: &[String],
        limit: Option<i64>,
        before: Option<f64>,
    ) -> Result<Vec<Value>, DunkError> {
        self.conversation_rows(mailboxes, limit, before, false)
    }

    /// The owner conversation window (t3592, repair for t3711/t3712): as `conversation_across`, but routine
    /// lane mail is excluded in the query. A row is routine when its subject starts with a routine prefix
    /// (ignoring leading whitespace) and it is not an owner reply (`reply_to`). Excluding it here, before
    /// the limit, means routine rows cannot use up the window and hide older decisions.
    pub fn conversation_across_visible(
        &self,
        mailboxes: &[String],
        limit: Option<i64>,
        before: Option<f64>,
    ) -> Result<Vec<Value>, DunkError> {
        self.conversation_rows(mailboxes, limit, before, true)
    }

    fn conversation_rows(
        &self,
        mailboxes: &[String],
        limit: Option<i64>,
        before: Option<f64>,
        exclude_routine: bool,
    ) -> Result<Vec<Value>, DunkError> {
        let mut conditions = Vec::with_capacity(mailboxes.len());
        let mut box_params: Vec<String> = Vec::new();
        for mailbox in mailboxes {
            let (condition, params) = mailbox_where(mailbox, "mailbox");
            conditions.push(condition);
            box_params.extend(params);
        }
        let condition = if conditions.is_empty() {
            "0".to_string()
        } else {
            format!("({})", conditions.join(" OR "))
        };
        let mut sql = format!("SELECT * FROM messages WHERE {condition} AND state != 'cancelled'");
        let mut values: Vec<SqlValue> = box_params.into_iter().map(SqlValue::Text).collect();
        if exclude_routine {
            // Same rule as `is_routine_mail`: the subject, after leading whitespace, starts with a routine
            // prefix, and the row is not an owner reply. The parameters bind in clause order.
            let prefix_tests: Vec<&str> = crate::owner::conversations::ROUTINE_SUBJECT_PREFIXES
                .iter()
                .map(|_| "instr(ltrim(COALESCE(subject, ''), ' ' || char(9) || char(10) || char(13)), ?) = 1")
                .collect();
            sql.push_str(&format!(
                " AND NOT (({}) AND (reply_to IS NULL OR reply_to = ''))",
                prefix_tests.join(" OR ")
            ));
            values.extend(
                crate::owner::conversations::ROUTINE_SUBJECT_PREFIXES
                    .iter()
                    .map(|prefix| SqlValue::Text((*prefix).to_string())),
            );
            // The sender explicitly chose agent-only delivery. Do not hide a human mailbox
            // row, even if its subject happens to contain the marker. Apply before LIMIT.
            let (owner_mailbox, owner_params) = mailbox_where(OWNER_MAILBOX, "mailbox");
            sql.push_str(&format!(
                " AND ({owner_mailbox} OR instr(ltrim(COALESCE(subject, ''), ' ' || char(9) || char(10) || char(13)), ?) != 1)"
            ));
            values.extend(owner_params.into_iter().map(SqlValue::Text));
            values.push(SqlValue::Text(
                crate::owner::conversations::AGENT_COORDINATION_PREFIX.into(),
            ));
        }
        if let Some(before) = before {
            sql.push_str(" AND created_at < ?");
            values.push(SqlValue::Real(before));
        }
        sql.push_str(" ORDER BY created_at DESC LIMIT ?");
        values.push(SqlValue::Integer(limit.unwrap_or(50)));

        self.readers.with(move |connection| {
            let mut selected = {
                let mut statement = connection.prepare(&sql).map_err(map_sqlite)?;
                let rows = statement
                    .query_map(params_from_iter(values.iter()), raw_row)
                    .map_err(map_sqlite)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite)?
            };
            let answered_ids: Vec<Value> = selected
                .iter()
                .filter_map(|row| row.get("reply_to"))
                .filter(|reply_to| truthy(reply_to))
                .cloned()
                .collect();
            for row in selected.iter_mut().rev() {
                let answered = row.get("id").is_some_and(|id| answered_ids.contains(id))
                    || row.get("acked_at").is_some_and(truthy);
                let Some(object) = row.as_object_mut() else {
                    continue;
                };
                object.insert("answered".to_string(), Value::Bool(answered));
            }
            Ok(selected.into_iter().rev().collect())
        })
    }

    /// Find a full message by its transport id in exactly the requested mailbox.
    pub fn find_by_external_id(
        &self,
        mailbox: &str,
        external_id: Option<&str>,
    ) -> Result<Option<Message>, DunkError> {
        let Some(external_id) = external_id.filter(|id| !id.is_empty()) else {
            return Ok(None);
        };
        let values = [
            SqlValue::Text(mailbox.to_string()),
            SqlValue::Text(external_id.to_string()),
        ];
        self.readers.with(|connection| {
            let mut messages = full_messages(
                connection,
                &format!(
                    "SELECT {FULL_COLUMNS} FROM messages WHERE mailbox = ? AND external_id = ?"
                ),
                &values,
            )?;
            Ok(if messages.is_empty() {
                None
            } else {
                Some(messages.remove(0))
            })
        })
    }

    /// The highest owner page the telegram transport has delivered, by its mailbox number (t665).
    /// Only delivered owner-page rows count, so a page that is still waiting never confirms.
    pub fn owner_page_delivered_through(&self) -> Result<Option<i64>, DunkError> {
        let values = [
            SqlValue::Text(OWNER_MAILBOX.to_string()),
            SqlValue::Text("telegram".to_string()),
        ];
        let pages = self.readers.with(|connection| {
            full_messages(
                connection,
                &format!(
                    "SELECT {FULL_COLUMNS} FROM messages WHERE mailbox = ? AND transport = ? \
                     AND sender = 'owner-page' AND delivered_at IS NOT NULL"
                ),
                &values,
            )
        })?;
        Ok(pages
            .iter()
            .filter_map(|page| {
                crate::owner::conversations::owner_page_number(&page.summary.message_id)
            })
            .max())
    }

    /// Owner-mailbox rows waiting for delivery by this transport: replies (reply_to set) and
    /// owner pages (sender `owner-page`, t665). Nothing else, so a human question or a web
    /// echo never reaches the phone.
    pub fn owner_outbox(
        &self,
        transport: Option<&str>,
        limit: Option<i64>,
    ) -> Result<Vec<Message>, DunkError> {
        let transport = required_transport(transport, "an outbox needs a transport")?;
        let values = [
            SqlValue::Text(OWNER_MAILBOX.to_string()),
            SqlValue::Text(transport),
            SqlValue::Integer(limit.unwrap_or(20)),
        ];
        self.readers.with(|connection| {
            full_messages(
                connection,
                &format!(
                    "SELECT {FULL_COLUMNS} FROM messages WHERE mailbox = ? AND transport = ? \
                     AND (reply_to IS NOT NULL OR sender = 'owner-page') AND delivered_at IS NULL \
                     AND cancelled_at IS NULL ORDER BY created_at, id LIMIT ?"
                ),
                &values,
            )
        })
    }

    /// Record a transport receipt atomically and return the full delivered message.
    pub fn mark_delivered(
        &self,
        message_id: &str,
        transport: Option<&str>,
        external_id: Option<&str>,
    ) -> Result<Message, DunkError> {
        let transport = required_transport(transport, "a delivery receipt needs a transport")?;
        let message_id = message_id.to_string();
        let external_id = external_id.filter(|id| !id.is_empty()).map(str::to_string);
        let receipt_transport = transport.clone();
        let returned_id = message_id.clone();
        self.writer.call(move |connection| {
            let transaction = connection.transaction().map_err(map_sqlite)?;
            let receipt: Option<(String, Option<String>, Option<f64>)> = transaction
                .query_row(
                    "SELECT mailbox, transport, delivered_at FROM messages WHERE id = ?",
                    [&message_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .map_err(map_sqlite)?;
            let Some((mailbox, actual_transport, delivered_at)) = receipt else {
                return Err(refuse(format!(
                    "no message with id {}",
                    py_repr(&message_id)
                )));
            };
            if mailbox != OWNER_MAILBOX {
                return Err(refuse(format!(
                    "{message_id} is not an {OWNER_MAILBOX} message"
                )));
            }
            if actual_transport.as_deref() != Some(receipt_transport.as_str()) {
                let actual = actual_transport
                    .as_deref()
                    .map(py_repr)
                    .unwrap_or_else(|| "None".to_string());
                return Err(refuse(format!(
                    "{message_id} belongs to transport {actual}, not {receipt_transport}"
                )));
            }
            if delivered_at.is_none() {
                transaction
                    .execute(
                        "UPDATE messages SET delivered_at = ?, delivery_transport = ?, \
                         external_id = COALESCE(external_id, ?) WHERE id = ?",
                        params![now_s(), receipt_transport, external_id, message_id],
                    )
                    .map_err(map_sqlite)?;
            }
            transaction.commit().map_err(map_sqlite)
        })?;
        self.readers
            .with(|connection| full_message(connection, &returned_id))?
            .ok_or_else(|| refuse(format!("no message with id {}", py_repr(&returned_id))))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rusqlite::types::Value as SqlValue;

    use super::*;
    use crate::inbox_store::Outgoing;

    // t665 owner pages: the outbox and receipt contract (kept out of this shared file's body).
    mod page_tests;

    // t3592 repair (t3711/t3712): routine mail is excluded in the query, before the limit and the boundary.
    mod routine_window_tests;

    struct Fixture {
        dir: tempfile::TempDir,
        store: Store,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("temporary inbox directory");
            let store = Store::open(dir.path().join("inbox.db"), dir.path().join("blobs"))
                .expect("temporary inbox store");
            Self { dir, store }
        }

        fn send(&self, mailbox: &str, body: &str) -> String {
            self.store
                .send(&Outgoing {
                    mailbox,
                    body: Some(body),
                    sender: Some("fixture"),
                    ..Default::default()
                })
                .expect("send fixture message")
        }

        fn send_outgoing(&self, outgoing: &Outgoing<'_>) -> String {
            self.store.send(outgoing).expect("send fixture message")
        }

        fn execute(&self, sql: &'static str, values: Vec<SqlValue>) {
            self.store
                .writer
                .call(move |connection| {
                    connection
                        .execute(sql, params_from_iter(values.iter()))
                        .map(|_| ())
                        .map_err(map_sqlite)
                })
                .expect("seed temporary inbox row");
        }

        fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.dir.path().join(name);
            std::fs::write(&path, bytes).expect("write temporary attachment");
            path
        }
    }

    fn text(value: &str) -> SqlValue {
        SqlValue::Text(value.to_string())
    }

    fn real(value: f64) -> SqlValue {
        SqlValue::Real(value)
    }

    #[test]
    fn conversation_uses_selected_window_raw_rows_and_mailbox_aliases() {
        let fixture = Fixture::new();
        let question = fixture.send("%9 (legacy)", "question");
        let reply = fixture.send("%9", "old reply");
        let acked = fixture.send("%9", "acked question");
        let cancelled = fixture.send("%9", "cancelled");
        fixture.execute(
            "UPDATE messages SET created_at = ?1 WHERE id = ?2",
            vec![real(20.0), text(&question)],
        );
        fixture.execute(
            "UPDATE messages SET created_at = ?1, reply_to = ?2, state = 'read', \
             cancelled_at = ?3 WHERE id = ?4",
            vec![real(10.0), text(&question), real(1.0), text(&reply)],
        );
        fixture.execute(
            "UPDATE messages SET created_at = ?1, acked_at = ?2 WHERE id = ?3",
            vec![real(30.0), real(2.0), text(&acked)],
        );
        fixture.execute(
            "UPDATE messages SET created_at = ?1, state = 'cancelled', \
             cancelled_at = ?2 WHERE id = ?3",
            vec![real(40.0), real(3.0), text(&cancelled)],
        );

        let window = fixture
            .store
            .conversation("%9", Some(2), Some(40.0))
            .expect("bounded conversation");
        assert_eq!(2, window.len());
        assert_eq!(question, window[0]["id"]);
        assert_eq!(Some(false), window[0]["answered"].as_bool());
        assert!(window[0].get("acked_at").is_some());
        assert!(window[0].get("pane_id").is_some());
        assert_eq!(acked, window[1]["id"]);
        assert_eq!(Some(true), window[1]["answered"].as_bool());

        let strict_before = fixture
            .store
            .conversation("%9", Some(5), Some(20.0))
            .expect("strict before");
        assert_eq!(1, strict_before.len());
        assert_eq!(reply, strict_before[0]["id"]);
        assert_eq!(
            Vec::<Value>::new(),
            fixture.store.conversation("%9", Some(0), None).unwrap()
        );
        assert_eq!(
            3,
            fixture
                .store
                .conversation("%9", Some(-1), None)
                .unwrap()
                .len()
        );
    }

    #[test]
    fn replies_to_returns_raw_rows_oldest_first_without_state_filtering() {
        let fixture = Fixture::new();
        let question = fixture.send("owner", "question");
        let later = fixture.send("owner", "later reply");
        let earlier = fixture.send("owner", "earlier cancelled reply");
        let unrelated = fixture.send("owner", "unrelated");
        fixture.execute(
            "UPDATE messages SET reply_to = ?1, created_at = ?2, state = 'cancelled', \
             cancelled_at = ?3 WHERE id = ?4",
            vec![text(&question), real(3.0), real(4.0), text(&earlier)],
        );
        fixture.execute(
            "UPDATE messages SET reply_to = ?1, created_at = ?2 WHERE id = ?3",
            vec![text(&question), real(5.0), text(&later)],
        );
        fixture.execute(
            "UPDATE messages SET reply_to = ?1 WHERE id = ?2",
            vec![text("another question"), text(&unrelated)],
        );

        let rows = fixture.store.replies_to(&question).expect("reply rows");
        assert_eq!(2, rows.len());
        assert_eq!(earlier, rows[0]["id"]);
        assert_eq!(later, rows[1]["id"]);
        assert_eq!("cancelled", rows[0]["state"]);
        assert!(rows[0].get("cancelled_at").is_some());
        assert!(rows[0].get("notify_state").is_some());
    }

    #[test]
    fn external_lookup_returns_full_message_and_attachments() {
        let fixture = Fixture::new();
        let attachment = fixture.file("evidence.txt", b"fixture attachment");
        let id = fixture.send_outgoing(&Outgoing {
            mailbox: "owner",
            body: Some("reply body"),
            sender: Some("owner"),
            attachments: std::slice::from_ref(&attachment),
            transport: Some("telegram"),
            reply_to: Some("question-1"),
            external_id: Some("external-7"),
            ..Default::default()
        });

        let found = fixture
            .store
            .find_by_external_id("owner", Some("external-7"))
            .expect("lookup")
            .expect("message exists");
        assert_eq!(id, found.summary.message_id);
        assert_eq!("reply body", found.body);
        assert_eq!(1, found.attachments.len());
        assert_eq!("evidence.txt", found.attachments[0].filename);
        assert_eq!(18, found.attachments[0].size);
        assert!(fixture
            .store
            .find_by_external_id("owner", Some(""))
            .expect("empty id")
            .is_none());
        assert!(fixture
            .store
            .find_by_external_id("owner", None)
            .expect("missing id")
            .is_none());
        assert!(fixture
            .store
            .find_by_external_id("wrong-mailbox", Some("external-7"))
            .expect("mailbox is exact")
            .is_none());
        assert!(fixture
            .store
            .find_by_external_id("owner", Some("missing"))
            .expect("missing row")
            .is_none());
    }

    #[test]
    fn owner_outbox_filters_transport_receipts_and_cancelled_rows() {
        let fixture = Fixture::new();
        let first = fixture.send_outgoing(&Outgoing {
            mailbox: "owner",
            body: Some("first reply"),
            transport: Some("telegram"),
            reply_to: Some("q1"),
            ..Default::default()
        });
        let second = fixture.send_outgoing(&Outgoing {
            mailbox: "owner",
            body: Some("second reply"),
            transport: Some("telegram"),
            reply_to: Some("q2"),
            ..Default::default()
        });
        let question = fixture.send_outgoing(&Outgoing {
            mailbox: "owner",
            body: Some("inbound question"),
            transport: Some("telegram"),
            ..Default::default()
        });
        let web_reply = fixture.send_outgoing(&Outgoing {
            mailbox: "owner",
            body: Some("other transport"),
            transport: Some("web"),
            reply_to: Some("q3"),
            ..Default::default()
        });
        let lane_reply = fixture.send_outgoing(&Outgoing {
            mailbox: "lane",
            body: Some("lane mail"),
            transport: Some("telegram"),
            reply_to: Some("q4"),
            ..Default::default()
        });
        let delivered = fixture.send_outgoing(&Outgoing {
            mailbox: "owner",
            body: Some("already delivered"),
            transport: Some("telegram"),
            reply_to: Some("q5"),
            ..Default::default()
        });
        let cancelled = fixture.send_outgoing(&Outgoing {
            mailbox: "owner",
            body: Some("cancelled"),
            transport: Some("telegram"),
            reply_to: Some("q6"),
            ..Default::default()
        });
        fixture.execute(
            "UPDATE messages SET created_at = 20 WHERE id = ?1",
            vec![text(&first)],
        );
        fixture.execute(
            "UPDATE messages SET created_at = 10 WHERE id = ?1",
            vec![text(&second)],
        );
        fixture.execute(
            "UPDATE messages SET delivered_at = 7 WHERE id = ?1",
            vec![text(&delivered)],
        );
        fixture.execute(
            "UPDATE messages SET cancelled_at = 8 WHERE id = ?1",
            vec![text(&cancelled)],
        );

        let outbox = fixture
            .store
            .owner_outbox(Some(" TELEGRAM "), None)
            .expect("telegram outbox");
        assert_eq!(
            vec![second.clone(), first.clone()],
            outbox
                .iter()
                .map(|m| m.summary.message_id.clone())
                .collect::<Vec<_>>()
        );
        assert!(outbox.iter().all(|m| m.summary.mailbox == OWNER_MAILBOX));
        assert!(!outbox.iter().any(|m| {
            [&question, &web_reply, &lane_reply, &delivered, &cancelled]
                .contains(&&m.summary.message_id)
        }));
        assert_eq!(
            vec![second],
            fixture
                .store
                .owner_outbox(Some("telegram"), Some(1))
                .unwrap()
                .iter()
                .map(|m| m.summary.message_id.clone())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            "an outbox needs a transport",
            fixture
                .store
                .owner_outbox(None, None)
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "an outbox needs a transport",
            fixture
                .store
                .owner_outbox(Some(""), None)
                .unwrap_err()
                .to_string()
        );
    }

    #[test]
    fn delivery_receipts_validate_in_order_and_keep_the_first_receipt() {
        let fixture = Fixture::new();
        let valid = fixture.send_outgoing(&Outgoing {
            mailbox: "owner",
            body: Some("reply"),
            transport: Some("telegram"),
            reply_to: Some("question"),
            ..Default::default()
        });
        let lane = fixture.send_outgoing(&Outgoing {
            mailbox: "lane",
            body: Some("lane reply"),
            transport: Some("telegram"),
            reply_to: Some("question"),
            ..Default::default()
        });
        let wrong_transport = fixture.send_outgoing(&Outgoing {
            mailbox: "owner",
            body: Some("web reply"),
            transport: Some("web"),
            reply_to: Some("question"),
            ..Default::default()
        });
        assert_eq!(
            "a delivery receipt needs a transport",
            fixture
                .store
                .mark_delivered(&valid, None, None)
                .unwrap_err()
                .to_string()
        );
        assert!(fixture
            .store
            .mark_delivered(&valid, Some("fax"), None)
            .unwrap_err()
            .to_string()
            .starts_with("transport must be one of "));
        assert_eq!(
            "no message with id 'missing'",
            fixture
                .store
                .mark_delivered("missing", Some("telegram"), None)
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            format!("{lane} is not an owner message"),
            fixture
                .store
                .mark_delivered(&lane, Some("telegram"), None)
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            format!("{wrong_transport} belongs to transport 'web', not telegram"),
            fixture
                .store
                .mark_delivered(&wrong_transport, Some("telegram"), None)
                .unwrap_err()
                .to_string()
        );

        let first = fixture
            .store
            .mark_delivered(&valid, Some(" TELEGRAM "), Some("receipt-1"))
            .expect("first receipt");
        let second = fixture
            .store
            .mark_delivered(&valid, Some("telegram"), Some("receipt-2"))
            .expect("repeat receipt");
        assert!(first.summary.delivered_at.is_some());
        assert_eq!(first.summary.delivered_at, second.summary.delivered_at);
        assert_eq!(Some("receipt-1".to_string()), second.summary.external_id);
        assert_eq!(
            Some("telegram".to_string()),
            second.summary.delivery_transport
        );
    }

    #[test]
    fn owner_query_sql_errors_propagate() {
        let fixture = Fixture::new();
        fixture
            .store
            .writer
            .call(|connection| {
                connection
                    .execute_batch("DROP TABLE messages")
                    .map_err(map_sqlite)
            })
            .expect("drop temporary table");
        assert!(fixture.store.replies_to("q").is_err());
        assert!(fixture.store.conversation("owner", None, None).is_err());
    }
}
