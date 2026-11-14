//! Guard inputs and read audience metadata before a privileged body lookup.
use super::{map_sqlite, DunkError, ReadPool};
use rusqlite::OptionalExtension;
use serde_json::{json, Value};

/// Inbox IDs are canonical positive i64 counter values prefixed by `i`.
pub fn valid_message_id(id: &str) -> bool {
    let Some(digits) = id.strip_prefix('i') else {
        return false;
    };
    !digits.is_empty()
        && !digits.starts_with('0')
        && digits.len() <= 19
        && digits.bytes().all(|byte| byte.is_ascii_digit())
        && digits
            .parse::<u64>()
            .is_ok_and(|value| value <= i64::MAX as u64)
}

pub(crate) struct MessageViewMetadata {
    pub audience: Option<String>,
}

pub(crate) fn message_view_metadata(
    pool: &ReadPool,
    mailbox: &str,
    message_id: &str,
) -> Result<Option<MessageViewMetadata>, DunkError> {
    pool.with(|connection| {
        connection.query_row(
            "SELECT a.audience FROM messages m LEFT JOIN message_audiences a ON a.message_id=m.id \
             WHERE m.mailbox=? AND m.id=?",
            [mailbox,message_id],
            |row| Ok(MessageViewMetadata { audience:row.get(0)? }),
        ).optional().map_err(map_sqlite)
    })
}

// The journal supplies the timestamp. JSON escaping prevents log injection and keeps one line.
// No body/subject/sender/token argument exists in this audit interface.
pub(crate) fn body_view_audit(
    role: &str,
    pane: Option<&str>,
    mailbox: &str,
    id: &str,
    include_agent: bool,
    result: &str,
) -> String {
    json!({"event":"inbox_body_view","role":role,"pane":pane,"mailbox":mailbox,
        "message_id":id,"include_agent":include_agent,"result":result})
    .to_string()
}

// Refusals can precede input validation: keep only bounded string metadata, never raw values.
pub(crate) fn body_view_refusal_audit(
    role: &str,
    pane: Option<&str>,
    mailbox: Option<&Value>,
    id: Option<&Value>,
    include_agent: Option<&Value>,
) -> String {
    let mailbox = mailbox
        .and_then(Value::as_str)
        .filter(|key| {
            !key.trim().is_empty() && key.len() <= 1024 && !key.chars().any(char::is_control)
        })
        .unwrap_or("<invalid>");
    let id = id
        .and_then(Value::as_str)
        .filter(|id| valid_message_id(id))
        .unwrap_or("<invalid>");
    let pane = pane.filter(|pane| pane.len() <= 128 && !pane.chars().any(char::is_control));
    body_view_audit(
        role,
        pane,
        mailbox,
        id,
        include_agent == Some(&Value::Bool(true)),
        "refused",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inbox_body_view_ids_and_audit_are_strict_and_body_free() {
        for id in ["i1", "i42", "i9223372036854775807"] {
            assert!(valid_message_id(id));
        }
        for id in [
            "",
            "1",
            "i0",
            "i01",
            "i-1",
            "i1\n",
            "i١",
            "i9223372036854775808",
            "i1 OR 1=1",
        ] {
            assert!(!valid_message_id(id));
        }
        let line = body_view_audit("web", None, "mail\"box\nname", "i42", true, "ok");
        assert!(!line.contains('\n'));
        let audit: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(audit["event"], "inbox_body_view");
        assert_eq!(audit["role"], "web");
        assert_eq!(audit["message_id"], "i42");
        let raw = json!({"body":"MUST_NOT_LOG"});
        let refusal = body_view_refusal_audit(
            "worker",
            Some("w1:p1"),
            Some(&raw),
            Some(&json!("i1\nMUST_NOT_LOG")),
            Some(&raw),
        );
        assert!(!refusal.contains("MUST_NOT_LOG") && !refusal.contains('\n'));
        let refused: Value = serde_json::from_str(&refusal).unwrap();
        assert_eq!(refused["result"], "refused");
        assert_eq!(refused["message_id"], "<invalid>");
        assert!(
            audit.get("body").is_none()
                && audit.get("subject").is_none()
                && audit.get("token").is_none()
        );
    }
}
