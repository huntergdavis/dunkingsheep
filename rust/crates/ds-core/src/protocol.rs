//! The wire protocol: newline-delimited JSON over a unix socket.
//!
//! Ported from `dunk_server.py`'s protocol doc (RUST_PORT.md 4.1). The envelope is
//! a compatibility contract: clients and tests match on the strings and on the
//! presence rules - `retryable` appears ONLY when true, `id` is echoed even when the
//! request was unparseable (as null), and `shutdown` ends the loop after its reply.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::error::DunkError;

/// One request line. `self_pane_id` and `service_token` are optional; the peer pid
/// comes from `SO_PEERCRED`, not from the body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Request {
    #[serde(default)]
    pub id: Value,
    pub cmd: String,
    #[serde(default)]
    pub args: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_pane_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_token: Option<String>,
}

/// One response line. `retryable` is serialized only when true, because clients test
/// for its presence (`{... "retryable": true}` on a retryable refusal, absent otherwise).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Response {
    pub id: Value,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
}

impl Response {
    /// A successful reply, echoing the request id.
    pub fn ok(id: Value, result: Value) -> Self {
        Response { id, ok: true, result: Some(result), error: None, retryable: None }
    }

    /// A refusal. The text is the client-visible message (`Display` of the error).
    pub fn refused(id: Value, error: &DunkError) -> Self {
        Response {
            id,
            ok: false,
            result: None,
            error: Some(error.to_string()),
            retryable: if error.retryable() { Some(true) } else { None },
        }
    }

    /// The envelope for a line that was not valid JSON: `id` null, fixed text.
    pub fn invalid_json() -> Self {
        Response { id: Value::Null, ok: false, result: None,
                   error: Some("invalid JSON".to_string()), retryable: None }
    }
}

/// Parse one request line. On failure the caller answers with [`Response::invalid_json`];
/// the id is unknown in that case, which is why it is `Null` rather than guessed.
pub fn decode_request(line: &str) -> Result<Request, ()> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(());
    }
    serde_json::from_str(trimmed).map_err(|_| ())
}

/// Encode one response line (newline appended by the caller or by [`encode_line`]).
pub fn encode_response(response: &Response) -> String {
    // The envelope has no non-string keys and no floats we do not control, so this
    // cannot fail; a panic here would be a bug in the envelope, not in a request.
    serde_json::to_string(response).expect("a Response always serializes")
}

/// Encode any value as one newline-terminated line.
pub fn encode_line<T: Serialize>(value: &T) -> String {
    let mut text = serde_json::to_string(value).expect("a serializable value encodes");
    text.push('\n');
    text
}

/// The `shutdown` command ends the connection loop after its reply (protocol 4.1).
pub fn is_shutdown(request: &Request) -> bool {
    request.cmd == "shutdown"
}

/// The request id as a string for logging, without assuming it is a number.
pub fn id_text(id: &Value) -> String {
    match id {
        Value::Null => "null".to_string(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// One JSON-RPC-style error object, for the MCP server (dunk_mcp).
pub fn rpc_error(code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "error": {"code": code, "message": message}, "id": Value::Null})
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_request_with_optional_fields_absent_parses() {
        let request = decode_request(r#"{"id": 1, "cmd": "status"}"#).unwrap();
        assert_eq!(Value::from(1), request.id);
        assert_eq!("status", request.cmd);
        assert_eq!(Value::Null, request.args);
        assert!(request.self_pane_id.is_none());
        assert!(request.service_token.is_none());
    }

    #[test]
    fn a_full_request_parses_and_round_trips() {
        let line = r#"{"id":1,"cmd":"task_next","args":{"scope":"global"},"self_pane_id":"%97","service_token":"tok"}"#;
        let request = decode_request(line).unwrap();
        assert_eq!(Some("%97".to_string()), request.self_pane_id);
        assert_eq!(Some("tok".to_string()), request.service_token);
        assert_eq!(line, serde_json::to_string(&request).unwrap());
    }

    #[test]
    fn an_unparseable_line_is_an_error_not_a_guess() {
        for bad in ["", "   ", "{not json", "[1,2]"] {
            assert!(decode_request(bad).is_err(), "{bad:?} parsed");
        }
    }

    #[test]
    fn the_invalid_json_envelope_is_exactly_the_documented_one() {
        let response = Response::invalid_json();
        assert_eq!(r#"{"id":null,"ok":false,"error":"invalid JSON"}"#,
                   encode_response(&response));
        assert!(!encode_response(&response).contains("retryable"),
                "retryable must be absent, not false");
    }

    #[test]
    fn a_success_carries_the_result_and_no_error_keys() {
        let response = Response::ok(Value::from(7), json!({"pane_id": "%1"}));
        let text = encode_response(&response);
        assert_eq!(r#"{"id":7,"ok":true,"result":{"pane_id":"%1"}}"#, text);
    }

    #[test]
    fn retryable_appears_only_for_retryable_errors() {
        let retry = Response::refused(Value::from(1), &DunkError::Busy);
        assert_eq!(r#"{"id":1,"ok":false,"error":"database busy; retry the request","retryable":true}"#,
                   encode_response(&retry));
        let refusal = Response::refused(Value::from(1),
                                        &DunkError::Refused("not permitted for role".into()));
        assert_eq!(r#"{"id":1,"ok":false,"error":"not permitted for role"}"#,
                   encode_response(&refusal));
    }

    #[test]
    fn shutdown_is_recognised_and_nothing_else_is() {
        assert!(is_shutdown(&decode_request(r#"{"id":1,"cmd":"shutdown"}"#).unwrap()));
        assert!(!is_shutdown(&decode_request(r#"{"id":1,"cmd":"status"}"#).unwrap()));
    }

    #[test]
    fn ids_are_echoed_whatever_their_type() {
        assert_eq!("null", id_text(&Value::Null));
        assert_eq!("42", id_text(&json!(42)));
        assert_eq!("abc", id_text(&json!("abc")));
    }
}
