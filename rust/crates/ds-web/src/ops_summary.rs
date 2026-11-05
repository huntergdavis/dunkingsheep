//! Read-only Rust equivalent of bridges/ops_metrics.py::summary.
//!
//! The caller supplies an initialized database connection, explicit clock, and
//! configured file paths. This module owns no service state, cache, or discovery.

use chrono::{DateTime, Datelike, Duration, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::{json, Map, Number, Value};
use std::path::Path;

const PROJECT: &str = "poolrad";

type SummaryResult<T> = Result<T, SummaryError>;

#[derive(Debug)]
pub enum SummaryError {
    Sql(rusqlite::Error),
    Timestamp(String),
    Json(serde_json::Error),
    InvalidPayloadType,
}

impl From<rusqlite::Error> for SummaryError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Sql(value)
    }
}

impl From<serde_json::Error> for SummaryError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

impl std::fmt::Display for SummaryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sql(error) => write!(formatter, "SQLite error: {error}"),
            Self::Timestamp(value) => write!(formatter, "invalid timestamp: {value}"),
            Self::Json(error) => write!(formatter, "invalid JSON: {error}"),
            Self::InvalidPayloadType => {
                write!(formatter, "telemetry payload must be JSON text or bytes")
            }
        }
    }
}

impl std::error::Error for SummaryError {}

fn parse_time(value: &str) -> SummaryResult<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|_| SummaryError::Timestamp(value.to_owned()))
}

// Python str.casefold differs from Unicode lowercase for these code points.
// The exception table is generated from Python 3.14's Unicode database; all other
// characters use Rust's full lowercase iterator, matching Python's default fold.
fn python_casefold(value: &str) -> String {
    let mut result = String::new();
    for character in value.chars() {
        if let Some(mapped) = casefold_exception(character) {
            result.push_str(mapped);
        } else {
            result.extend(character.to_lowercase());
        }
    }
    result
}

