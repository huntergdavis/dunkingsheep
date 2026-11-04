//! Python's JSON numeric extensions at the common socket boundary.
//!
//! Values remain typed numbers, never marker strings or magic objects. The pinned
//! arbitrary-precision Number carrier preserves nonfinite tokens through cloning and
//! direct serialization. Do not send such Values through serde_json::to_value/json!:
//! that conversion reparses Number tokens using strict JSON (see the pinned probe).

use serde::Serialize;
use serde_json::{Map, Number, Value};
use std::io;

mod serialize;

#[derive(Debug, Clone, Copy)]
struct DecodeError;

/// Decode structure ourselves so a literal `$serde_json::private::Number` key is
/// an ordinary user key, not serde's internal arbitrary-precision number tag.
/// Serde still validates every string and finite JSON number token.
pub fn from_str(text: &str) -> Result<Value, serde_json::Error> {
    let parsed = (|| {
        let mut parser = Parser { text, at: 0 };
        let value = parser.value(0)?;
        parser.space();
        if parser.at != text.len() {
            return Err(DecodeError);
        }
        Ok(value)
    })();
    parsed.map_err(|_| {
        // Preserve the established client's detailed strict-JSON error for malformed
        // frames. A valid Python extension cannot reach this branch.
        serde_json::from_str::<Value>(text)
            .err()
            .unwrap_or_else(|| <serde_json::Error as serde::de::Error>::custom("invalid JSON"))
    })
}

struct Parser<'a> {
    text: &'a str,
    at: usize,
}
impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.at).copied()
    }
    fn space(&mut self) {
        while self.peek().is_some_and(|b| b" \r\n\t".contains(&b)) {
            self.at += 1;
        }
    }
    fn take(&mut self, byte: u8) -> Result<(), DecodeError> {
        self.space();
        if self.peek() != Some(byte) {
            return Err(DecodeError);
        }
        self.at += 1;
        Ok(())
    }
    fn string(&mut self) -> Result<String, DecodeError> {
        self.space();
        let start = self.at;
        self.take(b'"')?;
        loop {
            match self.peek().ok_or(DecodeError)? {
                b'"' => {
                    self.at += 1;
                    return serde_json::from_str(&self.text[start..self.at])
                        .map_err(|_| DecodeError);
                }
                b'\\' => self.at += 2,
                _ => self.at += 1,
            }
        }
    }
    fn value(&mut self, depth: usize) -> Result<Value, DecodeError> {
        self.space();
        match self.peek().ok_or(DecodeError)? {
            b'"' => self.string().map(Value::String),
            b'[' | b'{' => {
                // Keep serde_json's existing 128-container limit at this boundary.
                if depth >= 127 {
                    return Err(DecodeError);
                }
                let object = self.peek() == Some(b'{');
                self.at += 1;
                let end = if object { b'}' } else { b']' };
                let mut map = Map::new();
                let mut array = Vec::new();
                self.space();
                if self.peek() != Some(end) {
                    loop {
                        if object {
                            let key = self.string()?;
                            self.take(b':')?;
                            map.insert(key, self.value(depth + 1)?);
                        } else {
                            array.push(self.value(depth + 1)?);
                        }
                        self.space();
                        if self.peek() == Some(end) {
                            break;
                        }
                        self.take(b',')?;
                    }
                }
                self.take(end)?;
                Ok(if object {
                    Value::Object(map)
                } else {
                    Value::Array(array)
                })
            }
            _ => {
                let start = self.at;
                while self.peek().is_some_and(|b| !b" \r\n\t,]}".contains(&b)) {
                    self.at += 1;
                }
                let token = &self.text[start..self.at];
                match token {
                    "null" => Ok(Value::Null),
                    "true" => Ok(Value::Bool(true)),
                    "false" => Ok(Value::Bool(false)),
                    "Infinity" => "1e999"
                        .parse::<Number>()
                        .map(Value::Number)
                        .map_err(|_| DecodeError),
                    "-Infinity" => "-1e999"
                        .parse::<Number>()
                        .map(Value::Number)
                        .map_err(|_| DecodeError),
                    "NaN" => {
                        // Isolated use of the pinned carrier; never accepts caller-supplied
                        // unchecked text. JSON grammar and strings cannot collide with it.
                        Ok(Value::Number(Number::from_string_unchecked(token.into())))
                    }
                    _ => token
                        .parse::<Number>()
                        .map(Value::Number)
                        .map_err(|_| DecodeError),
                }
            }
        }
    }
}

/// Construct a numeric Value without serde_json's NaN-to-null conversion. Infinity
/// retains the established exponent carrier; only NaN requires the pinned raw token.
pub fn float_value(value: f64) -> Value {
    if let Some(number) = Number::from_f64(value) {
        Value::Number(number)
    } else if value.is_nan() {
        Value::Number(Number::from_string_unchecked("NaN".into()))
    } else {
        let token = if value.is_sign_negative() {
            "-1e999"
        } else {
            "1e999"
        };
        Value::Number(token.parse().expect("valid infinity exponent carrier"))
    }
}

