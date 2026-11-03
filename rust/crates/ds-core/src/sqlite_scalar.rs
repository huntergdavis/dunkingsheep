//! Python `sqlite3` scalar binding for JSON values, including its refusal boundaries.

use rusqlite::types::Value as SqlValue;
use serde_json::Value as JsonValue;

use crate::error::DunkError;

pub(crate) fn python_sqlite_scalar(
    value: Option<&JsonValue>,
    parameter: usize,
) -> Result<SqlValue, DunkError> {
    let Some(value) = value else {
        return Ok(SqlValue::Null);
    };
    match value {
        JsonValue::Null => Ok(SqlValue::Null),
        JsonValue::Bool(value) => Ok(SqlValue::Integer(i64::from(*value))),
        JsonValue::String(value) => Ok(SqlValue::Text(value.clone())),
        JsonValue::Number(value) if value.is_i64() => {
            Ok(SqlValue::Integer(value.as_i64().expect("is_i64")))
        }
        JsonValue::Number(value) if value.is_u64() => value
            .as_u64()
            .and_then(|number| i64::try_from(number).ok())
            .map(SqlValue::Integer)
            .ok_or_else(|| too_large()),
        JsonValue::Number(value) => {
            let raw = value.to_string();
            if !raw
                .chars()
                .any(|character| matches!(character, '.' | 'e' | 'E'))
            {
                return Err(too_large());
            }
            value.as_f64().map(SqlValue::Real).ok_or_else(too_large)
        }
        JsonValue::Array(_) => Err(DunkError::Refused(format!(
            "Error binding parameter {parameter}: type 'list' is not supported"
        ))),
        JsonValue::Object(_) => Err(DunkError::Refused(format!(
            "Error binding parameter {parameter}: type 'dict' is not supported"
        ))),
    }
}

fn too_large() -> DunkError {
    DunkError::Refused("Python int too large to convert to SQLite INTEGER".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn python_sqlite_scalars_keep_affinity_and_parameter_specific_errors() {
        assert_eq!(SqlValue::Null, python_sqlite_scalar(None, 4).unwrap());
        assert_eq!(
            SqlValue::Null,
            python_sqlite_scalar(Some(&Value::Null), 4).unwrap()
        );
        assert_eq!(
            SqlValue::Integer(1),
            python_sqlite_scalar(Some(&json!(true)), 2).unwrap()
        );
        assert_eq!(
            SqlValue::Integer(42),
            python_sqlite_scalar(Some(&json!(42)), 5).unwrap()
        );
        assert_eq!(
            SqlValue::Real(1.5),
            python_sqlite_scalar(Some(&json!(1.5)), 5).unwrap()
        );
        assert_eq!(
            SqlValue::Text("x".into()),
            python_sqlite_scalar(Some(&json!("x")), 3).unwrap()
        );
        assert_eq!(
            "Error binding parameter 5: type 'list' is not supported",
            python_sqlite_scalar(Some(&json!([])), 5)
                .unwrap_err()
                .to_string()
        );
        assert_eq!(
            "Error binding parameter 2: type 'dict' is not supported",
            python_sqlite_scalar(Some(&json!({})), 2)
                .unwrap_err()
                .to_string()
        );
    }
}