fn casefold_exception(character: char) -> Option<&'static str> {
    match character {
        '\u{00B5}' => Some("\u{03BC}"),
        '\u{00DF}' => Some("\u{0073}\u{0073}"),
        '\u{0149}' => Some("\u{02BC}\u{006E}"),
        '\u{017F}' => Some("\u{0073}"),
        '\u{01F0}' => Some("\u{006A}\u{030C}"),
        '\u{0345}' => Some("\u{03B9}"),
        '\u{0390}' => Some("\u{03B9}\u{0308}\u{0301}"),
        '\u{03B0}' => Some("\u{03C5}\u{0308}\u{0301}"),
        '\u{03C2}' => Some("\u{03C3}"),
        '\u{03D0}' => Some("\u{03B2}"),
        '\u{03D1}' => Some("\u{03B8}"),
        '\u{03D5}' => Some("\u{03C6}"),
        '\u{03D6}' => Some("\u{03C0}"),
        '\u{03F0}' => Some("\u{03BA}"),
        '\u{03F1}' => Some("\u{03C1}"),
        '\u{03F5}' => Some("\u{03B5}"),
        '\u{0587}' => Some("\u{0565}\u{0582}"),
        '\u{13A0}' => Some("\u{13A0}"),
        '\u{13A1}' => Some("\u{13A1}"),
        '\u{13A2}' => Some("\u{13A2}"),
        '\u{13A3}' => Some("\u{13A3}"),
        '\u{13A4}' => Some("\u{13A4}"),
        '\u{13A5}' => Some("\u{13A5}"),
        '\u{13A6}' => Some("\u{13A6}"),
        '\u{13A7}' => Some("\u{13A7}"),
        '\u{13A8}' => Some("\u{13A8}"),
        '\u{13A9}' => Some("\u{13A9}"),
        '\u{13AA}' => Some("\u{13AA}"),
        '\u{13AB}' => Some("\u{13AB}"),
        '\u{13AC}' => Some("\u{13AC}"),
        '\u{13AD}' => Some("\u{13AD}"),
        '\u{13AE}' => Some("\u{13AE}"),
        '\u{13AF}' => Some("\u{13AF}"),
        '\u{13B0}' => Some("\u{13B0}"),
        '\u{13B1}' => Some("\u{13B1}"),
        '\u{13B2}' => Some("\u{13B2}"),
        '\u{13B3}' => Some("\u{13B3}"),
        '\u{13B4}' => Some("\u{13B4}"),
        '\u{13B5}' => Some("\u{13B5}"),
        '\u{13B6}' => Some("\u{13B6}"),
        '\u{13B7}' => Some("\u{13B7}"),
        '\u{13B8}' => Some("\u{13B8}"),
        '\u{13B9}' => Some("\u{13B9}"),
        '\u{13BA}' => Some("\u{13BA}"),
        '\u{13BB}' => Some("\u{13BB}"),
        '\u{13BC}' => Some("\u{13BC}"),
        '\u{13BD}' => Some("\u{13BD}"),
        '\u{13BE}' => Some("\u{13BE}"),
        '\u{13BF}' => Some("\u{13BF}"),
        '\u{13C0}' => Some("\u{13C0}"),
        '\u{13C1}' => Some("\u{13C1}"),
        '\u{13C2}' => Some("\u{13C2}"),
        '\u{13C3}' => Some("\u{13C3}"),
        '\u{13C4}' => Some("\u{13C4}"),
        '\u{13C5}' => Some("\u{13C5}"),
        '\u{13C6}' => Some("\u{13C6}"),
        '\u{13C7}' => Some("\u{13C7}"),
        '\u{13C8}' => Some("\u{13C8}"),
        '\u{13C9}' => Some("\u{13C9}"),
        '\u{13CA}' => Some("\u{13CA}"),
        '\u{13CB}' => Some("\u{13CB}"),
        '\u{13CC}' => Some("\u{13CC}"),
        '\u{13CD}' => Some("\u{13CD}"),
        '\u{13CE}' => Some("\u{13CE}"),
        '\u{13CF}' => Some("\u{13CF}"),
        '\u{13D0}' => Some("\u{13D0}"),
        '\u{13D1}' => Some("\u{13D1}"),
        '\u{13D2}' => Some("\u{13D2}"),
        '\u{13D3}' => Some("\u{13D3}"),
        '\u{13D4}' => Some("\u{13D4}"),
        '\u{13D5}' => Some("\u{13D5}"),
        '\u{13D6}' => Some("\u{13D6}"),
        '\u{13D7}' => Some("\u{13D7}"),
        '\u{13D8}' => Some("\u{13D8}"),
        '\u{13D9}' => Some("\u{13D9}"),
        '\u{13DA}' => Some("\u{13DA}"),
        '\u{13DB}' => Some("\u{13DB}"),
        '\u{13DC}' => Some("\u{13DC}"),
        '\u{13DD}' => Some("\u{13DD}"),
        '\u{13DE}' => Some("\u{13DE}"),
        '\u{13DF}' => Some("\u{13DF}"),
        '\u{13E0}' => Some("\u{13E0}"),
        '\u{13E1}' => Some("\u{13E1}"),
        '\u{13E2}' => Some("\u{13E2}"),
        '\u{13E3}' => Some("\u{13E3}"),
        '\u{13E4}' => Some("\u{13E4}"),
        '\u{13E5}' => Some("\u{13E5}"),
        '\u{13E6}' => Some("\u{13E6}"),
        '\u{13E7}' => Some("\u{13E7}"),
        '\u{13E8}' => Some("\u{13E8}"),
        '\u{13E9}' => Some("\u{13E9}"),
        '\u{13EA}' => Some("\u{13EA}"),
        '\u{13EB}' => Some("\u{13EB}"),
        '\u{13EC}' => Some("\u{13EC}"),
        '\u{13ED}' => Some("\u{13ED}"),
        '\u{13EE}' => Some("\u{13EE}"),
        '\u{13EF}' => Some("\u{13EF}"),
        '\u{13F0}' => Some("\u{13F0}"),
        '\u{13F1}' => Some("\u{13F1}"),
        '\u{13F2}' => Some("\u{13F2}"),
        '\u{13F3}' => Some("\u{13F3}"),
        '\u{13F4}' => Some("\u{13F4}"),
        '\u{13F5}' => Some("\u{13F5}"),
        '\u{13F8}' => Some("\u{13F0}"),
        '\u{13F9}' => Some("\u{13F1}"),
        '\u{13FA}' => Some("\u{13F2}"),
        '\u{13FB}' => Some("\u{13F3}"),
        '\u{13FC}' => Some("\u{13F4}"),
        '\u{13FD}' => Some("\u{13F5}"),
        '\u{1C80}' => Some("\u{0432}"),
        '\u{1C81}' => Some("\u{0434}"),
        '\u{1C82}' => Some("\u{043E}"),
        '\u{1C83}' => Some("\u{0441}"),
        '\u{1C84}' => Some("\u{0442}"),
        '\u{1C85}' => Some("\u{0442}"),
        '\u{1C86}' => Some("\u{044A}"),
        '\u{1C87}' => Some("\u{0463}"),
        '\u{1C88}' => Some("\u{A64B}"),
        '\u{1E96}' => Some("\u{0068}\u{0331}"),
        '\u{1E97}' => Some("\u{0074}\u{0308}"),
        '\u{1E98}' => Some("\u{0077}\u{030A}"),
        '\u{1E99}' => Some("\u{0079}\u{030A}"),
        '\u{1E9A}' => Some("\u{0061}\u{02BE}"),
        '\u{1E9B}' => Some("\u{1E61}"),
        '\u{1E9E}' => Some("\u{0073}\u{0073}"),
        '\u{1F50}' => Some("\u{03C5}\u{0313}"),
        '\u{1F52}' => Some("\u{03C5}\u{0313}\u{0300}"),
        '\u{1F54}' => Some("\u{03C5}\u{0313}\u{0301}"),
        '\u{1F56}' => Some("\u{03C5}\u{0313}\u{0342}"),
        '\u{1F80}' => Some("\u{1F00}\u{03B9}"),
        '\u{1F81}' => Some("\u{1F01}\u{03B9}"),
        '\u{1F82}' => Some("\u{1F02}\u{03B9}"),
        '\u{1F83}' => Some("\u{1F03}\u{03B9}"),
        '\u{1F84}' => Some("\u{1F04}\u{03B9}"),
        '\u{1F85}' => Some("\u{1F05}\u{03B9}"),
        '\u{1F86}' => Some("\u{1F06}\u{03B9}"),
        '\u{1F87}' => Some("\u{1F07}\u{03B9}"),
        '\u{1F88}' => Some("\u{1F00}\u{03B9}"),
        '\u{1F89}' => Some("\u{1F01}\u{03B9}"),
        '\u{1F8A}' => Some("\u{1F02}\u{03B9}"),
        '\u{1F8B}' => Some("\u{1F03}\u{03B9}"),
        '\u{1F8C}' => Some("\u{1F04}\u{03B9}"),
        '\u{1F8D}' => Some("\u{1F05}\u{03B9}"),
        '\u{1F8E}' => Some("\u{1F06}\u{03B9}"),
        '\u{1F8F}' => Some("\u{1F07}\u{03B9}"),
        '\u{1F90}' => Some("\u{1F20}\u{03B9}"),
        '\u{1F91}' => Some("\u{1F21}\u{03B9}"),
        '\u{1F92}' => Some("\u{1F22}\u{03B9}"),
        '\u{1F93}' => Some("\u{1F23}\u{03B9}"),
        '\u{1F94}' => Some("\u{1F24}\u{03B9}"),
        '\u{1F95}' => Some("\u{1F25}\u{03B9}"),
        '\u{1F96}' => Some("\u{1F26}\u{03B9}"),
        '\u{1F97}' => Some("\u{1F27}\u{03B9}"),
        '\u{1F98}' => Some("\u{1F20}\u{03B9}"),
        '\u{1F99}' => Some("\u{1F21}\u{03B9}"),
        '\u{1F9A}' => Some("\u{1F22}\u{03B9}"),
        '\u{1F9B}' => Some("\u{1F23}\u{03B9}"),
        '\u{1F9C}' => Some("\u{1F24}\u{03B9}"),
        '\u{1F9D}' => Some("\u{1F25}\u{03B9}"),
        '\u{1F9E}' => Some("\u{1F26}\u{03B9}"),
        '\u{1F9F}' => Some("\u{1F27}\u{03B9}"),
        '\u{1FA0}' => Some("\u{1F60}\u{03B9}"),
        '\u{1FA1}' => Some("\u{1F61}\u{03B9}"),
        '\u{1FA2}' => Some("\u{1F62}\u{03B9}"),
        '\u{1FA3}' => Some("\u{1F63}\u{03B9}"),
        '\u{1FA4}' => Some("\u{1F64}\u{03B9}"),
        '\u{1FA5}' => Some("\u{1F65}\u{03B9}"),
        '\u{1FA6}' => Some("\u{1F66}\u{03B9}"),
        '\u{1FA7}' => Some("\u{1F67}\u{03B9}"),
        '\u{1FA8}' => Some("\u{1F60}\u{03B9}"),
        '\u{1FA9}' => Some("\u{1F61}\u{03B9}"),
        '\u{1FAA}' => Some("\u{1F62}\u{03B9}"),
        '\u{1FAB}' => Some("\u{1F63}\u{03B9}"),
        '\u{1FAC}' => Some("\u{1F64}\u{03B9}"),
        '\u{1FAD}' => Some("\u{1F65}\u{03B9}"),
        '\u{1FAE}' => Some("\u{1F66}\u{03B9}"),
        '\u{1FAF}' => Some("\u{1F67}\u{03B9}"),
        '\u{1FB2}' => Some("\u{1F70}\u{03B9}"),
        '\u{1FB3}' => Some("\u{03B1}\u{03B9}"),
        '\u{1FB4}' => Some("\u{03AC}\u{03B9}"),
        '\u{1FB6}' => Some("\u{03B1}\u{0342}"),
        '\u{1FB7}' => Some("\u{03B1}\u{0342}\u{03B9}"),
        '\u{1FBC}' => Some("\u{03B1}\u{03B9}"),
        '\u{1FBE}' => Some("\u{03B9}"),
        '\u{1FC2}' => Some("\u{1F74}\u{03B9}"),
        '\u{1FC3}' => Some("\u{03B7}\u{03B9}"),
        '\u{1FC4}' => Some("\u{03AE}\u{03B9}"),
        '\u{1FC6}' => Some("\u{03B7}\u{0342}"),
        '\u{1FC7}' => Some("\u{03B7}\u{0342}\u{03B9}"),
        '\u{1FCC}' => Some("\u{03B7}\u{03B9}"),
        '\u{1FD2}' => Some("\u{03B9}\u{0308}\u{0300}"),
        '\u{1FD3}' => Some("\u{03B9}\u{0308}\u{0301}"),
        '\u{1FD6}' => Some("\u{03B9}\u{0342}"),
        '\u{1FD7}' => Some("\u{03B9}\u{0308}\u{0342}"),
        '\u{1FE2}' => Some("\u{03C5}\u{0308}\u{0300}"),
        '\u{1FE3}' => Some("\u{03C5}\u{0308}\u{0301}"),
        '\u{1FE4}' => Some("\u{03C1}\u{0313}"),
        '\u{1FE6}' => Some("\u{03C5}\u{0342}"),
        '\u{1FE7}' => Some("\u{03C5}\u{0308}\u{0342}"),
        '\u{1FF2}' => Some("\u{1F7C}\u{03B9}"),
        '\u{1FF3}' => Some("\u{03C9}\u{03B9}"),
        '\u{1FF4}' => Some("\u{03CE}\u{03B9}"),
        '\u{1FF6}' => Some("\u{03C9}\u{0342}"),
        '\u{1FF7}' => Some("\u{03C9}\u{0342}\u{03B9}"),
        '\u{1FFC}' => Some("\u{03C9}\u{03B9}"),
        '\u{AB70}' => Some("\u{13A0}"),
        '\u{AB71}' => Some("\u{13A1}"),
        '\u{AB72}' => Some("\u{13A2}"),
        '\u{AB73}' => Some("\u{13A3}"),
        '\u{AB74}' => Some("\u{13A4}"),
        '\u{AB75}' => Some("\u{13A5}"),
        '\u{AB76}' => Some("\u{13A6}"),
        '\u{AB77}' => Some("\u{13A7}"),
        '\u{AB78}' => Some("\u{13A8}"),
        '\u{AB79}' => Some("\u{13A9}"),
        '\u{AB7A}' => Some("\u{13AA}"),
        '\u{AB7B}' => Some("\u{13AB}"),
        '\u{AB7C}' => Some("\u{13AC}"),
        '\u{AB7D}' => Some("\u{13AD}"),
        '\u{AB7E}' => Some("\u{13AE}"),
        '\u{AB7F}' => Some("\u{13AF}"),
        '\u{AB80}' => Some("\u{13B0}"),
        '\u{AB81}' => Some("\u{13B1}"),
        '\u{AB82}' => Some("\u{13B2}"),
        '\u{AB83}' => Some("\u{13B3}"),
        '\u{AB84}' => Some("\u{13B4}"),
        '\u{AB85}' => Some("\u{13B5}"),
        '\u{AB86}' => Some("\u{13B6}"),
        '\u{AB87}' => Some("\u{13B7}"),
        '\u{AB88}' => Some("\u{13B8}"),
        '\u{AB89}' => Some("\u{13B9}"),
        '\u{AB8A}' => Some("\u{13BA}"),
        '\u{AB8B}' => Some("\u{13BB}"),
        '\u{AB8C}' => Some("\u{13BC}"),
        '\u{AB8D}' => Some("\u{13BD}"),
        '\u{AB8E}' => Some("\u{13BE}"),
        '\u{AB8F}' => Some("\u{13BF}"),
        '\u{AB90}' => Some("\u{13C0}"),
        '\u{AB91}' => Some("\u{13C1}"),
        '\u{AB92}' => Some("\u{13C2}"),
        '\u{AB93}' => Some("\u{13C3}"),
        '\u{AB94}' => Some("\u{13C4}"),
        '\u{AB95}' => Some("\u{13C5}"),
        '\u{AB96}' => Some("\u{13C6}"),
        '\u{AB97}' => Some("\u{13C7}"),
        '\u{AB98}' => Some("\u{13C8}"),
        '\u{AB99}' => Some("\u{13C9}"),
        '\u{AB9A}' => Some("\u{13CA}"),
        '\u{AB9B}' => Some("\u{13CB}"),
        '\u{AB9C}' => Some("\u{13CC}"),
        '\u{AB9D}' => Some("\u{13CD}"),
        '\u{AB9E}' => Some("\u{13CE}"),
        '\u{AB9F}' => Some("\u{13CF}"),
        '\u{ABA0}' => Some("\u{13D0}"),
        '\u{ABA1}' => Some("\u{13D1}"),
        '\u{ABA2}' => Some("\u{13D2}"),
        '\u{ABA3}' => Some("\u{13D3}"),
        '\u{ABA4}' => Some("\u{13D4}"),
        '\u{ABA5}' => Some("\u{13D5}"),
        '\u{ABA6}' => Some("\u{13D6}"),
        '\u{ABA7}' => Some("\u{13D7}"),
        '\u{ABA8}' => Some("\u{13D8}"),
        '\u{ABA9}' => Some("\u{13D9}"),
        '\u{ABAA}' => Some("\u{13DA}"),
        '\u{ABAB}' => Some("\u{13DB}"),
        '\u{ABAC}' => Some("\u{13DC}"),
        '\u{ABAD}' => Some("\u{13DD}"),
        '\u{ABAE}' => Some("\u{13DE}"),
        '\u{ABAF}' => Some("\u{13DF}"),
        '\u{ABB0}' => Some("\u{13E0}"),
        '\u{ABB1}' => Some("\u{13E1}"),
        '\u{ABB2}' => Some("\u{13E2}"),
        '\u{ABB3}' => Some("\u{13E3}"),
        '\u{ABB4}' => Some("\u{13E4}"),
        '\u{ABB5}' => Some("\u{13E5}"),
        '\u{ABB6}' => Some("\u{13E6}"),
        '\u{ABB7}' => Some("\u{13E7}"),
        '\u{ABB8}' => Some("\u{13E8}"),
        '\u{ABB9}' => Some("\u{13E9}"),
        '\u{ABBA}' => Some("\u{13EA}"),
        '\u{ABBB}' => Some("\u{13EB}"),
        '\u{ABBC}' => Some("\u{13EC}"),
        '\u{ABBD}' => Some("\u{13ED}"),
        '\u{ABBE}' => Some("\u{13EE}"),
        '\u{ABBF}' => Some("\u{13EF}"),
        '\u{FB00}' => Some("\u{0066}\u{0066}"),
        '\u{FB01}' => Some("\u{0066}\u{0069}"),
        '\u{FB02}' => Some("\u{0066}\u{006C}"),
        '\u{FB03}' => Some("\u{0066}\u{0066}\u{0069}"),
        '\u{FB04}' => Some("\u{0066}\u{0066}\u{006C}"),
        '\u{FB05}' => Some("\u{0073}\u{0074}"),
        '\u{FB06}' => Some("\u{0073}\u{0074}"),
        '\u{FB13}' => Some("\u{0574}\u{0576}"),
        '\u{FB14}' => Some("\u{0574}\u{0565}"),
        '\u{FB15}' => Some("\u{0574}\u{056B}"),
        '\u{FB16}' => Some("\u{057E}\u{0576}"),
        '\u{FB17}' => Some("\u{0574}\u{056D}"),
        _ => None,
    }
}