struct PythonNumbers;
impl serde_json::ser::Formatter for PythonNumbers {
    fn write_number_str<W: ?Sized + io::Write>(
        &mut self,
        writer: &mut W,
        token: &str,
    ) -> io::Result<()> {
        let number = if matches!(token, "NaN" | "Infinity" | "-Infinity") {
            token.to_owned()
        } else if token.contains(['.', 'e', 'E']) {
            let value = token.parse::<f64>().map_err(io::Error::other)?;
            if value == f64::INFINITY {
                "Infinity".into()
            } else if value == f64::NEG_INFINITY {
                "-Infinity".into()
            } else {
                crate::usage::py_float_repr(value)
            }
        } else if token == "-0" {
            "0".into()
        } else {
            token.to_owned()
        };
        writer.write_all(number.as_bytes())
    }
}

/// Serialize typed protocol values directly, without a lossy to_value intermediate.
/// Floating values entering a Value must first use a nonfinite-aware numeric adapter.
pub fn to_string<T: ?Sized + Serialize>(value: &T) -> Result<String, serde_json::Error> {
    let mut bytes = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(&mut bytes, PythonNumbers);
    value.serialize(serialize::PythonSerializer(&mut serializer))?;
    Ok(String::from_utf8(bytes).expect("JSON serializer emits UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_nested_native_floats_never_become_null() {
        #[derive(Serialize)]
        struct Payload {
            raw: Vec<Option<f64>>,
            text: String,
        }
        let payload = Payload {
            raw: vec![
                Some(f64::NAN),
                Some(f64::INFINITY),
                Some(f64::NEG_INFINITY),
                None,
            ],
            text: "NaN".into(),
        };
        assert_eq!(
            to_string(&payload).unwrap(),
            r#"{"raw":[NaN,Infinity,-Infinity,null],"text":"NaN"}"#
        );
    }

    #[test]
    fn shared_float_adapter_preserves_nan_and_existing_infinity_carriers() {
        for (number, raw, wire) in [
            (f64::NAN, "NaN", "NaN"),
            (f64::INFINITY, "1e+999", "Infinity"),
            (f64::NEG_INFINITY, "-1e+999", "-Infinity"),
            (1.5, "1.5", "1.5"),
        ] {
            let value = crate::github::python_float_json_value(number).expect("numeric carrier");
            assert!(value.is_number());
            assert_eq!(value.to_string(), raw);
            assert_eq!(to_string(&value).unwrap(), wire);
        }
    }

    #[test]
    fn wire_parser_preserves_existing_container_depth_limit() {
        for depth in [1, 126, 127, 128, 129] {
            let raw = format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
            assert_eq!(
                from_str(&raw).is_ok(),
                serde_json::from_str::<Value>(&raw).is_ok(),
                "depth {depth}"
            );
        }
    }

    #[test]
    fn wire_codec_matches_actual_python_numeric_oracle() {
        let cases: Vec<Value> = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../../tests/fixtures/wire_nonfinite_oracle.json"
        )))
        .unwrap();
        for case in cases {
            let actual = to_string(&from_str(case["input"].as_str().unwrap()).unwrap()).unwrap();
            assert_eq!(actual, case["output"].as_str().unwrap(), "{case}");
        }
    }

    #[test]
    fn pinned_serde_nonfinite_carrier_and_to_value_limitation() {
        for token in ["NaN", "Infinity", "-Infinity"] {
            assert!(serde_json::from_str::<Value>(token).is_err());
            let value = Value::Number(Number::from_string_unchecked(token.into()));
            assert_eq!(serde_json::to_string(&value).unwrap(), token);
            assert!(serde_json::to_value(&value).is_err());
            assert_eq!(value.as_f64(), None);
            assert_eq!(to_string(&value.clone()).unwrap(), token);
        }
        assert_eq!(serde_json::to_value(f64::NAN).unwrap(), Value::Null);
        assert!(Number::from_f64(f64::NAN).is_none());
    }

    #[test]
    fn wire_numbers_do_not_collide_with_strings_objects_or_integer_tokens() {
        let raw = r#"[NaN,Infinity,-Infinity,1e309,"NaN","Infinity","\\\"NaN",{"$serde_json::private::Number":"NaN","\u004e aN":"Infinity"},123456789012345678901234567890,-0,-0.0]"#;
        let value = from_str(raw).unwrap();
        assert!(value[0].is_number());
        assert_eq!(value[4], "NaN");
        assert_eq!(value[7]["$serde_json::private::Number"], "NaN");
        assert!(value[7].is_object());
        assert_eq!(
            value[8].as_number().unwrap().to_string(),
            "123456789012345678901234567890"
        );
        let encoded = to_string(&value).unwrap();
        assert!(encoded.starts_with("[NaN,Infinity,-Infinity,Infinity,"));
        assert_eq!(to_string(&from_str(&encoded).unwrap()).unwrap(), encoded);
        assert_eq!(
            to_string(&from_str(r#"{"a":NaN,"b":1,"a":Infinity}"#).unwrap()).unwrap(),
            r#"{"a":Infinity,"b":1}"#
        );
    }

    #[test]
    fn wire_parser_keeps_strict_grammar_around_python_constants() {
        for invalid in [
            "",
            "nan",
            "+Infinity",
            "-NaN",
            "NaNx",
            "[NaN,]",
            "{NaN:1}",
            "01",
            "1.",
            "[1 2]",
            "[NaN]x",
            "\"bad\\",
            "[Infinity:2]",
        ] {
            assert!(from_str(invalid).is_err(), "{invalid}");
        }
        for valid in [
            "null",
            "false",
            "0",
            "{}",
            "[]",
            "[\"\\u004e aN\",NaN]",
            "1e-999",
        ] {
            assert!(from_str(valid).is_ok(), "{valid}");
        }
    }
}
