//! JSON values at the Python terminal boundary; conversion happens only at the operation.
use crate::error::DunkError;
use serde_json::Value;

pub(crate) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::Number(v) => v.as_f64() != Some(0.0),
        Value::String(v) => !v.is_empty(),
        Value::Array(v) => !v.is_empty(),
        Value::Object(v) => !v.is_empty(),
    }
}
pub(crate) fn string(value: &Value) -> String {
    match value {
        Value::String(v) => v.clone(),
        _ => crate::api::python_value_repr(value),
    }
}
pub(crate) fn optional(value: &Value) -> Option<String> {
    truthy(value).then(|| string(value))
}
/// Python equality, including bool == number, recursively in containers.
pub(crate) fn equal(a: &Value, b: &Value) -> bool {
    #[derive(PartialEq)]
    enum Numeric {
        Integer(String),
        Float(f64),
    }
    fn numeric(value: &Value) -> Option<Numeric> {
        let raw = match value {
            Value::Bool(value) => return Some(Numeric::Integer(u8::from(*value).to_string())),
            Value::Number(value) => value.to_string(),
            _ => return None,
        };
        if raw.chars().all(|c| c.is_ascii_digit() || c == '-') {
            return Some(Numeric::Integer(if raw == "-0" { "0".into() } else { raw }));
        }
        let number = crate::github::parse_python_float(&raw)?;
        if number.is_finite() && number.fract() == 0.0 {
            Some(Numeric::Integer(if number == 0.0 {
                "0".into()
            } else {
                format!("{number:.0}")
            }))
        } else {
            Some(Numeric::Float(number))
        }
    }
    match (a, b) {
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| equal(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len() && a.iter().all(|(k, v)| b.get(k).is_some_and(|b| equal(v, b)))
        }
        _ => match (numeric(a), numeric(b)) {
            (Some(a), Some(b)) => a == b,
            (None, None) => a == b,
            _ => false,
        },
    }
}
/// `_text_chunks`, followed by TmuxBackend._run's str(arg). Input errors are thrown,
/// separate from a terminal command returning a nonzero status.
pub(crate) fn text_chunks(value: &Value) -> Result<Vec<String>, DunkError> {
    match value {
        Value::String(v) => Ok(super::tmux::text_chunks(
            v,
            super::tmux::SEND_TEXT_CHUNK_CHARS,
        )),
        Value::Array(v) => Ok(v
            .chunks(super::tmux::SEND_TEXT_CHUNK_CHARS)
            .map(|v| string(&Value::Array(v.to_vec())))
            .collect()),
        Value::Object(v) if v.is_empty() => Ok(vec![]),
        Value::Object(v) => Err(DunkError::Refused(format!(
            "slice({}, {}, None)",
            if v.len() > super::tmux::SEND_TEXT_CHUNK_CHARS {
                super::tmux::SEND_TEXT_CHUNK_CHARS - 1
            } else {
                0
            },
            if v.len() > super::tmux::SEND_TEXT_CHUNK_CHARS {
                super::tmux::SEND_TEXT_CHUNK_CHARS + 1
            } else {
                v.len()
            }
        ))),
        other => {
            let name = match other {
                Value::Null => "NoneType",
                Value::Bool(_) => "bool",
                Value::Number(n) if n.to_string().contains(['.', 'e', 'E']) => "float",
                _ => "int",
            };
            Err(DunkError::Refused(format!(
                "object of type '{name}' has no len()"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::{tmux::TmuxBackend, Terminal};
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn raw_tmux_create_registry_and_subprocess_match_python() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("tmux");
        std::fs::write(&script,r#"#!/bin/sh
cd "$(dirname "$0")" || exit 1
printf '%s\n' "$@" >> argv
case "$1" in
new-session) printf '%s' "$4" > name ;;
list-sessions) printf '$1\037%s\0370\n' "$(cat name)" ;;
list-windows) printf '@1\037$1\037tab\0370\0371\n' ;;
list-panes) printf '%%1\037$1\037@1\037/work\037bash\037shell\0371\037%s\037tab\0370\0370\0371\03780\03724\n' "$(cat name)" ;;
new-window) printf '@1\037%%1\n' ;;
esac
"#).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let registry = dir.path().join("registry.json");
        let mut backend = TmuxBackend::with_registry(&script, &registry);
        for label in [
            Value::Null,
            json!(false),
            json!(0),
            json!([]),
            json!({}),
            json!(""),
            json!(1),
            json!(["x", false]),
            json!({"x":1}),
            json!("text"),
        ] {
            backend
                .create_workspace_raw(&label, &json!(["cwd"]), true)
                .unwrap();
            let persisted: Value =
                serde_json::from_slice(&std::fs::read(&registry).unwrap()).unwrap();
            let expected = if truthy(&label) {
                label.clone()
            } else {
                json!("dunkingsheep")
            };
            assert_eq!(persisted["workspaces"]["$1"]["label"], expected);
            backend
                .create_tab_raw(Some("$1"), &label, &json!(false), true)
                .unwrap();
            let persisted: Value =
                serde_json::from_slice(&std::fs::read(&registry).unwrap()).unwrap();
            assert_eq!(
                persisted["tabs"]["@1"]["label"],
                if truthy(&label) { label } else { json!("tab") }
            );
        }
        for command in [
            json!(["echo", false]),
            json!("echo safe"),
            json!([]),
            json!({}),
        ] {
            backend.send_text_raw("%1", &command).unwrap().unwrap();
        }
        assert_eq!(
            backend
                .send_text_raw("%1", &json!(1))
                .unwrap_err()
                .to_string(),
            "object of type 'int' has no len()"
        );
        assert_eq!(
            backend
                .send_text_raw("%1", &json!({"x":1}))
                .unwrap_err()
                .to_string(),
            "slice(0, 1, None)"
        );
        let argv = std::fs::read_to_string(dir.path().join("argv")).unwrap();
        assert!(argv.contains("-c\n['cwd']\n"));
        assert!(argv.contains("-l\n--\n['echo', False]\n"));
        assert!(!argv.contains("select-window"));
        assert!(!argv.contains("switch-client"));
    }
    #[test]
    fn raw_history_python_equality_and_collapse() {
        let dir = tempfile::tempdir().unwrap();
        let flock = crate::flock::Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        );
        flock.remember_typed_raw("%1", &json!([true,{"x":false}]));
        flock.remember_typed_raw("%1", &json!([1,{"x":0}]));
        assert_eq!(
            flock.sent_text_by_pane.lock().unwrap()["%1"],
            vec![json!([1,{"x":0}])]
        );
        assert!(flock.looks_like_ours("%1", "[1, {'x': 0}]"));
        assert!(!equal(
            &json!(9007199254740993u64),
            &json!(9007199254740992u64)
        ));
        assert!(!equal(
            &json!(9007199254740993u64),
            &json!(9007199254740992.0)
        ));
        assert!(equal(&json!(1), &json!(1.0)));
    }
    #[test]
    fn raw_large_numbers_keep_integer_precision_and_float_equality() {
        let huge:Value=serde_json::from_str("10000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000").unwrap();
        assert_eq!(string(&huge), huge.to_string());
        assert_eq!(
            text_chunks(&huge).unwrap_err().to_string(),
            "object of type 'int' has no len()"
        );
        let exact: Value = serde_json::from_str("100000000000000000000").unwrap();
        assert!(equal(&exact, &json!(1e20)));
        let next: Value = serde_json::from_str("100000000000000000001").unwrap();
        assert!(!equal(&next, &json!(1e20)));
        assert!(truthy(&serde_json::from_str("1e999").unwrap()));
        assert_eq!(string(&serde_json::from_str("1e999").unwrap()), "inf");
        assert!(!truthy(&serde_json::from_str("1e-999").unwrap()));
    }
}