fn iso(value: DateTime<Utc>) -> String {
    value.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn age(now: DateTime<Utc>, value: &str) -> SummaryResult<i64> {
    Ok((now - parse_time(value)?).num_seconds().max(0))
}

// Python round uses ties-to-even; f64::round does not.
pub(super) fn round_even(value: f64, digits: i32) -> f64 {
    if !value.is_finite() {
        return value;
    }
    if digits < 0 || digits > 18 {
        return value;
    }
    let Some(scaled) = rounded_scaled_integer(value, digits as u32) else {
        return value;
    };
    scaled as f64 / 10_f64.powi(digits)
}

// Round the exact binary value after multiplying by 10^digits. Scaling in
// f64 first loses the low bits that distinguish Python's round(2.675, 2).
fn rounded_scaled_integer(value: f64, digits: u32) -> Option<i128> {
    let bits = value.to_bits();
    let negative = bits >> 63 != 0;
    let exponent = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & ((1_u64 << 52) - 1);
    let (mantissa, binary_exponent) = if exponent == 0 {
        (fraction as u128, -1074)
    } else {
        (((1_u64 << 52) | fraction) as u128, exponent - 1075)
    };
    if mantissa == 0 {
        return Some(0);
    }
    let five = 5_u128.checked_pow(digits)?;
    let numerator = mantissa.checked_mul(five)?;
    let shift = binary_exponent + digits as i32;
    let magnitude = if shift >= 0 {
        let shift = shift as u32;
        if shift >= 128 || numerator > (u128::MAX >> shift) {
            return None;
        }
        numerator << shift
    } else {
        let denominator_shift = (-shift) as u32;
        if denominator_shift >= 128 {
            0
        } else {
            let denominator = 1_u128 << denominator_shift;
            let quotient = numerator / denominator;
            let remainder = numerator % denominator;
            match remainder.cmp(&(denominator - remainder)) {
                std::cmp::Ordering::Less => quotient,
                std::cmp::Ordering::Greater => quotient.checked_add(1)?,
                std::cmp::Ordering::Equal if quotient % 2 == 1 => quotient.checked_add(1)?,
                std::cmp::Ordering::Equal => quotient,
            }
        }
    };
    let signed = i128::try_from(magnitude).ok()?;
    Some(if negative { -signed } else { signed })
}

fn number(value: f64) -> Value {
    Number::from_f64(value)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

fn field(row: &Row<'_>, name: &str) -> rusqlite::Result<Value> {
    let index = row.as_ref().column_index(name)?;
    match row.get_ref(index)? {
        rusqlite::types::ValueRef::Null => Ok(Value::Null),
        rusqlite::types::ValueRef::Integer(value) => Ok(json!(value)),
        rusqlite::types::ValueRef::Real(value) => Ok(number(value)),
        rusqlite::types::ValueRef::Text(value) => {
            let text = String::from_utf8(value.to_vec()).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    index,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })?;
            Ok(json!(text))
        }
        rusqlite::types::ValueRef::Blob(value) => Ok(json!(value)),
    }
}

fn row_object(row: &Row<'_>, columns: &[&str]) -> rusqlite::Result<Value> {
    let mut result = Map::new();
    for name in columns {
        result.insert((*name).to_owned(), field(row, name)?);
    }
    Ok(Value::Object(result))
}

fn query_objects(
    conn: &Connection,
    sql: &str,
    values: &[&dyn rusqlite::ToSql],
    columns: &[&str],
) -> SummaryResult<Vec<Value>> {
    let mut statement = conn.prepare(sql)?;
    let rows = statement.query_map(values, |row| row_object(row, columns))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn latest_trial_for_accepted_body(
    conn: &Connection,
    function_id: &str,
    head: &str,
) -> SummaryResult<Option<String>> {
    let mut statement=conn.prepare("SELECT DISTINCT trial_id FROM function_body_events WHERE project=?1 AND function_id=?2 AND source_head=?3 AND status='authored' AND trial_id IS NOT NULL")?;
    let mut rows = statement.query(params![PROJECT, function_id, head])?;
    let Some(first) = rows.next()? else {
        return Ok(None);
    };
    let trial: String = first.get(0)?;
    if rows.next()?.is_some() {
        Ok(None)
    } else {
        Ok(Some(trial))
    }
}

fn experiment_summary(conn: &Connection, now: DateTime<Utc>) -> SummaryResult<Value> {
    let start = iso(now - Duration::hours(24));
    let end = iso(now);
    let mut messages = query_objects(conn,
        "SELECT pane_id,SUM(kind='inbox.sent') AS inbox_deliveries,SUM(kind='message.sent') AS direct_sends FROM agent_message_events WHERE occurred_at>=?1 AND occurred_at<=?2 GROUP BY pane_id ORDER BY inbox_deliveries+direct_sends DESC,pane_id",
        &[&start, &end], &["pane_id","inbox_deliveries","direct_sends"])?;
    for message in &mut messages {
        let pane = message["pane_id"].as_str().unwrap_or_default();
        let identity = conn.query_row(
            "SELECT agent_name,lane_name FROM pane_samples WHERE pane_id=?1 AND agent_name IS NOT NULL ORDER BY run_id DESC LIMIT 1",
            [pane], |row| Ok((row.get::<_, String>(0)?,row.get::<_, Option<String>>(1)?)),
        ).optional()?;
        let (agent, lane) = if let Some((agent, lane)) = identity {
            (Some(agent), lane)
        } else {
            let alias = conn.query_row(
                "SELECT mailbox_alias FROM agent_message_events WHERE pane_id=?1 AND kind='inbox.sent' AND mailbox_alias IS NOT NULL AND mailbox_alias NOT LIKE '%@%' GROUP BY mailbox_alias ORDER BY COUNT(*) DESC,mailbox_alias LIMIT 1",
                [pane], |row| row.get::<_,String>(0),
            ).optional()?;
            (alias, None)
        };
        message["agent_name"] = agent.map(Value::String).unwrap_or(Value::Null);
        message["lane_name"] = lane.map(Value::String).unwrap_or(Value::Null);
    }

    let mut bodies=query_objects(conn,
        "SELECT e.function_id,e.status,e.pane_id,e.trial_id,e.source_head,e.evidence_ref,e.reviewed_by,e.occurred_at FROM function_body_events e WHERE e.project=?1 AND e.id=(SELECT x.id FROM function_body_events x WHERE x.project=e.project AND x.function_id=e.function_id ORDER BY x.occurred_at DESC,x.id DESC LIMIT 1) ORDER BY e.occurred_at DESC,e.function_id",
        &[&PROJECT],&["function_id","status","pane_id","trial_id","source_head","evidence_ref","reviewed_by","occurred_at"])?;
    let mut trials=query_objects(conn,
        "SELECT * FROM agent_trials WHERE project=?1 AND started_at>=?2 ORDER BY started_at DESC,trial_id",
        &[&PROJECT,&iso(now-Duration::hours(168))],
        &["trial_id","project","pane_id","agent_type","provider","model","reasoning_effort","hypothesis","started_at","ended_at","input_tokens","output_tokens","metered_usd","correction_minutes","host_psi10","usage_source","evidence_ref"])?;

    let mut in_window = std::collections::HashSet::<String>::new();
    for trial in &trials {
        in_window.insert(trial["trial_id"].as_str().unwrap_or_default().to_owned());
    }
    let authored=query_objects(conn,
        "SELECT DISTINCT trial_id,function_id FROM function_body_events WHERE project=?1 AND status='authored' AND trial_id IS NOT NULL",
        &[&PROJECT],&["trial_id","function_id"])?;
    let mut authored_keys = std::collections::HashSet::<(String, String)>::new();
    let mut counts = std::collections::HashMap::<String, (i64, i64)>::new();
    for body in authored {
        let trial = body["trial_id"].as_str().unwrap_or_default().to_owned();
        let function = body["function_id"].as_str().unwrap_or_default().to_owned();
        if in_window.contains(&trial) {
            authored_keys.insert((trial.clone(), function));
            counts.entry(trial).or_default().0 += 1;
        }
    }
    for body in &mut bodies {
        if body["status"] == "withdrawn" {
            continue;
        }
        if body["status"] == "accepted" && body["trial_id"].is_null() {
            if let Some(trial) = latest_trial_for_accepted_body(
                conn,
                body["function_id"].as_str().unwrap_or_default(),
                body["source_head"].as_str().unwrap_or_default(),
            )? {
                body["trial_id"] = json!(trial);
                body["trial_attribution"] = json!("matching_authored_head_backfill");
            }
        }
        let Some(trial) = body["trial_id"].as_str() else {
            continue;
        };
        if !in_window.contains(trial) {
            continue;
        }
        let counts_for_trial = counts.entry(trial.to_owned()).or_default();
        if body["status"] == "accepted" {
            let key = (
                trial.to_owned(),
                body["function_id"].as_str().unwrap_or_default().to_owned(),
            );
            if authored_keys.insert(key) {
                counts_for_trial.0 += 1
            }
            counts_for_trial.1 += 1;
        }
    }

    for trial in &mut trials {
        let id = trial["trial_id"].as_str().unwrap_or_default().to_owned();
        let (authored_count, accepted_count) = counts.get(&id).copied().unwrap_or_default();
        trial["authored_bodies"] = json!(authored_count);
        trial["accepted_bodies"] = json!(accepted_count);
        let started = parse_time(trial["started_at"].as_str().unwrap_or_default())?;
        let ended = match trial["ended_at"].as_str() {
            Some(text) => Some(parse_time(text)?),
            None => None,
        };
        let hours =
            ((ended.unwrap_or(now) - started).num_milliseconds() as f64 / 3_600_000.0).max(0.0);
        trial["elapsed_hours"] = number(round_even(hours, 3));
        trial["accepted_per_hour"] = if hours > 0.0 && ended.is_some() {
            number(round_even(accepted_count as f64 / hours, 3))
        } else {
            Value::Null
        };
        trial["accepted_per_usd"] = match trial["metered_usd"].as_f64() {
            Some(cost) if cost != 0.0 => number(round_even(accepted_count as f64 / cost, 3)),
            _ => Value::Null,
        };
        let end_text = trial["ended_at"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| iso(now));
        let (inbox,direct):(i64,i64)=conn.query_row(
            "SELECT SUM(kind='inbox.sent'),SUM(kind='message.sent') FROM agent_message_events WHERE pane_id=?1 AND occurred_at>=?2 AND occurred_at<=?3",
            params![trial["pane_id"].as_str().unwrap_or_default(),trial["started_at"].as_str().unwrap_or_default(),end_text],
            |row|Ok((row.get::<_,Option<i64>>(0)?.unwrap_or(0),row.get::<_,Option<i64>>(1)?.unwrap_or(0))))?;
        trial["inbox_deliveries"] = json!(inbox);
        trial["direct_sends"] = json!(direct);
    }

    let mut groups = std::collections::BTreeMap::<(String, String, String), Value>::new();
    for trial in &trials {
        let agent = python_casefold(trial["agent_type"].as_str().unwrap_or_default().trim());
        let model = trial["model"].as_str().unwrap_or_default().trim();
        let model_key = python_casefold(model);
        let effort = python_casefold(
            trial["reasoning_effort"]
                .as_str()
                .filter(|value| !value.is_empty())
                .unwrap_or("unspecified")
                .trim(),
        );
        let group=groups.entry((agent.clone(),model_key.clone(),effort.clone())).or_insert_with(||json!({
            "agent_type":agent,"model":if model_key=="gpt-6-sol"{"GPT-6-Sol"}else{model},
            "reasoning_effort":effort,"trials":0,"completed_trials":0,"authored_bodies":0,
            "accepted_bodies":0,"completed_accepted_bodies":0,"completed_hours":0.0,
            "inbox_deliveries":0,"direct_sends":0,"metered_usd":0.0,"all_completed_costs_sourced":true
        }));
        group["trials"] = json!(group["trials"].as_i64().unwrap_or(0) + 1);
        for name in [
            "authored_bodies",
            "accepted_bodies",
            "inbox_deliveries",
            "direct_sends",
        ] {
            let current = group[name].as_i64().unwrap_or(0);
            group[name] = json!(current + trial[name].as_i64().unwrap_or(0));
        }
        if !trial["ended_at"].is_null() {
            group["completed_trials"] = json!(group["completed_trials"].as_i64().unwrap_or(0) + 1);
            group["completed_accepted_bodies"] = json!(
                group["completed_accepted_bodies"].as_i64().unwrap_or(0)
                    + trial["accepted_bodies"].as_i64().unwrap_or(0)
            );
            group["completed_hours"] = number(
                group["completed_hours"].as_f64().unwrap_or(0.0)
                    + trial["elapsed_hours"].as_f64().unwrap_or(0.0),
            );
            if let Some(cost) = trial["metered_usd"].as_f64() {
                group["metered_usd"] = number(group["metered_usd"].as_f64().unwrap_or(0.0) + cost)
            } else {
                group["all_completed_costs_sourced"] = json!(false)
            }
        }
    }
    let mut efficiency = Vec::new();
    for (_, mut group) in groups {
        let hours = group["completed_hours"].as_f64().unwrap_or(0.0);
        let accepted = group["completed_accepted_bodies"].as_i64().unwrap_or(0);
        group["completed_hours"] = number(round_even(hours, 3));
        group["accepted_per_hour"] = if hours > 0.0 {
            number(round_even(accepted as f64 / hours, 3))
        } else {
            Value::Null
        };
        group["usd_per_accepted"] = if accepted > 0 && group["all_completed_costs_sourced"] == true
        {
            number(round_even(
                group["metered_usd"].as_f64().unwrap_or(0.0) / accepted as f64,
                3,
            ))
        } else {
            Value::Null
        };
        if group["all_completed_costs_sourced"] == false {
            group["metered_usd"] = Value::Null
        }
        efficiency.push(group)
    }
    efficiency.sort_by(|a, b| {
        (
            a["agent_type"].as_str().unwrap_or_default(),
            a["model"].as_str().unwrap_or_default(),
            a["reasoning_effort"].as_str().unwrap_or_default(),
        )
            .cmp(&(
                b["agent_type"].as_str().unwrap_or_default(),
                b["model"].as_str().unwrap_or_default(),
                b["reasoning_effort"].as_str().unwrap_or_default(),
            ))
    });
    let authored_bodies = bodies
        .iter()
        .filter(|b| b["status"] == "authored" || b["status"] == "accepted")
        .count();
    let accepted_bodies = bodies.iter().filter(|b| b["status"] == "accepted").count();
    Ok(json!({
        "window_start":start,"messages_by_pane":messages,
        "message_definition":"Dunking Sheep inbox.sent plus confirmed message.sent delivery events to each pane; these are prompts/notifications, not model turns or token usage. A direct notification and its inbox item can both appear.",
        "bodies":bodies,"authored_bodies":authored_bodies,"accepted_bodies":accepted_bodies,
        "trials":trials,"efficiency_by_type_model":efficiency
    }))
}

pub(super) fn host_health(now: DateTime<Utc>, path: &Path) -> Option<Value> {
    let bytes = std::fs::read(path).ok()?;
    let state: Value = serde_json::from_slice(&bytes).ok()?;
    let sample = state.get("last_sample")?;
    let at = sample.get("at")?.as_f64()?;
    if !at.is_finite() {
        return None;
    }
    let micros = rounded_scaled_integer(at, 6)?;
    let observed = DateTime::<Utc>::from_timestamp_micros(i64::try_from(micros).ok()?)?;
    let now_seconds =
        now.timestamp() as f64 + f64::from(now.timestamp_subsec_nanos()) / 1_000_000_000.0;
    let sample_age = (now_seconds - at).max(0.0) as i64;
    let free = sample.get("tmp_free_bytes")?.as_f64()?;
    Some(json!({
        "observed_at":iso(observed),"age_s":sample_age,"stale":sample_age>180,
        "levels":state.get("levels").cloned().unwrap_or_else(||json!({})),
        "tmp_used_fraction":sample.get("tmp_used_fraction")?,
        "tmp_free_gib":round_even(free/1024_f64.powi(3),2),
        "tmp_inode_used_fraction":sample.get("tmp_inode_used_fraction")?,
        "load1":sample.get("load1")?,"load5":sample.get("load5")?,
        "cpus":sample.get("cpus")?,"cpu_psi_some_avg60":sample.get("cpu_psi_some_avg60")?,
        "process_count":sample.get("process_count").cloned().unwrap_or(Value::Null),
        "zombie_count":sample.get("zombie_count").cloned().unwrap_or(Value::Null)
    }))
}

fn nth_sunday(year: i32, month: u32, nth: u32) -> u32 {
    let first = chrono::NaiveDate::from_ymd_opt(year, month, 1).expect("valid month");
    1 + ((7 - first.weekday().num_days_from_sunday()) % 7) + 7 * (nth - 1)
}

// America/Los_Angeles offsets at UTC instants, using the US daylight-saving rule.
fn la_offset_at_utc(value: DateTime<Utc>) -> i32 {
    let year = value.year();
    let start_day = nth_sunday(year, 3, 2);
    let end_day = nth_sunday(year, 11, 1);
    let start = chrono::NaiveDate::from_ymd_opt(year, 3, start_day)
        .unwrap()
        .and_hms_opt(10, 0, 0)
        .unwrap()
        .and_utc();
    let end = chrono::NaiveDate::from_ymd_opt(year, 11, end_day)
        .unwrap()
        .and_hms_opt(9, 0, 0)
        .unwrap()
        .and_utc();
    if value >= start && value < end {
        -7 * 3600
    } else {
        -8 * 3600
    }
}

fn local_midnight_utc(year: i32, month: u32) -> Option<DateTime<Utc>> {
    let local = chrono::NaiveDate::from_ymd_opt(year, month, 1)?.and_hms_opt(0, 0, 0)?;
    let mut utc = local.and_utc() - Duration::hours(8);
    for _ in 0..2 {
        utc = local.and_utc() - Duration::seconds(la_offset_at_utc(utc) as i64)
    }
    Some(utc)
}

pub(super) fn opencode_usage(now: DateTime<Utc>, path: &Path) -> Option<Value> {
    if !path.is_file() {
        return None;
    }
    let local = now + Duration::seconds(la_offset_at_utc(now) as i64);
    let year = local.year();
    let month = local.month();
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let start = local_midnight_utc(year, month)?;
    let end = local_midnight_utc(next_year, next_month)?;
    let conn =
        Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    conn.busy_timeout(std::time::Duration::from_secs(2)).ok()?;
    let (count,total,month_cost,updated):(i64,f64,f64,Option<i64>)=conn.query_row(
        "SELECT COUNT(*),COALESCE(SUM(cost),0),COALESCE(SUM(CASE WHEN time_created>=?1 AND time_created<?2 THEN cost ELSE 0 END),0),MAX(time_updated) FROM session",
        params![start.timestamp_millis(),end.timestamp_millis()],
        |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
    ).ok()?;
    let last = updated
        .and_then(DateTime::<Utc>::from_timestamp_millis)
        .map(iso);
    Some(json!({
        "source":"OpenCode local session cost (same source as opencode stats)",
        "scope":"All local OpenCode sessions; model cost estimate, not billed overage",
        "observed_at":iso(now),"billing_month":format!("{year:04}-{month:02}"),
        "month_estimated_usd":round_even(month_cost,4),
        "all_time_estimated_usd":round_even(total,4),"session_count":count,"last_session_at":last
    }))
}

/// Produce the full source-backed summary from one already-open operations database.
pub fn summary(
    conn: &Connection,
    now: DateTime<Utc>,
    host_watch_path: &Path,
    opencode_db_path: &Path,
) -> SummaryResult<Value> {
    let run = conn
        .query_row(
            "SELECT * FROM collection_runs ORDER BY id DESC LIMIT 1",
            [],
            |row| {
                row_object(
                    row,
                    &[
                        "id",
                        "observed_at",
                        "panes_state",
                        "github_state",
                        "vp_messages_state",
                        "dunk_metrics_state",
                        "accounting_state",
                        "agent_messages_state",
                        "errors_json",
                    ],
                )
            },
        )
        .optional()?;
    let mut freshness = Map::new();
    for source in [
        "panes",
        "github",
        "vp_messages",
        "agent_messages",
        "dunk_metrics",
        "accounting",
    ] {
        let state = format!("{source}_state");
        let latest:Option<String>=conn.query_row(
            &format!("SELECT observed_at FROM collection_runs WHERE {state}='ok' ORDER BY id DESC LIMIT 1"),[],
            |row|row.get(0),
        ).optional()?;
        let last = latest.clone().map(Value::String).unwrap_or(Value::Null);
        let age_value = match latest.as_deref() {
            Some(value) => json!(age(now, value)?),
            None => Value::Null,
        };
        freshness.insert(
            source.to_owned(),
            json!({"last_ok_at":last,"age_s":age_value}),
        );
    }

    let mut staff = json!({"pm":{},"coder":{},"reviewer":{},"other":{}});
    let mut roster = Vec::new();
    if run.as_ref().is_some_and(|r| r["panes_state"] == "ok") {
        let run_id = run.as_ref().unwrap()["id"].as_i64().unwrap_or_default();
        for row in query_objects(conn,
            "SELECT role,COALESCE(agent_status,'unknown') AS status,COUNT(*) AS n FROM pane_samples WHERE run_id=?1 GROUP BY role,status",
            &[&run_id],&["role","status","n"])?{
            if let (Some(role),Some(status))=(row["role"].as_str(),row["status"].as_str()){
                staff[role][status]=row["n"].clone();
            }
        }
        roster=query_objects(conn,
            "SELECT pane_id,role,agent_kind,agent_status,agent_name,lane_name FROM pane_samples WHERE run_id=?1 AND project=?2 ORDER BY pane_id",
            &[&run_id,&PROJECT],&["pane_id","role","agent_kind","agent_status","agent_name","lane_name"])?;
    }

    let mut quotas = json!({"codex":null,"claude":null});
    for provider in ["codex", "claude"] {
        let latest=conn.query_row(
            "SELECT * FROM quota_observations WHERE project=?1 AND provider=?2 ORDER BY observed_at DESC,id DESC LIMIT 1",
            params![PROJECT,provider],|row|row_object(row,&["pane_id","cohort","observed_at","precision","weekly_percent_left","reset_at","evidence_ref"]),
        ).optional()?;
        let Some(latest) = latest else { continue };
        let mut item = json!({
            "pane":latest["pane_id"],"provider":provider,"cohort":latest["cohort"],
            "observed_at":latest["observed_at"],"age_s":age(now,latest["observed_at"].as_str().unwrap_or_default())?,
            "precision":latest["precision"],"weekly_percent_left":latest["weekly_percent_left"],
            "reset_at":latest["reset_at"],"evidence_ref":latest["evidence_ref"],
            "burn_percentage_points_per_hour":null
        });
        let prior=conn.query_row(
            "SELECT weekly_percent_left,observed_at FROM quota_observations WHERE project=?1 AND provider=?2 AND cohort=?3 AND reset_at=?4 AND observed_at<?5 ORDER BY observed_at DESC,id DESC LIMIT 1",
            params![PROJECT,provider,latest["cohort"].as_str().unwrap_or_default(),latest["reset_at"].as_str().unwrap_or_default(),latest["observed_at"].as_str().unwrap_or_default()],
            |row|Ok((row.get::<_,f64>(0)?,row.get::<_,String>(1)?)),
        ).optional()?;
        if let Some((percent, at)) = prior {
            let hours = (parse_time(latest["observed_at"].as_str().unwrap_or_default())?
                - parse_time(&at)?)
            .num_milliseconds() as f64
                / 3_600_000.0;
            if hours >= 0.5 {
                item["burn_percentage_points_per_hour"] = number(round_even(
                    (percent - latest["weekly_percent_left"].as_f64().unwrap_or(0.0)) / hours,
                    2,
                ))
            }
        }
        quotas[provider] = item;
    }
    let quota = quotas
        .as_object()
        .and_then(|map| {
            map.values()
                .filter(|v| !v.is_null())
                .max_by_key(|v| v["observed_at"].as_str().unwrap_or_default())
                .cloned()
        })
        .unwrap_or(Value::Null);

    let mut subscriptions=query_objects(conn,
        "SELECT provider,monthly_usd_cents,observed_at,evidence_ref FROM subscription_costs s WHERE project=?1 AND id=(SELECT MAX(id) FROM subscription_costs WHERE project=s.project AND provider=s.provider) ORDER BY provider",
        &[&PROJECT],&["provider","monthly_usd_cents","observed_at","evidence_ref"])?;
    for item in &mut subscriptions {
        let cents = item["monthly_usd_cents"].as_i64().unwrap_or_default();
        let provider = item["provider"].clone();
        let observed_at = item["observed_at"].clone();
        let evidence_ref = item["evidence_ref"].clone();
        *item = json!({
            "provider": provider,
            "observed_at": observed_at,
            "evidence_ref": evidence_ref,
            "monthly_usd": number(cents as f64 / 100.0)
        });
    }
    let monthly = if subscriptions.is_empty() {
        Value::Null
    } else {
        number(
            subscriptions
                .iter()
                .map(|v| v["monthly_usd"].as_f64().unwrap_or(0.0))
                .sum(),
        )
    };
    let first = chrono::NaiveDate::from_ymd_opt(now.year(), now.month(), 1).unwrap();
    let (next_year, next_month) = if now.month() == 12 {
        (now.year() + 1, 1)
    } else {
        (now.year(), now.month() + 1)
    };
    let days =
        (chrono::NaiveDate::from_ymd_opt(next_year, next_month, 1).unwrap() - first).num_days();
    let calendar_hours = days * 24;
    let fixed_hourly = monthly
        .as_f64()
        .map(|v| number(round_even(v / calendar_hours as f64, 4)))
        .unwrap_or(Value::Null);

    let goals=query_objects(conn,
        "SELECT scope,goal_key,observed_at,status,value,target,unit,evidence_ref,note FROM goal_observations g WHERE project=?1 AND id=(SELECT MAX(id) FROM goal_observations WHERE project=g.project AND goal_key=g.goal_key) ORDER BY scope,goal_key",
        &[&PROJECT],&["scope","goal_key","observed_at","status","value","target","unit","evidence_ref","note"])?;
    let accounting=conn.query_row(
        "SELECT * FROM accounting_snapshots WHERE project=?1 ORDER BY observed_at DESC,id DESC LIMIT 1",
        [&PROJECT],|row|row_object(row,&["observed_at","commit_oid","strict_full_c","verified_game_identities","ceiling","bound_kind","source_signature","evidence_ref"]),
    ).optional()?;
    let mut progress = Value::Null;
    let mut completion = Value::Null;
    if let Some(item) = accounting {
        let ceiling = item["ceiling"].as_i64().unwrap_or_default();
        let strict = item["strict_full_c"].as_i64().unwrap_or_default();
        let identities = item["verified_game_identities"]
            .as_i64()
            .unwrap_or_default();
        let upper = item["bound_kind"] == "upper_bound";
        let percent = |n: i64| {
            if upper {
                ((n * 1000) / ceiling) as f64 / 10.0
            } else {
                round_even(100.0 * n as f64 / ceiling as f64, 1)
            }
        };
        progress = json!({
            "strict_full_c":strict,"verified_game_identities":identities,"ceiling":ceiling,
            "bound_kind":item["bound_kind"],"strict_percent":percent(strict),"identity_percent":percent(identities),
            "observed_at":item["observed_at"],"age_s":age(now,item["observed_at"].as_str().unwrap_or_default())?,
            "commit_oid":item["commit_oid"],"evidence_ref":item["evidence_ref"]
        });
        let end = parse_time(item["observed_at"].as_str().unwrap_or_default())?;
        let from = iso(end - Duration::hours(24));
        let through = iso(end - Duration::hours(1));
        let prior=conn.query_row(
            "SELECT observed_at,strict_full_c FROM accounting_snapshots WHERE project=?1 AND observed_at<=?2 AND observed_at>=?3 AND ceiling=?4 AND bound_kind=?5 AND source_signature=?6 ORDER BY observed_at ASC,id ASC LIMIT 1",
            params![PROJECT,through,from,ceiling,item["bound_kind"].as_str().unwrap_or_default(),item["source_signature"].as_str().unwrap_or_default()],
            |row|Ok((row.get::<_,String>(0)?,row.get::<_,i64>(1)?)),
        ).optional()?;
        if let Some((prior_at, prior_count)) = prior {
            if strict >= prior_count {
                let elapsed =
                    (end - parse_time(&prior_at)?).num_milliseconds() as f64 / 3_600_000.0;
                let delta = strict - prior_count;
                completion = json!({
                    "strict_functions_per_hour":round_even(delta as f64/elapsed,4),
                    "function_coverage_points_per_hour":round_even(100.0*delta as f64/ceiling as f64/elapsed,4),
                    "delta_strict_functions":delta,"window_hours":round_even(elapsed,2),
                    "window_start":prior_at,"window_end":item["observed_at"],
                    "ceiling":ceiling,"bound_kind":item["bound_kind"]
                });
            }
        }
    }
    let prs=query_objects(conn,
        "SELECT number,state,merged_at,last_seen_at FROM pr_latest WHERE project=?1 ORDER BY number DESC LIMIT 12",
        &[&PROJECT],&["number","state","merged_at","last_seen_at"])?;
    let telemetry=conn.query_row(
        "SELECT generated_at,interval_start,interval_end,schema_version,payload_json FROM dunk_metrics_snapshots ORDER BY id DESC LIMIT 1",
        [],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,i64>(3)?,row.get_ref(4)?.to_owned())),
    ).optional()?;
    let telemetry = match telemetry {
        Some((generated, start, end, version, payload)) => Some(json!({
            "generated_at":generated,"age_s":age(now,&generated)?,
            "interval":{"start":start,"end":end},"schema_version":version,
            "data":match payload {
                rusqlite::types::Value::Text(payload) => serde_json::from_str::<Value>(&payload)?,
                rusqlite::types::Value::Blob(payload) => serde_json::from_slice::<Value>(&payload)?,
                _ => return Err(SummaryError::InvalidPayloadType),
            }
        })),
        None => None,
    };
    let mut vp_counts = Vec::new();
    if run.as_ref().is_some_and(|r| r["vp_messages_state"] == "ok") {
        let run_id = run.as_ref().unwrap()["id"].as_i64().unwrap_or_default();
        vp_counts=query_objects(conn,
            "SELECT channel,role,total FROM vp_message_counts WHERE run_id=?1 ORDER BY channel,role",
            &[&run_id],&["channel","role","total"])?;
    }
    let sources = if let Some(run) = &run {
        let mut map = Map::new();
        for name in [
            "panes",
            "github",
            "vp_messages",
            "agent_messages",
            "dunk_metrics",
            "accounting",
        ] {
            map.insert(name.to_owned(), run[format!("{name}_state")].clone());
        }
        Value::Object(map)
    } else {
        json!({})
    };
    let latest_sample = run
        .as_ref()
        .map(|r| r["observed_at"].clone())
        .unwrap_or(Value::Null);
    let latest_sample_age = match latest_sample.as_str() {
        Some(value) => json!(age(now, value)?),
        None => Value::Null,
    };
    let experiment = experiment_summary(conn, now)?;
    let now_iso = iso(now);
    Ok(json!({
        "generated_at":now_iso,"project":PROJECT,
        "latest_sample_at":latest_sample,"latest_sample_age_s":latest_sample_age,
        "sources":sources,"source_freshness":freshness,"staff":staff,"pane_roster":roster,
        "quota":quota,"quotas":quotas,"subscriptions":subscriptions,
        "monthly_fixed_usd":monthly,"fixed_subscription_usd_per_calendar_hour":fixed_hourly,
        "calendar_hours_in_month":calendar_hours,"billing_month":format!("{:04}-{:02}",now.year(),now.month()),
        "usage_cost_usd":null,"opencode_usage":opencode_usage(now,opencode_db_path),
        "host_health":host_health(now,host_watch_path),"goals":goals,
        "function_progress":progress,"completion_rate":completion,
        "tracked_recent_prs":prs,"vp_message_counts":vp_counts,"dunk_metrics":telemetry,
        "experiment_lab":experiment,
        "limitations":[
            "Pane state is sampled presence, not verified productive work or token consumption.",
            "PR list tracks recent visible PRs, not a whole-game completion denominator.",
            "Quota burn is percentage points per hour only after two direct readings at least 30 minutes apart with one reset.",
            "Usage-based money and token cost are unknown without a billing source.",
            "Fixed subscription totals are owner-reported and exclude unknown taxes, add-ons, and usage charges.",
            "The hourly price divides fixed monthly subscriptions by calendar hours, not coder hours or metered usage.",
            "Completion speed is change in PM-reviewed strict C credit divided by the actual interval, using the same reviewed function bound; it is not whole-game completion speed.",
            "Agent delivery events count prompts and inbox items delivered to a pane; they do not count model turns, and notification sends can duplicate inbox items.",
            "Function body trial records are append-only local evidence and remain separate from the PM-reviewed published 815 denominator."
        ]
    }))
}
